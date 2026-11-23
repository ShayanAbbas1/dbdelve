//! The one place a profile's agent visibility changes.
//!
//! One door, because the sweep of the agent calls still pending against a
//! profile has to run exactly when the profile is hidden, and a second writer
//! would be one more path to tell about it. Granting is the asymmetric half:
//! it needs the owner's confirmation, so it is refused unless the visibility
//! dialog is open for that very profile.
//!
//! The other half is the bridge: what an agent's call becomes once the socket
//! thread has handed it over. Everything a call does happens here, on the
//! foreground, reached only through [`Workspace::on_agent_message`].

use std::{
    collections::VecDeque,
    sync::mpsc::SyncSender,
    time::{Duration, Instant},
};

use super::agent_read::{self, AgentWork, Approved, Resolved, explain_statement};
use super::agent_write;
use super::*;
use crate::{
    agent::{CallId, EXPIRY, Inbound, Reply, Status, result::Total, tools::Tool},
    db::ConnectionConfig,
    sql::{AgentVerdict, ApprovalKind, agent_gate, approval_kind, creates_index_concurrently},
    ui::size_fact,
};

/// What the agent bridge holds while it is running.
#[derive(Default)]
pub(crate) struct AgentState {
    /// The socket and the task that drains it, or nothing when no bridge runs.
    /// Dropping it is what stops listening.
    pub(crate) bridge: Option<Bridge>,
    /// The presence connections open now: agents attached, whether or not they
    /// have asked anything.
    pub(crate) presence: Vec<crate::agent::ConnId>,
    /// The calls waiting on an answer from a person, found by their id and
    /// never by their place in this list. Reads are not here: they wait for
    /// nobody, and live in their [`agent_read::Lane`].
    pub(crate) calls: Vec<PendingCall>,
    /// What this window's own access action decided and no save has written
    /// yet: `true` from turning access on, `false` from the kill switch. Set
    /// by those two actions only, and cleared by the next save that writes
    /// the file, so it is the one way this window changes `agent_access`.
    pub(crate) access_choice: Option<bool>,
    /// The profiles, by id, whose `agent_visible` this window changed and no
    /// save has written yet; a failed write keeps them. Every other profile's
    /// flag is the file's: see
    /// [`Workspace::adopt_saved_visibility`].
    pub(crate) visibility_choices: HashSet<String>,
    /// The calls that have ended, newest first, at most [`AUDIT_CAP`].
    pub(crate) log: VecDeque<LogEntry>,
    /// [`store::profiles_stamp`] as [`Workspace::adopt_saved_visibility`] last
    /// found it, taken before that read.
    pub(crate) visibility_stamp: Option<(std::time::SystemTime, u64)>,
    /// The agent's own connections, by profile id: never the user's.
    pub(crate) lanes: HashMap<String, agent_read::Lane>,
    /// What opens an agent's connection; `None` is the real open. Tests put
    /// one here to stand in for an open that never returns.
    pub(crate) opener: Option<agent_read::Opener>,
    /// What runs an approved write; `None` is [`agent_write::open_for_write`].
    /// Tests put one here to hold a write running.
    pub(crate) write_runner: Option<agent_write::WriteRunner>,
    /// The approved writes running now, found by their id. Each holds no
    /// connection and no way to cancel it: see [`agent_write`].
    pub(crate) writes: Vec<agent_write::Writing>,
    /// The call whose Approve is live: the approval in front, once
    /// [`ARM_DELAY`] has passed since it was shown. At most one.
    pub(crate) armed: Option<CallId>,
    /// The approval [`Workspace::reveal_approval`] last started the arm for.
    pub(crate) revealed: Option<CallId>,
    /// The last call whose reveal asked the OS for the person's attention. Calls
    /// are shown oldest first and ids rise, so a call that never asked is one
    /// other than this.
    pub(crate) attended: Option<CallId>,
    /// The one-shot timer that sets `armed`. Replaced, and so cancelled, each
    /// time the dialog in front is revealed.
    _arming: Option<gpui::Task<()>>,
    /// Unix milliseconds when this window's bridge started, or 0 when none
    /// has: with a call's id, what pairs the lines of the audit file. A call's
    /// id is the counter of its connection, which starts again each run.
    pub(crate) session: u64,
    /// The file approved statements are recorded in.
    #[cfg(unix)]
    audit: store::AuditLog,
}

// ponytail: the wait between an approval being shown and its Approve working,
// a decision and not a setting: long enough that a click or keystroke already
// in flight when the dialog appeared cannot land on an Approve that did not
// exist when it began. The upgrade is a setting.
pub(crate) const ARM_DELAY: Duration = Duration::from_millis(1500);

/// What an approval that was approved is told when the line that says it is
/// about to run could not be written. Nothing was opened.
pub(super) const NOT_RECORDED: &str =
    "DBDelve could not record the statement before running it, so it was not run.";

/// What an approval that was approved is told when the statement, the
/// connection or the profile's access level is not what it was when the person
/// was shown it. Nothing was opened.
pub(super) const CHANGED_SINCE_SHOWN: &str = "The statement, the connection or its access level changed after it was shown, so it was not run.";

// ponytail: the list the Agent activity dialog reads is this session's own and
// keeps the last five hundred calls, in memory only, because a longer list is
// not read. `agent-audit.jsonl` is what outlives the window.
pub(crate) const AUDIT_CAP: usize = 500;

/// How many characters of an agent's own statement an audit entry keeps.
pub(super) const AUDIT_SQL_CAP: usize = 4096;

/// What an entry's SQL ends with once it was cut at [`AUDIT_SQL_CAP`].
pub(super) const SQL_CLIPPED: &str = " [clipped]";

/// What was decided about a statement before it ran, as the audit says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AuditVerdict {
    /// The gate let it through and it ran on the agent's own connection.
    Run,
    /// The gate held it for a person: the lowest mode that lets it run, and the
    /// expiry text of a statement that schedules a removal.
    Approve { mode: Mode, expiry: Option<String> },
    /// The gate refused it, for this reason.
    Refuse(String),
}

impl AuditVerdict {
    /// The line the Agent activity dialog shows for it.
    pub(crate) fn line(&self) -> String {
        match self {
            Self::Run => "Ran".to_string(),
            Self::Approve {
                mode,
                expiry: Some(expiry),
            } => format!("Needs approval at {}. {expiry}", mode.label()),
            Self::Approve { mode, expiry: None } => {
                format!("Needs approval at {}", mode.label())
            }
            Self::Refuse(reason) => format!("Refused: {reason}"),
        }
    }

    /// What the audit says of a gate verdict that held or refused a statement;
    /// `None` for `Run`, which the read itself gives its verdict.
    pub(super) fn held_by(verdict: AgentVerdict) -> Option<Self> {
        match verdict {
            AgentVerdict::Run => None,
            AgentVerdict::Approve(verdict, expiry) => Some(Self::Approve {
                mode: verdict.mode,
                expiry: expiry.map(|expiry| expiry.to_string()),
            }),
            AgentVerdict::Refuse(reason) => Some(Self::Refuse(reason.to_string())),
        }
    }
}

/// What a call knew about itself at its end, beyond who made it and how it
/// ended. A call that has none of these, such as a catalog listing, takes the
/// default.
#[derive(Default)]
pub(crate) struct CallFacts {
    pub(crate) verdict: Option<AuditVerdict>,
    /// The statement exactly as the agent sent it.
    pub(crate) sql: Option<String>,
    /// How many rows went back to the agent, and how many there were.
    pub(crate) shown: Option<usize>,
    pub(crate) total: Option<Total>,
    /// Whole milliseconds from the call's arrival to its end.
    pub(crate) elapsed_ms: u64,
    /// What the server said of a call that failed, as [`audited_error`] keeps
    /// it, with any sentence of DBDelve's own after it.
    pub(crate) error: Option<String>,
    /// The caller hung up before the call ended.
    pub(crate) requester_gone: bool,
    /// What the person decided about a statement that waited for them. It also
    /// keeps the audit file's statement whole.
    pub(crate) decision: Option<Decision>,
    /// The engine of the profile the call was about, for the audit file.
    pub(crate) engine: Option<Engine>,
    /// The bridge session the call was accepted in, for a call that can
    /// outlive its bridge (an approved write); `None` is the current one.
    pub(crate) session: Option<u64>,
}

/// What a person did with a statement that waited for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Decision {
    Approved,
    /// The person's Deny.
    Rejected,
    Expired,
    /// DBDelve took the question back before the person answered: its profile
    /// was hidden from agents, or agent access was turned off.
    Withdrawn,
}

/// One line of `agent-audit.jsonl`, the record of every call an agent made. It
/// names the statement, who decided when a person did, and how the call ended, and
/// never a row value: what the database held does not belong in a file kept
/// for longer than a session. An error is kept as [`audited_error`] has it,
/// for that reason; a value a server writes unquoted in its own words is the
/// one kind the cut cannot tell apart. `v` leads so a later shape can be told
/// apart.
/// JSON escapes every newline, so text an agent sent cannot start a line.
#[derive(serde::Serialize)]
pub(super) struct AuditLine<'a> {
    v: u8,
    session: u64,
    id: u64,
    at: u64,
    /// `started` before a statement is sent, `outcome` once the call ended.
    phase: &'static str,
    tool: Tool,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    engine: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<Decision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<Status>,
    requester_gone: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    shown: Option<usize>,
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    /// The statement whole when a person approved it, and cut at
    /// [`AUDIT_SQL_CAP`] characters otherwise: clipping the record of what a
    /// person approved would defeat it.
    #[serde(skip_serializing_if = "Option::is_none")]
    sql: Option<&'a str>,
}

/// One call that ended: its names, its code, its time and what the agent sent.
/// Never a row: the audit shows that a read happened and how much came back,
/// not what was in it. Held in memory and gone with the window.
#[derive(Clone, Debug)]
pub(crate) struct LogEntry {
    /// Unix milliseconds when the call ended, as the palette's history keeps
    /// its own.
    pub(crate) at: u64,
    pub(crate) id: CallId,
    pub(crate) tool: Tool,
    /// The profile's own name, as it read when the call resolved to it.
    pub(crate) resolved: Option<String>,
    /// What the agent called a connection that resolved to nothing: cut to 64
    /// characters with control characters removed, and shown as the agent's
    /// words, never as DBDelve's.
    pub(crate) asked: Option<String>,
    pub(crate) status: Status,
    /// The answer could not be delivered, because its caller had already gone.
    pub(crate) requester_gone: bool,
    /// `None` for the tools that carry no statement.
    pub(crate) verdict: Option<AuditVerdict>,
    /// The agent's statement, cut at [`AUDIT_SQL_CAP`] characters.
    pub(crate) sql: Option<String>,
    pub(crate) shown: Option<usize>,
    pub(crate) total: Option<Total>,
    pub(crate) elapsed_ms: u64,
    /// The error of a call that failed, as [`CallFacts::error`] has it, cut at
    /// [`AUDIT_SQL_CAP`] characters.
    pub(crate) error: Option<String>,
    /// What the person decided, for a call that waited for one.
    pub(crate) decision: Option<Decision>,
}

impl LogEntry {
    /// Whether the agent was given fewer rows than there were, or than the
    /// statement could have returned.
    pub(crate) fn capped(&self) -> bool {
        match (self.shown, self.total) {
            (_, Some(Total::AtLeast(_))) => true,
            (Some(shown), Some(Total::Exact(total))) => total > shown,
            _ => false,
        }
    }
}

/// How many characters of an agent's own text the log keeps.
const ASKED_CAP: usize = 64;

pub(crate) fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Whole milliseconds since `arrived`, truncated.
pub(crate) fn elapsed_ms(arrived: Instant) -> u64 {
    arrived.elapsed().as_millis() as u64
}

impl AgentState {
    /// Write one ended call at the front of the log. Called once per call, from
    /// the places that answer it. Entries are ordered by insertion and never by
    /// their time, so two calls that end in the same millisecond keep the order
    /// they ended in.
    pub(super) fn record(
        &mut self,
        id: CallId,
        tool: Tool,
        resolved: Option<String>,
        asked: Option<&str>,
        status: Status,
        facts: CallFacts,
    ) {
        // What the in-memory log keeps of the statement and of an error that
        // may echo it, cut so the dialog stays cheap to draw.
        let cut_sql = facts.sql.clone().map(clipped);
        let cut_error = facts.error.clone().map(clipped);
        // The one place an outcome line is written, so the file and the log say
        // the same of every call, whether it waited or not. A call that waited
        // for a person keeps its statement whole, since cutting the record of
        // what was approved would defeat it; a plain read keeps the cut.
        let waited = facts.decision.is_some();
        let line = AuditLine {
            v: 1,
            session: facts.session.unwrap_or(self.session),
            id: id.0,
            at: unix_millis(),
            phase: "outcome",
            tool,
            profile: resolved.as_deref(),
            engine: facts.engine.map(Engine::as_str),
            verdict: facts.verdict.as_ref().map(AuditVerdict::line),
            decision: facts.decision,
            status: Some(status),
            requester_gone: facts.requester_gone,
            shown: facts.shown,
            elapsed_ms: facts.elapsed_ms,
            error: if waited {
                facts.error.as_deref()
            } else {
                cut_error.as_deref()
            },
            sql: if waited {
                facts.sql.as_deref()
            } else {
                cut_sql.as_deref()
            },
        };
        if let Err(error) = self.persist(&line) {
            // The call has ended and nothing can un-run it, so the only thing
            // left to do is say that its record is missing. The error names
            // the file and never the statement.
            eprintln!("{error}");
        }
        self.log.push_front(LogEntry {
            at: unix_millis(),
            id,
            tool,
            resolved,
            asked: asked.map(|text| {
                text.chars()
                    .filter(|c| !c.is_control())
                    .take(ASKED_CAP)
                    .collect()
            }),
            status,
            requester_gone: facts.requester_gone,
            verdict: facts.verdict,
            sql: cut_sql,
            shown: facts.shown,
            total: facts.total,
            elapsed_ms: facts.elapsed_ms,
            error: cut_error,
            decision: facts.decision,
        });
        self.log.truncate(AUDIT_CAP);
    }
}

impl AgentState {
    /// Write the `started` line of a call that is about to run on a lane and
    /// that no person approved (an approved one has its own, written before it
    /// was handed on). Not fail-closed: a read changes nothing, so one whose
    /// line could not be written still runs, and the failure names the file and
    /// never the statement.
    pub(super) fn note_started(
        &mut self,
        id: CallId,
        tool: Tool,
        profile: &str,
        engine: Engine,
        sql: Option<&str>,
        arrived: Instant,
    ) {
        let cut = sql.map(|sql| clipped(sql.to_string()));
        let line = AuditLine {
            v: 1,
            session: self.session,
            id: id.0,
            at: unix_millis(),
            phase: "started",
            tool,
            profile: Some(profile),
            engine: Some(engine.as_str()),
            verdict: sql.map(|_| AuditVerdict::Run.line()),
            decision: None,
            status: None,
            requester_gone: false,
            shown: None,
            elapsed_ms: elapsed_ms(arrived),
            error: None,
            sql: cut.as_deref(),
        };
        if let Err(error) = self.persist(&line) {
            eprintln!("{error}");
        }
    }

    /// Write one line to the audit file. The one door to it, so the writes of
    /// the two phases cannot disagree about the file.
    #[cfg(unix)]
    pub(super) fn persist(&mut self, line: &AuditLine) -> Result<(), String> {
        let json = serde_json::to_string(line)
            .map_err(|error| format!("Could not encode the audit line: {error}"))?;
        self.audit.append(&json)
    }

    /// No agent call reaches a window here: the bridge listens only on Unix.
    #[cfg(not(unix))]
    pub(super) fn persist(&mut self, _line: &AuditLine) -> Result<(), String> {
        Ok(())
    }
}

/// `text` cut at [`AUDIT_SQL_CAP`] characters, on a character boundary: a
/// statement, or an error that may echo one. The dialog draws every entry on
/// every frame.
fn clipped(mut text: String) -> String {
    if let Some((cut, _)) = text.char_indices().nth(AUDIT_SQL_CAP) {
        text.truncate(cut);
        text.push_str(SQL_CLIPPED);
    }
    text
}

/// What the audit keeps of what a server said about a call that failed: its
/// first line, up to where it starts to quote or bracket something, with `…`
/// in place of the rest. That is where a server puts a value it names, and the
/// value may be a row's: Postgres's `DETAIL` (`Failing row contains (…)`) is a
/// later line, MySQL writes `Duplicate entry '…'`, SQL Server `(…)`, MongoDB
/// `{ … }`, and a read that casts a column quotes the column's value. A quote
/// between two letters is an apostrophe. `server_text` is already
/// [`agent_read::scrubbed`], so a host or a user written unquoted is gone too.
pub(crate) fn audited_error(server_text: &str) -> String {
    let line = server_text.lines().next().unwrap_or_default();
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let letter = |at: Option<&(usize, char)>| at.is_some_and(|(_, c)| c.is_alphabetic());
    let cut = chars.iter().enumerate().find_map(|(index, &(at, c))| {
        let opens = match c {
            // Not `[`: the scrubber's own `[hidden]` is written with it.
            '"' | '`' | '(' | '{' => true,
            '\'' => {
                !(letter(index.checked_sub(1).and_then(|before| chars.get(before)))
                    && letter(chars.get(index + 1)))
            }
            _ => false,
        };
        opens.then_some(at)
    });
    match cut {
        Some(at) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

/// A call that has been accepted and not yet finished. Dropping it without
/// replying closes the caller's connection with no answer.
pub(crate) struct PendingCall {
    pub(crate) id: CallId,
    /// The profile the call's connection name resolved to when it arrived. The
    /// dialog and the sweeps read the profile by this id, so a rename or a
    /// second profile of the same name cannot change which one a call is about.
    pub(crate) profile_id: String,
    /// The profile's own name when the call resolved, kept so the log can name
    /// it after the profile is gone.
    profile_name: String,
    reply: std::sync::mpsc::SyncSender<Reply>,
    /// When the call reached [`Workspace::on_agent_message`].
    arrived: Instant,
    /// What the person is asked to approve: the statement the gate held.
    pub(crate) approval: Approval,
    /// Ends the call as `expired` when it fires. Dropped with the call, which
    /// is what cancels it.
    _expiry: gpui::Task<()>,
}

/// The statement a held call asks to run, with what the gate and the profile
/// said of it when it arrived. `sql` is exactly what the agent sent: what the
/// dialog draws and what runs, never a copy that was cleaned for the screen.
pub(crate) struct Approval {
    pub(crate) sql: String,
    /// The gate's answer at arrival, which the re-gate at Approve must
    /// reproduce.
    pub(crate) verdict: AgentVerdict,
    /// The profile's config at arrival, compared with the current one at
    /// Approve.
    pub(crate) config: ConnectionConfig,
    /// Whether the statement holds the profile's SQLite file against the
    /// user's own readers and writers while it runs, read once when the call
    /// arrived.
    pub(crate) locks_the_file: bool,
    /// Set by the socket's hangup watcher the moment the caller leaves, before
    /// the window hears of it. Read at Approve, so a click that lands in that
    /// gap starts nothing.
    pub(crate) gone: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// What a pending call is told when the kill switch ends it.
const TURNED_OFF: &str = "Agent access was turned off in DBDelve.";

/// Said when the kill switch could not save `agent_access = false`: calls are
/// refused, but the socket stays bound, and so does the lock, until it can.
const NOT_TURNED_OFF: &str = "Agent access could not be saved as off, so DBDelve keeps the socket and refuses every call; use the kill switch again to retry.";

/// What the kill switch says in a window that is not serving while another
/// window of the variant is. Its sentence is what stays true: that window's
/// agents are not disconnected.
const STILL_SERVED_ELSEWHERE: &str = "Agent access was turned off. Another DBDelve window is still serving agents; they stay connected until that window closes or its own kill switch is used.";

/// What every call waiting on a profile is told when it is hidden or removed.
/// One sentence for both, so the agent cannot tell which happened.
pub(super) const NO_LONGER_AVAILABLE: &str = "The connection is no longer available to agents.";

/// What an unanswered approval is told. The wait it names is [`EXPIRY`] itself.
fn expired_text() -> String {
    format!(
        "Nobody answered the approval in DBDelve within {} minutes; the statement was not run.",
        EXPIRY.as_secs() / 60
    )
}

/// What a denied call is told: the person read the statement and said no, and
/// nothing ran.
const DENIED: &str = "The statement was denied in DBDelve and was not run.";

/// A running bridge. Neither field is read: holding them is the point.
pub(crate) struct Bridge {
    _listener: crate::agent::Listener,
    _pump: gpui::Task<()>,
}

/// One entry of `list_connections`: a name and an engine, and nothing that
/// says where the database is or how to get in.
#[derive(serde::Serialize)]
struct Listed<'a> {
    name: &'a str,
    engine: &'static str,
}

/// The `list_connections` answer: the JSON array of `visible`, held to the
/// reply budget as every other answer is, since a name has no length limit.
/// When they do not all fit it is an object instead, the connections that fit
/// and how many were left out, so the agent is told rather than handed a list
/// it would take for the whole.
fn listed_text(visible: &[Listed]) -> String {
    let encoded = |listed: &[Listed]| serde_json::to_string(listed).unwrap_or_default();
    let whole = encoded(visible);
    if whole.len() <= crate::agent::result::RESULT_BUDGET {
        return whole;
    }
    let mut fit = visible.len();
    loop {
        let text = serde_json::json!({
            "connections": visible[..fit],
            "omitted": visible.len() - fit,
            "note": "Not every connection fits in one answer: their names are too long.",
        })
        .to_string();
        if text.len() <= crate::agent::result::RESULT_BUDGET || fit == 0 {
            return text;
        }
        fit -= 1;
    }
}

/// Why a profile name did not resolve for an agent. Two answers only, and
/// neither echoes the name asked for: the text goes back to the agent as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentLookup {
    /// No agent-visible profile has that name. A hidden profile, a missing one,
    /// one this build cannot open and a near-miss all come back as this, so the
    /// answer says nothing about which of them it was.
    NotAvailable,
    /// More than one agent-visible profile has that name. Counts visible
    /// profiles only, so it discloses nothing about hidden ones.
    Ambiguous,
}

impl std::fmt::Display for AgentLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAvailable => "no agent-visible connection has that name",
            Self::Ambiguous => "more than one agent-visible connection has that name",
        })
    }
}

/// What `agent_access` holds in the file right now; see
/// [`Workspace::saved_agent_access`].
fn saved_agent_access(plugins: &[&'static crate::db::Plugin]) -> Option<bool> {
    store::peek_profiles(plugins)
        .and_then(|list| list.settings)
        .and_then(|settings| settings.agent_access)
}

/// What the visibility dialog tells the owner about this engine and mode, one
/// entry per sentence. A pure function of the two, so opening the dialog asks
/// no server anything: the hold is what the engine is built to do while the
/// profile is Read-only, not what a live role allows. The sentence about a call
/// to a function DBDelve does not list is what `agent_gate` answers such a call
/// in that mode, and is left out where the gate refuses it. `read_can_block_commits`
/// is [`db::ConnectionConfig::agent_read_can_block_commits`], read by the caller
/// so this stays pure.
pub(crate) fn visibility_facts(
    engine: Engine,
    mode: Mode,
    read_can_block_commits: bool,
) -> Vec<String> {
    let mut facts = vec![
        format!("This connection's access level is {}.", mode.label()),
        engine.read_only_hold().to_string(),
        "Rows the agent reads leave this machine.".to_string(),
    ];
    // MongoDB and Redis have no function call to ask about, and Generic at
    // Read-only refuses one, so neither sentence is true there.
    if engine.syntax() == db::Syntax::Sql {
        if mode >= Mode::ReadWrite {
            facts.push(
                "Calls to functions DBDelve does not list wait for your approval.".to_string(),
            );
        } else if !engine.reads_only_listed_calls() {
            facts
                .push("Calls to functions DBDelve does not list run without approval.".to_string());
        }
    }
    if read_can_block_commits {
        facts.push(
            "An agent's read in progress can make your commit on this file wait, and then fail \
             with \"database is locked\"."
                .to_string(),
        );
    }
    facts
}

/// What the approval says of where the statement runs.
pub(crate) const OWN_CONNECTION: &str = "Runs on DBDelve's own connection, not yours.";
/// What the approval says of a concurrent index build.
pub(crate) const INVALID_INDEX: &str =
    "A concurrent index build that stops can leave an invalid index of that name on the server.";
const LOCKS_THE_FILE: &str =
    "While it runs it locks the file, and your own reads and writes on it wait.";
/// What the approval says of a read that runs where the database holds the
/// agent's connection to reads, and of one where nothing does.
pub(crate) const HELD_TO_READS: &str = "It runs on a read-only connection.";
pub(crate) const NOT_HELD_TO_READS: &str = "Nothing on the server holds it to reads.";

/// What DBDelve itself says of a statement it holds for approval, ahead of the
/// statement: the access level the profile has and the one the statement needs,
/// what kind of statement it is, any expiry it schedules or invalid index it
/// can leave, whose connection it runs on, and its size. Pure, so opening the
/// dialog asks no server anything, and every sentence is DBDelve's: the
/// statement is never formatted into one, because the agent wrote it.
/// `locks_the_file` is [`Approval::locks_the_file`], and `reads_held`
/// [`ConnectionConfig::agent_reads_are_held`] of the profile: a read the gate
/// holds runs on the agent's read connection, and what holds that connection
/// to reads is the engine's.
pub(crate) fn approval_facts(
    engine: Engine,
    mode: Mode,
    verdict: &AgentVerdict,
    sql: &str,
    locks_the_file: bool,
    reads_held: bool,
) -> Vec<String> {
    // Only a held statement has an approval; nothing else is described.
    let AgentVerdict::Approve(held, expiry) = verdict else {
        return Vec::new();
    };
    let kind = approval_kind(engine, sql, held);
    let mut facts = vec![
        format!(
            "This connection's access level is {}; the statement needs {}.",
            mode.label(),
            held.mode.label()
        ),
        match kind {
            ApprovalKind::Write => "It changes data.",
            ApprovalKind::EgressRead => "It reads a file, a URL or another server.",
            ApprovalKind::UnlistedCall => "It calls a function DBDelve does not list.",
        }
        .to_string(),
    ];
    if kind != ApprovalKind::Write {
        facts.push(
            if reads_held {
                HELD_TO_READS
            } else {
                NOT_HELD_TO_READS
            }
            .to_string(),
        );
    }
    facts.extend(expiry.map(|expiry| expiry.to_string()));
    if creates_index_concurrently(engine, sql) {
        facts.push(INVALID_INDEX.to_string());
    }
    if locks_the_file && kind == ApprovalKind::Write {
        facts.push(LOCKS_THE_FILE.to_string());
    }
    facts.push(OWN_CONNECTION.to_string());
    facts.push(size_fact(sql));
    facts
}

impl Workspace {
    /// Put up the dialog that makes a profile visible to agents. Changes no
    /// flag: only [`Workspace::confirm_agent_visibility`] does. The index is
    /// resolved to the profile's id here, once, because the dialog outlives
    /// whatever the list does meanwhile.
    pub(crate) fn open_agent_visibility(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        self.agent_visibility_blocks_commits = profile.config.agent_read_can_block_commits();
        self.agent_visibility_prompt = Some(profile.id.clone());
        cx.notify();
    }

    /// The dialog's `Make visible`. Closes the dialog whether or not the
    /// profile it was open for is still there; the grant itself is
    /// [`Workspace::set_agent_visible`]'s, which refuses a profile that is gone.
    pub(crate) fn confirm_agent_visibility(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.agent_visibility_prompt.clone() else {
            return;
        };
        self.set_agent_visible(&id, true, cx);
        self.agent_visibility_prompt = None;
        self.refocus_front();
        cx.notify();
    }

    /// Close the dialog without granting anything. Answers whether a dialog was
    /// drawn, so `escape` is only spent on one that was: a prompt left over a
    /// profile that has since been removed draws nothing.
    pub(crate) fn cancel_agent_visibility(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.agent_visibility_prompt.take() else {
            return false;
        };
        let drawn = self.profiles.iter().any(|profile| profile.id == id);
        if drawn {
            self.refocus_front();
            cx.notify();
        }
        drawn
    }

    /// Put up the `Agent activity` dialog. Changes nothing but what is drawn.
    pub(crate) fn show_agent_activity(&mut self, cx: &mut Context<Self>) {
        self.agent_audit_open = true;
        cx.notify();
    }

    /// Close the dialog and hand the keyboard back. Answers whether it was
    /// open, so `escape` is only spent on a dialog that was drawn.
    pub(crate) fn close_agent_audit(&mut self, cx: &mut Context<Self>) -> bool {
        if !std::mem::take(&mut self.agent_audit_open) {
            return false;
        }
        self.refocus_front();
        cx.notify();
        true
    }

    /// Make a profile visible to agents or hide it. Keyed by id, never by
    /// index: ids can repeat in a hand-edited file, and the first one wins as
    /// everywhere else in `profiles.rs`.
    ///
    /// Asked for `true` this changes nothing unless
    /// [`Workspace::agent_visibility_prompt`] holds the profile's id, and takes
    /// the prompt when it grants. Asked for `false` it hides at once, because
    /// narrowing needs no confirmation.
    pub(crate) fn set_agent_visible(&mut self, id: &str, visible: bool, cx: &mut Context<Self>) {
        if !visible {
            if self.withdraw_agent_visibility(id, cx) {
                self.remember_profiles(cx);
                cx.notify();
            }
            return;
        }
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        if self.agent_visibility_prompt.as_deref() != Some(id) {
            return;
        }
        self.agent_visibility_prompt = None;
        profile.agent_visible = true;
        self.agent.visibility_choices.insert(id.to_string());
        self.remember_profiles(cx);
        cx.notify();
    }

    /// The hide of [`Self::set_agent_visible`] without its save, for a caller
    /// that saves next: the flag goes down, the hide is this window's pending
    /// choice, so the save writes it whatever the file holds, and the
    /// profile's calls are rejected. A visibility prompt open for the profile
    /// goes with it: it was raised for what the profile was, and answering it
    /// would grant what it is now. False when no profile has the id.
    pub(crate) fn withdraw_agent_visibility(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return false;
        };
        if self.agent_visibility_prompt.as_deref() == Some(id) {
            self.agent_visibility_prompt = None;
        }
        profile.agent_visible = false;
        self.agent.visibility_choices.insert(id.to_string());
        self.reject_calls_for(id, cx);
        true
    }

    /// Answer every call waiting on this profile `rejected`, oldest first, each
    /// once, and end its reads, running and waiting, and drop its agent
    /// connection. Calls on any other profile are left as they are, and so is
    /// an approved write already running on this one: it finishes and is
    /// audited (D-16).
    pub(crate) fn reject_calls_for(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let waiting: Vec<CallId> = self
            .agent
            .calls
            .iter()
            .filter(|call| call.profile_id == profile_id)
            .map(|call| call.id)
            .collect();
        for id in waiting {
            self.finish_call(id, Status::Rejected, NO_LONGER_AVAILABLE.to_string(), cx);
        }
        self.end_lane(profile_id, Status::Rejected, NO_LONGER_AVAILABLE, cx);
    }

    /// The call the approval dialog is about: the oldest one whose profile is still
    /// in the list. A call whose profile has gone is swept with it, so the
    /// filter only guards the moment between the two.
    pub(crate) fn front_approval(&self) -> Option<&PendingCall> {
        self.agent.calls.iter().find(|call| {
            self.profiles
                .iter()
                .any(|profile| profile.id == call.profile_id)
        })
    }

    /// The dialog's Deny, for a call named by id: the one rejection that is
    /// the person's own decision.
    pub(crate) fn deny_call(&mut self, id: CallId, cx: &mut Context<Self>) {
        self.end_call(
            id,
            Status::Rejected,
            DENIED.to_string(),
            Some(Decision::Rejected),
            cx,
        );
    }

    /// The call the dialog in front is about.
    pub(crate) fn front_approval_id(&self) -> Option<CallId> {
        self.front_approval().map(|call| call.id)
    }

    /// The timer that ends a call nobody answered as `expired`. Dropping it
    /// with the call is what cancels it.
    fn expiry_of(&self, id: CallId, cx: &mut Context<Self>) -> gpui::Task<()> {
        cx.spawn(async move |workspace, cx| {
            cx.background_executor().timer(EXPIRY).await;
            _ = workspace.update(cx, |workspace, cx| {
                workspace.finish_call(id, Status::Expired, expired_text(), cx)
            });
        })
    }

    /// The approval in front has just been drawn, as the dialog in front: its
    /// Approve is dead until [`ARM_DELAY`] has passed from now. Called one frame
    /// after the dialog is raised or the front call changes, so it counts from
    /// what the person can see and never from the call's arrival.
    ///
    /// The first time a call is revealed it also asks the OS to draw the
    /// person's attention to the window, once. That is a request and nothing
    /// more: it never takes focus from another application and never raises
    /// the window (PITFALLS P-09). On Wayland it does nothing, and the agent
    /// mark's waiting count is the signal.
    pub(crate) fn reveal_approval(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.front_approval_id() else {
            return;
        };
        self.agent.revealed = Some(id);
        self.agent.armed = None;
        if self.agent.attended != Some(id) {
            self.agent.attended = Some(id);
            window.request_attention();
        }
        // Replacing the old timer cancels it.
        self.agent._arming = Some(cx.spawn(async move |workspace, cx| {
            cx.background_executor().timer(ARM_DELAY).await;
            _ = workspace.update(cx, |workspace, cx| {
                // Only while the approval is what the person is looking at: a
                // dialog or the palette over it that they are reading is not
                // the approval being shown, and it is revealed again when that
                // closes.
                if workspace.front_approval_id() == Some(id)
                    && workspace.dialog_in_front.get() == Some("agent-approval")
                    && workspace.palette.is_none()
                {
                    workspace.agent.armed = Some(id);
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// The dialog's Approve for a held statement. Does nothing until the call is
    /// armed. Then the call is taken out of the waiting list, and ended as
    /// `cancelled`, unrun and unrecorded as started, when its caller has already
    /// left, even if the window has not heard so. Otherwise it is checked in
    /// this order: its profile, found by id and still visible to agents
    /// (`rejected` otherwise); the statement, which must be byte for byte the
    /// source the dialog was last drawn from for this call, so a call never
    /// drawn is refused; and the gate's judgment of it now,
    /// which must come out as it did when it arrived, on the config it arrived
    /// with. A difference in the last two answers `refused_by_gate`. Only then
    /// is the `started` line written and the write started, on a connection of
    /// its own, with the very `String` the dialog was drawn from.
    pub(crate) fn approve_call(&mut self, id: CallId, cx: &mut Context<Self>) {
        if self.agent.armed != Some(id) {
            return;
        }
        // A hide saved by another window since this call arrived: the answer
        // is this call's own rejection, found below as no call at all.
        self.adopt_saved_visibility_for_read(cx);
        let Some(index) = self.agent.calls.iter().position(|call| call.id == id) else {
            return;
        };
        let was_in_front = self.front_approval().is_some_and(|front| front.id == id);
        // Dropping the call's expiry with the rest of it.
        let call = self.agent.calls.remove(index);
        let PendingCall {
            profile_id,
            profile_name,
            reply,
            arrived,
            approval,
            ..
        } = call;
        let engine = approval.config.engine();
        // Before anything is started. The caller may have left in the moment
        // before the window heard of it; its call then ends as cancelled, with
        // nothing recorded as started and nothing opened. A write that has
        // started is never stopped (see [`agent_write`]), so this is the last
        // point where the caller leaving changes what runs.
        if approval.gone.load(std::sync::atomic::Ordering::Acquire) {
            self.agent.record(
                id,
                Tool::ExecuteSql,
                Some(profile_name),
                None,
                Status::Cancelled,
                CallFacts {
                    verdict: AuditVerdict::held_by(approval.verdict),
                    sql: Some(approval.sql),
                    elapsed_ms: elapsed_ms(arrived),
                    requester_gone: true,
                    // The person did click Approve; the call is what ended.
                    decision: Some(Decision::Approved),
                    engine: Some(engine),
                    ..CallFacts::default()
                },
            );
            // Nobody would read a reply, and dropping the sender unsent is how
            // a cancelled call ends.
            drop(reply);
            self.finish_approval(was_in_front, cx);
            return;
        }
        let profile = self
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id && profile.agent_visible);
        let held = AuditVerdict::held_by(approval.verdict.clone());
        let (status, text, error) = match profile {
            None => (Status::Rejected, NO_LONGER_AVAILABLE, None),
            // Byte for byte the source the dialog was drawn from for this
            // call: no normalising, folding or trimming (GATE-10).
            Some(_)
                if !self
                    .approval_shown
                    .borrow()
                    .as_ref()
                    .is_some_and(|(shown, sql)| *shown == id && *sql == approval.sql) =>
            {
                (Status::RefusedByGate, CHANGED_SINCE_SHOWN, None)
            }
            Some(profile)
                if agent_gate(profile.config.engine(), &approval.sql, profile.mode)
                    != approval.verdict
                    || profile.config != approval.config =>
            {
                (Status::RefusedByGate, CHANGED_SINCE_SHOWN, None)
            }
            Some(_) => {
                // Before anything is opened: a write that cannot be recorded
                // is not run, so the file never lacks a statement the server
                // may have received.
                let started = AuditLine {
                    v: 1,
                    session: self.agent.session,
                    id: id.0,
                    at: unix_millis(),
                    phase: "started",
                    tool: Tool::ExecuteSql,
                    profile: Some(&profile_name),
                    engine: Some(engine.as_str()),
                    verdict: held.as_ref().map(AuditVerdict::line),
                    decision: Some(Decision::Approved),
                    status: None,
                    requester_gone: false,
                    shown: None,
                    elapsed_ms: elapsed_ms(arrived),
                    error: None,
                    sql: Some(&approval.sql),
                };
                match self.agent.persist(&started) {
                    // No decision on the failure: the statement was never
                    // started, and its outcome line meets the same file.
                    Err(error) => (Status::Failed, NOT_RECORDED, Some(error)),
                    Ok(()) => {
                        match held {
                            // What the person read runs on the profile's
                            // read-only lane, which a write fails against.
                            Some(
                                verdict @ AuditVerdict::Approve {
                                    mode: Mode::ReadOnly,
                                    ..
                                },
                            ) => self.run_agent_work(
                                id,
                                Resolved {
                                    id: profile_id,
                                    name: profile_name,
                                    config: approval.config,
                                },
                                AgentWork::Query {
                                    sql: approval.sql,
                                    approved: Some(Approved { verdict, engine }),
                                },
                                reply,
                                arrived,
                                cx,
                            ),
                            held => self.start_write(
                                id,
                                profile_id,
                                profile_name,
                                approval.config,
                                approval.sql,
                                held,
                                reply,
                                arrived,
                                approval.gone,
                                cx,
                            ),
                        }
                        self.finish_approval(was_in_front, cx);
                        return;
                    }
                }
            }
        };
        // The person did click Approve; what the answer says is that it did not
        // run. A failure to record has no decision (see above).
        let decision = (status != Status::Failed).then_some(Decision::Approved);
        self.agent_answered(
            id,
            Tool::ExecuteSql,
            Some(profile_name),
            None,
            &reply,
            Reply {
                status,
                text: text.to_string(),
            },
            CallFacts {
                verdict: held,
                sql: Some(approval.sql),
                elapsed_ms: elapsed_ms(arrived),
                error,
                decision,
                engine: Some(engine),
                ..CallFacts::default()
            },
        );
        self.finish_approval(was_in_front, cx);
    }

    /// What every end of an approval does after the call left the list: the
    /// arm is spent, and the keyboard goes back if the dialog that took it went.
    fn finish_approval(&mut self, was_in_front: bool, cx: &mut Context<Self>) {
        self.agent.armed = None;
        if was_in_front {
            self.refocus_front();
        }
        cx.notify();
    }

    /// Deny whichever call the dialog in front is about. Answers whether there
    /// was one, so `escape` is only spent on a dialog that was drawn.
    pub(crate) fn deny_front_approval(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.front_approval().map(|call| call.id) else {
            return false;
        };
        self.deny_call(id, cx);
        true
    }

    /// Start answering agents on the socket at `path`. Answers `Ok` when a
    /// bridge already runs.
    ///
    /// The socket's threads hand each message to a channel, and a foreground
    /// task drains it into [`Workspace::on_agent_message`]. That task only ever
    /// awaits the channel: nothing here needs a reactor, which GPUI's executor
    /// does not have.
    pub(crate) fn start_bridge_at(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.start_bridge_if(path, || true, cx)
    }

    /// [`Self::start_bridge_at`] only if `permitted` still holds once the
    /// serving lock is taken. A withdrawn permission is no failure: nothing
    /// listens, as the saved choice now says, and nothing is said.
    fn start_bridge_if(
        &mut self,
        path: PathBuf,
        permitted: impl FnOnce() -> bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.agent.bridge.is_some() {
            return Ok(());
        }
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let listener = match crate::agent::listen_if(&path, sender, permitted) {
            Ok(listener) => listener,
            Err(crate::agent::Declined::Withdrawn) => return Ok(()),
            Err(crate::agent::Declined::ServedElsewhere) => {
                return Err("Another DBDelve window is already serving agents.".to_string());
            }
            Err(crate::agent::Declined::Failed(text)) => return Err(text),
        };
        let pump = cx.spawn(async move |workspace, cx| {
            while let Some(message) = receiver.recv().await {
                if workspace
                    .update(cx, |workspace, cx| workspace.on_agent_message(message, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.agent.session = unix_millis();
        self.agent.bridge = Some(Bridge {
            _listener: listener,
            _pump: pump,
        });
        Ok(())
    }

    /// Start answering agents on this build's own socket.
    pub(crate) fn listen_for_agents(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        self.start_bridge_at(crate::agent::socket_path()?, cx)
    }

    /// [`Self::listen_for_agents`] for a launch that read `agent_access =
    /// true`: the file is read again under the serving lock, and only a
    /// `true` there binds. A kill switch saves `false` before it lets go of
    /// the lock, so a value read at launch cannot outlive it.
    pub(crate) fn listen_for_agents_at_launch(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let plugins = self.plugins.clone();
        self.start_bridge_if(
            crate::agent::socket_path()?,
            move || saved_agent_access(&plugins) == Some(true),
            cx,
        )
    }

    /// What `agent_access` holds in the file right now, `None` when it holds
    /// nothing or the file cannot be read, which both read as off.
    fn saved_agent_access(&self) -> Option<bool> {
        saved_agent_access(&self.plugins)
    }

    /// The `agent_access` the next save writes: this window's own choice when
    /// one of its two access actions made one, else whatever the file holds at
    /// this moment. Settings are written whole by whichever window saves, so a
    /// value remembered from launch would put back `true` over another
    /// window's kill switch, or `false` over its turn-on. The choice is read,
    /// not taken: the save clears it only once the file holds it, so a failed
    /// write leaves it for the next save.
    ///
    /// ponytail: the read and the write that follows are not one step across
    /// processes, so two windows saving within the same instant can still
    /// lose one of the two. The upgrade is a lock around the settings write.
    pub(crate) fn agent_access_to_save(&self) -> Option<bool> {
        self.agent
            .access_choice
            .or_else(|| self.saved_agent_access())
    }

    /// Take on each profile's `agent_visible` as the file holds it now, except
    /// where this window made a choice no save has written yet. Profiles are
    /// written whole by whichever window saves, so a flag remembered from
    /// launch would put a hidden profile back in front of agents, with no
    /// dialog, over another window's hide, or drop another window's grant.
    /// And the window serving agents must answer with the hide made in
    /// another one. A profile hidden this way has its waiting calls rejected,
    /// as a hide here does.
    ///
    /// ponytail: the read and the write that follows are not one step across
    /// processes, as for `agent_access`; the upgrade is the same lock.
    pub(crate) fn adopt_saved_visibility(&mut self, cx: &mut Context<Self>) {
        for id in self.read_saved_visibility() {
            self.reject_calls_for(&id, cx);
        }
    }

    /// The flags half of [`Self::adopt_saved_visibility`]: takes on what the
    /// file holds and returns the ids it hid, whose calls the caller rejects.
    pub(crate) fn read_saved_visibility(&mut self) -> Vec<String> {
        // Sampled before the read, so a save landing during it reads as a
        // change, and kept only once the read has worked: a file that could
        // not be read is read again on the next call.
        let stamp = store::profiles_stamp();
        let Some(saved) = store::peek_profiles(&self.plugins) else {
            return Vec::new();
        };
        self.agent.visibility_stamp = stamp;
        let mut hidden = Vec::new();
        for profile in &mut self.profiles {
            if self.agent.visibility_choices.contains(&profile.id) {
                continue;
            }
            // Only an id the file holds once: a repeated one cannot say which
            // entry granted what, so neither is taken on. A profile the file
            // no longer holds was removed in another window: it stays in this
            // one's list, but nothing granted it any more. A grant is for the
            // target it was given on, so one saved for a target this window's
            // copy does not reach is not taken on either.
            let mut same = saved
                .profiles
                .iter()
                .filter(|stored| stored.id == profile.id);
            let sole = match (same.next(), same.next()) {
                (Some(stored), None) => Some(stored),
                _ => None,
            };
            let visible = sole.is_some_and(|stored| {
                stored.agent_visible
                    && profiles::stored_target(stored, &self.plugins)
                        .is_some_and(|target| !profile.config.retargets(&target))
            });
            if profile.agent_visible && !visible {
                hidden.push(profile.id.clone());
            }
            profile.agent_visible = visible;
        }
        hidden
    }

    /// [`Workspace::adopt_saved_visibility`] for a call that runs a statement
    /// or reads a catalog, which can come many at once: the file is read and
    /// parsed again only when its modified time or length moved, so a hide
    /// saved by another window is never missed and an unchanged file costs one
    /// `stat` on the window's thread.
    ///
    /// ponytail: a save that keeps the length within the file system's time
    /// resolution reads as unchanged; that resolution is nanoseconds on APFS
    /// and ext4 and 100 ns on NTFS, and a visibility flip changes the length.
    pub(super) fn adopt_saved_visibility_for_read(&mut self, cx: &mut Context<Self>) {
        let stamp = store::profiles_stamp();
        if stamp.is_none() || stamp != self.agent.visibility_stamp {
            self.adopt_saved_visibility(cx);
        }
    }

    /// Open the socket and, only once that has worked, save that access is on.
    /// A refusal is shown as it is and changes nothing saved, so a window that
    /// found another one serving can be asked again later.
    pub(crate) fn turn_on_agent_access(&mut self, cx: &mut Context<Self>) {
        if self.agent.bridge.is_some() {
            return;
        }
        match self.listen_for_agents(cx) {
            Ok(()) => {
                self.agent.access_choice = Some(true);
                self.remember_profiles(cx);
                cx.notify();
            }
            Err(text) => self.note(text, cx),
        }
    }

    /// Stop listening and forget who was attached. Dropping the bridge stops
    /// the listener, removes the socket file and shuts every presence
    /// connection; it leaves the saved choice alone, so quitting keeps it.
    ///
    // ponytail: quitting while an approved write runs does not wait for it and
    // asks nothing, so the write may be cut off with the process. Its `started`
    // line is already in the audit file with no `outcome` line, which is the
    // trace an interrupted call leaves. The upgrade is a confirmation on quit,
    // which the owner declined (D-31).
    pub(crate) fn drop_bridge(&mut self) {
        self.agent.bridge = None;
        self.agent.presence.clear();
    }

    /// The kill switch: nothing an agent started stays open, and nothing
    /// listens until the user turns access on again. In order:
    ///
    /// 1. Every pending call and every read, running or waiting, is answered
    ///    `rejected` and every agent connection is dropped. An approved write
    ///    already running is not among them: it finishes and is audited, as
    ///    nothing but its own result ends it (D-16). This comes first and runs
    ///    with or without a bridge, because dropping the bridge never closes a
    ///    call's connection: each call's writer thread sends its frame and then
    ///    closes its own socket, so a waiting client reads a whole answer and
    ///    not a bare end of file.
    /// 2. `agent_access = false` is saved when this window was serving or when
    ///    the file says `true`. It is the user's own action, so no later launch
    ///    of the variant listens, even if this window never did. With neither,
    ///    nothing is written.
    /// 3. The bridge is dropped, if there is one. After the save, so the
    ///    serving lock goes only once the file says `false`.
    /// 4. In a window that was not serving, while another window of the
    ///    variant is, the notice says that window's agents stay connected. This
    ///    window cannot reach them, because Phase 4 has no frame that tells
    ///    another window to stop; what it can do is save the choice and say
    ///    that. The notice repeats on every press, since it stays true.
    pub(crate) fn disconnect_agents(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<CallId> = self.agent.calls.iter().map(|call| call.id).collect();
        for id in pending {
            self.finish_call(id, Status::Rejected, TURNED_OFF.to_string(), cx);
        }
        let lanes: Vec<String> = self.agent.lanes.keys().cloned().collect();
        for profile_id in lanes {
            self.end_lane(&profile_id, Status::Rejected, TURNED_OFF, cx);
        }
        let was_serving = self.agent.bridge.is_some();
        // Saved while the bridge still holds the serving lock: a launch that
        // takes the lock next reads `false` under it (see
        // [`Self::listen_for_agents_at_launch`]). A save that fails keeps the
        // lock, so no launch binds over a file still saying `true`; the
        // pending `false` refuses every call meanwhile (see
        // [`Self::on_agent_message`]), and pressing again retries the save.
        if was_serving || self.saved_agent_access() == Some(true) {
            self.agent.access_choice = Some(false);
            if let Err(error) = self.remember_profiles_or_fail(cx) {
                if was_serving {
                    self.note(format!("{NOT_TURNED_OFF} {error}"), cx);
                } else {
                    self.note(error, cx);
                }
                cx.notify();
                return;
            }
        }
        self.drop_bridge();
        if !was_serving && crate::agent::socket_is_live() {
            self.note(STILL_SERVED_ELSEWHERE.to_string(), cx);
        }
        cx.notify();
    }

    /// What the titlebar's agent mark shows: nothing while no bridge runs, the
    /// dim mark while one listens alone, and the lit mark, with the count of
    /// calls waiting, while an agent is attached or a call waits.
    pub(crate) fn agent_mark_state(&self) -> Option<ui::AgentMark> {
        self.agent.bridge.as_ref()?;
        // A read or an approved write in flight is an agent attached, though
        // nobody is asked.
        let attached = !self.agent.presence.is_empty()
            || !self.agent.calls.is_empty()
            || !self.agent.writes.is_empty()
            || self.agent.lanes.values().any(|lane| lane.running.is_some());
        Some(if attached {
            ui::AgentMark::Connected {
                waiting: self.agent.calls.len(),
            }
        } else {
            ui::AgentMark::Listening
        })
    }

    /// What an agent asked for. Opens no connection and starts no database
    /// work: the answer is read from the profile list as it stands.
    pub(crate) fn on_agent_message(&mut self, message: Inbound, cx: &mut Context<Self>) {
        let arrived = Instant::now();
        // The kill switch was used and its save has not landed: the socket is
        // kept only so the lock is, and nothing it hears is taken on.
        if self.agent.access_choice == Some(false)
            && let Inbound::Call {
                id, tool, reply, ..
            } = &message
        {
            self.agent_answered(
                *id,
                *tool,
                None,
                None,
                reply,
                Reply {
                    status: Status::Rejected,
                    text: TURNED_OFF.to_string(),
                },
                CallFacts {
                    elapsed_ms: elapsed_ms(arrived),
                    ..CallFacts::default()
                },
            );
            return;
        }
        match message {
            Inbound::PresenceOpened { conn } => {
                self.agent.presence.push(conn);
                cx.notify();
            }
            Inbound::PresenceClosed { conn } => {
                self.agent.presence.retain(|open| *open != conn);
                cx.notify();
            }
            Inbound::Call {
                tool: Tool::ListConnections,
                id,
                reply,
                ..
            } => {
                self.adopt_saved_visibility(cx);
                let visible: Vec<Listed> = self
                    .profiles
                    .iter()
                    .filter(|profile| self.agent_may_reach(profile))
                    .map(|profile| Listed {
                        name: &profile.name,
                        engine: profile.config.engine().as_str(),
                    })
                    .collect();
                let text = listed_text(&visible);
                // The caller may have hung up, and then nobody reads this.
                let _ = reply.send(Reply {
                    status: Status::Executed,
                    text,
                });
                self.agent.record(
                    id,
                    Tool::ListConnections,
                    None,
                    None,
                    Status::Executed,
                    CallFacts {
                        elapsed_ms: elapsed_ms(arrived),
                        ..CallFacts::default()
                    },
                );
            }
            Inbound::Call {
                tool: Tool::ExecuteSql,
                id,
                connection,
                args,
                reply,
                gone,
            } => {
                self.adopt_saved_visibility_for_read(cx);
                let found = match connection.as_deref() {
                    Some(name) => self
                        .agent_profile(name)
                        .map(|profile| (Resolved::of(profile), profile.mode)),
                    None => Err(AgentLookup::NotAvailable),
                };
                let (resolved, mode) = match found {
                    Ok(found) => found,
                    Err(lookup) => {
                        self.agent_answered(
                            id,
                            Tool::ExecuteSql,
                            None,
                            connection.as_deref(),
                            &reply,
                            Reply {
                                status: Status::RefusedByGate,
                                text: lookup.to_string(),
                            },
                            CallFacts {
                                elapsed_ms: elapsed_ms(arrived),
                                ..CallFacts::default()
                            },
                        );
                        return;
                    }
                };
                // The schema requires a statement, so only an injected call
                // has none; it is the empty statement.
                let sql = args.sql.unwrap_or_default();
                let engine = resolved.config.engine();
                let verdict = agent_gate(engine, &sql, mode);
                // The verdict's contents are what an approval will show,
                // which agents do not get.
                let (refused, audited) = match &verdict {
                    AgentVerdict::Run => {
                        self.run_agent_sql(id, resolved, sql, reply, arrived, cx);
                        return;
                    }
                    // Everything the gate approves is held for a person, a
                    // read of a file or a call it does not list included. A
                    // statement is refused here, before there is a pending call
                    // or a dialog to be shown, when nothing can run it for an
                    // agent on this profile: no read on an in-memory DuckDB,
                    // and no write on DuckDB at all.
                    AgentVerdict::Approve(held, _) => {
                        let write = held.mode == Mode::ReadWrite;
                        let refusal = resolved
                            .config
                            .agent_refusal()
                            .or_else(|| resolved.config.agent_write_refusal().filter(|_| write));
                        match refusal {
                            Some(refusal) => (
                                refusal.to_string(),
                                Some(AuditVerdict::Refuse(refusal.to_string())),
                            ),
                            None => {
                                let expiry = self.expiry_of(id, cx);
                                self.agent.calls.push(PendingCall {
                                    id,
                                    profile_id: resolved.id,
                                    profile_name: resolved.name,
                                    reply,
                                    arrived,
                                    approval: Approval {
                                        sql,
                                        verdict,
                                        locks_the_file: resolved
                                            .config
                                            .agent_read_can_block_commits(),
                                        config: resolved.config,
                                        gone,
                                    },
                                    _expiry: expiry,
                                });
                                cx.notify();
                                return;
                            }
                        }
                    }
                    AgentVerdict::Refuse(reason) => {
                        (reason.to_string(), AuditVerdict::held_by(verdict))
                    }
                };
                self.agent_answered(
                    id,
                    Tool::ExecuteSql,
                    Some(resolved.name),
                    None,
                    &reply,
                    Reply {
                        status: Status::RefusedByGate,
                        text: refused,
                    },
                    CallFacts {
                        verdict: audited,
                        sql: Some(sql),
                        elapsed_ms: elapsed_ms(arrived),
                        ..CallFacts::default()
                    },
                );
            }
            Inbound::Call {
                tool: Tool::ExplainQuery,
                id,
                connection,
                args,
                reply,
                ..
            } => {
                self.adopt_saved_visibility_for_read(cx);
                let found = match connection.as_deref() {
                    Some(name) => self
                        .agent_profile(name)
                        .map(|profile| (Resolved::of(profile), profile.mode)),
                    None => Err(AgentLookup::NotAvailable),
                };
                let (resolved, mode) = match found {
                    Ok(found) => found,
                    Err(lookup) => {
                        self.agent_answered(
                            id,
                            Tool::ExplainQuery,
                            None,
                            connection.as_deref(),
                            &reply,
                            Reply {
                                status: Status::RefusedByGate,
                                text: lookup.to_string(),
                            },
                            CallFacts {
                                elapsed_ms: elapsed_ms(arrived),
                                ..CallFacts::default()
                            },
                        );
                        return;
                    }
                };
                // The schema requires a statement, so only an injected call
                // has none; it is the empty statement.
                let sql = args.sql.unwrap_or_default();
                let engine = resolved.config.engine();
                match explain_statement(engine, &sql, args.analyze.unwrap_or(false), mode) {
                    Ok(statement) => self.run_agent_work(
                        id,
                        resolved,
                        AgentWork::Explain { statement },
                        reply,
                        arrived,
                        cx,
                    ),
                    Err(declined) => self.agent_answered(
                        id,
                        Tool::ExplainQuery,
                        Some(resolved.name),
                        None,
                        &reply,
                        Reply {
                            status: declined.status,
                            text: declined.text,
                        },
                        CallFacts {
                            verdict: declined.verdict.and_then(AuditVerdict::held_by),
                            sql: declined.sql,
                            elapsed_ms: elapsed_ms(arrived),
                            ..CallFacts::default()
                        },
                    ),
                }
            }
            Inbound::Call {
                tool:
                    tool @ (Tool::SearchSchema
                    | Tool::ListSchemas
                    | Tool::ListTables
                    | Tool::DescribeTable
                    | Tool::ListRelationships
                    | Tool::ListRoutines
                    | Tool::DescribeRoutine),
                id,
                connection,
                args,
                reply,
                ..
            } => {
                self.adopt_saved_visibility_for_read(cx);
                let resolved = match connection.as_deref() {
                    Some(name) => self.agent_profile(name).map(Resolved::of),
                    None => Err(AgentLookup::NotAvailable),
                };
                let resolved = match resolved {
                    Ok(resolved) => resolved,
                    Err(lookup) => {
                        self.agent_answered(
                            id,
                            tool,
                            None,
                            connection.as_deref(),
                            &reply,
                            Reply {
                                status: Status::RefusedByGate,
                                text: lookup.to_string(),
                            },
                            CallFacts {
                                elapsed_ms: elapsed_ms(arrived),
                                ..CallFacts::default()
                            },
                        );
                        return;
                    }
                };
                // The names are required by the tools that take them, so only
                // an injected call lacks one; it names nothing the catalog holds.
                let work = match tool {
                    Tool::DescribeTable => AgentWork::Structure {
                        schema: args.schema.unwrap_or_default(),
                        table: args.table.unwrap_or_default(),
                    },
                    Tool::ListTables => AgentWork::Catalog {
                        schema: Some(args.schema.unwrap_or_default()),
                    },
                    Tool::SearchSchema => AgentWork::Search {
                        query: args.query.unwrap_or_default(),
                    },
                    Tool::ListRelationships => AgentWork::Relationships {
                        schema: args.schema.unwrap_or_default(),
                    },
                    Tool::ListRoutines => AgentWork::Routines {
                        schema: args.schema.unwrap_or_default(),
                        routine: None,
                    },
                    Tool::DescribeRoutine => AgentWork::Routines {
                        schema: args.schema.unwrap_or_default(),
                        routine: Some(args.routine.unwrap_or_default()),
                    },
                    _ => AgentWork::Catalog { schema: None },
                };
                self.run_agent_work(id, resolved, work, reply, arrived, cx);
            }
            Inbound::Hangup { id } => {
                self.finish_call(id, Status::Cancelled, String::new(), cx);
            }
            Inbound::Undelivered { id } => {
                // Only the in-memory entry: the audit file is append-only and an
                // outcome line already written is not rewritten. A write's own
                // line says the caller was gone from the socket's flag, known
                // before it was written.
                if let Some(entry) = self.agent.log.iter_mut().find(|entry| entry.id == id) {
                    entry.requester_gone = true;
                    cx.notify();
                }
            }
        }
    }

    /// Answer a read call at once and log it: the end of a call that never
    /// waits for anyone. `resolved` is the profile's own name, `asked` what the
    /// agent called a connection that resolved to nothing.
    #[allow(
        clippy::too_many_arguments,
        reason = "the call's identity, its two names, both halves of the answer and its facts are each one thing; a struct of them would be built once per call to be taken apart at once"
    )]
    pub(super) fn agent_answered(
        &mut self,
        id: CallId,
        tool: Tool,
        resolved: Option<String>,
        asked: Option<&str>,
        reply: &SyncSender<Reply>,
        answer: Reply,
        facts: CallFacts,
    ) {
        let status = answer.status;
        // The caller may have hung up, and then nobody reads this.
        let _ = reply.send(answer);
        self.agent.record(id, tool, resolved, asked, status, facts);
    }

    /// The one door every call ends through: answered, expired or hung up.
    /// It removes the call by id before it replies, so two things ending the
    /// same call at once produce one outcome and at most one reply. It looks
    /// among the calls waiting on a person first (held statements),
    /// then among the approved writes running, which a hangup is noted on and
    /// nothing else ends, and last among the reads of every lane
    /// ([`Workspace::finish_read`]), where a running read is cancelled on the
    /// server. Answers whether there was such a call.
    ///
    /// A `Cancelled` call is dropped without a reply, because its caller has
    /// gone and nobody would read one.
    pub(crate) fn finish_call(
        &mut self,
        id: CallId,
        status: Status,
        text: String,
        cx: &mut Context<Self>,
    ) -> bool {
        // A hangup is no decision of anyone's: its caller left. A rejection
        // that comes here is DBDelve's own; the person's Deny is
        // [`Self::deny_call`].
        let decision = match status {
            Status::Rejected => Some(Decision::Withdrawn),
            Status::Expired => Some(Decision::Expired),
            _ => None,
        };
        self.end_call(id, status, text, decision, cx)
    }

    /// [`Self::finish_call`], with what is recorded as decided about a held
    /// statement.
    fn end_call(
        &mut self,
        id: CallId,
        status: Status,
        text: String,
        decision: Option<Decision>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(index) = self.agent.calls.iter().position(|call| call.id == id) else {
            // A running write ends by its own result alone: a hangup is noted
            // for the audit and nothing else reaches it.
            // Only a write of this bridge: one that outlived an earlier one
            // may carry the same id.
            let session = self.agent.session;
            if let Some(write) = self
                .agent
                .writes
                .iter_mut()
                .find(|write| write.id == id && write.session == session)
            {
                write.requester_gone |= status == Status::Cancelled;
                return true;
            }
            return self.finish_read(id, status, text, true, Default::default(), cx);
        };
        let was_in_front = self.front_approval().is_some_and(|front| front.id == id);
        let call = self.agent.calls.remove(index);
        if status != Status::Cancelled {
            // The caller may have hung up, and then nobody reads this.
            let _ = call.reply.send(Reply { status, text });
        }
        // A held statement is audited with what it asked, so a denied, expired
        // or hung-up approval says which statement it was.
        let approval = call.approval;
        let engine = approval.config.engine();
        // Past the early return above, so a hangup that arrives after the call
        // ended adds nothing: one entry per call, written here.
        self.agent.record(
            id,
            Tool::ExecuteSql,
            Some(call.profile_name),
            None,
            status,
            CallFacts {
                verdict: AuditVerdict::held_by(approval.verdict),
                sql: Some(approval.sql),
                elapsed_ms: elapsed_ms(call.arrived),
                // Only a hangup ends a call `Cancelled`.
                requester_gone: status == Status::Cancelled,
                decision,
                engine: Some(engine),
                ..CallFacts::default()
            },
        );
        if self.agent.armed == Some(id) {
            self.agent.armed = None;
        }
        if was_in_front {
            // The dialog took the keyboard when it went up and goes away with
            // the call, by whichever route the call ended.
            self.refocus_front();
        }
        cx.notify();
        true
    }

    /// The agent-visible profile called `name`.
    ///
    /// Filters to visible profiles before it compares names, so a hidden
    /// profile of the same name cannot change the answer, and names are not
    /// unique: the connection form accepts any non-empty one. The comparison is
    /// on bytes, with no case folding, trimming or normalisation, so what an
    /// agent was told is the only spelling that works. A profile this build
    /// cannot open is never in `self.profiles`, so it is never a candidate,
    /// and neither is one whose id another profile shares (see
    /// [`Self::agent_may_reach`]).
    pub(crate) fn agent_profile(&self, name: &str) -> Result<&Profile, AgentLookup> {
        let mut matches = self
            .profiles
            .iter()
            .filter(|profile| self.agent_may_reach(profile))
            .filter(|profile| profile.name == name);
        match (matches.next(), matches.next()) {
            (Some(profile), None) => Ok(profile),
            (Some(_), Some(_)) => Err(AgentLookup::Ambiguous),
            (None, _) => Err(AgentLookup::NotAvailable),
        }
    }

    /// Whether an agent may reach `profile`: it is visible, and no other
    /// profile has its id. Calls, lanes and the approval are all found by id,
    /// and an id repeated in a hand-edited file would let a lookup land on the
    /// other entry, hidden or pointed somewhere else, so neither is offered.
    pub(crate) fn agent_may_reach(&self, profile: &Profile) -> bool {
        profile.agent_visible && self.sole_profile(&profile.id).is_some()
    }

    /// The profile with this id, when exactly one has it.
    pub(crate) fn sole_profile(&self, id: &str) -> Option<&Profile> {
        let mut same = self.profiles.iter().filter(|profile| profile.id == id);
        match (same.next(), same.next()) {
            (Some(profile), None) => Some(profile),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_connection_list_past_the_budget_says_what_it_left_out() {
        use crate::agent::result::RESULT_BUDGET;

        let short = super::Listed {
            name: "Orders",
            engine: "sqlite",
        };
        assert_eq!(
            super::listed_text(&[short]),
            r#"[{"name":"Orders","engine":"sqlite"}]"#
        );
        let long = "n".repeat(RESULT_BUDGET / 3);
        let many: Vec<_> = (0..5)
            .map(|_| super::Listed {
                name: &long,
                engine: "postgres",
            })
            .collect();
        let text = super::listed_text(&many);
        assert!(text.len() <= RESULT_BUDGET, "{}", text.len());
        let read: serde_json::Value = serde_json::from_str(&text).expect("the answer is JSON");
        assert_eq!(read["connections"].as_array().map(Vec::len), Some(2));
        assert_eq!(read["omitted"], 3);
        let huge = "n".repeat(RESULT_BUDGET * 2);
        let text = super::listed_text(&[super::Listed {
            name: &huge,
            engine: "postgres",
        }]);
        assert!(text.len() <= RESULT_BUDGET, "{}", text.len());
    }

    use super::*;

    fn corpus() -> [&'static str; 6] {
        [
            "UPDATE t SET a = 'Approve me'",
            "SELECT my_udf('Approve me')",
            "SELECT * FROM 'Approve me.csv'",
            "CREATE INDEX CONCURRENTLY approve_me ON t (a)",
            "SET k v EX 10",
            "SELECT pg_read_file('Approve me')",
        ]
    }

    #[test]
    fn the_approval_facts_are_dbdelves_own() {
        use crate::sql::{ApprovalKind, approval_kind};

        let mut seen = 0;
        for engine in Engine::ALL {
            for mode in crate::sql::Mode::ALL {
                for sql in corpus() {
                    let verdict = agent_gate(engine, sql, mode);
                    let AgentVerdict::Approve(held, _) = &verdict else {
                        continue;
                    };
                    seen += 1;
                    let facts = approval_facts(engine, mode, &verdict, sql, true, true);
                    let at = format!("{engine:?} {mode:?} {sql}");
                    let count = |fact: &str| facts.iter().filter(|each| *each == fact).count();
                    assert_eq!(count(OWN_CONNECTION), 1, "{at}: {facts:?}");
                    assert_eq!(
                        count(&format!(
                            "This connection's access level is {}; the statement needs {}.",
                            mode.label(),
                            held.mode.label()
                        )),
                        1,
                        "{at}: {facts:?}"
                    );
                    assert_eq!(count(&crate::ui::size_fact(sql)), 1, "{at}: {facts:?}");
                    let kinds = [
                        (ApprovalKind::Write, "It changes data."),
                        (
                            ApprovalKind::EgressRead,
                            "It reads a file, a URL or another server.",
                        ),
                        (
                            ApprovalKind::UnlistedCall,
                            "It calls a function DBDelve does not list.",
                        ),
                    ];
                    let kind = approval_kind(engine, sql, held);
                    for (each, sentence) in kinds {
                        assert_eq!(
                            count(sentence),
                            usize::from(each == kind),
                            "{at}: {facts:?}"
                        );
                    }
                    for fact in &facts {
                        for own in ["Approve me", "approve_me"] {
                            assert!(!fact.contains(own), "{at}: {fact}");
                        }
                        let lower = fact.to_lowercase();
                        for word in ["yet", "phase", "mcp", "try", "should", "upgrade"] {
                            assert!(!lower.contains(word), "{at}: {fact} says {word}");
                        }
                    }
                }
            }
        }
        assert!(seen > 20, "the corpus reached the approval {seen} times");
    }

    #[test]
    fn the_approval_facts_add_only_what_the_statement_carries() {
        use crate::sql::{Expiry, Mode};

        let facts = |engine, sql: &str, locks| {
            let verdict = agent_gate(engine, sql, Mode::ReadWrite);
            assert!(matches!(verdict, AgentVerdict::Approve(..)), "{sql}");
            approval_facts(engine, Mode::ReadWrite, &verdict, sql, locks, true)
        };
        let count =
            |facts: &[String], fact: &str| facts.iter().filter(|each| *each == fact).count();

        let plain = facts(Engine::Postgres, "UPDATE t SET a = 1", false);
        assert_eq!(count(&plain, &Expiry::Key.to_string()), 0, "{plain:?}");
        assert_eq!(count(&plain, &Expiry::Field.to_string()), 0, "{plain:?}");
        assert_eq!(count(&plain, INVALID_INDEX), 0, "{plain:?}");
        assert_eq!(count(&plain, "1 line, 18 bytes"), 1, "{plain:?}");

        let index = facts(
            Engine::Postgres,
            "CREATE INDEX CONCURRENTLY i ON t (a)",
            false,
        );
        assert_eq!(count(&index, INVALID_INDEX), 1, "{index:?}");

        let expiring = facts(Engine::Redis, "SET k v EX 10", false);
        assert_eq!(
            count(&expiring, &Expiry::Key.to_string()),
            1,
            "{expiring:?}"
        );

        let lock = "While it runs it locks the file, and your own reads and writes on it wait.";
        let write = facts(Engine::Sqlite, "UPDATE t SET a = 1", true);
        assert_eq!(count(&write, lock), 1, "{write:?}");
        let unlocked = facts(Engine::Sqlite, "UPDATE t SET a = 1", false);
        assert_eq!(count(&unlocked, lock), 0, "{unlocked:?}");
        let read = facts(Engine::DuckDb, "SELECT * FROM 'a.csv'", true);
        assert_eq!(count(&read, lock), 0, "a read is no write: {read:?}");
    }

    #[test]
    fn an_audited_error_keeps_the_servers_words_and_never_the_value_they_name() {
        for (server, kept) in [
            (
                "duplicate key value violates unique constraint \"users_email_key\"\nKey (email)=(alice@example.com) already exists.",
                "duplicate key value violates unique constraint …",
            ),
            (
                "new row for relation \"t\" violates check constraint \"c\"\nFailing row contains (7, alice, s3cret).",
                "new row for relation …",
            ),
            // A read that casts a column: the value is the row's.
            (
                "invalid input syntax for type integer: \"s3cret\"",
                "invalid input syntax for type integer: …",
            ),
            (
                "Duplicate entry 'alice@example.com' for key 'users.email'",
                "Duplicate entry …",
            ),
            (
                "Truncated incorrect DOUBLE value: 's3cret'",
                "Truncated incorrect DOUBLE value: …",
            ),
            (
                "Violation of UNIQUE KEY constraint 'UQ_email'. The duplicate key value is (alice@example.com).",
                "Violation of UNIQUE KEY constraint …",
            ),
            (
                "E11000 duplicate key error collection: app.users index: email_1 dup key: { email: \"alice@example.com\" }",
                "E11000 duplicate key error collection: app.users index: email_1 dup key: …",
            ),
            (
                "WRONGTYPE Operation against a key holding the wrong kind of value",
                "WRONGTYPE Operation against a key holding the wrong kind of value",
            ),
            // An apostrophe is not a quote.
            (
                "Can't connect to server on 'db.internal'",
                "Can't connect to server on …",
            ),
            ("", ""),
        ] {
            assert_eq!(audited_error(server), kept, "{server}");
        }
    }

    #[test]
    fn an_approved_read_says_whether_the_database_holds_it_to_reads() {
        for (engine, sql) in [
            (Engine::SqlServer, "SELECT dbo.my_udf(1)"),
            (Engine::Postgres, "SELECT my_udf(1)"),
            (Engine::DuckDb, "SELECT * FROM 'a.csv'"),
        ] {
            let verdict = agent_gate(engine, sql, Mode::ReadWrite);
            assert!(
                matches!(verdict, AgentVerdict::Approve(..)),
                "{engine:?} {sql}"
            );
            for held in [true, false] {
                let facts = approval_facts(engine, Mode::ReadWrite, &verdict, sql, false, held);
                let says = |fact: &str| facts.iter().any(|each| each == fact);
                assert_eq!(says(HELD_TO_READS), held, "{engine:?} {held}: {facts:?}");
                assert_eq!(
                    says(NOT_HELD_TO_READS),
                    !held,
                    "{engine:?} {held}: {facts:?}"
                );
            }
        }
        let verdict = agent_gate(Engine::SqlServer, "UPDATE t SET a = 1", Mode::ReadWrite);
        let write = approval_facts(
            Engine::SqlServer,
            Mode::ReadWrite,
            &verdict,
            "UPDATE t SET a = 1",
            false,
            false,
        );
        assert!(
            !write
                .iter()
                .any(|fact| fact == HELD_TO_READS || fact == NOT_HELD_TO_READS),
            "a write is not said to be held to reads: {write:?}"
        );
    }

    #[test]
    fn an_audit_entry_keeps_a_cut_of_a_long_error_as_it_does_of_a_long_statement() {
        // `record` appends to the audit file, so it goes to a test home and
        // never to the user's own.
        crate::store::tests::with_home(|| {
            let mut state = AgentState::default();
            let long = "e".repeat(AUDIT_SQL_CAP * 3);
            state.record(
                CallId(1),
                Tool::ExecuteSql,
                None,
                None,
                Status::Failed,
                CallFacts {
                    sql: Some(long.clone()),
                    error: Some(long),
                    ..CallFacts::default()
                },
            );
            let entry = state.log.front().expect("the call is logged");
            for kept in [entry.sql.as_deref(), entry.error.as_deref()] {
                let kept = kept.expect("kept");
                assert_eq!(kept.chars().count(), AUDIT_SQL_CAP + SQL_CLIPPED.len());
                assert!(kept.ends_with(SQL_CLIPPED));
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn an_outcome_names_the_session_its_call_was_accepted_in() {
        crate::store::tests::with_home(|| {
            let mut state = AgentState {
                session: 2,
                ..AgentState::default()
            };
            for session in [Some(1), None] {
                state.record(
                    CallId(0),
                    Tool::ExecuteSql,
                    None,
                    None,
                    Status::Executed,
                    CallFacts {
                        session,
                        ..CallFacts::default()
                    },
                );
            }
            let path = crate::store::dbdelve_directory()
                .expect("the test home has a data directory")
                .join(crate::store::AGENT_AUDIT_FILE);
            let text = std::fs::read_to_string(path).expect("the audit is readable");
            let sessions: Vec<u64> = text
                .lines()
                .map(|line| {
                    serde_json::from_str::<serde_json::Value>(line).expect("a JSON line")["session"]
                        .as_u64()
                        .expect("a session")
                })
                .collect();
            assert_eq!(sessions, [1, 2]);
        });
    }

    #[test]
    fn the_dialog_states_the_engines_hold_and_that_rows_leave_the_machine() {
        for engine in Engine::ALL {
            for mode in Mode::ALL {
                let facts = visibility_facts(engine, mode, false);
                assert!(facts.iter().any(|fact| fact == engine.read_only_hold()));
                assert!(
                    facts
                        .iter()
                        .any(|fact| fact == "Rows the agent reads leave this machine.")
                );
                assert!(facts.iter().any(|fact| fact.contains(mode.label())));
                for fact in &facts {
                    for word in ["yet", "Phase", "MCP", "try", "should", "upgrade"] {
                        assert!(!fact.contains(word), "{engine:?} {mode:?}: {fact}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_dialog_says_whether_a_call_to_an_unlisted_function_waits() {
        use crate::sql::{AgentVerdict, agent_gate};

        const RUNS: &str = "Calls to functions DBDelve does not list run without approval.";
        const WAITS: &str = "Calls to functions DBDelve does not list wait for your approval.";
        for engine in Engine::ALL {
            for mode in Mode::ALL {
                // The sentence comes from what the gate answers, so the dialog
                // cannot state an answer the gate contradicts.
                let (stated, other) = match agent_gate(engine, "SELECT my_udf(a) FROM t", mode) {
                    AgentVerdict::Run => (Some(RUNS), WAITS),
                    AgentVerdict::Approve(..) => (Some(WAITS), RUNS),
                    AgentVerdict::Refuse(_) => (None, ""),
                };
                let facts = visibility_facts(engine, mode, false);
                let count = |sentence: &str| facts.iter().filter(|fact| *fact == sentence).count();
                match stated {
                    Some(stated) => {
                        assert_eq!(count(stated), 1, "{engine:?} {mode:?}: {facts:?}");
                        assert_eq!(count(other), 0, "{engine:?} {mode:?}: {facts:?}");
                    }
                    None => {
                        assert_eq!(
                            count(RUNS) + count(WAITS),
                            0,
                            "{engine:?} {mode:?}: {facts:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_dialog_states_the_commit_wait_only_when_a_read_can_block_it() {
        const WAIT: &str = "An agent's read in progress can make your commit on this file wait, \
                            and then fail with \"database is locked\".";
        for engine in Engine::ALL {
            for mode in Mode::ALL {
                let count = |facts: &[String]| facts.iter().filter(|fact| *fact == WAIT).count();
                let blocked = visibility_facts(engine, mode, true);
                assert_eq!(count(&blocked), 1, "{engine:?} {mode:?}: {blocked:?}");
                let free = visibility_facts(engine, mode, false);
                assert_eq!(count(&free), 0, "{engine:?} {mode:?}: {free:?}");
                for fact in blocked.iter().chain(&free) {
                    for word in ["yet", "Phase", "MCP", "try", "should", "upgrade"] {
                        assert!(!fact.contains(word), "{engine:?} {mode:?}: {fact}");
                    }
                }
            }
        }
    }

    #[test]
    fn an_approval_with_an_expiry_is_audited_with_its_text() {
        use crate::sql::{Expiry, Verdict};

        let verdict = Verdict {
            mode: Mode::ReadWrite,
            destructive: Vec::new(),
        };
        assert_eq!(
            AuditVerdict::held_by(AgentVerdict::Approve(verdict.clone(), Some(Expiry::Key))),
            Some(AuditVerdict::Approve {
                mode: Mode::ReadWrite,
                expiry: Some(Expiry::Key.to_string()),
            })
        );
        assert_eq!(
            AuditVerdict::held_by(AgentVerdict::Approve(verdict, None)),
            Some(AuditVerdict::Approve {
                mode: Mode::ReadWrite,
                expiry: None,
            })
        );
        assert_eq!(AuditVerdict::held_by(AgentVerdict::Run), None);
    }

    #[test]
    fn the_approval_texts_say_the_statement_was_not_run() {
        for text in [DENIED.to_string(), expired_text()] {
            assert!(text.contains("statement"), "{text}");
            assert!(text.contains("not run"), "{text}");
            assert!(text.contains("DBDelve"), "{text}");
        }
        assert!(
            expired_text().contains(&format!("{} minutes", EXPIRY.as_secs() / 60)),
            "{}",
            expired_text()
        );
    }
}

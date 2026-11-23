//! The agent's own connections, and the reads that run on them.
//!
//! An agent never shares the user's connection: its statements would queue
//! behind the user's, ride inside a transaction the user has open, and see
//! session settings the user changed. So each profile an agent reads gets a
//! [`Lane`], a connection opened from the profile's config for the agent alone,
//! held read-only whatever the profile's mode, and kept for the next call. The
//! user's profile is never connected, reconnected or marked by it.
//!
//! The statement is sent exactly as the agent wrote it, after
//! [`crate::sql::agent_gate_on`] judged that text. Nothing is appended to it,
//! wrapped around it or rewritten, so the gate's verdict is about what the
//! server receives. The one exception is `explain_query`, which sends the
//! engine's EXPLAIN of the statement: [`explain_statement`] composes it first
//! and the gate judges that composed text, so the verdict is again about what
//! the server receives.

use std::{
    collections::{HashMap, VecDeque},
    sync::mpsc::SyncSender,
    time::{Duration, Instant},
};

use super::agent::{AuditVerdict, CallFacts, Decision, audited_error, elapsed_ms};
use super::*;
use crate::{
    agent::{CallId, Reply, Status, result, result::Total, tools::Tool},
    db::{
        CancelToken, Catalog, Connection, ConnectionConfig, DbError, ExplainMode, Fetch, IndexKind,
        KeyTarget, QueryResult, RelationKind, Routine, RoutineKind, Structure,
    },
    explain::compact,
    sql::{AgentVerdict, agent_gate_on},
};

// ponytail: an agent is told the first hundred rows, and the tool takes no
// count, so what a call can cost the model's context does not depend on what
// the model asks for. The upgrade is a setting, if a user ever needs one.
pub(crate) const AGENT_ROWS: usize = 100;

// ponytail: the most rows of a plan that are read back. A real plan is tens of
// lines, and an ANALYZE has already run the statement by the time they arrive,
// so the rows are drained rather than stopped; the ceiling only keeps a server
// that answers a plan with a table from filling memory. The upgrade is a setting,
// if a plan ever needs more.
pub(crate) const PLAN_ROWS: usize = 10_000;

// ponytail: the longest an agent's statement runs when the profile sets no
// timeout of its own, so no agent connection is ever unbounded. A profile's own
// shorter timeout wins. The upgrade is a setting, if a user ever needs one.
pub(crate) const AGENT_TIMEOUT: u32 = 30;

// ponytail: the longest any one call lives, from the moment it starts to its
// answer: the open, a tunnel, a keychain prompt nobody answers and one
// statement at its [`AGENT_TIMEOUT`]. Time spent waiting behind other calls is
// not counted; it is bounded by their deadlines and by the call cap. The
// upgrade is a deadline per tool.
pub(crate) const CALL_DEADLINE: Duration = Duration::from_secs(60);

/// Said for a call that ran past [`CALL_DEADLINE`]. Names the same 60 seconds;
/// a test holds the two together, since a constant cannot be built from one.
const DEADLINE_PASSED: &str = "The call did not finish within 60 seconds.";

/// What opens the agent's connection to a profile and says what an error from
/// it must not name. `None` in [`AgentState::opener`](super::agent::AgentState::opener)
/// is [`open_agent`]; a test puts one here that never returns.
pub(crate) type Opener = fn(
    &gpui::BackgroundExecutor,
    ConnectionConfig,
    String,
) -> gpui::Task<Result<(Connection, Vec<String>), DbError>>;

/// The statement timeout in seconds an agent connection carries, given the
/// profile's own: the smaller of the two, with 0 (none) meaning the agent's.
pub(crate) fn agent_timeout(user: u32) -> u32 {
    match user {
        0 => AGENT_TIMEOUT,
        user => user.min(AGENT_TIMEOUT),
    }
}

/// The config an agent connection is opened from: the profile's, bounded by
/// [`agent_timeout`]. [`Lane::opened_from`] stays the profile's own config, the
/// value it is compared with.
pub(crate) fn agent_config(config: &ConnectionConfig) -> ConnectionConfig {
    config
        .clone()
        .with_statement_timeout(agent_timeout(config.statement_timeout()))
}

/// Said when the agent's connection could not be made. Names nothing about
/// where the database is: the reason is the server's and may carry a host, a
/// user or a path.
pub(crate) const OPEN_FAILED: &str =
    "DBDelve could not open the agent's connection to this database.";

// ponytail: Claude Code trims an error result past about 11,000 characters, so
// a statement's error is cut well before that. The upgrade is a setting.
pub(crate) const ERROR_CAP: usize = 4_000;

/// What replaces a hidden string in an error an agent reads.
const HIDDEN: &str = "[hidden]";

/// What an error cut at [`ERROR_CAP`] ends with.
const CLIPPED: &str = " [clipped]";

/// A character of a name: what a hidden string must not be glued to.
fn in_a_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `message` with every string of `hidden` replaced by `[hidden]` wherever it
/// stands whole, then cut to [`ERROR_CAP`] characters.
///
/// A string stands whole when the character before and after it, if its own
/// edge is a name character, is not one: `sa` goes from `by sa.` and `'sa'`
/// and stays in `usage`. There is no length below which a string is let
/// through. Case is ignored in ASCII, because a server re-cases what it echoes
/// (Snowflake upper-cases identifiers). Longer strings are tried first, so
/// `db.internal:5432` goes as one marker rather than a marker and a port. A
/// MySQL account, `'user'@'host'`, goes as one, whatever it is made of.
pub(crate) fn scrubbed(message: &str, hidden: &[String]) -> String {
    let mut hidden: Vec<&str> = hidden
        .iter()
        .map(String::as_str)
        .filter(|text| !text.is_empty())
        .collect();
    hidden.sort_by_key(|text| std::cmp::Reverse(text.len()));
    let bytes = message.as_bytes();
    let mut out = String::with_capacity(message.len());
    let mut at = 0;
    while at < message.len() {
        let end = account_end(message, at).or_else(|| {
            hidden.iter().find_map(|text| {
                let end = at + text.len();
                let first = text.as_bytes()[0];
                let last = text.as_bytes()[text.len() - 1];
                (bytes.get(at..end)?.eq_ignore_ascii_case(text.as_bytes())
                    && !(in_a_name(first) && at > 0 && in_a_name(bytes[at - 1]))
                    && !(in_a_name(last) && bytes.get(end).is_some_and(|&next| in_a_name(next))))
                .then_some(end)
            })
        });
        match end {
            Some(end) => {
                out.push_str(HIDDEN);
                at = end;
            }
            None => {
                let character = message[at..].chars().next().unwrap_or_default();
                out.push(character);
                at += character.len_utf8().max(1);
            }
        }
    }
    match out.char_indices().nth(ERROR_CAP) {
        Some((cut, _)) => {
            out.truncate(cut);
            out.push_str(CLIPPED);
            out
        }
        None => out,
    }
}

/// Where the account spelled `'user'@'host'` at `at` ends, if one starts there.
fn account_end(message: &str, at: usize) -> Option<usize> {
    let rest = message[at..].strip_prefix('\'')?;
    // The user ends at the next quote, which must open the host: a quote that
    // closes something else is not an account.
    let (_, after_user) = rest.split_once('\'')?;
    let host = after_user.strip_prefix("@'")?;
    let (_, after_host) = host.split_once('\'')?;
    Some(message.len() - after_host.len())
}

/// The profile an agent's call resolved to, taken from the list in the turn
/// that resolved it. [`Workspace::run_agent_work`] is handed this and not an id
/// to look up again: nothing between the two can change the list.
pub(crate) struct Resolved {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) config: ConnectionConfig,
}

impl Resolved {
    pub(super) fn of(profile: &Profile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name.clone(),
            config: profile.config.clone(),
        }
    }
}

/// Said for an `explain_query` of a statement that needs the user's approval:
/// the plan tool runs reads only, so an `EXPLAIN ANALYZE` of a write never goes
/// through approval and no connection is opened for it. `execute_sql` holds such
/// a statement for the person instead.
pub(crate) const NOT_EXPLAINED: &str =
    "a statement that needs the user's approval is not run by explain_query";

/// The agent's calls on one profile, and the connection of its own they share.
///
/// One call runs at a time: the connection runs one statement, and a call that
/// arrived while another ran would otherwise be answered with the first's rows
/// or cancel the first's statement. Later calls wait here, in the order they
/// came, and never in [`AgentState::calls`](super::agent::AgentState::calls),
/// which holds only what a person is asked about: a read draws no dialog and
/// is not a waiting call.
pub(crate) struct Lane {
    /// The config `connection` was opened from, its password slot as the
    /// profile holds it (blank when the keychain has it), so a profile whose
    /// config has since changed is told apart from one that has not. Without a
    /// connection it is the config the next read opens from.
    pub(crate) opened_from: ConnectionConfig,
    /// `None` until a read opens it, and again after one that ended early or
    /// failed.
    pub(crate) connection: Option<Connection>,
    /// What an error that came back through this connection must not say
    /// ([`ConnectionConfig::identifying_text`] of the config it opened with,
    /// its password included). It lives exactly as long as the connection,
    /// which holds the same strings.
    pub(crate) hidden: Vec<String>,
    pub(crate) running: Option<Running>,
    pub(crate) waiting: VecDeque<Waiting>,
}

/// The call a lane is working on. Dropping it drops the work, which is what
/// keeps a result that comes back late from landing.
pub(crate) struct Running {
    pub(crate) id: CallId,
    tool: Tool,
    profile_name: String,
    reply: SyncSender<Reply>,
    /// The statement as the agent sent it, for the audit. A catalog call has
    /// none.
    sql: Option<String>,
    /// When the call reached the window, for the audit's elapsed time.
    arrived: Instant,
    /// The approval a person gave the statement, which the audit says instead
    /// of that the gate let it run.
    approved: Option<Approved>,
    /// What stops the statement on the server when the call ends early.
    cancel: CancelToken,
    /// The connection the statement runs on, held here as soon as it exists so
    /// a cancel can reach it: a call that opens its own, as the first of a lane
    /// and every DuckDB call do, has none until its open returns. `None` only
    /// while that open is still in flight, when no statement has been sent.
    connection: Option<Connection>,
    _work: gpui::Task<()>,
    /// Ends the call at [`CALL_DEADLINE`]. Dropped with the call, whatever
    /// ended it, so it never fires for one that is over.
    _deadline: gpui::Task<()>,
}

impl Running {
    /// Whether a cancel has a connection to go to.
    #[cfg(test)]
    pub(crate) fn holds_connection(&self) -> bool {
        self.connection.is_some()
    }
}

/// A call that has not started: it holds its caller's reply and what to do.
pub(crate) struct Waiting {
    pub(crate) id: CallId,
    tool: Tool,
    profile_name: String,
    reply: SyncSender<Reply>,
    arrived: Instant,
    work: AgentWork,
    /// The config the call was admitted for. It starts only on that target:
    /// see [`no_longer_admitted`].
    admitted: ConnectionConfig,
}

/// What a waiting call will do once its lane is free.
#[derive(Clone)]
pub(crate) enum AgentWork {
    /// A statement. `approved` is what a person's approval of it left, for a
    /// read the gate held for them; a read the gate let run has none.
    Query {
        sql: String,
        approved: Option<Approved>,
    },
    /// The agent connection's catalog: its schemas, or with `schema` the
    /// relations of that one. DBDelve reads it through the driver and sends no
    /// statement of its own for it.
    Catalog { schema: Option<String> },
    /// One relation's definition, read through the driver like the catalog.
    Structure { schema: String, table: String },
    /// The catalog's schema, relation and routine names that match `query`.
    Search { query: String },
    /// One schema's foreign keys, read in bulk as the ER diagram reads them.
    Relationships { schema: String },
    /// One schema's routines by signature, or with `routine` the definitions
    /// of every one of that name.
    Routines {
        schema: String,
        routine: Option<String>,
    },
    /// The engine's EXPLAIN of the agent's statement, already composed and
    /// already judged by the gate as this exact text.
    Explain { statement: String },
}

impl AgentWork {
    fn tool(&self) -> Tool {
        match self {
            AgentWork::Query { .. } => Tool::ExecuteSql,
            AgentWork::Catalog { schema: None } => Tool::ListSchemas,
            AgentWork::Catalog { schema: Some(_) } => Tool::ListTables,
            AgentWork::Structure { .. } => Tool::DescribeTable,
            AgentWork::Search { .. } => Tool::SearchSchema,
            AgentWork::Relationships { .. } => Tool::ListRelationships,
            AgentWork::Routines { routine: None, .. } => Tool::ListRoutines,
            AgentWork::Routines {
                routine: Some(_), ..
            } => Tool::DescribeRoutine,
            AgentWork::Explain { .. } => Tool::ExplainQuery,
        }
    }

    /// The approval this work ran under, if a person gave one.
    fn approved(&self) -> Option<Approved> {
        match self {
            AgentWork::Query { approved, .. } => approved.clone(),
            _ => None,
        }
    }

    /// The statement the audit records, for the work that sends one.
    fn sql(&self) -> Option<String> {
        match self {
            AgentWork::Query { sql, .. } => Some(sql.clone()),
            AgentWork::Explain { statement } => Some(statement.clone()),
            AgentWork::Catalog { .. }
            | AgentWork::Structure { .. }
            | AgentWork::Search { .. }
            | AgentWork::Relationships { .. }
            | AgentWork::Routines { .. } => None,
        }
    }
}

/// Said for a read the gate let run whose profile's access level changed
/// before it started.
pub(crate) const ACCESS_CHANGED: &str =
    "The connection's access level changed before the statement started, so it was not run.";

/// Why `waiting` may not start on a profile that now holds `config` at `mode`,
/// or `None` when it still may: it was admitted for another target, or the
/// gate no longer gives its statement the verdict it was admitted under (a
/// read it let run, or the approval a person gave).
///
/// `known` is the server version known now; an approval is asked again on
/// the version it was given on, so a version learned meanwhile changes
/// nothing a person decided.
fn no_longer_admitted(
    waiting: &Waiting,
    config: &ConnectionConfig,
    mode: Mode,
    known: Option<u32>,
) -> Option<(Status, &'static str)> {
    if waiting.admitted.retargets(config) {
        return Some((Status::Rejected, super::agent::NO_LONGER_AVAILABLE));
    }
    let sql = match &waiting.work {
        AgentWork::Query { sql, .. } => sql,
        AgentWork::Explain { statement } => statement,
        AgentWork::Catalog { .. }
        | AgentWork::Structure { .. }
        | AgentWork::Search { .. }
        | AgentWork::Relationships { .. }
        | AgentWork::Routines { .. } => return None,
    };
    let version = waiting
        .work
        .approved()
        .map_or(known, |approved| approved.server_version);
    let approved = waiting.work.approved().map(|approved| approved.verdict);
    let now = AuditVerdict::held_by(agent_gate_on(config.engine(), version, sql, mode));
    match (approved, now) {
        (was, now) if was == now => None,
        (Some(_), _) => Some((Status::RefusedByGate, super::agent::CHANGED_SINCE_SHOWN)),
        (None, _) => Some((Status::RefusedByGate, ACCESS_CHANGED)),
    }
}

/// Said for a `list_tables` call whose schema the catalog does not hold.
pub(crate) const NO_SUCH_SCHEMA: &str = "there is no schema of that name";

/// Said for a `describe_table` call whose schema or relation the catalog does
/// not hold.
pub(crate) const NOT_IN_CATALOG: &str = "there is no table or view of that name in that schema";

/// Said for a `describe_routine` call whose schema or routine the catalog does
/// not hold.
pub(crate) const NO_SUCH_ROUTINE: &str =
    "there is no function or procedure of that name in that schema";

/// What the audit learns of a read from how it ended, beyond what it sent.
#[derive(Default)]
pub(super) struct Ended {
    /// How many rows went back and how many there were, when it was answered
    /// with a result.
    pub(super) rows: Option<(usize, Total)>,
    /// What the audit keeps of the error of a read that failed:
    /// [`audited_error`] of what the agent was given.
    pub(super) error: Option<String>,
}

/// What a person's approval leaves on a read that ran after it: the verdict the
/// dialog showed, and the engine the audit file names.
#[derive(Clone)]
pub(crate) struct Approved {
    pub(crate) verdict: AuditVerdict,
    pub(crate) engine: Engine,
    /// The server version the gate gave `verdict` on.
    pub(crate) server_version: Option<u32>,
}

/// What a read that ended records of itself: for a statement, that the gate
/// let it run and what it sent; for a catalog call, which has no statement
/// and so no verdict, nothing of either. Then how long it took, and what
/// `ended` adds.
fn facts_of(
    sql: Option<String>,
    approved: Option<Approved>,
    arrived: Instant,
    ended: Ended,
) -> CallFacts {
    CallFacts {
        verdict: match &approved {
            Some(approved) => Some(approved.verdict.clone()),
            None => sql.is_some().then_some(AuditVerdict::Run),
        },
        decision: approved.as_ref().map(|_| Decision::Approved),
        engine: approved.map(|approved| approved.engine),
        sql,
        shown: ended.rows.map(|(shown, _)| shown),
        total: ended.rows.map(|(_, total)| total),
        elapsed_ms: elapsed_ms(arrived),
        error: ended.error,
        ..CallFacts::default()
    }
}

/// What the blocking half of a read came back with.
#[allow(
    clippy::large_enum_variant,
    reason = "one value per read, moved straight into its landing"
)]
enum Ran {
    /// The connection could not be made, or could not be held read-only, for
    /// this reason.
    NotOpened(DbError),
    Done {
        connection: Connection,
        hidden: Vec<String>,
        result: Result<Shaped, DbError>,
    },
    /// A catalog call came back: `None` is a name the catalog does not hold.
    Listed {
        connection: Connection,
        hidden: Vec<String>,
        listed: Result<Option<Listing>, DbError>,
    },
}

impl Ran {
    /// Whether the statement failed and the session it ran on is gone, so
    /// nothing sent on that connection again can succeed.
    fn session_lost(&self) -> bool {
        match self {
            Ran::Done {
                connection,
                result: Err(_),
                ..
            }
            | Ran::Listed {
                connection,
                listed: Err(_),
                ..
            } => connection.is_lost(),
            Ran::NotOpened(_) | Ran::Done { .. } | Ran::Listed { .. } => false,
        }
    }
}

/// What a statement's result became for the agent, written on the thread that
/// ran the statement: a result can hold one very large cell, and copying it
/// into text is not work for the thread that draws the window.
struct Shaped {
    text: String,
    /// How many rows went back and how many there were; `None` for a plan,
    /// whose input rows are not what is sent.
    rows: Option<(usize, Total)>,
}

/// A catalog reply and how many rows it lists.
struct Listing {
    text: String,
    /// How many rows went back and how many there were.
    rows: (usize, Total),
}

impl Listing {
    /// A reply that lists every one of its `rows`.
    fn all(text: String, rows: usize) -> Self {
        Self::shown(text, rows, rows)
    }

    /// A reply that wrote `shown` of `total` rows, the rest dropped for the
    /// byte budget.
    fn shown(text: String, shown: usize, total: usize) -> Self {
        Self {
            text,
            rows: (shown, Total::Exact(total)),
        }
    }
}

/// One result set as the agent reads it. A Postgres spatial cell is rendered
/// first, as an export renders it, so hex EWKB never spends the agent's tokens.
pub(super) fn table_of(engine: Engine, result: &QueryResult) -> result::Table {
    // Only the cells that wait on their type are copied out and replaced; the
    // rest are read where they stand.
    let rendered: HashMap<(usize, usize), String> = db::render(db::unrendered(engine, result))
        .into_iter()
        .map(|(row, column, _, value)| ((row, column), value))
        .collect();
    let rows: Vec<Vec<Option<String>>> = result
        .rows
        .iter()
        .enumerate()
        .map(|(at, row)| {
            row.iter()
                .enumerate()
                .map(|(column, cell)| {
                    rendered
                        .get(&(at, column))
                        .cloned()
                        .or_else(|| cell.map(str::to_string))
                })
                .collect()
        })
        .collect();
    // A statement stopped at the limit has no known count: the server was not
    // asked for the rest.
    let total = if result.stopped.is_some() {
        result::Total::AtLeast(rows.len())
    } else {
        result::Total::Exact(result.capped_from.unwrap_or(rows.len()))
    };
    result::Table {
        label: "columns",
        heads: result
            .columns
            .iter()
            .map(|column| result::Head {
                name: column.name.clone(),
                data_type: column.data_type.clone(),
            })
            .collect(),
        rows,
        total,
    }
}

/// A statement's rows as the agent reads them.
fn shaped(engine: Engine, result: &QueryResult) -> Shaped {
    let table = table_of(engine, result);
    let total = table.total;
    // The rows the byte budget dropped were not sent, so they are not counted.
    let (text, shown) = result::text_shown(&[table], result.rest.len(), None);
    Shaped {
        text,
        rows: Some((shown, total)),
    }
}

/// A plan as the agent reads it.
fn shaped_plan(engine: Engine, result: &QueryResult) -> Shaped {
    let (_, text) = plan_reply(engine, result);
    // The plan's lines are rewritten into findings and a tree, so no count of
    // the server's rows says what the agent was sent.
    Shaped { text, rows: None }
}

impl Workspace {
    /// Take one statement the gate let through for `profile_id`: it runs on
    /// the agent's own connection to that profile when the lane is free, and
    /// after the calls ahead of it when not. Answers with its first rows.
    pub(crate) fn run_agent_sql(
        &mut self,
        id: CallId,
        profile: Resolved,
        sql: String,
        reply: SyncSender<Reply>,
        arrived: Instant,
        cx: &mut Context<Self>,
    ) {
        self.run_agent_work(
            id,
            profile,
            AgentWork::Query {
                sql,
                approved: None,
            },
            reply,
            arrived,
            cx,
        );
    }

    /// Take work for `profile`'s lane, on the same terms as
    /// [`Workspace::run_agent_sql`]: the profile must be one an agent may read,
    /// and the work waits its turn on the agent's own connection.
    pub(crate) fn run_agent_work(
        &mut self,
        id: CallId,
        profile: Resolved,
        work: AgentWork,
        reply: SyncSender<Reply>,
        arrived: Instant,
        cx: &mut Context<Self>,
    ) {
        let Resolved {
            id: profile_id,
            name: profile_name,
            config,
        } = profile;
        if let Some(refusal) = config.agent_refusal() {
            // Before a lane exists: nothing is opened for a profile an agent
            // cannot read.
            self.agent_answered(
                id,
                work.tool(),
                Some(profile_name),
                None,
                &reply,
                Reply {
                    status: Status::RefusedByGate,
                    text: refusal.to_string(),
                },
                CallFacts {
                    // A catalog call has no statement to judge, so no verdict.
                    verdict: work
                        .sql()
                        .map(|_| AuditVerdict::Refuse(refusal.to_string())),
                    sql: work.sql(),
                    elapsed_ms: elapsed_ms(arrived),
                    ..CallFacts::default()
                },
            );
            return;
        }
        let admitted = config.clone();
        self.agent
            .lanes
            .entry(profile_id.clone())
            .or_insert_with(|| Lane {
                opened_from: config,
                connection: None,
                hidden: Vec::new(),
                running: None,
                waiting: VecDeque::new(),
            })
            .waiting
            .push_back(Waiting {
                id,
                tool: work.tool(),
                profile_name,
                reply,
                arrived,
                work,
                admitted,
            });
        self.start_next(&profile_id, cx);
    }

    /// Start the oldest waiting call of `profile_id`'s lane if nothing runs
    /// there, opening the lane's connection first when it has none or when the
    /// profile's config is no longer the one it opened from. A lane left with
    /// nothing to do and no connection is removed.
    pub(crate) fn start_next(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let Some(lane) = self.agent.lanes.get_mut(profile_id) else {
            return;
        };
        if lane.running.is_some() {
            return;
        }
        if lane.waiting.is_empty() {
            if lane.connection.is_none() {
                self.agent.lanes.remove(profile_id);
            }
            return;
        }
        // A hide saved by another window since these reads were admitted
        // ends the lane, as one made here does (`reject_calls_for`): the
        // file is read again only when it changed.
        self.adopt_saved_visibility_for_read(cx);
        if !self.agent.lanes.contains_key(profile_id) {
            return;
        }
        // The one profile with this id, as `agent_profile` resolved it: a
        // repeated id is never resolved, so the lane cannot open another
        // entry's config. It must still be one an agent may reach.
        let Some((config, mode)) = self
            .sole_profile(profile_id)
            .filter(|profile| profile.agent_visible)
            .map(|profile| (profile.config.clone(), profile.mode))
        else {
            // A hide or a removal ends the lane in the same turn through
            // `reject_calls_for`, so this is a backstop: if a profile ever did
            // go, or go hidden, with calls waiting, or its id were ever shared,
            // each is answered and none is sent or stranded.
            self.end_lane(
                profile_id,
                Status::Rejected,
                super::agent::NO_LONGER_AVAILABLE,
                cx,
            );
            return;
        };
        // What waited may no longer be what was admitted: the target moved,
        // or the mode the gate judged it at changed. Such a call is answered
        // here and never sent; one already running is left to finish, as the
        // mode it was sent under was in force when it was sent.
        let known = self.known_server_version(profile_id, &config);
        let Some(lane) = self.agent.lanes.get_mut(profile_id) else {
            return;
        };
        let mut stale = Vec::new();
        let mut kept = VecDeque::new();
        for waiting in lane.waiting.drain(..) {
            match no_longer_admitted(&waiting, &config, mode, known) {
                Some(why) => stale.push((waiting, why)),
                None => kept.push_back(waiting),
            }
        }
        lane.waiting = kept;
        for (waiting, (status, text)) in stale {
            let _ = waiting.reply.send(Reply {
                status,
                text: text.to_string(),
            });
            self.agent.record(
                waiting.id,
                waiting.tool,
                Some(waiting.profile_name),
                None,
                status,
                facts_of(
                    waiting.work.sql(),
                    waiting.work.approved(),
                    waiting.arrived,
                    Ended::default(),
                ),
            );
        }
        let Some(lane) = self.agent.lanes.get_mut(profile_id) else {
            return;
        };
        if lane.waiting.is_empty() {
            if lane.connection.is_none() {
                self.agent.lanes.remove(profile_id);
            }
            cx.notify();
            return;
        }
        if lane.opened_from != config {
            // ponytail: a changed connection is reopened on the next read
            // rather than on the change, since nothing is read until then.
            lane.connection = None;
            lane.hidden.clear();
            lane.opened_from = config.clone();
        }
        let Some(Waiting {
            id,
            tool,
            profile_name,
            reply,
            arrived,
            work,
            ..
        }) = lane.waiting.pop_front()
        else {
            return;
        };
        let cached = lane
            .connection
            .clone()
            .map(|connection| (connection, lane.hidden.clone()));
        let engine = config.engine();
        let cancel = CancelToken::default();
        let sent = work.sql();
        let approved = work.approved();
        if approved.is_none() {
            self.agent
                .note_started(id, tool, &profile_name, engine, sent.as_deref(), arrived);
        }

        // Called here and not in the task, so what opens is decided when the
        // call starts.
        let open = self.agent.opener.unwrap_or(open_agent);
        let mut reused = cached.is_some();
        let mut opening = match cached {
            Some(cached) => gpui::Task::ready(Ok(cached)),
            None => open(
                cx.background_executor(),
                config.clone(),
                profile_id.to_string(),
            ),
        };
        let task = cx.spawn({
            let profile_id = profile_id.to_string();
            let cancel = cancel.clone();
            async move |workspace, cx| {
                let ran = loop {
                    let (connection, hidden) = match opening.await {
                        Ok(opened) => opened,
                        Err(error) => break Ran::NotOpened(error),
                    };
                    // Before the statement is sent, so every end of the call
                    // after this point has a connection to cancel on. A call
                    // that ended during the open finds nothing running under
                    // its id, and sends no statement.
                    let held = workspace.update(cx, |workspace, _| {
                        workspace.hold_agent_connection(id, &profile_id, &connection)
                    });
                    if !held.unwrap_or(false) {
                        return;
                    }
                    let (ran, lost) = spawn_blocking(cx.background_executor(), {
                        let (work, cancel) = (work.clone(), cancel.clone());
                        move || {
                            let ran = run_work(work, connection, hidden, &cancel, engine);
                            // Asked only of a connection that sat idle between
                            // calls and then failed: the driver learns the
                            // server dropped it from the statement that finds
                            // out, so nothing before one can tell.
                            let lost = reused && ran.session_lost();
                            (ran, lost)
                        }
                    })
                    .await;
                    if !lost {
                        break ran;
                    }
                    // ponytail: the one reopen is a retry of a read that
                    // failed on a dead session, and a read that died with the
                    // session while running is run again, inside the same
                    // call deadline. The upgrade is telling the two apart.
                    reused = false;
                    opening = open(cx.background_executor(), config.clone(), profile_id.clone());
                };
                // The window may have closed meanwhile, and then nobody is
                // left to answer.
                let _ = workspace.update(cx, |workspace, cx| {
                    workspace.land_agent_read(id, &profile_id, ran, cx);
                });
            }
        });
        let deadline = cx.spawn(async move |workspace, cx| {
            cx.background_executor().timer(CALL_DEADLINE).await;
            _ = workspace.update(cx, |workspace, cx| {
                workspace.finish_call(id, Status::Failed, DEADLINE_PASSED.to_string(), cx)
            });
        });
        if let Some(lane) = self.agent.lanes.get_mut(profile_id) {
            lane.running = Some(Running {
                id,
                tool,
                profile_name,
                reply,
                sql: sent,
                arrived,
                approved,
                cancel,
                connection: None,
                _work: task,
                _deadline: deadline,
            });
        }
    }

    /// Remember `connection` as the one running call `id` is on, so a hangup,
    /// the deadline, a hide or the kill switch can cancel the statement there.
    /// Answers whether `id` is still the call running on `profile_id`'s lane.
    fn hold_agent_connection(
        &mut self,
        id: CallId,
        profile_id: &str,
        connection: &Connection,
    ) -> bool {
        let Some(lane) = self.agent.lanes.get_mut(profile_id) else {
            return false;
        };
        // Learned here, so the next call is gated on the server it reaches.
        if let Some(version) = connection.server_version() {
            self.agent
                .server_versions
                .insert(profile_id.to_string(), (lane.opened_from.clone(), version));
        }
        match lane.running.as_mut().filter(|running| running.id == id) {
            Some(running) => {
                running.connection = Some(connection.clone());
                true
            }
            None => false,
        }
    }

    /// Take what a read came back with: keep its connection and answer. A read
    /// whose call ended meanwhile finds nothing running under its id, and what
    /// it came back with is dropped.
    fn land_agent_read(&mut self, id: CallId, profile_id: &str, ran: Ran, cx: &mut Context<Self>) {
        let Some(lane) = self.agent.lanes.get_mut(profile_id) else {
            return;
        };
        if lane.running.as_ref().is_none_or(|running| running.id != id) {
            return;
        }
        let mut ended = Ended::default();
        // What a catalog call that found nothing says: which tool asked decides
        // whether it was a schema or a relation.
        let tool = lane.running.as_ref().map(|running| running.tool);
        let not_found = match tool {
            Some(Tool::DescribeTable) => NOT_IN_CATALOG,
            Some(Tool::DescribeRoutine) => NO_SUCH_ROUTINE,
            _ => NO_SUCH_SCHEMA,
        };
        let (status, text) = match ran {
            Ran::NotOpened(error) => {
                lane.connection = None;
                // What the open's error must not name, as the config the lane
                // opened from says; the keychain's password is not in that
                // config, and was scrubbed where it was put in (`open_error`).
                ended.error = Some(audited_open_error(
                    &error.message,
                    &lane.opened_from.identifying_text(),
                ));
                (Status::Failed, OPEN_FAILED.to_string())
            }
            Ran::Done {
                connection,
                hidden,
                result: Ok(Shaped { text, rows }),
            } => {
                // A DuckDB connection is let go here, so the next call opens
                // afresh and sees what the user has committed since.
                if !lane.opened_from.agent_opens_per_call() {
                    lane.connection = Some(connection);
                    lane.hidden = hidden;
                }
                ended.rows = rows;
                (Status::Executed, text)
            }
            Ran::Listed {
                connection,
                hidden,
                listed: Ok(listed),
            } => {
                // Kept on the same terms as a read's: the catalog call ended
                // cleanly, and so did the connection it ran on.
                if !lane.opened_from.agent_opens_per_call() {
                    lane.connection = Some(connection);
                    lane.hidden = hidden;
                }
                match listed {
                    Some(Listing { text, rows }) => {
                        ended.rows = Some(rows);
                        (Status::Executed, text)
                    }
                    None => (Status::Failed, not_found.to_string()),
                }
            }
            Ran::Done {
                hidden,
                result: Err(error),
                ..
            }
            | Ran::Listed {
                hidden,
                listed: Err(error),
                ..
            } => {
                // ponytail: every failure ends the connection rather than
                // asking which ones end a session, since a Redis timeout
                // closes it without `is_lost` knowing (D-02). The cost is one
                // reopen after a failed statement.
                lane.connection = None;
                let text = scrubbed(&error.message, &hidden);
                ended.error = Some(audited_error(&text));
                (Status::Failed, text)
            }
        };
        // An answer that is not an error leaves the connection, and what an
        // error through it must not say, as they were.
        if status == Status::Failed && lane.connection.is_none() {
            lane.hidden.clear();
        }
        self.finish_read(id, status, text, false, ended, cx);
    }

    /// End the read `id`, running or waiting, once: it is removed first, so a
    /// call that two things end at once is answered and recorded by whichever
    /// comes first. `early` is an end that is not the statement's own answer:
    /// the statement is cancelled on the server and the lane's connection is
    /// dropped. `ended` is what the audit learns from the end: the rows that went
    /// back, or the full error. Then the lane's next call starts. Answers whether there was
    /// such a call.
    pub(super) fn finish_read(
        &mut self,
        id: CallId,
        status: Status,
        text: String,
        early: bool,
        mut ended: Ended,
        cx: &mut Context<Self>,
    ) -> bool {
        let found = self.agent.lanes.iter_mut().find_map(|(profile_id, lane)| {
            if lane
                .running
                .as_ref()
                .is_some_and(|running| running.id == id)
            {
                let running = lane.running.take()?;
                if early {
                    // ponytail: the connection is dropped rather than reused,
                    // because a cancelled statement may still be finishing on
                    // it and a cancel stops whatever the connection is running,
                    // which would reach the next call. The cost is one reopen.
                    lane.connection = None;
                    lane.hidden.clear();
                }
                let stop = running
                    .connection
                    .map(|connection| (connection, running.cancel));
                return Some((
                    profile_id.clone(),
                    running.tool,
                    running.profile_name,
                    running.reply,
                    facts_of(
                        running.sql,
                        running.approved,
                        running.arrived,
                        std::mem::take(&mut ended),
                    ),
                    stop.filter(|_| early),
                ));
            }
            let place = lane.waiting.iter().position(|waiting| waiting.id == id)?;
            let waiting = lane.waiting.remove(place)?;
            Some((
                profile_id.clone(),
                waiting.tool,
                waiting.profile_name,
                waiting.reply,
                facts_of(
                    waiting.work.sql(),
                    waiting.work.approved(),
                    waiting.arrived,
                    std::mem::take(&mut ended),
                ),
                None,
            ))
        });
        let Some((profile_id, tool, name, reply, mut facts, stop)) = found else {
            return false;
        };
        // Only a hangup ends a read `Cancelled`: a deadline fails it and a
        // hidden profile rejects it.
        facts.requester_gone = status == Status::Cancelled;
        if let Some((connection, cancel)) = stop {
            // A thread of its own: the pool may be full of the very statements
            // this stops.
            spawn_cancel(cx.background_executor(), move || connection.cancel(&cancel)).detach();
        }
        if status != Status::Cancelled {
            // The caller may have hung up, and then nobody reads this.
            let _ = reply.send(Reply { status, text });
        }
        self.agent.record(id, tool, Some(name), None, status, facts);
        self.start_next(&profile_id, cx);
        cx.notify();
        true
    }

    /// End every read of `profile_id`'s lane, the running one first and then
    /// the waiting ones in the order they came, each with `status` and `text`,
    /// and drop the lane with its connection. Nothing of the lane starts again.
    pub(crate) fn end_lane(
        &mut self,
        profile_id: &str,
        status: Status,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(mut lane) = self.agent.lanes.remove(profile_id) else {
            return;
        };
        let running = lane.running.take();
        if let Some(running) = &running
            && let Some(connection) = running.connection.clone()
        {
            let cancel = running.cancel.clone();
            spawn_cancel(cx.background_executor(), move || connection.cancel(&cancel)).detach();
        }
        let ended = running
            .map(|running| {
                (
                    running.id,
                    running.tool,
                    running.profile_name,
                    running.reply,
                    facts_of(
                        running.sql,
                        running.approved,
                        running.arrived,
                        Ended::default(),
                    ),
                )
            })
            .into_iter()
            .chain(lane.waiting.drain(..).map(|waiting| {
                (
                    waiting.id,
                    waiting.tool,
                    waiting.profile_name,
                    waiting.reply,
                    facts_of(
                        waiting.work.sql(),
                        waiting.work.approved(),
                        waiting.arrived,
                        Ended::default(),
                    ),
                )
            }));
        for (id, tool, name, reply, facts) in ended {
            let _ = reply.send(Reply {
                status,
                text: text.to_string(),
            });
            self.agent.record(id, tool, Some(name), None, status, facts);
        }
        cx.notify();
    }
}

/// Open the agent's connection to a profile and hold it read-only, with the
/// strings an error through it must not name.
fn open_agent(
    executor: &gpui::BackgroundExecutor,
    config: ConnectionConfig,
    profile_id: String,
) -> gpui::Task<Result<(Connection, Vec<String>), DbError>> {
    spawn_blocking(executor, move || {
        let mut config = agent_config(&config);
        // A file engine has nothing to authenticate to, so it never reaches
        // the keychain.
        if let Some(slot) = config.password_mut()
            && slot.is_empty()
        {
            match store::password(&profile_id) {
                Ok(Some(password)) => *slot = password,
                Ok(None) => {}
                Err(error) => {
                    return Err(DbError {
                        message: error,
                        position: None,
                    });
                }
            }
        }
        // Taken after the password is in: it is one of the strings a server's
        // error may echo.
        let hidden = config.identifying_text();
        // Opened through `open_for_agent`: the hold is what makes a read the gate
        // misjudged harmless, so a connection that cannot take it is not one the
        // agent may use.
        let connection =
            Connection::open_for_agent(config).map_err(|error| open_error(error, &hidden))?;
        Ok((connection, hidden))
    })
}

/// An open's error scrubbed of `hidden`, taken after the keychain's password
/// was put in: the lane's own config, which the audit scrubs against later,
/// never holds that password. A password command's error is left out first,
/// as the audit leaves it out.
fn open_error(error: DbError, hidden: &[String]) -> DbError {
    DbError {
        message: scrubbed(crate::db::loggable_error(&error.message), hidden),
        ..error
    }
}

/// What the audit keeps of an error opening the agent's connection. A
/// password command's error can echo the secret it printed, so none of it is
/// kept, as none of it is logged; the rest is scrubbed of `hidden`.
pub(super) fn audited_open_error(message: &str, hidden: &[String]) -> String {
    audited_error(&scrubbed(crate::db::loggable_error(message), hidden))
}

/// The blocking half of any call: what `work` asks, on `connection`.
fn run_work(
    work: AgentWork,
    connection: Connection,
    hidden: Vec<String>,
    cancel: &CancelToken,
    engine: Engine,
) -> Ran {
    match work {
        AgentWork::Query { sql, .. } => read_on(connection, hidden, &sql, cancel, engine),
        AgentWork::Catalog { schema } => list_on(
            connection,
            hidden,
            schema.as_deref(),
            engine.agent_catalog_note(),
        ),
        AgentWork::Explain { statement } => {
            explain_on(connection, hidden, &statement, cancel, engine)
        }
        AgentWork::Structure { schema, table } => describe_on(
            connection,
            hidden,
            &schema,
            &table,
            engine.agent_catalog_note(),
        ),
        AgentWork::Search { query } => {
            let listed = search(&connection, &query, engine.agent_catalog_note());
            Ran::Listed {
                connection,
                hidden,
                listed,
            }
        }
        AgentWork::Relationships { schema } => {
            let listed = relationships(&connection, &schema, engine);
            Ran::Listed {
                connection,
                hidden,
                listed,
            }
        }
        AgentWork::Routines { schema, routine } => {
            let listed = routines(&connection, &schema, routine.as_deref());
            Ran::Listed {
                connection,
                hidden,
                listed,
            }
        }
    }
}

/// The blocking half of a read: run the statement on the agent's connection as
/// written.
fn read_on(
    connection: Connection,
    hidden: Vec<String>,
    sql: &str,
    cancel: &CancelToken,
    engine: Engine,
) -> Ran {
    let result = connection.query(
        sql,
        cancel,
        Fetch {
            limit: Some(AGENT_ROWS),
            reads_only: crate::sql::stoppable(engine, sql),
            ..Fetch::default()
        },
    );
    Ran::Done {
        connection,
        hidden,
        result: result.map(|result| shaped(engine, &result)),
    }
}

/// The blocking half of `explain_query`: send the composed EXPLAIN as it is
/// and read the whole plan back. Drained, never stopped at a limit: an ANALYZE
/// has already run the statement it wraps when the first row arrives.
fn explain_on(
    connection: Connection,
    hidden: Vec<String>,
    statement: &str,
    cancel: &CancelToken,
    engine: Engine,
) -> Ran {
    let result = connection.query(
        statement,
        cancel,
        Fetch {
            limit: Some(PLAN_ROWS),
            ..Fetch::default()
        },
    );
    Ran::Done {
        connection,
        hidden,
        result: result.map(|result| shaped_plan(engine, &result)),
    }
}

/// The blocking half of a catalog call: read the catalog on the agent's
/// connection and write it as the agent reads it. `schema` is `None` for the
/// schemas and a name for the relations of that one.
fn list_on(
    connection: Connection,
    hidden: Vec<String>,
    schema: Option<&str>,
    note: Option<&str>,
) -> Ran {
    let listed = list(&connection, schema, note);
    Ran::Listed {
        connection,
        hidden,
        listed,
    }
}

fn list(
    connection: &Connection,
    schema: Option<&str>,
    note: Option<&str>,
) -> Result<Option<Listing>, DbError> {
    let mut catalog = connection.catalog()?;
    let Some(schema) = schema else {
        let rows = catalog.schemas.len();
        let (text, shown) = schemas_text(&catalog, note);
        return Ok(Some(Listing::shown(text, shown, rows)));
    };
    let Some(rows) = catalog
        .schemas
        .iter()
        .find(|held| held.name == schema)
        .map(|held| held.relations.len())
    else {
        // Nothing is asked of the server for a schema that is not there.
        return Ok(None);
    };
    // The estimate is optional, and the query behind it can need privileges
    // the catalog does not, so a failure here leaves the rows `null`.
    if let Ok(sizes) = connection.sizes() {
        catalog.set_sizes(&sizes);
    }
    Ok(tables_text(&catalog, schema, note).map(|(text, shown)| Listing::shown(text, shown, rows)))
}

/// The blocking half of `describe_table`: see [`describe`].
fn describe_on(
    connection: Connection,
    hidden: Vec<String>,
    schema: &str,
    table: &str,
    note: Option<&str>,
) -> Ran {
    let listed = connection.catalog().and_then(|catalog| {
        describe(&catalog, schema, table, note, |s, t| {
            connection.structure(s, t)
        })
    });
    Ran::Listed {
        connection,
        hidden,
        listed,
    }
}

/// Describe `table` of `schema`, once the catalog holds both by exact name.
///
/// The catalog is looked in first, and `load` is called only for a pair found
/// there: MongoDB opens whatever database it is given, so a name the agent
/// made up must never reach a loader. `None` is a pair the catalog does not
/// hold.
fn describe(
    catalog: &Catalog,
    schema: &str,
    table: &str,
    note: Option<&str>,
    load: impl FnOnce(&str, &str) -> Result<Structure, DbError>,
) -> Result<Option<Listing>, DbError> {
    let held = catalog
        .schemas
        .iter()
        .find(|held| held.name == schema)
        .is_some_and(|held| held.relations.iter().any(|relation| relation.name == table));
    if !held {
        return Ok(None);
    }
    let structure = load(schema, table)?;
    // The columns are what is counted: the other sections are about them.
    let (text, columns) = structure_text(&structure, note);
    Ok(Some(Listing::shown(text, columns, structure.columns.len())))
}

/// A name the catalog holds, as `search_schema` ranks it.
struct Found<'a> {
    schema: &'a str,
    name: &'a str,
    kind: &'static str,
    /// `schema.name`, so a query that names both finds it.
    qualified: [String; 1],
}

/// The reply for `search_schema`: the catalog's schemas, relations and
/// routines whose names match `query`, best first, at most [`AGENT_ROWS`].
///
/// ponytail: names only. The catalog holds no columns by design, and reading
/// them means a `schema_graph` per schema on every call; the upgrade is that,
/// kept on the lane between calls.
fn search(
    connection: &Connection,
    query: &str,
    note: Option<&str>,
) -> Result<Option<Listing>, DbError> {
    let mut catalog = connection.catalog()?;
    // The routines can need privileges the catalog does not, and a server that
    // refuses them still has its relations to search.
    if let Ok(routines) = connection.routines() {
        catalog.merge(routines);
    }
    let found = catalog.by_name().into_iter().flat_map(|(_, schema)| {
        let found = |name, kind| Found {
            schema: &schema.name,
            name,
            kind,
            qualified: [format!("{}.{name}", schema.name)],
        };
        std::iter::once(Found {
            qualified: [String::new()],
            ..found(&schema.name, "schema")
        })
        .chain(
            schema
                .relations
                .iter()
                .map(move |relation| found(&relation.name, kind_name(relation.kind))),
        )
        .chain(
            schema
                .routines
                .iter()
                .map(move |routine| found(&routine.name, routine_kind_name(routine.kind))),
        )
    });
    let ranked = crate::fuzzy::Fuzzy::new().rank(
        &crate::fuzzy::Query::new(query),
        found,
        |found| found.name,
        |found| &found.qualified,
    );
    let matched = ranked.len();
    let rows = ranked
        .into_iter()
        .take(AGENT_ROWS)
        .map(|(found, _)| {
            vec![
                Some(found.schema.to_string()),
                Some(found.name.to_string()),
                Some(found.kind.to_string()),
            ]
        })
        .collect();
    let mut table = table_of_names(&["schema", "name", "kind"], rows);
    table.total = Total::Exact(matched);
    // Counted as written: past [`AGENT_ROWS`], or past the byte budget, a
    // match was found and not sent.
    let (text, shown) = result::text_shown(&[table], 0, note);
    Ok(Some(Listing {
        text,
        rows: (shown, Total::Exact(matched)),
    }))
}

/// The reply for `list_relationships`: one row per foreign key of `schema`,
/// its columns and the ones it references as JSON arrays paired by position.
/// `None` is a schema the catalog does not hold, looked for first so a name
/// the agent made up reaches no loader.
fn relationships(
    connection: &Connection,
    schema: &str,
    engine: Engine,
) -> Result<Option<Listing>, DbError> {
    if !connection
        .catalog()?
        .schemas
        .iter()
        .any(|held| held.name == schema)
    {
        return Ok(None);
    }
    // A plugin answers for itself whatever the target says, so the engine's
    // own target is enough to ask with.
    if !connection.draws_diagrams(crate::db::Target::built_in(engine)) {
        return Ok(Some(Listing::all(
            format!("DBDelve reads no foreign keys from {}.", engine.label()),
            0,
        )));
    }
    let graph = connection.schema_graph(schema)?;
    let names = |table: u32, columns: &[u32]| -> Option<String> {
        let table = graph.tables.get(table as usize)?;
        let names = columns
            .iter()
            .map(|&column| Some(table.columns.get(column as usize)?.name.to_string()))
            .collect::<Option<Vec<_>>>()?;
        serde_json::to_string(&names).ok()
    };
    let rows: Vec<_> = graph
        .keys
        .iter()
        .map(|key| {
            let (references_schema, references_table, references_columns) = match &key.referenced {
                KeyTarget::Table { table, columns } => (
                    graph.schema.to_string(),
                    graph
                        .tables
                        .get(*table as usize)
                        .map(|table| table.name.to_string()),
                    names(*table, columns),
                ),
                // A table in another schema is drawn as a stub, its columns
                // unread.
                KeyTarget::Elsewhere { schema, table } => {
                    (schema.to_string(), Some(table.to_string()), None)
                }
            };
            vec![
                graph
                    .tables
                    .get(key.table as usize)
                    .map(|table| table.name.to_string()),
                Some(key.name.to_string()),
                names(key.table, &key.columns),
                Some(references_schema),
                references_table,
                references_columns,
            ]
        })
        .collect();
    let count = rows.len();
    let mut table = table_of_names(
        &[
            "table",
            "constraint",
            "columns",
            "references_schema",
            "references_table",
            "references_columns",
        ],
        rows,
    );
    table.label = "foreign_keys";
    let (text, shown) = result::text_shown(&[table], 0, None);
    Ok(Some(Listing::shown(text, shown, count)))
}

/// The reply for `list_routines`, or with `routine` for `describe_routine`.
/// `None` is a schema the catalog does not hold, or for `describe_routine` a
/// name none of its routines has.
fn routines(
    connection: &Connection,
    schema: &str,
    routine: Option<&str>,
) -> Result<Option<Listing>, DbError> {
    let catalog = connection.routines()?;
    // The routines catalog holds only the schemas that have some, so one with
    // none is told apart from one that is not there by the relations catalog.
    let held = match catalog.schemas.iter().find(|held| held.name == schema) {
        Some(held) => held.routines.iter().collect::<Vec<_>>(),
        None if connection
            .catalog()?
            .schemas
            .iter()
            .any(|held| held.name == schema) =>
        {
            Vec::new()
        }
        None => return Ok(None),
    };
    let Some(name) = routine else {
        let (text, shown) = result::text_shown(&[signatures(&held)], 0, None);
        return Ok(Some(Listing::shown(text, shown, held.len())));
    };
    let named: Vec<_> = held.into_iter().filter(|held| held.name == name).collect();
    if named.is_empty() {
        return Ok(None);
    }
    let (text, shown) = definitions_text(&named);
    Ok(Some(Listing::shown(text, shown, named.len())))
}

fn signatures(routines: &[&Routine]) -> result::Table {
    let rows = routines
        .iter()
        .map(|routine| {
            vec![
                Some(routine.name.clone()),
                Some(routine_kind_name(routine.kind).to_string()),
                Some(routine.identity_arguments.clone()),
                Some(routine.result_type.clone()).filter(|result| !result.is_empty()),
                Some(routine.language.clone()).filter(|language| !language.is_empty()),
            ]
        })
        .collect();
    let mut table = table_of_names(
        &["name", "kind", "arguments", "result_type", "language"],
        rows,
    );
    table.label = "routines";
    table
}

/// The reply for `describe_routine`: the signatures, then each body whole
/// under a line naming its overload. A body is not a table cell, because a
/// cell is clipped at [`result::CELL_CAP`] and a body is what was asked for;
/// the whole text is held to [`result::RESULT_BUDGET`] instead.
///
/// Also how many routines went back whole: a signature dropped for the budget,
/// or a definition the clip cut, is one that did not.
fn definitions_text(routines: &[&Routine]) -> (String, usize) {
    let (mut text, signed) = result::text_shown(&[signatures(routines)], 0, None);
    let mut ends = Vec::with_capacity(routines.len());
    for routine in routines {
        text.push_str(&format!(
            "definition of {}({}):\n{}",
            routine.name, routine.identity_arguments, routine.definition
        ));
        if !text.ends_with('\n') {
            text.push('\n');
        }
        ends.push(text.len());
    }
    let (text, kept) = result::clipped_kept(&text, result::RESULT_BUDGET);
    let whole = ends.iter().filter(|&&end| end <= kept).count();
    (text, whole.min(signed))
}

fn routine_kind_name(kind: RoutineKind) -> &'static str {
    match kind {
        RoutineKind::Function => "function",
        RoutineKind::Procedure => "procedure",
    }
}

/// Said for a statement that is already an EXPLAIN: wrapping it again would
/// ask the server to explain an explanation.
pub(crate) const ALREADY_EXPLAIN: &str = "the statement is already an EXPLAIN";

/// Said for an engine that has no way to ask for a plan.
fn no_plan(engine: Engine) -> String {
    format!("DBDelve does not read query plans from {}.", engine.label())
}

/// Said for `analyze` on an engine that can plan but not measure. Nothing is
/// run, and a plain plan is not given in its place.
fn no_analyzed_plan(engine: Engine) -> String {
    format!(
        "DBDelve reads no analyzed plan from {}; nothing was run.",
        engine.label()
    )
}

/// Said for an answer that held no plan node.
fn empty_plan(engine: Engine) -> String {
    format!("{} returned no plan DBDelve could read.", engine.label())
}

/// What `explain_statement` answered without sending anything: how, and what
/// the audit records of it. `verdict` and `sql` are set when the gate judged a
/// composed statement.
#[derive(Debug)]
pub(crate) struct Declined {
    pub(crate) status: Status,
    pub(crate) text: String,
    pub(crate) verdict: Option<AgentVerdict>,
    pub(crate) sql: Option<String>,
}

impl Declined {
    fn said(status: Status, text: String) -> Self {
        Declined {
            status,
            text,
            verdict: None,
            sql: None,
        }
    }
}

/// The statement an `explain_query` call sends, or the answer it gets instead.
///
/// Every check comes before anything is sent, in this order: an input that is
/// already an EXPLAIN; an engine with no plan; `analyze` on an engine with no
/// analyzed plan, which is never answered with a plain plan, and where the
/// engine composes its EXPLAIN; a statement the engine cannot explain. Then
/// [`agent_gate_on`] judges that composed text, so an ANALYZE follows the verdict
/// of the statement it wraps. Only `Run` returns the statement, and that string
/// is what is sent.
pub(crate) fn explain_statement(
    engine: Engine,
    server_version: Option<u32>,
    sql: &str,
    analyze: bool,
    mode: Mode,
) -> Result<String, Declined> {
    if crate::sql::typed_explain(engine, sql).is_some() {
        return Err(Declined::said(
            Status::RefusedByGate,
            ALREADY_EXPLAIN.to_string(),
        ));
    }
    if engine.explain_prefix(ExplainMode::Plan).is_none() {
        return Err(Declined::said(Status::Executed, no_plan(engine)));
    }
    let explaining = if analyze {
        ExplainMode::Analyze
    } else {
        ExplainMode::Plan
    };
    let Some(composed) = engine.explain(explaining, sql) else {
        return Err(Declined::said(Status::Executed, no_analyzed_plan(engine)));
    };
    if let Err(message) = crate::sql::explainable(engine, sql) {
        return Err(Declined::said(Status::RefusedByGate, message));
    }
    let verdict = agent_gate_on(engine, server_version, &composed, mode);
    // What an approval would show is not for the agent.
    let text = match &verdict {
        AgentVerdict::Run => return Ok(composed),
        AgentVerdict::Approve(..) => NOT_EXPLAINED.to_string(),
        AgentVerdict::Refuse(reason) => reason.to_string(),
    };
    Err(Declined {
        status: Status::RefusedByGate,
        text,
        verdict: Some(verdict),
        sql: Some(composed),
    })
}

/// The reply for a plan that came back: the compact text, findings before the
/// tree, or a sentence when no node could be read.
fn plan_reply(engine: Engine, result: &QueryResult) -> (Status, String) {
    let columns: Vec<String> = result
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();
    if crate::explain::parse(&columns, &result.rows)
        .nodes
        .is_empty()
    {
        return (Status::Executed, empty_plan(engine));
    }
    (
        Status::Executed,
        compact::agent_plan(
            &columns,
            &result.rows,
            engine,
            compact::NODE_CAP,
            result::RESULT_BUDGET,
        ),
    )
}

fn table_of_names(heads: &[&str], rows: Vec<Vec<Option<String>>>) -> result::Table {
    result::Table {
        label: "columns",
        heads: heads
            .iter()
            .map(|name| result::Head {
                name: name.to_string(),
                data_type: None,
            })
            .collect(),
        total: Total::Exact(rows.len()),
        rows,
    }
}

/// The reply for `list_schemas`: one row per schema, in name order. The names
/// are the database's own and go out as they are; `note` is DBDelve's.
///
/// Also how many schemas it wrote, the budget having dropped the rest.
fn schemas_text(catalog: &Catalog, note: Option<&str>) -> (String, usize) {
    let rows = catalog
        .by_name()
        .into_iter()
        .map(|(_, schema)| vec![Some(schema.name.clone())])
        .collect();
    result::text_shown(&[table_of_names(&["name"], rows)], 0, note)
}

/// The reply for `list_tables`: one row per relation of `schema`, with its
/// kind, the relation it is a partition of and the row estimate, `null` where
/// there is none. `None` is a schema the catalog does not hold. Also how many
/// relations it wrote, the budget having dropped the rest.
fn tables_text(catalog: &Catalog, schema: &str, note: Option<&str>) -> Option<(String, usize)> {
    let schema = catalog.schemas.iter().find(|held| held.name == schema)?;
    let rows = schema
        .relations
        .iter()
        .map(|relation| {
            vec![
                Some(relation.name.clone()),
                Some(kind_name(relation.kind).to_string()),
                relation.partition_of.clone(),
                relation.rows.map(|rows| rows.to_string()),
                relation.size.map(|bytes| bytes.to_string()),
            ]
        })
        .collect();
    let heads = ["name", "kind", "partition_of", "rows", "bytes"];
    Some(result::text_shown(&[table_of_names(&heads, rows)], 0, note))
}

/// The reply for `describe_table`: four tables, in the order the agent reads a
/// table's shape. An index's key entries are one JSON array in one cell,
/// because an entry can itself hold a comma. The names are the database's own.
/// Also how many column rows it wrote, the budget having dropped the rest.
fn structure_text(structure: &Structure, note: Option<&str>) -> (String, usize) {
    let labelled = |label: &'static str, heads: &[&str], rows: Vec<Vec<Option<String>>>| {
        let mut table = table_of_names(heads, rows);
        table.label = label;
        table
    };
    let columns = structure
        .columns
        .iter()
        .map(|column| {
            vec![
                Some(column.name.clone()),
                Some(column.data_type.clone()),
                Some(column.nullable.to_string()),
                column.default.clone(),
            ]
        })
        .collect();
    let primary_key = structure
        .primary_key()
        .into_iter()
        .map(|name| vec![Some(name)])
        .collect();
    let foreign_keys = structure
        .foreign_keys
        .iter()
        .map(|key| {
            vec![
                Some(key.column.clone()),
                Some(key.referenced_schema.clone()),
                Some(key.referenced_table.clone()),
                Some(key.referenced_column.clone()),
            ]
        })
        .collect();
    let indexes = structure
        .indexes
        .iter()
        .map(|index| {
            vec![
                Some(index.name.clone()),
                Some(index_kind_name(index.kind).to_string()),
                // A list of strings always serializes; an empty list is what
                // the impossible case would say.
                Some(serde_json::to_string(&index.columns).unwrap_or_else(|_| "[]".to_string())),
                index.predicate.clone(),
            ]
        })
        .collect();
    let (text, kept) = result::text_kept(
        &[
            labelled("columns", &["name", "type", "nullable", "default"], columns),
            labelled("primary_key", &["column"], primary_key),
            labelled(
                "foreign_keys",
                &[
                    "column",
                    "references_schema",
                    "references_table",
                    "references_column",
                ],
                foreign_keys,
            ),
            labelled(
                "indexes",
                &["name", "kind", "columns", "predicate"],
                indexes,
            ),
        ],
        0,
        note,
    );
    (text, kept[0])
}

fn index_kind_name(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::Primary => "primary",
        IndexKind::Unique => "unique",
        IndexKind::Plain => "plain",
    }
}

fn kind_name(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "table",
        RelationKind::PartitionedTable => "partitioned table",
        RelationKind::View => "view",
        RelationKind::MaterializedView => "materialized view",
        RelationKind::ForeignTable => "foreign table",
        RelationKind::Dictionary => "dictionary",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnDefinition, ForeignKey, Index, NamedDefinition};

    #[test]
    fn an_approved_read_is_not_audited_as_a_plain_read() {
        let sql = || Some("SELECT 1".to_string());
        let plain = facts_of(sql(), None, Instant::now(), Ended::default());
        assert_eq!(plain.verdict, Some(AuditVerdict::Run));
        assert_eq!(plain.decision, None);

        let approved = Approved {
            verdict: AuditVerdict::Approve {
                mode: Mode::ReadOnly,
                expiry: None,
            },
            engine: Engine::DuckDb,
            server_version: None,
        };
        let facts = facts_of(sql(), Some(approved), Instant::now(), Ended::default());
        assert_eq!(
            facts.verdict,
            Some(AuditVerdict::Approve {
                mode: Mode::ReadOnly,
                expiry: None
            })
        );
        assert_eq!(facts.decision, Some(Decision::Approved));
        assert_eq!(facts.engine, Some(Engine::DuckDb));
    }

    fn relation(
        name: &str,
        kind: RelationKind,
        parent: Option<&str>,
        rows: Option<u64>,
    ) -> crate::db::Relation {
        crate::db::Relation {
            name: name.to_string(),
            kind,
            partition_of: parent.map(str::to_string),
            size: None,
            rows,
        }
    }

    fn schema(name: &str, relations: Vec<crate::db::Relation>) -> crate::db::Schema {
        crate::db::Schema {
            name: name.to_string(),
            relations,
            routines: Vec::new(),
            sequences: Vec::new(),
        }
    }

    #[test]
    fn a_statement_is_sent_only_as_the_composed_explain_the_gate_ran() {
        let select = "SELECT * FROM t";
        let composed = explain_statement(Engine::Postgres, None, select, false, Mode::ReadOnly)
            .expect("a plain plan of a read runs");
        assert_eq!(composed, "EXPLAIN SELECT * FROM t");
        let composed = explain_statement(Engine::Postgres, None, select, true, Mode::ReadOnly)
            .expect("an analyzed plan of a read runs");
        assert_eq!(composed, "EXPLAIN ANALYZE SELECT * FROM t");

        // An ANALYZE of a write is never sent, and says what the gate judged.
        for mode in Mode::ALL {
            let held = explain_statement(Engine::Postgres, None, "UPDATE t SET a = 1", true, mode)
                .expect_err("an analyzed update is not run");
            assert_eq!(held.status, Status::RefusedByGate);
            assert_eq!(
                held.sql.as_deref(),
                Some("EXPLAIN ANALYZE UPDATE t SET a = 1")
            );
            assert!(held.verdict.is_some());
            assert_ne!(held.text, "");
            let removed = explain_statement(Engine::Postgres, None, "DELETE FROM t", true, mode)
                .expect_err("an analyzed delete is not run");
            assert_eq!(removed.status, Status::RefusedByGate);
        }
        let held = explain_statement(
            Engine::Postgres,
            None,
            "UPDATE t SET a = 1",
            true,
            Mode::Full,
        )
        .expect_err("a person approves it first");
        assert_eq!(held.text, NOT_EXPLAINED);
        assert_eq!(
            NOT_EXPLAINED,
            "a statement that needs the user's approval is not run by explain_query"
        );
    }

    #[test]
    fn explain_analyze_follows_the_wrapped_statement() {
        let mode = Mode::ReadWrite;
        let composed = explain_statement(Engine::Postgres, None, "SELECT a FROM t", true, mode)
            .expect("an analyzed read runs");
        assert_eq!(composed, "EXPLAIN ANALYZE SELECT a FROM t");

        let held = explain_statement(
            Engine::Postgres,
            None,
            "UPDATE t SET a = 1 WHERE id = 1",
            true,
            mode,
        )
        .expect_err("an analyzed update waits for a person");
        assert_eq!(held.status, Status::RefusedByGate);
        assert_eq!(held.text, NOT_EXPLAINED);

        let removed = explain_statement(
            Engine::Postgres,
            None,
            "DELETE FROM t WHERE id = 1",
            true,
            mode,
        )
        .expect_err("an analyzed delete is refused");
        assert_eq!(removed.status, Status::RefusedByGate);
        assert_eq!(
            removed.text,
            "deleting data is refused for agent statements"
        );

        // A leading keyword says nothing: this reads as a SELECT and deletes.
        let writable = "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING id) SELECT id FROM d";
        for mode in Mode::ALL {
            let refused = explain_statement(Engine::Postgres, None, writable, true, mode)
                .expect_err("an analyzed writable CTE is never run");
            assert_eq!(refused.status, Status::RefusedByGate);
        }

        let refused = explain_statement(Engine::MongoDb, None, "db.c.deleteMany({})", true, mode)
            .expect_err("a delete is not explained");
        assert_eq!(refused.status, Status::RefusedByGate);
        assert_eq!(
            Err(refused.text),
            crate::sql::explainable(Engine::MongoDb, "db.c.deleteMany({})")
        );
    }

    #[test]
    fn explain_defaults_to_the_plain_plan_and_never_downgrades_analyze() {
        for engine in Engine::ALL {
            let plain = explain_statement(engine, None, "SELECT 1", false, Mode::ReadOnly);
            match engine.explain_prefix(ExplainMode::Plan) {
                Some(_) => {
                    // Mongo and SQL alike: whatever is sent is what the engine
                    // composes for a plan, and only the gate may stop it.
                    if let Ok(composed) = &plain {
                        assert_eq!(
                            Some(composed.clone()),
                            engine.explain(ExplainMode::Plan, "SELECT 1")
                        );
                    }
                }
                None => assert!(plain.is_err(), "{engine:?} has no plan to ask for"),
            }

            let analyzed = explain_statement(engine, None, "SELECT 1", true, Mode::ReadOnly);
            if matches!(engine, Engine::Postgres | Engine::MySql | Engine::MariaDb) {
                assert!(plain.is_ok() && analyzed.is_ok(), "{engine:?} runs a read");
            }
            if let Ok(composed) = &analyzed {
                assert_eq!(
                    Some(composed.clone()),
                    engine.explain(ExplainMode::Analyze, "SELECT 1"),
                    "{engine:?} answered analyze with another plan"
                );
            }
            if engine.explain_prefix(ExplainMode::Plan).is_some()
                && engine.explain_prefix(ExplainMode::Analyze).is_none()
            {
                let said = analyzed.expect_err("no analyzed plan is run");
                assert_eq!(said.status, Status::Executed);
                assert_eq!(
                    said.text,
                    format!(
                        "DBDelve reads no analyzed plan from {}; nothing was run.",
                        engine.label()
                    )
                );
                assert!(said.sql.is_none());
            }
        }
        // Not left to the loop above: it must have found these two.
        for engine in [Engine::Sqlite, Engine::ClickHouse] {
            let said = explain_statement(engine, None, "SELECT 1", true, Mode::Full)
                .expect_err("neither measures a plan");
            assert_eq!(said.status, Status::Executed);
            assert_eq!(
                said.text,
                format!(
                    "DBDelve reads no analyzed plan from {}; nothing was run.",
                    engine.label()
                )
            );
        }
    }

    #[test]
    fn an_engine_without_a_plan_says_so_in_a_sentence() {
        for engine in [
            Engine::Turso,
            Engine::DuckDb,
            Engine::Snowflake,
            Engine::SqlServer,
            Engine::Redis,
            Engine::Generic,
        ] {
            for analyze in [false, true] {
                let said = explain_statement(engine, None, "SELECT 1", analyze, Mode::Full)
                    .expect_err("there is no plan to ask for");
                assert_eq!(said.status, Status::Executed, "{engine:?}");
                assert_eq!(
                    said.text,
                    format!("DBDelve does not read query plans from {}.", engine.label())
                );
                assert!(said.sql.is_none());
            }
        }
    }

    #[test]
    fn an_explain_typed_by_the_agent_is_refused() {
        for (engine, sql) in [
            (Engine::Postgres, "EXPLAIN SELECT 1"),
            (Engine::MongoDb, r#"db.c.find({}).explain("queryPlanner")"#),
        ] {
            for analyze in [false, true] {
                let refused = explain_statement(engine, None, sql, analyze, Mode::Full)
                    .expect_err("an explanation is not explained again");
                assert_eq!(refused.status, Status::RefusedByGate, "{engine:?}");
                assert_eq!(refused.text, "the statement is already an EXPLAIN");
            }
        }
    }

    #[test]
    fn a_plan_that_parses_to_nothing_says_so() {
        for engine in [Engine::Postgres, Engine::MySql, Engine::Sqlite] {
            assert_eq!(
                plan_reply(engine, &QueryResult::default()),
                (
                    Status::Executed,
                    format!("{} returned no plan DBDelve could read.", engine.label())
                )
            );
        }
    }

    /// The compose Postgres, from the standard `PG*` variables, opened the way
    /// the agent opens it.
    fn live_agent_connection() -> Connection {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
        let config = ConnectionConfig::postgres(crate::db::ServerConfig {
            host: var("PGHOST"),
            port: std::env::var("PGPORT")
                .ok()
                .map(|port| port.parse().expect("PGPORT must be a number")),
            database: var("PGDATABASE"),
            user: var("PGUSER"),
            password: std::env::var("PGPASSWORD").unwrap_or_default(),
            // The compose database speaks no TLS, and this test is the one
            // place a plaintext connection is the point.
            sslmode: crate::db::SslMode::Disable,
            root_certificate: None,
            statement_timeout: 0,
            ssh: None,
            ssm: None,
            password_command: None,
            schema: None,
            read_only: false,
        });
        Connection::open_for_agent(config).expect("the compose Postgres opens for the agent")
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_an_explain_analyze_on_postgres_reads_a_measured_plan() {
        let connection = live_agent_connection();
        let statement = explain_statement(
            Engine::Postgres,
            None,
            "SELECT * FROM accounts",
            true,
            Mode::ReadOnly,
        )
        .expect("an analyzed read runs");
        let Ran::Done { result, .. } = explain_on(
            connection,
            Vec::new(),
            &statement,
            &CancelToken::default(),
            Engine::Postgres,
        ) else {
            panic!("the plan was not drained");
        };
        let text = result.expect("the server plans it").text;
        assert!(text.contains("analyzed=yes"), "{text}");
        assert!(text.contains("findings"), "{text}");
    }

    #[test]
    fn tables_are_listed_with_their_kind_parent_estimate_and_size() {
        let catalog = Catalog {
            schemas: vec![schema(
                "public",
                vec![
                    relation("orders", RelationKind::Table, None, Some(42)),
                    relation("orders_2024", RelationKind::Table, Some("orders"), None),
                    relation("v", RelationKind::View, None, None),
                ],
            )],
            database: None,
        };
        let (text, _) = tables_text(&catalog, "public", None).expect("the schema is there");
        assert_eq!(
            text,
            "truncated: none (3 rows)\n\
             columns: [{\"name\":\"name\",\"type\":null},{\"name\":\"kind\",\"type\":null},\
             {\"name\":\"partition_of\",\"type\":null},{\"name\":\"rows\",\"type\":null},\
             {\"name\":\"bytes\",\"type\":null}]\n\
             [\"orders\",\"table\",null,\"42\",null]\n\
             [\"orders_2024\",\"table\",\"orders\",null,null]\n\
             [\"v\",\"view\",null,null,null]\n"
        );
        assert_eq!(tables_text(&catalog, "Public", None), None);
    }

    fn column(
        name: &str,
        data_type: &str,
        nullable: bool,
        default: Option<&str>,
    ) -> ColumnDefinition {
        ColumnDefinition {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable,
            default: default.map(str::to_string),
        }
    }

    /// The lines of a reply that start with `label: ` and the row lines after
    /// each, up to the next label.
    fn section<'a>(text: &'a str, label: &str) -> Vec<&'a str> {
        let labels = ["columns: ", "primary_key: ", "foreign_keys: ", "indexes: "];
        let start = format!("{label}: ");
        let mut lines = text.lines().skip_while(|line| !line.starts_with(&start));
        let mut found = vec![lines.next().expect("the label is there")];
        found.extend(lines.take_while(|line| !labels.iter().any(|l| line.starts_with(l))));
        found
    }

    #[test]
    fn a_structure_is_described_in_four_labelled_tables() {
        let structure = Structure {
            columns: vec![
                column("id", "integer", false, None),
                column("email", "text", true, Some("'x'")),
            ],
            indexes: vec![
                Index {
                    name: "by_email".to_string(),
                    kind: IndexKind::Unique,
                    columns: vec!["lower(email)".to_string(), "id".to_string()],
                    predicate: None,
                    definition: String::new(),
                },
                Index {
                    name: "live".to_string(),
                    kind: IndexKind::Plain,
                    columns: vec!["a, b".to_string()],
                    predicate: Some("a > 0".to_string()),
                    definition: String::new(),
                },
            ],
            constraints: vec![NamedDefinition {
                name: "t_pkey".to_string(),
                definition: "PRIMARY KEY (id)".to_string(),
            }],
            foreign_keys: vec![ForeignKey {
                column: "team".to_string(),
                referenced_schema: "main".to_string(),
                referenced_table: "teams".to_string(),
                referenced_column: "id".to_string(),
            }],
            referenced_by: Vec::new(),
        };
        let (text, _) = structure_text(&structure, None);
        let labels: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split_once(": ").map(|(label, _)| label))
            .filter(|label| ["columns", "primary_key", "foreign_keys", "indexes"].contains(label))
            .collect();
        assert_eq!(
            labels,
            ["columns", "primary_key", "foreign_keys", "indexes"]
        );
        assert_eq!(
            section(&text, "columns")[1..],
            [
                "[\"id\",\"integer\",\"false\",null]",
                "[\"email\",\"text\",\"true\",\"'x'\"]"
            ]
        );
        assert_eq!(section(&text, "primary_key")[1..], ["[\"id\"]"]);
        assert_eq!(
            section(&text, "foreign_keys")[1..],
            ["[\"team\",\"main\",\"teams\",\"id\"]"]
        );
        assert_eq!(
            section(&text, "indexes")[1..],
            [
                "[\"by_email\",\"unique\",\"[\\\"lower(email)\\\",\\\"id\\\"]\",null]",
                "[\"live\",\"plain\",\"[\\\"a, b\\\"]\",\"a > 0\"]"
            ]
        );
    }

    #[test]
    fn a_table_with_no_keys_or_indexes_is_described_with_empty_tables() {
        let structure = Structure {
            columns: vec![
                column("a", "text", true, None),
                column("b", "text", true, None),
            ],
            ..Structure::default()
        };
        let (text, _) = structure_text(&structure, None);
        assert_eq!(section(&text, "columns").len(), 3, "{text}");
        for label in ["primary_key", "foreign_keys", "indexes"] {
            assert_eq!(section(&text, label).len(), 1, "{label}: {text}");
        }
    }

    #[test]
    fn a_pair_the_catalog_does_not_hold_never_reaches_the_loader() {
        let catalog = Catalog {
            schemas: vec![schema(
                "main",
                vec![relation("t", RelationKind::Table, None, None)],
            )],
            database: None,
        };
        let refuse = |_: &str, _: &str| -> Result<Structure, DbError> {
            panic!("the loader was handed a name the catalog does not hold")
        };
        for (schema, table) in [
            ("main", "nope"),
            ("other", "t"),
            ("Main", "t"),
            ("main", "T"),
        ] {
            assert!(
                matches!(describe(&catalog, schema, table, None, refuse), Ok(None)),
                "{schema}.{table}"
            );
        }
        let mut asked = Vec::new();
        let held = describe(&catalog, "main", "t", None, |schema, table| {
            asked.push((schema.to_string(), table.to_string()));
            Ok(Structure::default())
        });
        assert!(matches!(held, Ok(Some(_))));
        assert_eq!(asked, [("main".to_string(), "t".to_string())]);
    }

    #[test]
    fn a_schema_with_no_relations_lists_none() {
        let catalog = Catalog {
            schemas: vec![schema("empty", Vec::new())],
            database: None,
        };
        let (text, _) = tables_text(&catalog, "empty", None).expect("the schema is there");
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("truncated: none (0 rows)"));
        assert!(
            lines
                .next()
                .is_some_and(|line| line.starts_with("columns: "))
        );
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn a_redis_catalog_reply_carries_the_preview_note() {
        let catalog = Catalog {
            schemas: vec![schema(
                "db0",
                vec![relation("keys", RelationKind::Table, None, Some(7))],
            )],
            database: None,
        };
        let note = Engine::Redis.agent_catalog_note();
        let note_line = format!("note: {}\n", note.expect("Redis has a note"));
        let (schemas, _) = schemas_text(&catalog, note);
        assert!(
            schemas.starts_with(&format!("truncated: none (1 row)\n{note_line}columns: ")),
            "{schemas}"
        );
        let (tables, _) = tables_text(&catalog, "db0", note).expect("the schema is there");
        assert!(
            tables.starts_with(&format!("truncated: none (1 row)\n{note_line}columns: ")),
            "{tables}"
        );
        assert!(
            tables.ends_with("[\"keys\",\"table\",null,\"7\",null]\n"),
            "{tables}"
        );
        // Another engine's reply has no such line.
        assert!(
            !schemas_text(&catalog, Engine::Sqlite.agent_catalog_note())
                .0
                .contains("note: ")
        );
    }

    #[test]
    fn schemas_are_listed_in_name_order() {
        let catalog = Catalog {
            schemas: vec![schema("b", Vec::new()), schema("a", Vec::new())],
            database: None,
        };
        assert_eq!(
            schemas_text(&catalog, None).0,
            "truncated: none (2 rows)\n\
             columns: [{\"name\":\"name\",\"type\":null}]\n\
             [\"a\"]\n\
             [\"b\"]\n"
        );
    }

    #[test]
    fn every_kind_of_relation_has_its_own_name() {
        let kinds = [
            RelationKind::Table,
            RelationKind::PartitionedTable,
            RelationKind::View,
            RelationKind::MaterializedView,
            RelationKind::ForeignTable,
            RelationKind::Dictionary,
        ];
        let names: std::collections::HashSet<_> = kinds.into_iter().map(kind_name).collect();
        assert_eq!(names.len(), kinds.len());
    }

    #[test]
    fn the_deadline_text_names_the_deadline() {
        assert!(DEADLINE_PASSED.contains(&format!("{} seconds", CALL_DEADLINE.as_secs())));
    }

    #[test]
    fn the_agent_timeout_is_the_users_capped_or_the_agents_own() {
        assert_eq!(agent_timeout(0), AGENT_TIMEOUT);
        assert_eq!(agent_timeout(5), 5);
        assert_eq!(agent_timeout(AGENT_TIMEOUT), AGENT_TIMEOUT);
        assert_eq!(agent_timeout(AGENT_TIMEOUT + 1), AGENT_TIMEOUT);
        assert_eq!(agent_timeout(u32::MAX), AGENT_TIMEOUT);
    }

    #[test]
    fn the_agent_config_carries_the_capped_timeout() {
        for config in crate::db::every_shape() {
            for user in [0, 5, AGENT_TIMEOUT, AGENT_TIMEOUT + 1] {
                let profile = config.clone().with_statement_timeout(user);
                let agent = agent_config(&profile);
                let engine = profile.engine();
                assert_eq!(
                    agent.statement_timeout(),
                    agent_timeout(user),
                    "{engine:?} at {user}"
                );
                // Only the timeout differs from what the profile holds.
                assert_eq!(agent.with_statement_timeout(user), profile, "{engine:?}");
            }
        }
    }

    /// Whether `text` holds `name` bounded by anything but a name character,
    /// in any ASCII case.
    fn stands_whole(text: &str, name: &str) -> bool {
        let (text, name) = (text.to_ascii_lowercase(), name.to_ascii_lowercase());
        let in_a_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
        text.match_indices(&name).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + name.len()..].chars().next();
            let first = name.chars().next().is_some_and(in_a_name);
            let last = name.chars().next_back().is_some_and(in_a_name);
            !(first && before.is_some_and(in_a_name)) && !(last && after.is_some_and(in_a_name))
        })
    }

    #[test]
    fn an_error_naming_the_profile_is_scrubbed_before_the_agent_reads_it() {
        let configs = restored(include_str!("../testdata/every-engine.toml"));
        assert!(!configs.is_empty());
        for (id, config) in &configs {
            let hidden = config.identifying_text();
            for name in &hidden {
                for message in [
                    format!("could not reach {name}."),
                    format!("user '{name}' denied"),
                    format!("\"{name}\""),
                    format!("{name}@host"),
                    name.to_ascii_uppercase(),
                ] {
                    let clean = scrubbed(&message, &hidden);
                    assert!(
                        !stands_whole(&clean, name),
                        "{id}: {name:?} survives in {clean:?} from {message:?}"
                    );
                }
            }
        }
        assert_eq!(
            scrubbed(
                "SELECT command denied to user 'app'@'10.0.0.5' for table 't'",
                &[]
            ),
            "SELECT command denied to user [hidden] for table 't'"
        );
        let long = "x".repeat(ERROR_CAP + 500);
        let clipped = scrubbed(&long, &[]);
        assert_eq!(clipped.chars().count(), ERROR_CAP + CLIPPED.chars().count());
        assert!(clipped.ends_with(" [clipped]"));
        let wide = "é".repeat(ERROR_CAP + 500);
        let clipped = scrubbed(&wide, &["é".to_string()]);
        assert!(clipped.chars().count() <= ERROR_CAP + CLIPPED.chars().count());
        let exact = "y".repeat(ERROR_CAP);
        assert_eq!(scrubbed(&exact, &[]), exact);
        let multibyte = "é".repeat(ERROR_CAP + 500);
        let clipped = scrubbed(&multibyte, &[]);
        assert!(clipped.starts_with("éé") && clipped.ends_with(" [clipped]"));
    }

    #[test]
    fn a_password_commands_open_error_never_reaches_the_audit() {
        let audited = audited_open_error(
            "The password command failed: + echo secret-token",
            &["db.internal".to_string()],
        );
        assert!(!audited.contains("secret-token"), "{audited}");
        // A server's error is still kept, scrubbed.
        assert_eq!(
            audited_open_error("no route to db.internal", &["db.internal".to_string()]),
            "no route to [hidden]"
        );
    }

    #[test]
    fn a_plan_counts_no_rows_as_sent() {
        let result = QueryResult {
            columns: vec![db::Column {
                name: "QUERY PLAN".to_string(),
                data_type: None,
            }],
            rows: db::rows(vec![vec![Some("Seq Scan on t".to_string())]; 3]),
            ..QueryResult::default()
        };
        assert_eq!(shaped_plan(Engine::Postgres, &result).rows, None);
    }

    #[test]
    fn an_open_error_is_scrubbed_of_the_keychains_password() {
        let error = DbError {
            message: "password authentication failed: got s3cr3t-from-keychain".to_string(),
            position: None,
        };
        let opened = open_error(error, &["s3cr3t-from-keychain".to_string()]);
        assert!(!opened.message.contains("s3cr3t"), "{}", opened.message);
        assert!(opened.message.contains("[hidden]"), "{}", opened.message);
    }

    #[test]
    fn a_name_is_scrubbed_only_where_it_stands_whole() {
        let hidden: Vec<String> = ["sa", "db.internal", "db.internal:5432"]
            .map(String::from)
            .into();
        for (message, scrubbed_to) in [
            ("usage of sa_role by sa.", "usage of sa_role by [hidden]."),
            ("cannot reach db.internal.", "cannot reach [hidden]."),
            ("at db.internal:5432,", "at [hidden],"),
            ("subdb.internal", "subdb.internal"),
            (
                "SA 'sa' \"sa\" sa@host",
                "[hidden] '[hidden]' \"[hidden]\" [hidden]@host",
            ),
        ] {
            assert_eq!(scrubbed(message, &hidden), scrubbed_to, "{message}");
        }
    }

    #[test]
    fn a_quote_that_closes_something_else_is_not_an_account() {
        assert_eq!(
            scrubbed("user 'x' near 'app'@'host' denied", &[]),
            "user 'x' near [hidden] denied"
        );
        assert_eq!(scrubbed("it's 'a' and b@c", &[]), "it's 'a' and b@c");
    }

    #[test]
    fn a_spatial_cell_reaches_the_agent_rendered_not_as_hex() {
        use crate::db::{Column, Engine, Row};
        let point = "0101000000000000000000F03F0000000000000040";
        let result = QueryResult {
            columns: vec![
                Column {
                    name: "geom".into(),
                    data_type: Some("geometry".into()),
                },
                Column {
                    name: "note".into(),
                    data_type: Some("text".into()),
                },
            ],
            rows: vec![Row::new([Some(point), Some(point)]).expect("a row")],
            ..QueryResult::default()
        };
        let table = table_of(Engine::Postgres, &result);
        let geometry = table.rows[0][0].as_deref().expect("a cell");
        assert!(geometry.starts_with("POINT"), "{geometry}");
        assert_eq!(table.rows[0][1].as_deref(), Some(point));
        // Another engine's cells are not read as EWKB.
        let other = table_of(Engine::MySql, &result);
        assert_eq!(other.rows[0][0].as_deref(), Some(point));
    }

    #[test]
    fn the_reply_is_written_where_the_statement_ran_with_a_wide_cell_clipped() {
        let dir = std::env::temp_dir().join(format!("dbdelve-agent-shaped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the directory is made");
        let path = dir.join("shaped.db");
        let _ = std::fs::remove_file(&path);
        rusqlite::Connection::open(&path).expect("the file is made");
        let connection = Connection::open_for_agent(ConnectionConfig::Sqlite {
            path: path.to_string_lossy().into_owned(),
            statement_timeout: 0,
        })
        .expect("the agent opens the file");
        let ran = read_on(
            connection,
            Vec::new(),
            "SELECT replace(hex(zeroblob(3000)), '0', 'x') AS wide",
            &CancelToken::default(),
            Engine::Sqlite,
        );
        let Ran::Done {
            result, connection, ..
        } = ran
        else {
            panic!("the statement ran");
        };
        let shaped = result.expect("the statement answered");
        // The text the window sends is already final: the header says a cell
        // was clipped and from how long.
        assert!(
            shaped
                .text
                .starts_with("truncated: 1 of 1 rows shown (clipped cells)"),
            "{}",
            &shaped.text[..80]
        );
        assert!(shaped.text.contains("[clipped from 6000 characters]"));
        assert_eq!(shaped.rows, Some((1, Total::Exact(1))));
        // Let go of the file first: Windows will not remove one held open.
        drop(connection);
        std::fs::remove_dir_all(&dir).expect("the directory goes");
    }

    /// A SQLite file holding `sql`, opened the way the agent opens it, and the
    /// directory to remove after.
    fn agent_sqlite(name: &str, sql: &str) -> (Connection, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("dbdelve-agent-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the directory is made");
        let path = dir.join("catalog.db");
        let _ = std::fs::remove_file(&path);
        rusqlite::Connection::open(&path)
            .expect("the file is made")
            .execute_batch(sql)
            .expect("the schema is made");
        let connection = Connection::open_for_agent(ConnectionConfig::Sqlite {
            path: path.to_string_lossy().into_owned(),
            statement_timeout: 0,
        })
        .expect("the agent opens the file");
        (connection, dir)
    }

    const SHOP: &str = "CREATE TABLE orders (id INTEGER PRIMARY KEY, placed TEXT);
        CREATE TABLE order_items (
            id INTEGER PRIMARY KEY,
            order_id INTEGER REFERENCES orders (id),
            sku TEXT
        );
        CREATE TABLE customers (id INTEGER PRIMARY KEY);
        CREATE VIEW recent_orders AS SELECT * FROM orders;";

    #[test]
    fn a_search_finds_names_across_the_catalog_best_first() {
        let (connection, dir) = agent_sqlite("search", SHOP);
        let listing = search(&connection, "ordr", None)
            .expect("the catalog is read")
            .expect("a search always answers");
        assert_eq!(listing.rows, (3, Total::Exact(3)), "{}", listing.text);
        let text = &listing.text;
        assert!(text.starts_with("truncated: none (3 rows)"), "{text}");
        assert!(text.contains(r#"["main","orders","table"]"#), "{text}");
        assert!(
            text.contains(r#"["main","recent_orders","view"]"#),
            "{text}"
        );
        assert!(!text.contains("customers"), "{text}");
        // The shortest of equal matches is first.
        assert!(text.find("\"orders\"") < text.find("order_items"), "{text}");
        // A qualified name finds the relation; nothing found is zero rows.
        let qualified = search(&connection, "main.customers", None)
            .unwrap()
            .unwrap();
        assert!(qualified.text.contains(r#"["main","customers","table"]"#));
        assert_eq!(
            search(&connection, "zzz", None).unwrap().unwrap().rows,
            (0, Total::Exact(0))
        );
        drop(connection);
        std::fs::remove_dir_all(&dir).expect("the directory goes");
    }

    #[test]
    fn a_search_counts_only_the_matches_it_sent() {
        let tables: String = (0..AGENT_ROWS + 5)
            .map(|at| format!("CREATE TABLE orders_{at} (id INTEGER);"))
            .collect();
        let (connection, dir) = agent_sqlite("search-capped", &tables);
        let listing = search(&connection, "orders", None)
            .expect("the catalog is read")
            .expect("a search always answers");
        assert_eq!(
            listing.rows,
            (AGENT_ROWS, Total::Exact(AGENT_ROWS + 5)),
            "{}",
            listing.text
        );
        drop(connection);
        std::fs::remove_dir_all(&dir).expect("the directory goes");
    }

    #[test]
    fn a_schemas_foreign_keys_are_listed_with_both_ends() {
        let (connection, dir) = agent_sqlite("relationships", SHOP);
        let listing = relationships(&connection, "main", Engine::Sqlite)
            .expect("the graph is read")
            .expect("the schema is there");
        assert_eq!(listing.rows, (1, Total::Exact(1)), "{}", listing.text);
        // SQLite names a key by its position in the table's key list.
        assert_eq!(
            listing.text,
            "truncated: none (1 row)\n\
             foreign_keys: [{\"name\":\"table\",\"type\":null},{\"name\":\"constraint\",\"type\":null},\
             {\"name\":\"columns\",\"type\":null},{\"name\":\"references_schema\",\"type\":null},\
             {\"name\":\"references_table\",\"type\":null},{\"name\":\"references_columns\",\"type\":null}]\n\
             [\"order_items\",\"0\",\"[\\\"order_id\\\"]\",\"main\",\"orders\",\"[\\\"id\\\"]\"]\n"
        );
        assert!(listing.text.contains("foreign_keys:"), "{}", listing.text);
        assert!(
            relationships(&connection, "nope", Engine::Sqlite)
                .unwrap()
                .is_none()
        );
        drop(connection);
        std::fs::remove_dir_all(&dir).expect("the directory goes");
    }

    #[test]
    fn a_schema_without_routines_lists_none_and_an_unknown_one_is_not_there() {
        let (connection, dir) = agent_sqlite("routines", SHOP);
        let listing = routines(&connection, "main", None)
            .expect("the routines are read")
            .expect("the schema is there");
        assert_eq!(listing.rows, (0, Total::Exact(0)));
        assert!(routines(&connection, "main", Some("f")).unwrap().is_none());
        assert!(routines(&connection, "nope", None).unwrap().is_none());
        drop(connection);
        std::fs::remove_dir_all(&dir).expect("the directory goes");
    }

    #[test]
    fn a_listing_cut_by_the_budget_counts_only_the_rows_it_wrote() {
        let mut catalog = Catalog::default();
        let name = "s".repeat(1000);
        catalog.schemas = (0..100)
            .map(|at| schema(&format!("{name}{at}"), Vec::new()))
            .collect();
        let (text, shown) = schemas_text(&catalog, None);
        assert!(0 < shown && shown < 100, "{shown}");
        assert_eq!(text.lines().count(), shown + 2, "{text}");

        let structure = Structure {
            columns: (0..100)
                .map(|at| ColumnDefinition {
                    name: format!("{name}{at}"),
                    data_type: "text".to_string(),
                    nullable: true,
                    default: None,
                })
                .collect(),
            ..Structure::default()
        };
        let (_, columns) = structure_text(&structure, None);
        assert!(0 < columns && columns < 100, "{columns}");
    }

    fn routine(name: &str, arguments: &str, definition: &str) -> Routine {
        Routine {
            name: name.to_string(),
            kind: RoutineKind::Function,
            identity_arguments: arguments.to_string(),
            result_type: "integer".to_string(),
            language: "sql".to_string(),
            definition: definition.to_string(),
        }
    }

    #[test]
    fn a_routines_body_is_given_whole_past_the_cell_cap_and_held_to_the_budget() {
        let long = "x".repeat(result::CELL_CAP * 2);
        let one = routine("f", "a integer", &long);
        let two = routine("f", "a text", "SELECT 1");
        let (text, whole) = definitions_text(&[&one, &two]);
        assert_eq!(whole, 2);
        assert!(text.contains(&format!("definition of f(a integer):\n{long}\n")));
        assert!(
            text.ends_with("definition of f(a text):\nSELECT 1\n"),
            "{text}"
        );
        assert!(text.contains(r#"["f","function","a text","integer","sql"]"#));

        let huge = routine("g", "", &"y".repeat(result::RESULT_BUDGET * 2));
        let (text, whole) = definitions_text(&[&huge]);
        assert_eq!(whole, 0, "the one definition was cut");
        assert!(text.len() <= result::RESULT_BUDGET, "{}", text.len());
        assert!(
            text.trim_end().ends_with("bytes]"),
            "{}",
            &text[text.len() - 60..]
        );
    }
}

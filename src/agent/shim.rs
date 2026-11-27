//! `dbdelve mcp`: the process Claude Code starts, speaking MCP on its stdio.
//!
//! This is the only file that names rmcp or tokio's sockets. It answers
//! `initialize` and `server/discover` itself, answers `tools/list` from the
//! static table in [`super::tools`] without connecting anywhere, and carries
//! each `tools/call` to the app over one connection of its own. The shim never
//! opens a database connection and answers nothing but `app_not_running` on its
//! own: every other status is the app's. A call that is cancelled is cancelled
//! by hanging up its connection, and no frame says so.
//!
//! Nothing here is remembered between calls about the app: whether it is
//! running is decided at each call, by whether its socket takes a connection.

#[cfg(unix)]
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[cfg(unix)]
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        JsonObject, ListToolsResult, PaginatedRequestParams, ProgressNotificationParam,
        ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    transport::stdio,
};
// The library's `Tool` is spelled the same as the agent's, which is the one
// this file means by `Tool`.
#[cfg(unix)]
use rmcp::model::Tool as McpTool;
#[cfg(unix)]
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::Mutex,
    time::{Instant, MissedTickBehavior, interval, interval_at},
};

#[cfg(unix)]
use super::{CallArgs, Reply, Status, tools::Tool, wire};

/// What an agent is told when it cannot reach the app. The shim cannot tell an
/// app that is closed from one with its agent access off, so it says both.
#[cfg(unix)]
const NOT_RUNNING: &str = "DBDelve is not running, or its agent access is off.";

// ponytail: Claude Code gives up on a call that stays silent for 30 seconds,
// and a person answering an approval takes longer, so a waiting call says it is
// still alive every 10: margin for a late tick, no setting.
#[cfg(unix)]
const PROGRESS_EVERY: Duration = Duration::from_secs(10);

// ponytail: while DBDelve is closed or its agent access is off, an idle shim
// makes one local connect every 2 seconds, so the window's mark lights within
// that of access being turned on. The upgrade path is a watch on the socket's
// directory instead of a poll.
#[cfg(unix)]
const PRESENCE_RETRY: Duration = Duration::from_secs(2);

/// What the shim serves.
#[cfg(unix)]
pub(crate) struct Shim {
    /// Where the app's socket is, or why that could not be worked out. Taken
    /// as it comes: a path that cannot be derived is the answer to every call,
    /// not a reason for the shim not to speak.
    socket: Result<PathBuf, String>,
    /// Where `profiles.toml` lives, read on `list_tools` to omit schema tools
    /// denied by the owner's AI Features policy.
    profiles: Option<PathBuf>,
    progress_every: Duration,
    presence_retry: Duration,
    /// Whether this shim's one presence connection is open. A lock rather than
    /// a flag because the check, the connect and marking it open are one step:
    /// the idle retry and a call must not each open one.
    presence: Arc<Mutex<bool>>,
}

#[cfg(unix)]
impl Shim {
    pub(crate) fn new(socket: Result<PathBuf, String>) -> Self {
        let profiles = crate::store::dbdelve_directory()
            .ok()
            .map(|dir| dir.join("profiles.toml"));
        Self {
            socket,
            profiles,
            progress_every: PROGRESS_EVERY,
            presence_retry: PRESENCE_RETRY,
            presence: Arc::new(Mutex::new(false)),
        }
    }

    #[cfg(test)]
    fn with_profiles_path(mut self, path: PathBuf) -> Self {
        self.profiles = Some(path);
        self
    }

    #[cfg(test)]
    fn with_progress_every(mut self, every: Duration) -> Self {
        self.progress_every = every;
        self
    }

    #[cfg(test)]
    fn with_presence_retry(mut self, retry: Duration) -> Self {
        self.presence_retry = retry;
        self
    }

    /// Open this shim's presence connection if it is not open: what tells the
    /// app that an agent is attached, though it may ask nothing. A failed
    /// connect is silent and leaves it down; a call reports what it finds for
    /// itself.
    async fn ensure_presence(path: &Path, presence: &Arc<Mutex<bool>>) {
        let mut open = presence.lock().await;
        if *open {
            return;
        }
        let Ok(mut stream) = connect_own(path).await else {
            return;
        };
        let hello = wire::encode(&wire::Hello::Presence {
            v: wire::PROTOCOL_VERSION,
        });
        if stream.write_all(hello.as_bytes()).await.is_err() {
            return;
        }
        *open = true;
        let presence = presence.clone();
        tokio::spawn(async move {
            // The app writes nothing on a presence, so any end of the read,
            // an end of stream or an error, is the app having let it go.
            let mut unread = [0u8; 64];
            while let Ok(read) = stream.read(&mut unread).await {
                if read == 0 {
                    break;
                }
            }
            *presence.lock().await = false;
        });
    }

    /// Keep the presence connection open for as long as this runtime lives:
    /// open it now, and look again every retry while it is down. The task
    /// never touches the client's connection, so it can answer nothing, and a
    /// presence that drops is never a reason for the shim to stop.
    pub(crate) fn keep_presence(&self) {
        let Ok(path) = self.socket.clone() else {
            return;
        };
        let presence = self.presence.clone();
        let retry = self.presence_retry;
        tokio::spawn(async move {
            // The first tick is at once.
            let mut ticks = interval(retry);
            ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                Shim::ensure_presence(&path, &presence).await;
            }
        });
    }

    /// Carry one call to the app and bring back its answer, telling the client
    /// the call is alive while it waits. The call ends by the app's answer, by
    /// the connection closing, or by the client cancelling it; no timer here
    /// answers it.
    async fn carry(
        &self,
        hello: String,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = match &self.socket {
            Ok(path) => path,
            Err(text) => return Ok(answer(Status::AppNotRunning, text)),
        };
        let mut stream = match connect_own(path).await {
            Ok(stream) => stream,
            Err(error) => {
                return Ok(answer(Status::AppNotRunning, &failed_connect_text(&error)));
            }
        };
        if stream.write_all(hello.as_bytes()).await.is_err() {
            return Ok(closed_without_answering());
        }
        // The token is on the context: the request's own `_meta` has already
        // been moved there by the time the handler runs.
        let token = context.meta.get_progress_token();
        let mut ticks = interval_at(Instant::now() + self.progress_every, self.progress_every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut sent = 0.0;
        // Built once: a tick must not rebuild the reader and lose what it
        // has already buffered.
        let reply = read_reply(&mut stream);
        tokio::pin!(reply);
        loop {
            tokio::select! {
                biased;
                // Leaving drops the stream, which is the app's hangup, and
                // the library drops the late answer of a cancelled request.
                _ = context.ct.cancelled() => {
                    return Err(ErrorData::internal_error("cancelled", None));
                }
                reply = &mut reply => {
                    return Ok(match reply {
                        Some(reply) => answer(reply.status, &reply.text),
                        None => closed_without_answering(),
                    });
                }
                _ = ticks.tick() => {
                    if let Some(token) = &token {
                        sent += 1.0;
                        // A client that has gone fails this, and the call is
                        // then ended by its cancel or the app's expiry.
                        let _ = context
                            .peer
                            .notify_progress(ProgressNotificationParam::new(token.clone(), sent))
                            .await;
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
impl ServerHandler for Shim {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
        // The struct cannot be built by literal from here, and its default
        // name would announce the library rather than this program.
        info.server_info = Implementation::new("dbdelve", env!("CARGO_PKG_VERSION"));
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let policy = self
            .profiles
            .as_deref()
            .map(super::policy::read_saved_policy)
            .unwrap_or_default();
        Ok(ListToolsResult::with_all_items(
            Tool::ALL
                .into_iter()
                .filter(|tool| policy.tool_listed(*tool))
                .map(|tool| McpTool::new(tool.name(), tool.description(), tool.input_schema()))
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Resolved before anything is opened: a call that is not one reaches
        // no socket, presence included.
        let (tool, connection, args) = resolve(&request)?;
        // Encoded before anything is opened, because the app drops a first
        // frame past its limit and the agent would be told the app had closed.
        let hello = wire::encode(&wire::Hello::Call {
            v: wire::PROTOCOL_VERSION,
            tool,
            connection,
            sql: args.sql,
            schema: args.schema,
            table: args.table,
            analyze: args.analyze,
            query: args.query,
            routine: args.routine,
        });
        if hello.len() as u64 > wire::FRAME_LIMIT {
            return Err(ErrorData::invalid_params(
                format!(
                    "The call is {} bytes once encoded; DBDelve reads a call of at most {}.",
                    hello.len(),
                    wire::FRAME_LIMIT
                ),
                None,
            ));
        }
        if let Ok(path) = &self.socket {
            Self::ensure_presence(path, &self.presence).await;
        }
        Ok(self.carry(hello, &context).await?.into())
    }
}

/// The tool a request names, the connection it gives and the other arguments it
/// carries, or why the request is not one. An unknown tool or an argument the tool does not take is a protocol
/// error answered here, not a tool result, and it never reaches the app.
#[cfg(unix)]
fn resolve(request: &CallToolRequestParams) -> Result<(Tool, Option<String>, CallArgs), ErrorData> {
    let tool = Tool::from_name(&request.name).ok_or_else(|| {
        ErrorData::invalid_params(format!("There is no tool named `{}`.", request.name), None)
    })?;
    let none = JsonObject::new();
    let arguments = request.arguments.as_ref().unwrap_or(&none);
    match tool {
        Tool::ListConnections if arguments.is_empty() => Ok((tool, None, CallArgs::default())),
        Tool::ListConnections => Err(ErrorData::invalid_params(
            "`list_connections` takes no arguments.",
            None,
        )),
        Tool::ListSchemas => match (arguments.len(), arguments.get("connection")) {
            (1, Some(serde_json::Value::String(name))) => {
                Ok((tool, Some(name.clone()), CallArgs::default()))
            }
            _ => Err(ErrorData::invalid_params(
                "`list_schemas` takes one string argument, `connection`.",
                None,
            )),
        },
        Tool::ListTables => match (
            arguments.len(),
            arguments.get("connection"),
            arguments.get("schema"),
        ) {
            (2, Some(serde_json::Value::String(name)), Some(serde_json::Value::String(schema))) => {
                Ok((
                    tool,
                    Some(name.clone()),
                    CallArgs {
                        schema: Some(schema.clone()),
                        ..CallArgs::default()
                    },
                ))
            }
            _ => Err(ErrorData::invalid_params(
                "`list_tables` takes two string arguments, `connection` and `schema`.",
                None,
            )),
        },
        Tool::DescribeTable => match (
            arguments.len(),
            arguments.get("connection"),
            arguments.get("schema"),
            arguments.get("table"),
        ) {
            (
                3,
                Some(serde_json::Value::String(name)),
                Some(serde_json::Value::String(schema)),
                Some(serde_json::Value::String(table)),
            ) => Ok((
                tool,
                Some(name.clone()),
                CallArgs {
                    schema: Some(schema.clone()),
                    table: Some(table.clone()),
                    ..CallArgs::default()
                },
            )),
            _ => Err(ErrorData::invalid_params(
                "`describe_table` takes three string arguments, `connection`, `schema` and `table`.",
                None,
            )),
        },
        Tool::ExplainQuery => {
            // `analyze` is the one optional argument: absent, or a boolean.
            let analyze = match arguments.get("analyze") {
                None => Some(None),
                Some(serde_json::Value::Bool(asked)) => Some(Some(*asked)),
                Some(_) => None,
            };
            let others = arguments.len() - usize::from(arguments.contains_key("analyze"));
            match (
                others,
                arguments.get("connection"),
                arguments.get("sql"),
                analyze,
            ) {
                (
                    2,
                    Some(serde_json::Value::String(name)),
                    Some(serde_json::Value::String(sql)),
                    Some(analyze),
                ) => Ok((
                    tool,
                    Some(name.clone()),
                    CallArgs {
                        sql: Some(sql.clone()),
                        analyze,
                        ..CallArgs::default()
                    },
                )),
                _ => Err(ErrorData::invalid_params(
                    "`explain_query` takes two string arguments, `connection` and `sql`, and an optional boolean, `analyze`.",
                    None,
                )),
            }
        }
        Tool::SearchSchema => match strings(arguments, ["connection", "query"]) {
            Some([name, query]) => Ok((
                tool,
                Some(name),
                CallArgs {
                    query: Some(query),
                    ..CallArgs::default()
                },
            )),
            None => Err(ErrorData::invalid_params(
                "`search_schema` takes two string arguments, `connection` and `query`.",
                None,
            )),
        },
        Tool::ListRelationships | Tool::ListRoutines => {
            match strings(arguments, ["connection", "schema"]) {
                Some([name, schema]) => Ok((
                    tool,
                    Some(name),
                    CallArgs {
                        schema: Some(schema),
                        ..CallArgs::default()
                    },
                )),
                None => Err(ErrorData::invalid_params(
                    format!(
                        "`{}` takes two string arguments, `connection` and `schema`.",
                        tool.name()
                    ),
                    None,
                )),
            }
        }
        Tool::DescribeRoutine => match strings(arguments, ["connection", "schema", "routine"]) {
            Some([name, schema, routine]) => Ok((
                tool,
                Some(name),
                CallArgs {
                    schema: Some(schema),
                    routine: Some(routine),
                    ..CallArgs::default()
                },
            )),
            None => Err(ErrorData::invalid_params(
                "`describe_routine` takes three string arguments, `connection`, `schema` and `routine`.",
                None,
            )),
        },
        Tool::ExecuteSql => match (
            arguments.len(),
            arguments.get("connection"),
            arguments.get("sql"),
        ) {
            (2, Some(serde_json::Value::String(name)), Some(serde_json::Value::String(sql))) => {
                Ok((
                    tool,
                    Some(name.clone()),
                    CallArgs {
                        sql: Some(sql.clone()),
                        ..CallArgs::default()
                    },
                ))
            }
            _ => Err(ErrorData::invalid_params(
                "`execute_sql` takes two string arguments, `connection` and `sql`.",
                None,
            )),
        },
    }
}

/// The values of exactly `names`, each a string, when `arguments` holds those
/// and nothing else.
#[cfg(unix)]
fn strings<const N: usize>(arguments: &JsonObject, names: [&str; N]) -> Option<[String; N]> {
    if arguments.len() != N {
        return None;
    }
    let mut values = names.map(|_| String::new());
    for (value, name) in values.iter_mut().zip(names) {
        *value = arguments.get(name)?.as_str()?.to_string();
    }
    Some(values)
}

/// A tool's result: its status first, then one sentence or the app's own text.
/// Only `executed` is not an error.
#[cfg(unix)]
fn answer(status: Status, text: &str) -> CallToolResult {
    let content = vec![ContentBlock::text(format!(
        "status: {}\n{text}",
        status.as_str()
    ))];
    if status == Status::Executed {
        CallToolResult::success(content)
    } else {
        CallToolResult::error(content)
    }
}

/// Connect to the app's socket, and keep the connection only when the process
/// listening on it runs as this user. The directory is the user's own, but a
/// call carries the agent's statement, so nothing is written to a listener
/// whose owner is not checked: one whose credentials cannot be read is refused
/// as permission denied.
#[cfg(unix)]
async fn connect_own(path: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(path).await?;
    if served_by(&stream, super::ipc::current_euid()) {
        Ok(stream)
    } else {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
}

/// Whether the peer of `stream` runs as `expected`. A parameter so a test can
/// give the wrong uid.
#[cfg(unix)]
fn served_by(stream: &UnixStream, expected: u32) -> bool {
    stream
        .peer_cred()
        .is_ok_and(|credentials| credentials.uid() == expected)
}

/// The connection ended with no answer on it, which is all the shim saw.
#[cfg(unix)]
fn closed_without_answering() -> CallToolResult {
    answer(
        Status::AppNotRunning,
        "DBDelve closed the connection without answering.",
    )
}

/// What a failed connect is reported as: the app's absence in the two ways a
/// socket path can say it, and for any other failure only what was observed.
#[cfg(unix)]
fn failed_connect_text(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            NOT_RUNNING.to_string()
        }
        std::io::ErrorKind::PermissionDenied => {
            "Could not connect to DBDelve's agent socket: permission denied.".to_string()
        }
        _ => format!("Could not connect to DBDelve's agent socket: {error}."),
    }
}

/// The one answer line a call gets, read without holding the runtime: a line
/// is at most the reply limit, which is wider than a request's because an
/// answer carries rows, and one that does not end in a newline, or does not
/// decode, is no answer.
#[cfg(unix)]
async fn read_reply(stream: &mut UnixStream) -> Option<Reply> {
    let mut line = Vec::new();
    let mut reader = BufReader::new(stream.take(wire::REPLY_LIMIT));
    reader.read_until(b'\n', &mut line).await.ok()?;
    if line.pop() != Some(b'\n') {
        return None;
    }
    serde_json::from_slice(&line).ok()
}

/// Serve MCP on stdio until the client closes it. The exit code is the
/// process's.
#[cfg(unix)]
pub(crate) fn run() -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("The agent shim could not start its runtime: {error}.");
            return 1;
        }
    };
    runtime.block_on(async {
        let shim = Shim::new(super::socket_path());
        shim.keep_presence();
        let service = match shim.serve(stdio()).await {
            Ok(service) => service,
            Err(error) => {
                eprintln!("The agent shim could not start serving: {error}.");
                return 1;
            }
        };
        match service.waiting().await {
            Ok(_) => 0,
            Err(error) => {
                eprintln!("The agent shim stopped serving: {error}.");
                1
            }
        }
    })
}

/// Release Windows builds have no console, so the exit code is the contract.
#[cfg(not(unix))]
pub(crate) fn run() -> i32 {
    eprintln!("{}", crate::agent::UNAVAILABLE);
    1
}

/// The line that registers this DBDelve with Claude Code, for the user to paste
/// into a terminal: `claude mcp add` in user scope, so that every project sees
/// the server, and the absolute path of the file Claude Code is to start with
/// `mcp`. DBDelve never runs the line or reads what it changes; it is text.
///
/// A non-empty `appimage` is the path named in place of `executable`, because
/// an AppImage's own executable path is a mount that is gone with the process.
/// A non-empty `variant` is the raw `DBDELVE_VARIANT`: it names the server
/// `dbdelve-<variant>`, so two builds register side by side, and it is passed
/// on as `-e`, because the shim finds the app's socket by that variable and
/// without it would look for the release build's. The option follows the name:
/// `-e` takes every word after it until `--`, so before the name it would take
/// the name.
///
/// None when the path is not UTF-8: a lossy copy would name another file.
pub(crate) fn registration_line(
    executable: &std::path::Path,
    appimage: Option<&std::ffi::OsStr>,
    variant: Option<&str>,
) -> Option<String> {
    let path = match appimage {
        Some(file) if !file.is_empty() => file.to_str()?,
        _ => executable.to_str()?,
    };
    // A variant may hold a space, and the shell would split the line there.
    let word = |text: String| {
        if text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '='))
        {
            text
        } else {
            single_quoted(&text)
        }
    };
    let (name, environment) = match variant.filter(|variant| !variant.is_empty()) {
        Some(variant) => (
            word(format!("dbdelve-{variant}")),
            format!(" -e {}", word(format!("DBDELVE_VARIANT={variant}"))),
        ),
        None => ("dbdelve".to_string(), String::new()),
    };
    Some(format!(
        "claude mcp add --scope user {name}{environment} -- {} mcp",
        single_quoted(path)
    ))
}

/// Which agent client the Connect snippet is formatted for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AgentClient {
    #[default]
    ClaudeCode,
    AntigravityCli,
    AntigravityApp,
}

impl AgentClient {
    pub(crate) const ALL: [Self; 3] =
        [Self::ClaudeCode, Self::AntigravityCli, Self::AntigravityApp];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::AntigravityCli => "Antigravity CLI",
            Self::AntigravityApp => "Antigravity App",
        }
    }

    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::AntigravityCli => "antigravity-cli",
            Self::AntigravityApp => "antigravity-app",
        }
    }

    pub(crate) fn note(self) -> &'static str {
        match self {
            Self::ClaudeCode => {
                "DBDelve cannot register an agent for you. Run this once in your terminal."
            }
            Self::AntigravityCli => {
                "Registers DBDelve for both Antigravity CLI and App. Run this once in your terminal."
            }
            Self::AntigravityApp => {
                "Add this to ~/.gemini/config/mcp_config.json (or Skills & Customizations → MCP Servers)."
            }
        }
    }
}

/// The `agy mcp add` line that registers this DBDelve with the Antigravity CLI
/// (and Antigravity App, which shares `~/.gemini/config/mcp_config.json`).
/// Flags precede `<name>`, as `agy mcp add` requires.
pub(crate) fn antigravity_cli_line(
    executable: &std::path::Path,
    appimage: Option<&std::ffi::OsStr>,
    variant: Option<&str>,
) -> Option<String> {
    let path = match appimage {
        Some(file) if !file.is_empty() => file.to_str()?,
        _ => executable.to_str()?,
    };
    let word = |text: String| {
        if text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '='))
        {
            text
        } else {
            single_quoted(&text)
        }
    };
    let (flags, name) = match variant.filter(|variant| !variant.is_empty()) {
        Some(variant) => (
            format!("-e {} ", word(format!("DBDELVE_VARIANT={variant}"))),
            word(format!("dbdelve-{variant}")),
        ),
        None => (String::new(), "dbdelve".to_string()),
    };
    Some(format!(
        "agy mcp add {flags}{name} {} mcp",
        single_quoted(path)
    ))
}

/// The `mcp_config.json` snippet for the Antigravity App.
pub(crate) fn antigravity_app_config(
    executable: &std::path::Path,
    appimage: Option<&std::ffi::OsStr>,
    variant: Option<&str>,
) -> Option<String> {
    let path = match appimage {
        Some(file) if !file.is_empty() => file.to_str()?,
        _ => executable.to_str()?,
    };
    let (name, entry) = match variant.filter(|variant| !variant.is_empty()) {
        Some(variant) => (
            format!("dbdelve-{variant}"),
            serde_json::json!({
                "command": path,
                "args": ["mcp"],
                "env": {
                    "DBDELVE_VARIANT": variant,
                }
            }),
        ),
        None => (
            "dbdelve".to_string(),
            serde_json::json!({
                "command": path,
                "args": ["mcp"],
            }),
        ),
    };
    let mut servers = serde_json::Map::new();
    servers.insert(name, entry);
    let config = serde_json::json!({ "mcpServers": servers });
    serde_json::to_string_pretty(&config).ok()
}

/// The registration command or JSON snippet for `client`.
pub(crate) fn registration_snippet(
    client: AgentClient,
    executable: &std::path::Path,
    appimage: Option<&std::ffi::OsStr>,
    variant: Option<&str>,
) -> Option<String> {
    match client {
        AgentClient::ClaudeCode => registration_line(executable, appimage, variant),
        AgentClient::AntigravityCli => antigravity_cli_line(executable, appimage, variant),
        AgentClient::AntigravityApp => antigravity_app_config(executable, appimage, variant),
    }
}

/// `text` as one shell word whose every character is literal: a single quote
/// cannot be written inside single quotes, so it closes them, is escaped, and
/// opens them again.
fn single_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Drivers that run the real [`Shim`] over a duplex, shared with the tests of
/// the plans that build on this one, so the library stays named here alone.
#[cfg(all(test, unix))]
pub(crate) mod testing {
    use std::{
        sync::Arc,
        thread::{self, JoinHandle},
    };

    use serde_json::{Value, json};
    use tokio::{
        io::{DuplexStream, Lines, ReadHalf, WriteHalf, split},
        sync::oneshot,
        time::timeout,
    };

    use super::*;

    /// Longer than anything here takes, so a wrong method name or a missing
    /// field fails the test instead of hanging it.
    pub(super) const PATIENCE: Duration = Duration::from_secs(5);

    pub(super) fn block_on<T>(test: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime builds")
            .block_on(test)
    }

    /// The client end of a duplex whose other end the shim serves, speaking raw
    /// JSON-RPC lines: the library's client side is not built.
    pub(super) struct Client {
        lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
        writer: WriteHalf<DuplexStream>,
        next_id: u64,
    }

    impl Client {
        /// Serving does not return until the first message arrives, so the
        /// shim runs as its own task rather than before the client speaks. The
        /// presence retry runs too, as it does in the process.
        pub(super) fn serve(shim: Shim) -> Self {
            shim.keep_presence();
            let (client, server) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                if let Ok(service) = shim.serve(server).await {
                    let _ = service.waiting().await;
                }
            });
            let (read, writer) = split(client);
            Self {
                lines: BufReader::new(read).lines(),
                writer,
                next_id: 1,
            }
        }

        pub(super) async fn send(&mut self, message: Value) {
            self.writer
                .write_all(format!("{message}\n").as_bytes())
                .await
                .expect("the shim takes the line");
        }

        pub(super) async fn line(&mut self) -> Value {
            let line = timeout(PATIENCE, self.lines.next_line())
                .await
                .expect("the shim writes a line in time")
                .expect("the line reads")
                .expect("the shim has not closed its end");
            serde_json::from_str(&line).expect("the shim writes JSON")
        }

        /// The next line, or `None` when none comes within `within` or the shim
        /// has closed its end.
        pub(super) async fn line_within(&mut self, within: Duration) -> Option<Value> {
            let line = timeout(within, self.lines.next_line()).await.ok()?;
            let line = line.expect("the line reads")?;
            Some(serde_json::from_str(&line).expect("the shim writes JSON"))
        }

        /// Send a request and return its id without waiting for the answer.
        pub(super) async fn start(&mut self, method: &str, params: Value) -> u64 {
            let id = self.next_id;
            self.next_id += 1;
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await;
            id
        }

        /// The response to a new request, and every other line the shim wrote
        /// on the way to it.
        pub(super) async fn request_with_notifications(
            &mut self,
            method: &str,
            params: Value,
        ) -> (Vec<Value>, Value) {
            let id = self.start(method, params).await;
            let mut others = Vec::new();
            loop {
                let message = self.line().await;
                if message["id"] == id {
                    return (others, message);
                }
                others.push(message);
            }
        }

        pub(super) async fn request(&mut self, method: &str, params: Value) -> Value {
            self.request_with_notifications(method, params).await.1
        }

        /// The legacy handshake, answered with the shim's result.
        pub(super) async fn initialize(&mut self) -> Value {
            let answered = self
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {"name": "test", "version": "0"},
                    }),
                )
                .await;
            self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .await;
            answered
        }

        pub(super) async fn call(&mut self, tool: &str, arguments: Value) -> Value {
            self.request("tools/call", json!({"name": tool, "arguments": arguments}))
                .await
        }
    }

    /// Run the real shim on a thread of its own, make one call through the
    /// handshake, and return that call's JSON-RPC `result`, or its `error`.
    pub(crate) fn call_through_the_shim(
        socket: PathBuf,
        tool: &str,
        arguments: Value,
    ) -> JoinHandle<Value> {
        let tool = tool.to_string();
        thread::spawn(move || {
            block_on(async move {
                let mut client = Client::serve(Shim::new(Ok(socket)));
                client.initialize().await;
                let answered = client.call(&tool, arguments).await;
                answered
                    .get("result")
                    .or_else(|| answered.get("error"))
                    .cloned()
                    .expect("a response carries a result or an error")
            })
        })
    }

    /// A shim that has done its handshake and been asked nothing, running on a
    /// thread of its own. Dropping it closes the client's end, which ends the
    /// shim and its presence connection, and waits for that.
    pub(crate) struct IdleShim {
        written: Arc<std::sync::Mutex<Vec<String>>>,
        stop: Option<oneshot::Sender<()>>,
        thread: Option<JoinHandle<()>>,
    }

    impl IdleShim {
        /// Every line the shim wrote to its client after the handshake.
        pub(crate) fn lines_after_handshake(&self) -> Vec<String> {
            self.written.lock().expect("the lines are readable").clone()
        }
    }

    impl Drop for IdleShim {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    pub(crate) fn idle_shim(socket: PathBuf, presence_retry: Duration) -> IdleShim {
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (stop, mut stopped) = oneshot::channel::<()>();
        let record = written.clone();
        let thread = thread::spawn(move || {
            block_on(async move {
                let shim = Shim::new(Ok(socket)).with_presence_retry(presence_retry);
                let mut client = Client::serve(shim);
                client.initialize().await;
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        line = client.lines.next_line() => match line {
                            Ok(Some(line)) => record
                                .lock()
                                .expect("the lines are writable")
                                .push(line),
                            _ => break,
                        },
                    }
                }
            })
        });
        IdleShim {
            written,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        os::unix::net::UnixListener,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
    };

    use serde_json::{Value, json};

    use super::{
        testing::{self, Client, PATIENCE, block_on},
        *,
    };
    use crate::agent::{CallId, ConnId, Inbound, Listener, tests::short_socket_path};

    /// What the app side of a test heard from its connections.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Call(CallId, Tool),
        Hangup(CallId),
        PresenceOpened(ConnId),
        PresenceClosed(ConnId),
    }

    /// What the app side of a test does with a call.
    enum Behaviour {
        /// Answer every call, after a wait.
        Answer(Reply, Duration),
        /// Leave the first call unanswered and answer every later one.
        HoldFirstThenAnswer(Reply),
        /// End every call without an answer.
        DropCalls,
    }

    fn executed(text: &str) -> Reply {
        Reply {
            status: Status::Executed,
            text: text.to_string(),
        }
    }

    /// The app side: the real listener, with a consumer on a std thread that
    /// reports what it hears.
    struct App {
        path: PathBuf,
        listener: Option<Listener>,
        heard: mpsc::Receiver<Seen>,
    }

    impl App {
        fn bound_at(path: PathBuf, behaviour: Behaviour) -> Self {
            let (inbound, mut incoming) = tokio::sync::mpsc::unbounded_channel();
            let Ok(listener) = crate::agent::listen(&path, inbound) else {
                panic!("the socket binds");
            };
            let (heard_by_test, heard) = mpsc::channel();
            thread::spawn(move || {
                let mut held = Vec::new();
                let mut calls = 0;
                while let Some(message) = incoming.blocking_recv() {
                    let seen = match message {
                        Inbound::Call {
                            id, tool, reply, ..
                        } => {
                            calls += 1;
                            match &behaviour {
                                Behaviour::Answer(answer, after) => {
                                    thread::sleep(*after);
                                    let _ = reply.send(answer.clone());
                                }
                                Behaviour::HoldFirstThenAnswer(answer) if calls > 1 => {
                                    let _ = reply.send(answer.clone());
                                }
                                Behaviour::HoldFirstThenAnswer(_) => held.push(reply),
                                Behaviour::DropCalls => drop(reply),
                            }
                            Seen::Call(id, tool)
                        }
                        Inbound::Hangup { id } => {
                            // Nobody is waiting on a held call once its caller
                            // is gone, and keeping it would keep its thread.
                            held.clear();
                            Seen::Hangup(id)
                        }
                        Inbound::PresenceOpened { conn } => Seen::PresenceOpened(conn),
                        Inbound::PresenceClosed { conn } => Seen::PresenceClosed(conn),
                        // The window's own bookkeeping: nothing here is about
                        // what the shim does.
                        Inbound::Undelivered { .. } => continue,
                    };
                    let _ = heard_by_test.send(seen);
                }
            });
            Self {
                path,
                listener: Some(listener),
                heard,
            }
        }

        fn answering(reply: Reply) -> Self {
            Self::bound_at(
                short_socket_path(),
                Behaviour::Answer(reply, Duration::ZERO),
            )
        }

        /// Stop listening, as the kill switch does.
        fn stop(&mut self) {
            self.listener.take();
        }

        /// Everything heard until `done` holds of it or `within` runs out.
        /// Waits without blocking the thread, because the shim under test runs
        /// on this one.
        async fn collect(&self, within: Duration, done: impl Fn(&[Seen]) -> bool) -> Vec<Seen> {
            let mut heard = Vec::new();
            let deadline = Instant::now() + within;
            loop {
                while let Ok(seen) = self.heard.try_recv() {
                    heard.push(seen);
                }
                if done(&heard) || Instant::now() >= deadline {
                    return heard;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    impl Drop for App {
        fn drop(&mut self) {
            self.listener.take();
            let _ = std::fs::remove_dir_all(self.path.parent().expect("the path has a directory"));
        }
    }

    /// The app side as raw wire: a plain socket that hands the test each call
    /// frame exactly as the shim wrote it, and answers it. Presence frames are
    /// not calls, so they are read and let go.
    struct FrameCapture {
        path: PathBuf,
        written: mpsc::Receiver<String>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl FrameCapture {
        fn bound() -> Self {
            use std::io::{BufRead, BufReader, Write};

            let path = short_socket_path();
            std::fs::create_dir_all(path.parent().expect("the path has a directory"))
                .expect("the directory is made");
            let listener = UnixListener::bind(&path).expect("the socket binds");
            let (frames, written) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let worker = thread::spawn(move || {
                for stream in listener.incoming() {
                    if stopping.load(Ordering::Acquire) {
                        return;
                    }
                    let Ok(mut stream) = stream else { return };
                    // A peer that never finishes its line cannot hold up the drop.
                    let _ = stream.set_read_timeout(Some(PATIENCE));
                    let mut line = String::new();
                    if BufReader::new(&stream).read_line(&mut line).is_ok()
                        && line.contains("\"call\"")
                    {
                        let _ = frames.send(line);
                        let _ = stream.write_all(b"{\"status\":\"executed\",\"text\":\"ok\"}\n");
                    }
                }
            });
            Self {
                path,
                written,
                stop,
                worker: Some(worker),
            }
        }
    }

    impl Drop for FrameCapture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            // Wake the accept so the worker sees the flag and returns.
            let _ = std::os::unix::net::UnixStream::connect(&self.path);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            let _ = std::fs::remove_dir_all(self.path.parent().expect("the path has a directory"));
        }
    }

    fn opened(heard: &[Seen]) -> usize {
        heard
            .iter()
            .filter(|seen| matches!(seen, Seen::PresenceOpened(_)))
            .count()
    }

    fn called(heard: &[Seen]) -> usize {
        heard
            .iter()
            .filter(|seen| matches!(seen, Seen::Call(..)))
            .count()
    }

    /// What a client of the `2026-07-28` era puts on every request instead of
    /// a handshake.
    fn modern_meta() -> Value {
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "test-client", "version": "1.0.0"},
            "io.modelcontextprotocol/clientCapabilities": {},
        })
    }

    fn listed_names(response: &Value) -> Vec<&str> {
        let tools = response["result"]["tools"]
            .as_array()
            .expect("tools are listed");
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
        tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("a tool has a name"))
            .collect()
    }

    /// The text of a tool result that is an error.
    fn error_text(response: &Value) -> &str {
        assert_eq!(response["result"]["isError"], true, "{response}");
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("a result carries text")
    }

    #[test]
    fn the_shim_answers_the_initialize_handshake() {
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(short_socket_path())));
            let answered = client.initialize().await;
            assert_eq!(answered["result"]["serverInfo"]["name"], "dbdelve");
            assert_eq!(
                answered["result"]["serverInfo"]["version"],
                env!("CARGO_PKG_VERSION")
            );
            let listed = client.request("tools/list", json!({})).await;
            assert_eq!(
                listed_names(&listed),
                [
                    "list_connections",
                    "search_schema",
                    "list_schemas",
                    "list_tables",
                    "describe_table",
                    "list_relationships",
                    "list_routines",
                    "describe_routine",
                    "execute_sql",
                    "explain_query"
                ]
            );
        });
    }

    #[test]
    fn the_shim_answers_server_discover() {
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(short_socket_path())));
            let discovered = client
                .request("server/discover", json!({"_meta": modern_meta()}))
                .await;
            let versions = discovered["result"]["supportedVersions"]
                .as_array()
                .expect("supported versions are listed");
            assert!(versions.contains(&json!("2026-07-28")), "{discovered}");
            assert!(versions.contains(&json!("2025-11-25")), "{discovered}");
            // In this era the server's name rides in the result's `_meta`.
            let info = &discovered["result"]["_meta"]["io.modelcontextprotocol/serverInfo"];
            assert_eq!(info["name"], "dbdelve", "{discovered}");
            assert_eq!(info["version"], env!("CARGO_PKG_VERSION"), "{discovered}");

            // No `initialize` was sent: each request carries its own version.
            let listed = client
                .request("tools/list", json!({"_meta": modern_meta()}))
                .await;
            assert_eq!(
                listed_names(&listed),
                [
                    "list_connections",
                    "search_schema",
                    "list_schemas",
                    "list_tables",
                    "describe_table",
                    "list_relationships",
                    "list_routines",
                    "describe_routine",
                    "execute_sql",
                    "explain_query"
                ]
            );
        });
    }

    #[test]
    fn every_tool_answers_app_not_running_with_no_socket() {
        let missing = short_socket_path();

        // A socket file that nothing listens on, as an app that quit leaves.
        let stale = short_socket_path();
        std::fs::create_dir_all(stale.parent().expect("the path has a directory"))
            .expect("the directory is made");
        drop(UnixListener::bind(&stale).expect("the socket binds"));
        assert!(stale.exists());

        let too_long = "The agent socket path is 130 bytes long; this system allows at most 103.";
        let cases = [
            (Ok(missing), NOT_RUNNING),
            (Ok(stale.clone()), NOT_RUNNING),
            (Err(too_long.to_string()), too_long),
        ];
        for (socket, sentence) in cases {
            block_on(async {
                let mut client = Client::serve(Shim::new(socket));
                client.initialize().await;
                for (tool, arguments) in [
                    ("list_connections", json!({})),
                    ("list_schemas", json!({"connection": "Orders"})),
                    (
                        "list_tables",
                        json!({"connection": "Orders", "schema": "main"}),
                    ),
                    (
                        "describe_table",
                        json!({"connection": "Orders", "schema": "main", "table": "t"}),
                    ),
                    (
                        "explain_query",
                        json!({"connection": "Orders", "sql": "SELECT 1"}),
                    ),
                    (
                        "execute_sql",
                        json!({"connection": "Orders", "sql": "UPDATE t SET a = 1"}),
                    ),
                ] {
                    let answered = client.call(tool, arguments).await;
                    assert_eq!(
                        error_text(&answered),
                        format!("status: app_not_running\n{sentence}")
                    );
                }
            });
        }
        let _ = std::fs::remove_dir_all(stale.parent().expect("the path has a directory"));
    }

    #[test]
    fn a_call_rides_its_own_connection_and_returns_the_apps_answer() {
        let app = App::answering(executed("[]"));
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(app.path.clone())));
            client.initialize().await;
            let answered = client.call("list_connections", json!({})).await;
            assert_eq!(answered["result"]["isError"], false, "{answered}");
            assert_eq!(
                answered["result"]["content"][0]["text"],
                "status: executed\n[]"
            );
            let heard = app
                .collect(PATIENCE, |heard| called(heard) > 0 && opened(heard) > 0)
                .await;
            assert_eq!(called(&heard), 1, "{heard:?}");
            assert!(
                heard
                    .iter()
                    .any(|seen| matches!(seen, Seen::Call(_, Tool::ListConnections)))
            );
        });
    }

    #[test]
    fn a_reply_past_the_old_frame_limit_arrives_whole() {
        let mut text = "truncated: none (1 rows)\n".to_string();
        text.push_str(&"x".repeat(100_000 - text.len()));
        let app = App::answering(executed(&text));
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(app.path.clone())));
            client.initialize().await;
            let answered = client.call("list_connections", json!({})).await;
            assert_eq!(answered["result"]["isError"], false, "{answered}");
            assert_eq!(
                answered["result"]["content"][0]["text"],
                format!("status: executed\n{text}")
            );
        });
    }

    #[test]
    fn a_waiting_call_sends_progress_against_its_token() {
        let app = App::bound_at(
            short_socket_path(),
            Behaviour::Answer(executed("done"), Duration::from_millis(300)),
        );
        block_on(async {
            let shim =
                Shim::new(Ok(app.path.clone())).with_progress_every(Duration::from_millis(50));
            let mut client = Client::serve(shim);
            client.initialize().await;
            let (others, answered) = client
                .request_with_notifications(
                    "tools/call",
                    json!({
                        "name": "list_connections",
                        "arguments": {},
                        "_meta": {"progressToken": 7},
                    }),
                )
                .await;
            let progress: Vec<f64> = others
                .iter()
                .filter(|line| line["method"] == "notifications/progress")
                .map(|line| {
                    assert_eq!(line["params"]["progressToken"], 7, "{line}");
                    line["params"]["progress"]
                        .as_f64()
                        .expect("progress is a number")
                })
                .collect();
            assert!(progress.len() >= 2, "{others:?}");
            assert!(progress.windows(2).all(|pair| pair[0] < pair[1]));
            assert_eq!(
                answered["result"]["content"][0]["text"],
                "status: executed\ndone"
            );
        });
    }

    #[test]
    fn a_cancelled_call_hangs_up_its_connection() {
        let app = App::bound_at(
            short_socket_path(),
            Behaviour::HoldFirstThenAnswer(executed("[]")),
        );
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(app.path.clone())));
            client.initialize().await;
            let waiting = client
                .start(
                    "tools/call",
                    json!({
                        "name": "execute_sql",
                        "arguments": {"connection": "Orders", "sql": "UPDATE t SET a = 1"},
                    }),
                )
                .await;
            let heard = app.collect(PATIENCE, |heard| called(heard) > 0).await;
            let Some(Seen::Call(id, _)) = heard
                .into_iter()
                .find(|seen| matches!(seen, Seen::Call(..)))
            else {
                panic!("the call reaches the app");
            };

            client
                .send(json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": waiting},
                }))
                .await;
            let heard = app
                .collect(Duration::from_secs(1), |heard| {
                    heard.contains(&Seen::Hangup(id))
                })
                .await;
            assert!(heard.contains(&Seen::Hangup(id)), "{heard:?}");

            // Nothing is written for the call that was cancelled.
            while let Some(line) = client.line_within(Duration::from_millis(300)).await {
                assert_ne!(line["id"], waiting, "{line}");
            }

            let answered = client.call("list_connections", json!({})).await;
            assert_eq!(
                answered["result"]["content"][0]["text"],
                "status: executed\n[]"
            );
        });
    }

    #[test]
    fn a_connection_closed_before_an_answer_reads_app_not_running() {
        let app = App::bound_at(short_socket_path(), Behaviour::DropCalls);
        let result =
            testing::call_through_the_shim(app.path.clone(), "list_connections", json!({}))
                .join()
                .expect("the shim's thread ends");
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(
            result["content"][0]["text"],
            "status: app_not_running\nDBDelve closed the connection without answering."
        );
    }

    #[test]
    fn the_shim_holds_one_presence_and_reopens_it_on_the_next_call() {
        let path = short_socket_path();
        let mut first = App::bound_at(
            path.clone(),
            Behaviour::Answer(executed("[]"), Duration::ZERO),
        );
        block_on(async {
            // Only a call can reopen presence within this test.
            let shim = Shim::new(Ok(path.clone())).with_presence_retry(Duration::from_secs(60));
            let mut client = Client::serve(shim);
            client.initialize().await;
            let heard = first.collect(PATIENCE, |heard| opened(heard) > 0).await;
            assert_eq!(opened(&heard), 1, "{heard:?}");
            let heard = first.collect(Duration::from_millis(200), |_| false).await;
            assert_eq!(opened(&heard), 0, "a second presence was opened: {heard:?}");

            // The app goes away, and the shim keeps serving.
            first.stop();
            let answered = client.call("list_connections", json!({})).await;
            assert_eq!(
                error_text(&answered),
                format!("status: app_not_running\n{NOT_RUNNING}")
            );

            // It comes back: the next call reopens presence and is answered.
            let second = App::bound_at(
                path.clone(),
                Behaviour::Answer(executed("[]"), Duration::ZERO),
            );
            let answered = client.call("list_connections", json!({})).await;
            assert_eq!(
                answered["result"]["content"][0]["text"],
                "status: executed\n[]"
            );
            let heard = second
                .collect(PATIENCE, |heard| opened(heard) > 0 && called(heard) > 0)
                .await;
            assert_eq!(opened(&heard), 1, "{heard:?}");
            assert_eq!(called(&heard), 1, "{heard:?}");
            let heard = second.collect(Duration::from_millis(200), |_| false).await;
            assert_eq!(opened(&heard), 0, "a second presence was opened: {heard:?}");
        });
    }

    #[test]
    fn an_idle_shim_opens_presence_once_the_app_listens_without_any_call() {
        let path = short_socket_path();
        let retry = Duration::from_millis(50);
        let idle = testing::idle_shim(path.clone(), retry);
        block_on(async {
            // Several retries fail silently with nothing bound.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let app = App::bound_at(path.clone(), Behaviour::DropCalls);

            let heard = app
                .collect(Duration::from_secs(1), |heard| opened(heard) > 0)
                .await;
            assert_eq!(opened(&heard), 1, "{heard:?}");
            let Some(Seen::PresenceOpened(conn)) = heard
                .into_iter()
                .find(|seen| matches!(seen, Seen::PresenceOpened(_)))
            else {
                panic!("a presence opens");
            };

            // Left alone, it stays one presence and asks nothing.
            let heard = app.collect(Duration::from_millis(300), |_| false).await;
            assert_eq!(opened(&heard), 0, "{heard:?}");
            assert_eq!(called(&heard), 0, "{heard:?}");
            assert!(idle.lines_after_handshake().is_empty());

            drop(idle);
            let heard = app
                .collect(PATIENCE, |heard| {
                    heard.contains(&Seen::PresenceClosed(conn))
                })
                .await;
            assert!(heard.contains(&Seen::PresenceClosed(conn)), "{heard:?}");
        });
    }

    #[test]
    fn an_unknown_tool_or_an_argument_a_tool_does_not_take_is_an_invalid_params_error() {
        let app = App::answering(executed("[]"));
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(app.path.clone())));
            client.initialize().await;
            for (tool, arguments) in [
                ("list_connections", json!({"connection": "a"})),
                ("not_a_tool", json!({})),
            ] {
                let answered = client.call(tool, arguments).await;
                assert_eq!(answered["error"]["code"], -32602, "{tool}: {answered}");
            }
            let heard = app.collect(Duration::from_millis(300), |_| false).await;
            assert_eq!(called(&heard), 0, "{heard:?}");
        });
    }

    #[test]
    fn a_statement_too_long_for_a_frame_is_refused_before_any_connection() {
        let app = App::answering(executed("[]"));
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(app.path.clone())));
            client.initialize().await;
            // The second is short enough to look fine and is widened past the
            // limit by JSON writing each control character as six bytes.
            for sql in ["x".repeat(70_000), "\u{1}".repeat(16_000)] {
                let answered = client
                    .call("execute_sql", json!({"connection": "Orders", "sql": sql}))
                    .await;
                assert_eq!(answered["error"]["code"], -32602, "{answered}");
                let message = answered["error"]["message"].as_str().unwrap_or_default();
                assert!(message.contains("65536"), "{message}");
            }
            let heard = app.collect(Duration::from_millis(300), |_| false).await;
            assert_eq!(called(&heard), 0, "{heard:?}");
            // The idle presence opens on its own; a refused call adds none.
            assert!(opened(&heard) <= 1, "{heard:?}");
        });
    }

    #[test]
    fn execute_sql_takes_a_connection_and_a_statement_and_nothing_else() {
        // The app side reads the frame as the wire carries it, so the test
        // sees what the shim wrote and not what it meant to.
        let capture = FrameCapture::bound();
        let (path, written) = (capture.path.clone(), &capture.written);
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(path.clone())));
            client.initialize().await;
            for arguments in [
                json!({"connection": "Orders"}),
                json!({"sql": "SELECT 1"}),
                json!({"connection": "Orders", "sql": 3}),
                json!({"connection": 3, "sql": "SELECT 1"}),
                json!({"connection": "Orders", "sql": "SELECT 1", "limit": 5}),
            ] {
                let answered = client.call("execute_sql", arguments.clone()).await;
                assert_eq!(answered["error"]["code"], -32602, "{arguments}: {answered}");
            }
            assert!(
                written.try_recv().is_err(),
                "an invalid call reached the socket"
            );

            let answered = client
                .call(
                    "execute_sql",
                    json!({"connection": "Orders", "sql": "SELECT 1"}),
                )
                .await;
            assert_eq!(answered["result"]["isError"], false, "{answered}");
            let frame = written
                .recv_timeout(PATIENCE)
                .expect("the call reaches the app");
            let frame: Value = serde_json::from_str(&frame).expect("the frame is JSON");
            assert_eq!(frame["tool"], "execute_sql");
            assert_eq!(frame["connection"], "Orders");
            assert_eq!(frame["sql"], "SELECT 1");
        });
    }

    #[test]
    fn each_catalog_tool_takes_only_its_own_arguments() {
        // The app side reads the frame as the wire carries it, so the test
        // sees what the shim wrote and not what it meant to.
        let capture = FrameCapture::bound();
        let (path, written) = (capture.path.clone(), &capture.written);
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(path.clone())));
            client.initialize().await;
            for (tool, arguments) in [
                ("list_schemas", json!({})),
                ("list_schemas", json!({"connection": 3})),
                (
                    "list_schemas",
                    json!({"connection": "Orders", "schema": "main"}),
                ),
                ("list_tables", json!({"connection": "Orders"})),
                ("list_tables", json!({"schema": "main"})),
                ("list_tables", json!({"connection": "Orders", "schema": 3})),
                ("list_tables", json!({"connection": 3, "schema": "main"})),
                (
                    "list_tables",
                    json!({"connection": "Orders", "schema": "main", "limit": 5}),
                ),
                ("describe_table", json!({})),
                (
                    "describe_table",
                    json!({"connection": "Orders", "schema": "main"}),
                ),
                (
                    "describe_table",
                    json!({"connection": "Orders", "table": "t"}),
                ),
                (
                    "describe_table",
                    json!({"connection": "Orders", "schema": "main", "table": 3}),
                ),
                (
                    "describe_table",
                    json!({"connection": 3, "schema": "main", "table": "t"}),
                ),
                (
                    "describe_table",
                    json!({"connection": "Orders", "schema": "main", "table": "t", "limit": 5}),
                ),
                ("search_schema", json!({"connection": "Orders"})),
                ("search_schema", json!({"connection": "Orders", "query": 3})),
                (
                    "search_schema",
                    json!({"connection": "Orders", "query": "o", "schema": "main"}),
                ),
                ("list_relationships", json!({"connection": "Orders"})),
                (
                    "list_relationships",
                    json!({"connection": "Orders", "schema": "main", "table": "t"}),
                ),
                (
                    "list_routines",
                    json!({"connection": "Orders", "schema": 3}),
                ),
                (
                    "describe_routine",
                    json!({"connection": "Orders", "schema": "main"}),
                ),
                (
                    "describe_routine",
                    json!({"connection": "Orders", "schema": "main", "table": "f"}),
                ),
            ] {
                let answered = client.call(tool, arguments.clone()).await;
                assert_eq!(
                    answered["error"]["code"], -32602,
                    "{tool} {arguments}: {answered}"
                );
            }
            assert!(
                written.try_recv().is_err(),
                "an invalid call reached the socket"
            );

            let answered = client
                .call("list_schemas", json!({"connection": "Orders"}))
                .await;
            assert_eq!(answered["result"]["isError"], false, "{answered}");
            let frame = written
                .recv_timeout(PATIENCE)
                .expect("the call reaches the app");
            let frame: Value = serde_json::from_str(&frame).expect("the frame is JSON");
            assert_eq!(frame["tool"], "list_schemas");
            assert_eq!(frame["connection"], "Orders");
            assert!(frame.get("schema").is_none(), "{frame}");

            let answered = client
                .call(
                    "list_tables",
                    json!({"connection": "Orders", "schema": "main"}),
                )
                .await;
            assert_eq!(answered["result"]["isError"], false, "{answered}");
            let frame = written
                .recv_timeout(PATIENCE)
                .expect("the call reaches the app");
            let frame: Value = serde_json::from_str(&frame).expect("the frame is JSON");
            assert_eq!(frame["tool"], "list_tables");
            assert_eq!(frame["connection"], "Orders");
            assert_eq!(frame["schema"], "main");
            assert!(frame.get("table").is_none(), "{frame}");

            let answered = client
                .call(
                    "describe_table",
                    json!({"connection": "Orders", "schema": "main", "table": "t"}),
                )
                .await;
            assert_eq!(answered["result"]["isError"], false, "{answered}");
            let frame = written
                .recv_timeout(PATIENCE)
                .expect("the call reaches the app");
            let frame: Value = serde_json::from_str(&frame).expect("the frame is JSON");
            assert_eq!(frame["tool"], "describe_table");
            assert_eq!(frame["connection"], "Orders");
            assert_eq!(frame["schema"], "main");
            assert_eq!(frame["table"], "t");

            for (tool, arguments, carried) in [
                (
                    "search_schema",
                    json!({"connection": "Orders", "query": "ord"}),
                    "query",
                ),
                (
                    "list_relationships",
                    json!({"connection": "Orders", "schema": "main"}),
                    "schema",
                ),
                (
                    "list_routines",
                    json!({"connection": "Orders", "schema": "main"}),
                    "schema",
                ),
                (
                    "describe_routine",
                    json!({"connection": "Orders", "schema": "main", "routine": "f"}),
                    "routine",
                ),
            ] {
                let answered = client.call(tool, arguments.clone()).await;
                assert_eq!(answered["result"]["isError"], false, "{answered}");
                let frame = written
                    .recv_timeout(PATIENCE)
                    .expect("the call reaches the app");
                let frame: Value = serde_json::from_str(&frame).expect("the frame is JSON");
                assert_eq!(frame["tool"], tool);
                assert_eq!(frame["connection"], "Orders");
                assert_eq!(frame[carried], arguments[carried], "{frame}");
                assert!(frame.get("table").is_none(), "{frame}");
            }
        });
    }

    #[test]
    fn explain_query_takes_a_statement_and_an_optional_analyze_flag() {
        let capture = FrameCapture::bound();
        let (path, written) = (capture.path.clone(), &capture.written);
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(path.clone())));
            client.initialize().await;
            for arguments in [
                json!({}),
                json!({"connection": "Orders"}),
                json!({"sql": "SELECT 1"}),
                json!({"connection": "Orders", "sql": 3}),
                json!({"connection": 3, "sql": "SELECT 1"}),
                json!({"connection": "Orders", "sql": "SELECT 1", "analyze": "yes"}),
                json!({"connection": "Orders", "sql": "SELECT 1", "analyze": 1}),
                json!({"connection": "Orders", "sql": "SELECT 1", "analyze": null}),
                json!({"connection": "Orders", "sql": "SELECT 1", "limit": 5}),
                json!({"connection": "Orders", "sql": "SELECT 1", "analyze": true, "limit": 5}),
            ] {
                let answered = client.call("explain_query", arguments.clone()).await;
                assert_eq!(answered["error"]["code"], -32602, "{arguments}: {answered}");
            }
            assert!(
                written.try_recv().is_err(),
                "an invalid call reached the socket"
            );

            let answered = client
                .call(
                    "explain_query",
                    json!({"connection": "Orders", "sql": "SELECT 1"}),
                )
                .await;
            assert_eq!(answered["result"]["isError"], false, "{answered}");
            let frame = written
                .recv_timeout(PATIENCE)
                .expect("the call reaches the app");
            let frame: Value = serde_json::from_str(&frame).expect("the frame is JSON");
            assert_eq!(frame["tool"], "explain_query");
            assert_eq!(frame["connection"], "Orders");
            assert_eq!(frame["sql"], "SELECT 1");
            assert!(frame.get("analyze").is_none(), "{frame}");

            let answered = client
                .call(
                    "explain_query",
                    json!({"connection": "Orders", "sql": "SELECT 1", "analyze": true}),
                )
                .await;
            assert_eq!(answered["result"]["isError"], false, "{answered}");
            let frame = written
                .recv_timeout(PATIENCE)
                .expect("the call reaches the app");
            let frame: Value = serde_json::from_str(&frame).expect("the frame is JSON");
            assert_eq!(frame["analyze"], true, "{frame}");
        });
    }

    #[test]
    fn the_shim_writes_only_to_a_listener_of_its_own_user() {
        block_on(async {
            let (ours, _peer) = UnixStream::pair().expect("a pair of sockets");
            let me = crate::agent::ipc::current_euid();
            assert!(served_by(&ours, me));
            assert!(!served_by(&ours, me.wrapping_add(1)));
        });
    }

    #[test]
    fn the_registration_line_quotes_the_path_and_names_the_variant() {
        use std::ffi::OsStr;

        let release = Path::new("/Applications/DBDelve.app/Contents/MacOS/DBDelve");
        assert_eq!(
            registration_line(release, None, None),
            Some(
                "claude mcp add --scope user dbdelve -- \
                 '/Applications/DBDelve.app/Contents/MacOS/DBDelve' mcp"
                    .to_string()
            )
        );
        // An empty variant is the release.
        assert_eq!(
            registration_line(release, None, Some("")),
            registration_line(release, None, None)
        );

        let dev = Path::new("/Users/a/DBDelve Dev.app/Contents/MacOS/DBDelve Dev");
        assert_eq!(
            registration_line(dev, None, Some("dev")),
            Some(
                "claude mcp add --scope user dbdelve-dev -e DBDELVE_VARIANT=dev -- \
                 '/Users/a/DBDelve Dev.app/Contents/MacOS/DBDelve Dev' mcp"
                    .to_string()
            )
        );

        // `$` stays literal in single quotes, and a quote closes and reopens them.
        assert_eq!(
            registration_line(Path::new("/tmp/it's $HOME/dbdelve"), None, None),
            Some(
                r"claude mcp add --scope user dbdelve -- '/tmp/it'\''s $HOME/dbdelve' mcp"
                    .to_string()
            )
        );

        let appimage = OsStr::new("/home/a/Apps/DBDelve.AppImage");
        assert_eq!(
            registration_line(
                Path::new("/tmp/.mount_x/usr/bin/dbdelve"),
                Some(appimage),
                None
            ),
            Some(
                "claude mcp add --scope user dbdelve -- '/home/a/Apps/DBDelve.AppImage' mcp"
                    .to_string()
            )
        );
        assert_eq!(
            registration_line(release, Some(OsStr::new("")), None),
            registration_line(release, None, None)
        );

        // A variant with a space is one word each in the name and the option.
        assert_eq!(
            registration_line(release, None, Some("my build")),
            Some(
                "claude mcp add --scope user 'dbdelve-my build' -e 'DBDELVE_VARIANT=my build' -- \
                 '/Applications/DBDelve.app/Contents/MacOS/DBDelve' mcp"
                    .to_string()
            )
        );

        // A path that is not UTF-8 gets no line rather than one naming another file.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let unreadable = OsStr::from_bytes(b"/tmp/db\xffdelve");
            assert_eq!(registration_line(Path::new(unreadable), None, None), None);
            assert_eq!(registration_line(release, Some(unreadable), None), None);
            assert_eq!(
                antigravity_cli_line(Path::new(unreadable), None, None),
                None
            );
            assert_eq!(
                antigravity_app_config(Path::new(unreadable), None, None),
                None
            );
        }

        // Antigravity CLI: flags must precede `<name>`.
        assert_eq!(
            antigravity_cli_line(release, None, None),
            Some(
                "agy mcp add dbdelve '/Applications/DBDelve.app/Contents/MacOS/DBDelve' mcp"
                    .to_string()
            )
        );
        assert_eq!(
            antigravity_cli_line(release, None, Some("dev")),
            Some(
                "agy mcp add -e DBDELVE_VARIANT=dev dbdelve-dev \
                 '/Applications/DBDelve.app/Contents/MacOS/DBDelve' mcp"
                    .to_string()
            )
        );
        assert_eq!(
            antigravity_cli_line(release, None, Some("my build")),
            Some(
                "agy mcp add -e 'DBDELVE_VARIANT=my build' 'dbdelve-my build' \
                 '/Applications/DBDelve.app/Contents/MacOS/DBDelve' mcp"
                    .to_string()
            )
        );

        // Antigravity App: valid JSON with `mcpServers`, `command`, `args`, and optional `env`.
        let app_json: Value = serde_json::from_str(
            &antigravity_app_config(release, None, None).expect("release json"),
        )
        .expect("valid json");
        assert_eq!(
            app_json["mcpServers"]["dbdelve"]["command"],
            "/Applications/DBDelve.app/Contents/MacOS/DBDelve"
        );
        assert_eq!(app_json["mcpServers"]["dbdelve"]["args"], json!(["mcp"]));
        assert!(app_json["mcpServers"]["dbdelve"].get("env").is_none());

        let dev_json: Value = serde_json::from_str(
            &antigravity_app_config(release, None, Some("dev")).expect("dev json"),
        )
        .expect("valid json");
        assert_eq!(
            dev_json["mcpServers"]["dbdelve-dev"]["env"]["DBDELVE_VARIANT"],
            "dev"
        );
    }

    #[test]
    fn a_failed_connect_says_only_what_was_observed() {
        use std::io::{Error, ErrorKind};

        for kind in [ErrorKind::NotFound, ErrorKind::ConnectionRefused] {
            assert_eq!(failed_connect_text(&Error::from(kind)), NOT_RUNNING);
        }
        assert_eq!(
            failed_connect_text(&Error::from(ErrorKind::PermissionDenied)),
            "Could not connect to DBDelve's agent socket: permission denied."
        );
        let other = failed_connect_text(&Error::from(ErrorKind::TimedOut));
        assert_eq!(
            other,
            format!(
                "Could not connect to DBDelve's agent socket: {}.",
                Error::from(ErrorKind::TimedOut)
            )
        );
        for text in [
            NOT_RUNNING.to_string(),
            other,
            failed_connect_text(&Error::from(ErrorKind::PermissionDenied)),
        ] {
            for advice in ["try", "please", "should", "make sure", "restart"] {
                assert!(!text.to_lowercase().contains(advice), "{text}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn list_tools_omits_schema_tools_denied_by_saved_policy() {
        let dir = std::env::temp_dir().join(format!("dbdelve-shim-policy-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("profiles.toml");
        std::fs::write(
            &path,
            "[settings.agent_policy]\nlist_tables = \"deny\"\nexplain_query = \"deny\"\ndescribe_table = \"ask\"\n",
        )
        .expect("profiles file writes");

        testing::block_on(async {
            let shim = Shim::new(Err("no socket".to_string())).with_profiles_path(path.clone());
            let mut client = testing::Client::serve(shim);
            client.initialize().await;
            let response = client.request("tools/list", json!({})).await;
            let names: Vec<&str> = response["result"]["tools"]
                .as_array()
                .expect("tools array")
                .iter()
                .map(|tool| tool["name"].as_str().expect("tool name"))
                .collect();
            assert!(!names.contains(&"list_tables"), "{names:?}");
            assert!(!names.contains(&"explain_query"), "{names:?}");
            assert!(names.contains(&"describe_table"), "{names:?}");
            assert!(names.contains(&"execute_sql"), "{names:?}");
            assert_eq!(names.len(), Tool::ALL.len() - 2);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(all(test, not(unix)))]
mod tests {
    use super::run;

    #[test]
    fn the_agent_command_exits_non_zero_on_windows() {
        assert_eq!(run(), 1);
    }
}

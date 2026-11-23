//! An approved write.
//!
//! It runs on a connection opened for that one call, never a lane's and never
//! the user's, because a session change the statement makes must die with the
//! connection (`SET`, `USE`, a temporary table, an open transaction). It has
//! no deadline and no cancel: a lane's calls end by cancelling the statement
//! on the server, and a write interrupted half-way is worse than one that
//! finishes, so nothing but its own result ends it.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
    },
    time::Instant,
};

use super::agent::{AuditVerdict, CallFacts, Decision, INVALID_INDEX, audited_error, elapsed_ms};
use super::agent_read::{AGENT_ROWS, OPEN_FAILED, scrubbed, table_of};
use super::*;
use crate::{
    agent::{CallId, Reply, Status, result, tools::Tool},
    db::{CancelToken, Connection, ConnectionConfig, DbError, Engine, Fetch, QueryResult},
    sql::{counts_rows, creates_index_concurrently},
};

// ponytail: the longest an approved write is left to run where the engine has
// a server-side bound. A profile's own shorter timeout wins, as for reads. It
// is not a deadline that cancels: a running write is never cancelled. The
// upgrade is a setting, if a user ever needs one.
pub(crate) const WRITE_TIMEOUT: u32 = 600;

// ponytail: how long an approved write's session waits for a lock, a row lock
// or a metadata one (on MySQL and MariaDB, `innodb_lock_wait_timeout` and
// `lock_wait_timeout`). Long enough for an ordinary row lock to clear, short
// enough that a queued `ALTER` does not hold up every later query on its table
// for the whole write bound (on MySQL, for a year). The upgrade is a setting.
pub(crate) const LOCK_WAIT: u32 = 30;

/// The statement timeout in seconds an approved write's connection carries,
/// given the profile's own: the smaller of the two, with 0 (none) meaning
/// [`WRITE_TIMEOUT`].
pub(crate) fn write_timeout(user: u32) -> u32 {
    match user {
        0 => WRITE_TIMEOUT,
        user => user.min(WRITE_TIMEOUT),
    }
}

/// The config an approved write's connection is opened from: the profile's,
/// bounded by [`write_timeout`]. A read's `agent_config` caps at thirty
/// seconds, which a `CREATE INDEX` would not survive.
pub(crate) fn agent_write_config(config: &ConnectionConfig) -> ConnectionConfig {
    config
        .clone()
        .with_statement_timeout(write_timeout(config.statement_timeout()))
}

/// An approved write that is running. Holds no connection, no cancel token and
/// no deadline: the connection lives in the blocking closure and is dropped
/// there, and the call ends only when the result lands in
/// [`Workspace::finish_write`].
pub(crate) struct Writing {
    pub(crate) id: CallId,
    profile_name: String,
    engine: Engine,
    /// Kept after a hangup: the answer is still sent and nobody reads it.
    reply: SyncSender<Reply>,
    arrived: Instant,
    /// The statement exactly as it was approved, for the audit.
    sql: String,
    /// The bridge session its `started` line was written in. The write
    /// outlives a disconnect, and a bridge started meanwhile numbers its calls
    /// from zero again, so its `outcome` line must name the same session.
    session: u64,
    verdict: Option<AuditVerdict>,
    /// The caller hung up while the window already knew, as a `Hangup` that
    /// found this write running said. It is not cancelled for that.
    pub(super) requester_gone: bool,
    /// The socket's own flag for the call, set by its hangup watcher before the
    /// window hears of it. Read when the write ends, so a hangup the window has
    /// not yet heard still counts.
    gone: Arc<AtomicBool>,
    _work: gpui::Task<()>,
}

/// What opens, runs and answers an approved write: [`open_for_write`], or one a
/// test puts in [`AgentState::write_runner`](super::agent::AgentState::write_runner),
/// as `Opener` is for reads, to keep a write running.
pub(crate) type WriteRunner =
    fn(&gpui::BackgroundExecutor, ConnectionConfig, String, String, Engine) -> gpui::Task<Wrote>;

/// What the blocking half of an approved write came back with.
/// `hidden` is what an error from the server or the open must not name.
pub(crate) enum Wrote {
    NotOpened {
        hidden: Vec<String>,
        error: DbError,
    },
    Done {
        hidden: Vec<String>,
        result: Result<String, DbError>,
    },
}

/// [`run_write`] on a blocking thread, so the window's thread never waits on a
/// server or shuts a runtime down.
pub(super) fn open_for_write(
    executor: &gpui::BackgroundExecutor,
    config: ConnectionConfig,
    profile_id: String,
    sql: String,
    engine: Engine,
) -> gpui::Task<Wrote> {
    spawn_blocking(executor, move || {
        run_write(config, &profile_id, &sql, engine)
    })
}

/// Open a connection for this one call, run `sql` on it as written, build the
/// answer and drop the connection before returning. Blocking, and a plain
/// function so a live test runs exactly what the window runs.
pub(super) fn run_write(
    config: ConnectionConfig,
    profile_id: &str,
    sql: &str,
    engine: Engine,
) -> Wrote {
    let mut config = agent_write_config(&config);
    // A file engine has nothing to authenticate to, so it never reaches the
    // keychain.
    if let Some(slot) = config.password_mut()
        && slot.is_empty()
    {
        match store::password(profile_id) {
            Ok(Some(password)) => *slot = password,
            Ok(None) => {}
            Err(error) => {
                return Wrote::NotOpened {
                    hidden: config.identifying_text(),
                    error: DbError {
                        message: error,
                        position: None,
                    },
                };
            }
        }
    }
    // Taken after the password is in: it is one of the strings a server's
    // error may echo.
    let hidden = config.identifying_text();
    let connection = match Connection::open_for_approved_write(config, LOCK_WAIT) {
        Ok(connection) => connection,
        Err(error) => return Wrote::NotOpened { hidden, error },
    };
    // Not `reads_only`: a write's rows are drained, never stopped early.
    let result = connection
        .query(
            sql,
            &CancelToken::default(),
            Fetch {
                limit: Some(AGENT_ROWS),
                ..Fetch::default()
            },
        )
        .map(|result| written_text(engine, sql, &result));
    drop(connection);
    Wrote::Done { hidden, result }
}

/// What an approved write answers: its rows when it returned any, the server's
/// count when `sql` is a statement whose count is the rows it changed, and
/// otherwise that it ran. A `CREATE INDEX` reports a count of 0 that means no
/// count, so it is not read as "no row changed".
pub(super) fn written_text(engine: Engine, sql: &str, result: &QueryResult) -> String {
    if !result.columns.is_empty() {
        return result::text(&[table_of(engine, result)], result.rest.len(), None);
    }
    match result.rows_affected {
        Some(1) if counts_rows(engine, sql) => "The server reported 1 row affected.".to_string(),
        Some(count) if counts_rows(engine, sql) => {
            format!("The server reported {count} rows affected.")
        }
        _ => "The statement ran.".to_string(),
    }
}

/// Said after the text of a write DBDelve's own timer stopped waiting for.
const UNKNOWN_OUTCOME: &str =
    "DBDelve stopped waiting at its timeout; whether the server applied the write is not known.";

/// What an approved write that ended `failed` answers and records: the
/// server's (scrubbed) text, then
///
/// - that the outcome is not known, when the engine's timeout is DBDelve's own
///   timer and the error is that timer's ([`DbError::stopped_waiting`]): the
///   write was sent and no reply came, so the server may still apply it. Any
///   other error is the server's answer, however late it came, and one that
///   failed to open was never sent; or
/// - that an invalid index of that name may remain, when `sql` is a Postgres
///   `CREATE INDEX CONCURRENTLY`: a stopped concurrent build leaves one, no
///   other statement does, so no other failure says it.
fn failed_text(engine: Engine, sql: &str, server_text: &str, stopped_waiting: bool) -> String {
    if stopped_waiting && engine.times_out_on_the_client() {
        format!("{server_text} {UNKNOWN_OUTCOME}")
    } else if creates_index_concurrently(engine, sql) {
        format!("{server_text} {INVALID_INDEX}")
    } else {
        server_text.to_string()
    }
}

impl Workspace {
    /// Start an approved write on a connection of its own. No deadline timer
    /// is made: see the module's header.
    #[allow(
        clippy::too_many_arguments,
        reason = "the call's identity, the profile it is about, the statement as approved, its verdict and the way back to its caller are each one thing; a struct of them would be built once per call to be taken apart at once"
    )]
    pub(super) fn start_write(
        &mut self,
        id: CallId,
        profile_id: String,
        profile_name: String,
        config: ConnectionConfig,
        sql: String,
        verdict: Option<AuditVerdict>,
        reply: SyncSender<Reply>,
        arrived: Instant,
        gone: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let engine = config.engine();
        let run = self.agent.write_runner.unwrap_or(open_for_write);
        let work = cx.spawn({
            let sql = sql.clone();
            async move |workspace, cx| {
                let wrote = run(cx.background_executor(), config, profile_id, sql, engine).await;
                // The window may have closed meanwhile, and then nobody is
                // left to answer.
                let _ = workspace.update(cx, |workspace, cx| workspace.finish_write(id, wrote, cx));
            }
        });
        self.agent.writes.push(Writing {
            id,
            profile_name,
            engine,
            reply,
            arrived,
            sql,
            session: self.agent.session,
            verdict,
            requester_gone: false,
            gone,
            _work: work,
        });
        cx.notify();
    }

    /// The one door an approved write ends through: its own result.
    fn finish_write(&mut self, id: CallId, wrote: Wrote, cx: &mut Context<Self>) {
        let Some(index) = self.agent.writes.iter().position(|write| write.id == id) else {
            return;
        };
        let write = self.agent.writes.remove(index);
        // The answer has the server's (scrubbed) text whole; the audit keeps
        // what `audited_error` does of it, with DBDelve's own sentence after.
        let (status, text, error) = match wrote {
            Wrote::NotOpened { hidden, error } => (
                Status::Failed,
                OPEN_FAILED.to_string(),
                Some(audited_error(&scrubbed(&error.message, &hidden))),
            ),
            Wrote::Done {
                result: Ok(text), ..
            } => (Status::Executed, text, None),
            Wrote::Done {
                hidden,
                result: Err(error),
            } => {
                let server = scrubbed(&error.message, &hidden);
                let unknown = error.stopped_waiting();
                (
                    Status::Failed,
                    failed_text(write.engine, &write.sql, &server, unknown),
                    Some(failed_text(
                        write.engine,
                        &write.sql,
                        &audited_error(&server),
                        unknown,
                    )),
                )
            }
        };
        // From the socket's flag as well as from a `Hangup` the window heard,
        // and known before the outcome line is written: `Undelivered` arrives
        // after it and could only change the in-memory entry.
        let requester_gone = write.requester_gone || write.gone.load(Ordering::Acquire);
        // The caller may have hung up, and then nobody reads this; the reply is
        // still sent, because a dropped sender would close the socket with no
        // answer for a caller that is still there.
        let _ = write.reply.send(Reply { status, text });
        self.agent.record(
            id,
            Tool::ExecuteSql,
            Some(write.profile_name),
            None,
            status,
            CallFacts {
                verdict: write.verdict,
                sql: Some(write.sql),
                elapsed_ms: elapsed_ms(write.arrived),
                error,
                requester_gone,
                decision: Some(Decision::Approved),
                engine: Some(write.engine),
                session: Some(write.session),
                ..CallFacts::default()
            },
        );
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::{Column, every_shape},
        workspace::agent_read::agent_config,
    };

    #[test]
    fn the_write_config_carries_the_write_bound() {
        assert_eq!(write_timeout(0), WRITE_TIMEOUT);
        assert_eq!(write_timeout(5), 5);
        assert_eq!(write_timeout(u32::MAX), WRITE_TIMEOUT);
        for profile in every_shape() {
            for user in [0, 5, WRITE_TIMEOUT, WRITE_TIMEOUT + 1] {
                let profile = profile.clone().with_statement_timeout(user);
                let opened = agent_write_config(&profile);
                assert_eq!(
                    opened.statement_timeout(),
                    write_timeout(user),
                    "{:?} {user}",
                    profile.engine()
                );
                assert_eq!(
                    opened.with_statement_timeout(user),
                    profile,
                    "only the timeout differs"
                );
            }
        }
    }

    #[test]
    fn a_failed_concurrent_index_build_says_an_invalid_index_may_remain() {
        let remains = format!("boom {}", INVALID_INDEX);
        assert_eq!(
            failed_text(
                Engine::Postgres,
                "CREATE INDEX CONCURRENTLY i ON t (a)",
                "boom",
                false
            ),
            remains
        );
        for (engine, sql) in [
            (Engine::Postgres, "CREATE INDEX i ON t (a)"),
            (Engine::MySql, "CREATE INDEX CONCURRENTLY i ON t (a)"),
            (Engine::Postgres, "UPDATE t SET a = 1"),
        ] {
            assert_eq!(
                failed_text(engine, sql, "boom", false),
                "boom",
                "{engine:?} {sql}"
            );
        }
    }

    #[test]
    fn a_write_stopped_by_dbdelves_own_timer_says_its_outcome_is_not_known() {
        let unknown = format!("boom {UNKNOWN_OUTCOME}");
        assert_eq!(failed_text(Engine::Redis, "SET k v", "boom", true), unknown);
        assert_eq!(
            failed_text(Engine::MongoDb, "db.t.drop()", "boom", true),
            unknown
        );
        assert_eq!(failed_text(Engine::Redis, "SET k v", "boom", false), "boom");
        assert_eq!(
            failed_text(Engine::Postgres, "UPDATE t SET a = 1", "boom", true),
            "boom"
        );
        assert_eq!(
            failed_text(
                Engine::Postgres,
                "CREATE INDEX CONCURRENTLY i ON t (a)",
                "boom",
                true
            ),
            format!("boom {INVALID_INDEX}")
        );
    }

    #[test]
    fn an_approved_write_answers_its_count_or_that_it_ran() {
        let counted = |rows_affected| QueryResult {
            rows_affected,
            ..QueryResult::default()
        };
        const UPDATE: &str = "UPDATE t SET a = 1";
        const INDEX: &str = "CREATE INDEX i ON t (a)";
        for (sql, affected, text) in [
            (UPDATE, Some(1), "The server reported 1 row affected."),
            (UPDATE, Some(3), "The server reported 3 rows affected."),
            (INDEX, Some(0), "The statement ran."),
            (INDEX, None, "The statement ran."),
            (UPDATE, None, "The statement ran."),
        ] {
            assert_eq!(
                written_text(Engine::Sqlite, sql, &counted(affected)),
                text,
                "{sql} {affected:?}"
            );
        }

        let returned = QueryResult {
            columns: vec![Column {
                name: "id".to_string(),
                data_type: None,
            }],
            ..QueryResult::default()
        };
        let text = written_text(Engine::Sqlite, "UPDATE t SET a = 1 RETURNING id", &returned);
        assert!(text.contains("id"), "{text}");
        assert!(!text.contains("affected"), "{text}");
    }

    /// The compose Postgres from the standard `PG*` variables, as
    /// `agent_read`'s live tests open it.
    fn live_postgres() -> ConnectionConfig {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
        ConnectionConfig::postgres(crate::db::ServerConfig {
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
        })
    }

    /// Runs `teardown` and then `setup` on a direct connection -- the user's
    /// kind, because an agent `DROP` is refused (GATE-06) -- and `teardown`
    /// again when it goes out of scope, so a failed assertion leaves nothing
    /// on the shared database.
    struct Cleanup {
        config: ConnectionConfig,
        teardown: String,
    }

    impl Cleanup {
        fn new(config: &ConnectionConfig, setup: &str, teardown: &str) -> Self {
            let cleanup = Self {
                config: config.clone(),
                teardown: teardown.to_string(),
            };
            cleanup.run(&[teardown, setup]);
            cleanup
        }

        fn run(&self, statements: &[&str]) {
            let connection =
                Connection::open(self.config.clone()).expect("the compose Postgres opens");
            for sql in statements.iter().filter(|sql| !sql.is_empty()) {
                connection
                    .query(sql, &CancelToken::default(), Fetch::default())
                    .unwrap_or_else(|error| panic!("{sql}: {}", error.message));
            }
        }
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.run(&[&self.teardown]);
        }
    }

    /// The first cell of every row `sql` returns on the agent's read-only
    /// open, one per line.
    fn read_lines(config: &ConnectionConfig, sql: &str) -> String {
        let connection = Connection::open_for_agent(agent_config(config))
            .expect("the compose Postgres opens for the agent");
        let result = connection
            .query(sql, &CancelToken::default(), Fetch::default())
            .unwrap_or_else(|error| panic!("{sql}: {}", error.message));
        result
            .rows
            .iter()
            .map(|row| row.get(0).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn approved(config: &ConnectionConfig, sql: &str) -> Wrote {
        run_write(agent_write_config(config), "", sql, config.engine())
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_an_approved_create_index_moves_the_plan() {
        let config = live_postgres();
        let index = format!("dbdelve_sc5_{}", std::process::id());
        let _cleanup = Cleanup::new(&config, "", &format!("DROP INDEX IF EXISTS {index}"));
        const READ: &str = "SELECT * FROM events WHERE occurred_at = '2024-01-05 00:00:00+00'";

        let before = read_lines(&config, &format!("EXPLAIN {READ}"));
        assert!(
            before.contains("Seq Scan on events"),
            "no Seq Scan before the index; an index on events (occurred_at) left by an earlier run?\n{before}"
        );

        let Wrote::Done { result, .. } = approved(
            &config,
            &format!("CREATE INDEX {index} ON events (occurred_at)"),
        ) else {
            panic!("the approved write did not open");
        };
        assert_eq!(result.expect("the server builds it"), "The statement ran.");

        let after = read_lines(&config, &format!("EXPLAIN {READ}"));
        assert!(
            after.contains(&format!("Index Scan using {index}"))
                || after.contains(&format!("Bitmap Index Scan on {index}")),
            "{after}"
        );
        assert!(!after.contains("Seq Scan"), "{after}");
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_a_concurrent_index_build_runs_in_autocommit() {
        let config = live_postgres();
        let table = format!("dbdelve_cic_{}", std::process::id());
        let _cleanup = Cleanup::new(
            &config,
            &format!("CREATE TABLE {table} AS SELECT n AS a FROM generate_series(1, 100) n"),
            &format!("DROP TABLE IF EXISTS {table}"),
        );

        let Wrote::Done { result, .. } = approved(
            &config,
            &format!("CREATE INDEX CONCURRENTLY {table}_a ON {table} (a)"),
        ) else {
            panic!("the approved write did not open");
        };
        result.expect("a concurrent build runs outside a transaction block");
        let valid = read_lines(
            &config,
            &format!("SELECT indisvalid FROM pg_index WHERE indexrelid = '{table}_a'::regclass"),
        );
        assert_eq!(valid, "t");
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_a_failed_concurrent_index_build_says_an_invalid_index_may_remain() {
        let config = live_postgres();
        let table = format!("dbdelve_cicu_{}", std::process::id());
        let _cleanup = Cleanup::new(
            &config,
            &format!("CREATE TABLE {table} AS SELECT 1 AS a UNION ALL SELECT 1"),
            &format!("DROP TABLE IF EXISTS {table}"),
        );
        let sql = format!("CREATE UNIQUE INDEX CONCURRENTLY {table}_u ON {table} (a)");

        let Wrote::Done {
            hidden,
            result: Err(error),
            ..
        } = approved(&config, &sql)
        else {
            panic!("the duplicate build did not fail on the server");
        };
        let text = failed_text(
            Engine::Postgres,
            &sql,
            &scrubbed(&error.message, &hidden),
            false,
        );
        assert!(text.ends_with(INVALID_INDEX), "{text}");
        assert!(
            text.starts_with(&scrubbed(&error.message, &hidden)),
            "{text}"
        );
        let valid = read_lines(
            &config,
            &format!("SELECT indisvalid FROM pg_index WHERE indexrelid = '{table}_u'::regclass"),
        );
        assert_eq!(valid, "f");
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_a_failed_write_audits_the_servers_words_and_not_the_row_value() {
        let config = live_postgres();
        let table = format!("dbdelve_dup_{}", std::process::id());
        let _cleanup = Cleanup::new(
            &config,
            &format!(
                "CREATE TABLE {table} (email text UNIQUE); INSERT INTO {table} VALUES ('alice@example.com')"
            ),
            &format!("DROP TABLE IF EXISTS {table}"),
        );

        let Wrote::Done {
            hidden,
            result: Err(error),
            ..
        } = approved(
            &config,
            &format!("INSERT INTO {table} SELECT email FROM {table}"),
        )
        else {
            panic!("the duplicate insert did not fail on the server");
        };
        let answered = scrubbed(&error.message, &hidden);
        assert!(answered.contains("alice@example.com"), "{answered}");
        let audited = audited_error(&answered);
        assert!(!audited.contains("alice"), "{audited}");
        assert!(audited.starts_with("duplicate key value"), "{audited}");
    }

    /// A write Redis's read timeout stopped is known by its error, so it says
    /// its outcome is not known.
    #[test]
    #[ignore = "requires the compose databases"]
    fn live_a_redis_write_its_timer_stopped_says_its_outcome_is_not_known() {
        let config = ConnectionConfig::Redis(crate::db::ServerConfig {
            host: "127.0.0.1".to_string(),
            port: Some(
                std::env::var("DBDELVE_REDIS_PORT")
                    .ok()
                    .map_or(56379, |port| port.parse().expect("a port number")),
            ),
            database: "0".to_string(),
            user: "dbdelve".to_string(),
            password: "dbdelve".to_string(),
            sslmode: crate::db::SslMode::Disable,
            root_certificate: None,
            statement_timeout: 1,
            ssh: None,
            ssm: None,
            password_command: None,
            schema: None,
            read_only: false,
        });
        const SQL: &str = "BLPOP dbdelve:never 0";
        let Wrote::Done {
            hidden,
            result: Err(error),
        } = approved(&config, SQL)
        else {
            panic!("the blocked command did not time out");
        };
        assert!(error.stopped_waiting(), "{}", error.message);
        let text = failed_text(
            Engine::Redis,
            SQL,
            &scrubbed(&error.message, &hidden),
            error.stopped_waiting(),
        );
        assert!(text.ends_with(UNKNOWN_OUTCOME), "{text}");
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_the_write_session_carries_its_bounds() {
        let config = live_postgres();
        let setting = |connection: &Connection, name: &str| {
            let result = connection
                .query(
                    &format!("SHOW {name}"),
                    &CancelToken::default(),
                    Fetch::default(),
                )
                .expect("the server answers SHOW");
            result.rows[0].get(0).unwrap_or_default().to_string()
        };

        let writer = Connection::open_for_approved_write(agent_write_config(&config), LOCK_WAIT)
            .expect("the write connection opens");
        assert_eq!(setting(&writer, "lock_timeout"), "30s");
        assert_eq!(setting(&writer, "statement_timeout"), "10min");

        let reader =
            Connection::open_for_agent(agent_config(&config)).expect("the read connection opens");
        assert_ne!(setting(&reader, "lock_timeout"), "30s");
    }
}

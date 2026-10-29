//! Running a plan: the order its submissions go out in, what each landing
//! means for the rest, and the sentence the import ends on.

use super::Plan;
use crate::db::{self, CancelToken, Connection, DbError, Engine};
use crate::sql::{self, Destructive, Mode, Stop};
use crate::ui::group_thousands;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Step {
    Begin,
    Chunk(usize),
    Commit,
    Rollback,
}

impl Plan {
    pub(crate) fn sql(&self, step: Step) -> Result<String, String> {
        match step {
            Step::Begin => self
                .begin
                .map(str::to_string)
                .ok_or_else(|| "This import runs without a transaction.".into()),
            Step::Chunk(chunk) => self.statement(chunk),
            Step::Commit => Ok("COMMIT".into()),
            Step::Rollback => Ok("ROLLBACK".into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Outcome {
    Imported {
        /// The rows sent, every chunk having landed.
        rows: usize,
        /// What the server said it wrote, when every chunk said.
        affected: Option<u64>,
    },
    Cancelled {
        committed: usize,
    },
    Failed {
        message: String,
        chunk: Option<usize>,
        committed: usize,
    },
}

fn rows(n: usize) -> String {
    match n {
        1 => "1 row".into(),
        _ => format!("{} rows", group_thousands(n as u64)),
    }
}

impl Outcome {
    pub(crate) fn summary(&self, plan: &Plan) -> String {
        let target = format!("`{}.{}`", plan.schema, plan.table);
        let kept = |committed: usize| match committed {
            0 => "Nothing was imported.".to_string(),
            1 => "1 row was committed.".into(),
            n => format!("{} were committed.", rows(n)),
        };
        match self {
            Self::Imported {
                rows: sent,
                affected: Some(affected),
            } if *affected != *sent as u64 => match plan.skip && *affected < *sent as u64 {
                true => format!(
                    "Imported {} into {target} ({} skipped as duplicates).",
                    rows(*affected as usize),
                    group_thousands(*sent as u64 - affected)
                ),
                // A trigger can take rows in, or drop them, without counting
                // them, so a shortfall is no sign of duplicates.
                false => format!(
                    "Sent {} to {target}; the server reported {} written.",
                    rows(*sent),
                    rows(*affected as usize)
                ),
            },
            Self::Imported { rows: sent, .. } => format!("Imported {} into {target}.", rows(*sent)),
            Self::Cancelled { committed: 0 } => "Cancelled: nothing was imported.".into(),
            Self::Cancelled { committed: 1 } => "Cancelled after 1 row was committed.".into(),
            Self::Cancelled { committed } => {
                format!("Cancelled after {} were committed.", rows(*committed))
            }
            Self::Failed {
                message,
                chunk,
                committed,
            } => {
                let covered = chunk.map(|chunk| {
                    let lines = plan.lines(chunk);
                    let (first, last) = (*lines.start() as u64, *lines.end() as u64);
                    match first == last {
                        true => format!(
                            "The failing statement covered line {}. ",
                            group_thousands(first)
                        ),
                        false => format!(
                            "The failing statement covered lines {}–{}. ",
                            group_thousands(first),
                            group_thousands(last)
                        ),
                    }
                });
                format!(
                    "{message}\n\n{}{}",
                    covered.unwrap_or_default(),
                    kept(*committed)
                )
            }
        }
    }
}

/// An import's submissions, in order. All or nothing sends `Begin`, every
/// chunk, then `Commit`, and `Rollback` once anything fails or is cancelled
/// before every chunk has landed. Commit as it goes sends the chunks alone and
/// stops at the first failure, or after the chunk in flight when cancelled.
pub(crate) struct Run {
    chunk_rows: Vec<usize>,
    atomic: bool,
    begun: bool,
    /// Chunks that landed `Ok`, so also the next chunk to send.
    landed: usize,
    /// Rows the chunk that failed or was cancelled wrote before it stopped.
    partial: usize,
    /// `None` once any chunk could not say how many rows it wrote.
    affected: Option<u64>,
    cancelled: bool,
    /// The first failure, and the chunk it came from.
    failure: Option<(String, Option<usize>)>,
    outcome: Option<Outcome>,
}

impl Run {
    pub(crate) fn new(chunk_rows: Vec<usize>, atomic: bool) -> Self {
        let mut run = Self {
            chunk_rows,
            atomic,
            begun: false,
            landed: 0,
            partial: 0,
            affected: Some(0),
            cancelled: false,
            failure: None,
            outcome: None,
        };
        run.settle();
        run
    }

    pub(crate) fn next(&self) -> Option<Step> {
        if self.outcome.is_some() {
            None
        } else if !self.atomic {
            Some(Step::Chunk(self.landed))
        } else if self.failure.is_some() || self.cancelled {
            Some(Step::Rollback)
        } else if !self.begun {
            Some(Step::Begin)
        } else if self.landed < self.chunk_rows.len() {
            Some(Step::Chunk(self.landed))
        } else {
            Some(Step::Commit)
        }
    }

    pub(crate) fn landed(&mut self, step: Step, result: Result<Option<u64>, Failure>) {
        match (step, result) {
            (Step::Begin, Ok(_)) => self.begun = true,
            (Step::Chunk(_), Ok(affected)) => {
                self.landed += 1;
                self.affected = self.affected.zip(affected).map(|(sum, n)| sum + n);
            }
            (Step::Commit, Ok(_)) => self.outcome = Some(self.imported()),
            (Step::Rollback, _) => {
                let (message, chunk) = self.failure.take().unwrap_or_default();
                self.outcome = Some(match self.cancelled {
                    true => Outcome::Cancelled { committed: 0 },
                    false => Outcome::Failed {
                        message,
                        chunk,
                        committed: 0,
                    },
                });
            }
            (Step::Begin | Step::Commit, Err(failure)) => {
                self.failure.get_or_insert((failure.error.message, None));
            }
            (Step::Chunk(chunk), Err(failure)) => {
                if !self.atomic {
                    self.partial = failure.applied as usize;
                }
                // Anything else that lands after a cancel is reported, since
                // it is the chunk's own failure and may have partly applied.
                if !(self.cancelled && failure.cancel) {
                    self.failure
                        .get_or_insert((failure.error.message, Some(chunk)));
                }
            }
        }
        self.settle();
    }

    /// No-op once the run has failed, or once every chunk has landed under
    /// All or nothing: the `Commit` is then on its way, and a `Rollback` after
    /// it would find nothing to undo.
    pub(crate) fn cancel(&mut self) {
        let committing = self.atomic && self.landed == self.chunk_rows.len();
        if self.outcome.is_some() || self.failure.is_some() || committing {
            return;
        }
        self.cancelled = true;
        self.settle();
    }

    /// Rows in the chunks that have landed, committed or not.
    pub(crate) fn inserted(&self) -> usize {
        self.chunk_rows[..self.landed].iter().sum()
    }

    pub(crate) fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }

    fn imported(&self) -> Outcome {
        Outcome::Imported {
            rows: self.inserted(),
            affected: self.affected,
        }
    }

    /// Commit as it goes ends on its own landings, with no closing step.
    fn settle(&mut self) {
        if self.atomic {
            return;
        }
        let committed = self.inserted() + self.partial;
        self.outcome = if let Some((message, chunk)) = &self.failure {
            Some(Outcome::Failed {
                message: message.clone(),
                chunk: *chunk,
                committed,
            })
        } else if self.landed == self.chunk_rows.len() {
            Some(self.imported())
        } else if self.cancelled {
            Some(Outcome::Cancelled { committed })
        } else {
            None
        };
    }
}

/// Whether DBDelve may send `sql` for an import under `mode`: the write gate
/// first, since a statement it did not write is refused whatever the mode.
pub(crate) fn admitted(
    engine: Engine,
    sql: &str,
    mode: Mode,
    confirmed: &[Destructive],
) -> Result<(), String> {
    if !sql::is_generated_write_on(engine, sql) {
        return Err("DBDelve refused to run a statement it wrote itself.".into());
    }
    match sql::gate(&sql::classify(engine, sql), mode, confirmed) {
        None => Ok(()),
        Some(Stop::Upgrade(_)) => Err("Read-only does not allow the import's writes.".into()),
        Some(Stop::Confirm(_) | Stop::RunOnce) => {
            Err("DBDelve refused to run a statement it wrote itself.".into())
        }
    }
}

/// A submission that did not go through, and what its error says of it.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) error: DbError,
    /// The user's Cancel landing, rather than anything the statement did.
    pub(crate) cancel: bool,
    /// The statement's own writes that stay applied.
    pub(crate) applied: u64,
}

impl From<DbError> for Failure {
    fn from(error: DbError) -> Self {
        Self {
            error,
            cancel: false,
            applied: 0,
        }
    }
}

/// One submission, on the import's own connection from [`Connection::apart`].
pub(crate) fn send(
    connection: &Connection,
    engine: Engine,
    sql: &str,
    mode: Mode,
    confirmed: &[Destructive],
    cancel: &CancelToken,
) -> Result<Option<u64>, Failure> {
    admitted(engine, sql, mode, confirmed).map_err(|message| DbError {
        message,
        position: None,
    })?;
    connection
        .generated(sql, cancel, None, None)
        .map(|result| result.rows_affected)
        .map_err(|error| Failure {
            cancel: db::is_cancel(engine, &error),
            applied: db::applied(engine, &error),
            error,
        })
}

/// The live round trip every engine's `live_an_import_round_trips_on_*` runs:
/// a failing chunk, a clean file, a skip, a cancel, and a session dropped
/// mid-transaction, each counted back on `connection`.
#[cfg(test)]
pub(crate) fn live_round_trip(connection: Connection, engine: Engine, schema: &str) {
    use std::path::Path;

    use super::{CHUNK_BYTES, OnDuplicate, Options, TargetColumn, Transaction, chunks, prepare};
    use crate::db::Fetch;

    const TABLE: &str = "dbdelve_import_test";
    const DUPLICATE: &str = "1,a\n2,b\n3,c\n3,dup\n5,e\n";
    const CLEAN: &str = "1,a\n2,b\n3,c\n4,d\n5,e\n";
    let document = engine.takes_new_fields();
    let atomic = engine.all_or_nothing().is_ok();
    let ran = |sql: &str| connection.query(sql, &CancelToken::default(), Fetch::default());
    let collection = format!("db.getCollection(\"{TABLE}\")");
    let (key, emptied, dropped, counted) = match document {
        true => (
            "_id",
            format!("{collection}.deleteMany({{}})"),
            format!("{collection}.drop()"),
            format!("{collection}.countDocuments()"),
        ),
        false => (
            "id",
            format!("DELETE FROM {TABLE}"),
            format!("DROP TABLE IF EXISTS {TABLE}"),
            format!("SELECT COUNT(*) FROM {TABLE}"),
        ),
    };
    let empty = || {
        ran(&emptied).unwrap();
    };
    let count = || -> u64 {
        let result = ran(&counted).unwrap();
        result.rows[0].get(0).unwrap().parse().unwrap()
    };
    let column = |name: &str, data_type: &str| TargetColumn {
        name: name.into(),
        data_type: data_type.into(),
        nullable: false,
        has_default: false,
        generated: false,
    };
    let targets = match document {
        true => Vec::new(),
        false => vec![column("id", "integer"), column("name", "varchar(50)")],
    };
    let plan = |rows: &str, transaction, on_duplicate| {
        let bytes = format!("{key},name\n{rows}");
        let mut options = Options::detected(Path::new("rows.csv"), bytes.as_bytes());
        options.transaction = transaction;
        options.on_duplicate = on_duplicate;
        let checked = prepare(bytes.as_bytes(), &options, &targets, None, engine)
            .unwrap()
            .checked;
        assert_eq!(checked.problems, []);
        let mut plan = Plan::new(
            engine,
            schema.into(),
            TABLE.into(),
            checked,
            &options,
            engine.skips_duplicates(),
        );
        plan.chunks = chunks(&plan.checked.rows, 2, CHUNK_BYTES, !document);
        plan
    };
    let import = |plan: &Plan, cancel_after_first: bool| {
        let own = connection.apart().unwrap();
        let cancel = CancelToken::default();
        let mut run = Run::new(
            plan.chunks.iter().map(|c| c.len()).collect(),
            plan.begin.is_some(),
        );
        while let Some(step) = run.next() {
            run.landed(
                step,
                send(
                    &own,
                    engine,
                    &plan.sql(step).unwrap(),
                    Mode::ReadWrite,
                    &[],
                    &cancel,
                ),
            );
            if cancel_after_first && step == Step::Chunk(0) {
                run.cancel();
            }
        }
        run.outcome().cloned().unwrap()
    };
    let _ = ran(&dropped);
    if !document {
        ran(&format!(
            "CREATE TABLE {TABLE} (id integer PRIMARY KEY, name varchar(50) NOT NULL)"
        ))
        .unwrap();
    }

    if atomic {
        let failed = import(
            &plan(DUPLICATE, Transaction::AllOrNothing, OnDuplicate::Stop),
            false,
        );
        assert!(
            matches!(
                failed,
                Outcome::Failed {
                    chunk: Some(1),
                    committed: 0,
                    ..
                }
            ),
            "{failed:?}"
        );
        assert_eq!(count(), 0);
    }
    let failed = import(
        &plan(DUPLICATE, Transaction::CommitAsItGoes, OnDuplicate::Stop),
        false,
    );
    let Outcome::Failed {
        chunk: Some(1),
        committed,
        message,
    } = &failed
    else {
        panic!("{failed:?}");
    };
    // The ordered insertMany kept the row before the duplicate.
    assert_eq!(*committed, if document { 3 } else { 2 }, "{message}");
    assert_eq!(count(), *committed as u64);

    empty();
    let clean = plan(CLEAN, Transaction::AllOrNothing, OnDuplicate::Stop);
    assert_eq!(
        import(&clean, false),
        Outcome::Imported {
            rows: 5,
            affected: Some(5)
        }
    );
    assert_eq!(count(), 5);

    if engine.skips_duplicates() {
        let again = import(
            &plan(CLEAN, Transaction::AllOrNothing, OnDuplicate::Skip),
            false,
        );
        assert_eq!(
            again,
            Outcome::Imported {
                rows: 5,
                affected: Some(0)
            }
        );
        assert_eq!(count(), 5);
    }

    empty();
    if atomic {
        assert_eq!(import(&clean, true), Outcome::Cancelled { committed: 0 });
        assert_eq!(count(), 0);
    }
    let going = plan(CLEAN, Transaction::CommitAsItGoes, OnDuplicate::Stop);
    assert_eq!(import(&going, true), Outcome::Cancelled { committed: 2 });
    assert_eq!(count(), 2);

    if atomic {
        empty();
        let own = connection.apart().unwrap();
        let cancel = CancelToken::default();
        for step in [Step::Begin, Step::Chunk(0)] {
            let sql = clean.sql(step).unwrap();
            send(&own, engine, &sql, Mode::ReadWrite, &[], &cancel).unwrap();
        }
        drop(own);
        assert_eq!(count(), 0, "a dropped session's transaction is rolled back");
    }

    ran(&dropped).unwrap();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::db::{ConnectionConfig, SslMode};
    use crate::import_rows::{Checked, CheckedRow};

    fn failure(message: &str, cancel: bool, applied: u64) -> Result<Option<u64>, Failure> {
        let error = DbError {
            message: message.into(),
            position: None,
        };
        Err(Failure {
            error,
            cancel,
            applied,
        })
    }

    fn err(message: &str) -> Result<Option<u64>, Failure> {
        failure(message, false, 0)
    }

    const CANCELLED: &str = "canceling statement due to user request";

    #[test]
    fn all_or_nothing_brackets_every_chunk_and_rolls_back_a_failure() {
        let mut run = Run::new(vec![2, 2, 1], true);
        assert_eq!(run.next(), Some(Step::Begin));
        run.landed(Step::Begin, Ok(None));
        assert_eq!(run.next(), Some(Step::Chunk(0)));
        run.landed(Step::Chunk(0), Ok(Some(2)));
        run.landed(Step::Chunk(1), err("duplicate key"));
        assert_eq!(run.next(), Some(Step::Rollback));
        run.landed(Step::Rollback, Ok(None));
        assert_eq!(run.next(), None);
        assert_eq!(
            run.outcome(),
            Some(&Outcome::Failed {
                message: "duplicate key".into(),
                chunk: Some(1),
                committed: 0
            })
        );
    }

    #[test]
    fn commit_as_it_goes_keeps_what_landed() {
        let mut run = Run::new(vec![2, 2, 1], false);
        assert_eq!(run.next(), Some(Step::Chunk(0)));
        run.landed(Step::Chunk(0), Ok(Some(2)));
        run.landed(Step::Chunk(1), err("x"));
        assert_eq!(run.next(), None);
        assert_eq!(
            run.outcome(),
            Some(&Outcome::Failed {
                message: "x".into(),
                chunk: Some(1),
                committed: 2
            })
        );
    }

    #[test]
    fn a_cancel_rolls_back_or_stops_after_the_chunk_in_flight() {
        let mut atomic = Run::new(vec![2, 2], true);
        atomic.landed(Step::Begin, Ok(None));
        atomic.landed(Step::Chunk(0), Ok(Some(2)));
        atomic.cancel();
        atomic.landed(Step::Chunk(1), failure(CANCELLED, true, 0));
        assert_eq!(atomic.next(), Some(Step::Rollback));
        atomic.landed(Step::Rollback, Ok(None));
        assert_eq!(atomic.outcome(), Some(&Outcome::Cancelled { committed: 0 }));

        let mut going = Run::new(vec![2, 2], false);
        going.landed(Step::Chunk(0), Ok(Some(2)));
        going.cancel();
        assert_eq!(going.next(), None);
        assert_eq!(going.outcome(), Some(&Outcome::Cancelled { committed: 2 }));

        let mut stopped = Run::new(vec![2, 2], false);
        stopped.landed(Step::Chunk(0), Ok(Some(2)));
        stopped.cancel();
        stopped.landed(Step::Chunk(1), failure(CANCELLED, true, 0));
        assert_eq!(stopped.next(), None);
        assert_eq!(
            stopped.outcome(),
            Some(&Outcome::Cancelled { committed: 2 })
        );

        // Without a cancel, the cancel's words are someone else's stop.
        let mut killed = Run::new(vec![2, 2], false);
        killed.landed(Step::Chunk(0), Ok(Some(2)));
        killed.landed(Step::Chunk(1), failure(CANCELLED, true, 0));
        assert!(matches!(killed.outcome(), Some(Outcome::Failed { .. })));

        // An error of the chunk's own in flight may have applied part of it
        // (an ordered insertMany), so it is reported, not read as the cancel.
        let mut racing = Run::new(vec![2, 2], false);
        racing.landed(Step::Chunk(0), Ok(Some(2)));
        racing.cancel();
        racing.landed(Step::Chunk(1), failure("duplicate key", false, 1));
        assert_eq!(racing.next(), None);
        assert_eq!(
            racing.outcome(),
            Some(&Outcome::Failed {
                message: "duplicate key".into(),
                chunk: Some(1),
                committed: 3
            })
        );

        let mut late = Run::new(vec![2], true);
        late.landed(Step::Begin, Ok(None));
        late.landed(Step::Chunk(0), Ok(Some(2)));
        assert_eq!(late.next(), Some(Step::Commit));
        late.cancel();
        late.landed(Step::Commit, Ok(None));
        assert_eq!(
            late.outcome(),
            Some(&Outcome::Imported {
                rows: 2,
                affected: Some(2)
            })
        );
    }

    #[test]
    fn a_chunk_that_partly_applied_counts_what_it_kept_unless_rolled_back() {
        let mut going = Run::new(vec![500, 500], false);
        going.landed(Step::Chunk(0), failure("E11000", false, 299));
        assert_eq!(
            going.outcome(),
            Some(&Outcome::Failed {
                message: "E11000".into(),
                chunk: Some(0),
                committed: 299
            })
        );

        let mut atomic = Run::new(vec![500], true);
        atomic.landed(Step::Begin, Ok(None));
        atomic.landed(Step::Chunk(0), failure("E11000", false, 299));
        atomic.landed(Step::Rollback, Ok(None));
        assert!(matches!(
            atomic.outcome(),
            Some(Outcome::Failed { committed: 0, .. })
        ));
    }

    #[test]
    fn the_affected_rows_are_summed_and_unknown_once_any_chunk_says_nothing() {
        let mut run = Run::new(vec![2, 2], false);
        run.landed(Step::Chunk(0), Ok(Some(1)));
        run.landed(Step::Chunk(1), Ok(Some(2)));
        assert_eq!(
            run.outcome(),
            Some(&Outcome::Imported {
                rows: 4,
                affected: Some(3)
            })
        );
        let mut unknown = Run::new(vec![2, 2], false);
        unknown.landed(Step::Chunk(0), Ok(Some(2)));
        unknown.landed(Step::Chunk(1), Ok(None));
        assert_eq!(
            unknown.outcome(),
            Some(&Outcome::Imported {
                rows: 4,
                affected: None
            })
        );
    }

    fn users(begin: Option<&'static str>) -> Plan {
        let total = 12_345;
        Plan {
            engine: Engine::Postgres,
            schema: "public".into(),
            table: "users".into(),
            checked: Arc::new(Checked {
                columns: Vec::new(),
                sources: Vec::new(),
                rows: (0..total)
                    .map(|i| CheckedRow {
                        line: i + 2,
                        values: Vec::new(),
                    })
                    .collect(),
                problems: Vec::new(),
            }),
            chunks: (0..total)
                .step_by(500)
                .map(|start| start..(start + 500).min(total))
                .collect(),
            skip: false,
            begin,
        }
    }

    #[test]
    fn every_outcome_reads_as_one_sentence_about_the_data() {
        let atomic = users(Some("BEGIN"));
        let going = users(None);
        assert_eq!(atomic.lines(8), 4002..=4501);
        let skipping = Plan {
            skip: true,
            ..users(Some("BEGIN"))
        };
        let imported = |rows, affected| Outcome::Imported { rows, affected };
        assert_eq!(
            imported(12_345, None).summary(&atomic),
            "Imported 12,345 rows into `public.users`."
        );
        assert_eq!(
            imported(12_345, Some(12_345)).summary(&skipping),
            "Imported 12,345 rows into `public.users`."
        );
        assert_eq!(
            imported(12_345, Some(12_308)).summary(&skipping),
            "Imported 12,308 rows into `public.users` (37 skipped as duplicates)."
        );
        // A view's INSTEAD OF trigger, or a BEFORE trigger returning NULL.
        assert_eq!(
            imported(500, Some(0)).summary(&atomic),
            "Sent 500 rows to `public.users`; the server reported 0 rows written."
        );
        assert_eq!(
            imported(1, None).summary(&atomic),
            "Imported 1 row into `public.users`."
        );
        assert_eq!(
            Outcome::Cancelled { committed: 0 }.summary(&atomic),
            "Cancelled: nothing was imported."
        );
        assert_eq!(
            Outcome::Cancelled { committed: 4_000 }.summary(&going),
            "Cancelled after 4,000 rows were committed."
        );
        let failed = |chunk, committed| Outcome::Failed {
            message: "boom".into(),
            chunk,
            committed,
        };
        assert_eq!(
            failed(Some(8), 0).summary(&atomic),
            "boom\n\nThe failing statement covered lines 4,002–4,501. Nothing was imported."
        );
        assert_eq!(
            failed(Some(8), 4_000).summary(&going),
            "boom\n\nThe failing statement covered lines 4,002–4,501. 4,000 rows were committed."
        );
        assert_eq!(
            failed(None, 0).summary(&atomic),
            "boom\n\nNothing was imported."
        );
    }

    #[test]
    fn every_submission_passes_the_write_gate_and_the_mode_check() {
        assert_eq!(
            admitted(Engine::Postgres, "DROP TABLE t", Mode::Full, &[]),
            Err("DBDelve refused to run a statement it wrote itself.".into())
        );
        assert_eq!(
            admitted(
                Engine::Postgres,
                "INSERT INTO t (a) VALUES ('1')",
                Mode::ReadOnly,
                &[]
            ),
            Err("Read-only does not allow the import's writes.".into())
        );
        assert_eq!(
            admitted(
                Engine::Postgres,
                "INSERT INTO t (a) VALUES ('1')",
                Mode::ReadWrite,
                &[]
            ),
            Ok(())
        );
        assert_eq!(
            admitted(Engine::Postgres, "ROLLBACK", Mode::ReadOnly, &[]),
            Ok(())
        );
    }

    fn var(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
    }

    fn live(url: &str) -> Connection {
        let mut config = ConnectionConfig::from_url(url).unwrap();
        // The compose servers speak no TLS, as the driver tests say.
        if config.engine() != Engine::SqlServer
            && let Some(server) = config.server_mut()
        {
            server.sslmode = SslMode::Disable;
        }
        Connection::open(config).unwrap()
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_an_import_round_trips_on_postgres() {
        let url = format!(
            "postgresql://{}:{}@{}:{}/{}",
            var("PGUSER"),
            var("PGPASSWORD"),
            var("PGHOST"),
            var("PGPORT"),
            var("PGDATABASE")
        );
        live_round_trip(live(&url), Engine::Postgres, "public");
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_an_import_round_trips_on_mysql() {
        live_round_trip(
            live(&var("dbdelve_MYSQL_URL")),
            Engine::MySql,
            "dbdelve_dev",
        );
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_an_import_round_trips_on_mariadb() {
        live_round_trip(
            live(&var("dbdelve_MARIADB_URL")),
            Engine::MariaDb,
            "dbdelve_dev",
        );
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_an_import_round_trips_on_sqlite() {
        let sqlite = Connection::open(ConnectionConfig::Sqlite {
            path: var("dbdelve_SQLITE_PATH"),
            statement_timeout: 0,
        });
        live_round_trip(sqlite.unwrap(), Engine::Sqlite, "main");
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_an_import_round_trips_on_sql_server() {
        live_round_trip(live(&var("dbdelve_MSSQL_URL")), Engine::SqlServer, "dbo");
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_an_import_round_trips_on_mongodb() {
        live_round_trip(
            live(&var("dbdelve_MONGO_URL")),
            Engine::MongoDb,
            "dbdelve_dev",
        );
    }
}

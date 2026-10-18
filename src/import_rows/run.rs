//! Running a plan: the order its submissions go out in, what each landing
//! means for the rest, and the sentence the import ends on.

use super::Plan;
use crate::db::{CancelToken, Connection, DbError, Engine};
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
        rows: usize,
        skipped: Option<u64>,
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
            1 => "1 row was committed before it.".into(),
            n => format!("{} were committed before it.", rows(n)),
        };
        match self {
            Self::Imported {
                rows: n,
                skipped: Some(skipped @ 1..),
            } => format!(
                "Imported {} into {target} ({} skipped as duplicates).",
                rows(*n),
                group_thousands(*skipped)
            ),
            Self::Imported { rows: n, .. } => format!("Imported {} into {target}.", rows(*n)),
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

    pub(crate) fn landed(&mut self, step: Step, result: Result<Option<u64>, DbError>) {
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
            // A chunk the cancel itself stopped failed for no reason of its own.
            (Step::Chunk(_), Err(_)) if self.cancelled && !self.atomic => {}
            (Step::Begin | Step::Commit, Err(error)) => {
                self.failure.get_or_insert((error.message, None));
            }
            (Step::Chunk(chunk), Err(error)) => {
                self.failure.get_or_insert((error.message, Some(chunk)));
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
        let total = self.inserted();
        let skipped = self
            .affected
            .map(|affected| (total as u64).saturating_sub(affected));
        Outcome::Imported {
            rows: total - skipped.unwrap_or(0) as usize,
            skipped,
        }
    }

    /// Commit as it goes ends on its own landings, with no closing step.
    fn settle(&mut self) {
        if self.atomic {
            return;
        }
        let committed = self.inserted();
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

/// One submission, on the import's own connection from [`Connection::apart`].
pub(crate) fn send(
    connection: &Connection,
    engine: Engine,
    sql: &str,
    mode: Mode,
    confirmed: &[Destructive],
    cancel: &CancelToken,
) -> Result<Option<u64>, DbError> {
    admitted(engine, sql, mode, confirmed).map_err(|message| DbError {
        message,
        position: None,
    })?;
    connection
        .generated(sql, cancel, None)
        .map(|result| result.rows_affected)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::import_rows::{Checked, CheckedRow};

    fn err(m: &str) -> Result<Option<u64>, DbError> {
        Err(DbError {
            message: m.into(),
            position: None,
        })
    }

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
        atomic.landed(
            Step::Chunk(1),
            err("canceling statement due to user request"),
        );
        assert_eq!(atomic.next(), Some(Step::Rollback));
        atomic.landed(Step::Rollback, Ok(None));
        assert_eq!(atomic.outcome(), Some(&Outcome::Cancelled { committed: 0 }));

        let mut going = Run::new(vec![2, 2], false);
        going.landed(Step::Chunk(0), Ok(Some(2)));
        going.cancel();
        assert_eq!(going.next(), None);
        assert_eq!(going.outcome(), Some(&Outcome::Cancelled { committed: 2 }));

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
                skipped: Some(0)
            })
        );
    }

    #[test]
    fn skipped_rows_are_counted_from_the_affected_rows() {
        let mut run = Run::new(vec![2, 2], false);
        run.landed(Step::Chunk(0), Ok(Some(1)));
        run.landed(Step::Chunk(1), Ok(Some(2)));
        assert_eq!(
            run.outcome(),
            Some(&Outcome::Imported {
                rows: 3,
                skipped: Some(1)
            })
        );
        let mut unknown = Run::new(vec![2], false);
        unknown.landed(Step::Chunk(0), Ok(None));
        assert_eq!(
            unknown.outcome(),
            Some(&Outcome::Imported {
                rows: 2,
                skipped: None
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
        let imported = |rows, skipped| Outcome::Imported { rows, skipped };
        assert_eq!(
            imported(12_345, None).summary(&atomic),
            "Imported 12,345 rows into `public.users`."
        );
        assert_eq!(
            imported(12_308, Some(37)).summary(&atomic),
            "Imported 12,308 rows into `public.users` (37 skipped as duplicates)."
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
            "boom\n\nThe failing statement covered lines 4,002–4,501. 4,000 rows were committed before it."
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
}

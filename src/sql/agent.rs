//! The single answer for a statement an agent originates: run it, ask the human
//! to approve it, or refuse it by name.
//!
//! [`agent_gate`] is a pure function of `(engine, sql, mode)`. It reads no
//! profile, no silenced confirmation and no earlier approval, so the mode passed
//! for a call is the ceiling for that call and nothing else, and two callers
//! cannot change each other's answers. It reads the parse [`super::classify`]
//! judges and decides about the text as written; it never returns another text.
//!
//! The ladder, each step returning at once. Steps 1 to 6 do not depend on the
//! mode, so a refusal's reason is the same on every profile.
//!
//! 1. `Unreadable` in the verdict is `Refuse(Unreadable)`, before `gate` is
//!    asked: `gate` answers `Upgrade` for an unreadable statement on an engine
//!    with no server-side hold, which would read as a negotiation. An
//!    unreadable statement that is seen to remove data is named a
//!    `Refuse(Delete)` (D-10): MongoDB's legacy `remove` is no method DBDelve
//!    reads, and it is still a deletion. One seen to disclose (a MongoDB
//!    `db.currentOp()`, a Redis `CONFIG GET` before a command DBDelve cannot
//!    read) is a `Refuse(Disclosure)`; a deletion is asked first.
//! 2. The statement count: the parse `classify` judged and the buffer's token
//!    bearing ranges. None is `Refuse(NoStatement)`, two or more is
//!    `Refuse(MoreThanOne)`.
//! 3. A Redis command the editor never sends (`MONITOR`, `SELECT`, ...) is
//!    `Refuse(NotSent)`, after the count so two commands are `MoreThanOne`.
//! 4. `DROP` is `Refuse(Drop)`, `TRUNCATE` is `Refuse(Truncate)`, any `DELETE`
//!    the walk reached, filtered or not, is `Refuse(Delete)`, and an `ALTER
//!    TABLE` carrying anything but `ADD COLUMN` is `Refuse(Alter)`. On MongoDB
//!    and Redis a command seen to remove data is `Refuse(Delete)`, a Redis
//!    expiry whose timeout is not a plain positive number among them.
//! 5. A server-side script (a Redis `EVAL`, `FCALL` and their read-only forms,
//!    a MongoDB `$where`) is `Refuse(Script)`, and a command that returns
//!    credentials, password hashes, the user's privileges or other clients'
//!    details (Redis `CONFIG GET`, `CLIENT LIST`, `ACL LIST`, `ACL GETUSER`;
//!    MongoDB `currentOp`, `connectionStatus` with `showPrivileges`) is
//!    `Refuse(Disclosure)` (D-27), in every mode: before the session and mode
//!    steps, so the reason is the script's and not the mode's.
//! 6. A statement that changes the session, or assigns a user variable with
//!    `:=`, is `Refuse(Session)`: a later approved write must hit the table the
//!    human read it for, so nothing an agent sends may repoint the session
//!    unseen. Stateless engines included. A plain read of what the session
//!    holds, `currval` or `last_insert_id()`, stays a read where the engine lists
//!    it.
//! 7. A verdict that needs `Mode::Full` is `Refuse(NeedsFull)`: a Full profile
//!    grants an agent nothing a Read-write one does not.
//! 8. `gate(&verdict, mode, &[])` stopping the statement for the mode is
//!    `Refuse(NeedsMode { needed, held })`. The silenced list is always empty,
//!    so a kind a human silenced changes nothing for an agent.
//! 9. A write the profile's mode allows is `Approve(verdict, expiry)`. The
//!    `expiry` is `Some` for a Redis command that schedules a removal:
//!    `EXPIRE`, `PEXPIRE`, `HEXPIRE` or `HPEXPIRE` with a positive timeout;
//!    `EXPIREAT`, `PEXPIREAT`, `HEXPIREAT` or `HPEXPIREAT` at any time; `SET`,
//!    `GETEX`, `HSETEX` or `HGETEX` carrying a positive `EX` or `PX`, or `EXAT`
//!    or `PXAT`; and `SETEX` or `PSETEX` with a positive seconds or
//!    milliseconds argument. Its text is the wording the approval surface
//!    states beside the statement. The gate has no clock, so the text says a
//!    time already past removes at once.
//! 10. A read that calls one of the engine's `egress_functions`, or on DuckDB
//!     holds a string that looks like a path ([`reads_a_bare_path`]), is
//!     `Approve(verdict, None)` with a Read-only verdict, in every mode: it
//!     reaches a file, a URL, a program or another server, and the human sees
//!     the statement first. After every refusal and after step 9, so a refused
//!     statement stays refused and a write that also reads a file is approved
//!     as the write it is. It narrows what the mode lets run and never widens
//!     it.
//! 11. A read that calls a function its engine's `builtin_functions` does not
//!     name is `Approve(verdict, None)` with a Read-only verdict at Read-write
//!     and Full, and `Run`s at Read-only: the call may be a user-defined
//!     function that writes, and at Read-write and Full no engine holds one
//!     back. The name is judged as written, so a schema-qualified or quoted one
//!     is unlisted. It narrows what the mode lets run and never widens it.
//! 12. Anything else is a read, and `Run`s.
//!
//! The mode stays the ceiling: `Run` and `Approve` both imply that `gate` with
//! the same mode and no silenced kind lets the statement through, and the
//! corpus in `sql_snapshots::agent` holds that over every cell.

use std::fmt;

use sqlparser::{
    ast::Statement,
    parser::Parser,
    tokenizer::{Token, Tokenizer},
};

use crate::db::Engine;

use super::{
    Buffer, Destructive, Mode, Stop, Verdict, dialect, executed_tokens, gate, holds_a_token, parsed,
};

/// What an agent's statement is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentVerdict {
    /// A read: nothing is asked of the human.
    Run,
    /// A write the profile's mode allows: it waits for the human, who sees the
    /// exact statement first. The expiry is what the statement schedules
    /// beyond itself, where it schedules one, for the approval to state
    /// beside the statement.
    Approve(Verdict, Option<Expiry>),
    /// Not allowed on any profile, or not on this one.
    Refuse(Refusal),
}

/// What an approved Redis write schedules beyond the statement itself: a
/// removal when a time passes. The gate has no clock, so it cannot tell a time
/// still to come from one already past, which removes at once; the text says
/// both, and is what the approval states beside the statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expiry {
    /// The key.
    Key,
    /// The named fields of a hash.
    Field,
}

impl fmt::Display for Expiry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expiry::Key => f.write_str(
                "Sets an expiry: the key is removed when the time passes. A time already past removes it at once.",
            ),
            Expiry::Field => f.write_str(
                "Sets an expiry: the named fields are removed when the time passes. A time already past removes them at once.",
            ),
        }
    }
}

/// Why a statement was refused. One variant per reason, so a refusal is
/// diagnosable by what it says; the text is what an agent reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The text holds no statement.
    NoStatement,
    /// The text holds two or more.
    MoreThanOne,
    /// DBDelve could not read the statement, so it cannot say what it does.
    Unreadable,
    /// A Redis command DBDelve never sends: it would take the session out from
    /// under the connection, or end it.
    NotSent,
    Drop,
    Truncate,
    /// Any `DELETE`, whatever its shape, and on MongoDB and Redis any command
    /// that removes data.
    Delete,
    /// An `ALTER TABLE` carrying anything but `ADD COLUMN`.
    Alter,
    /// A statement that leaves the session changed, savepoints included.
    Session,
    /// A server-side script, which no statement read can say the effect of.
    Script,
    /// A command that returns credentials, password hashes, the user's
    /// privileges or other clients' connection details.
    Disclosure,
    /// A statement that needs Full.
    NeedsFull,
    /// A statement that needs more than the profile's mode.
    NeedsMode {
        needed: Mode,
        held: Mode,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NoStatement => f.write_str("there is no statement to run"),
            Refusal::MoreThanOne => f.write_str("more than one statement"),
            Refusal::Unreadable => f.write_str("DBDelve can't read this statement"),
            Refusal::NotSent => f.write_str("DBDelve never sends this command"),
            Refusal::Drop => f.write_str("DROP is refused for agent statements"),
            Refusal::Truncate => f.write_str("TRUNCATE is refused for agent statements"),
            Refusal::Delete => f.write_str("deleting data is refused for agent statements"),
            Refusal::Alter => {
                f.write_str("ALTER other than ADD COLUMN is refused for agent statements")
            }
            Refusal::Session => {
                f.write_str("statements that change the session are refused for agent statements")
            }
            Refusal::Script => {
                f.write_str("server-side scripts are refused for agent statements")
            }
            Refusal::Disclosure => f.write_str(
                "commands that return credentials or other clients' details are refused for agent statements",
            ),
            Refusal::NeedsFull => {
                f.write_str("statements that need Full are refused for agent statements")
            }
            Refusal::NeedsMode { needed, held } => {
                write!(
                    f,
                    "needs {}; the profile is {}",
                    needed.label(),
                    held.label()
                )
            }
        }
    }
}

/// The answer for `sql` as an agent originates it on a profile whose mode is
/// `mode`. See the module header for the order the reasons are asked in.
pub(crate) fn agent_gate(engine: Engine, sql: &str, mode: Mode) -> AgentVerdict {
    let read = parsed(engine, sql);
    let verdict = read.verdict;

    if verdict.destructive.contains(&Destructive::Unreadable) {
        // Named a deletion or a disclosure where it was seen to be one (D-10).
        // On SQL both flags come only from statements the parser read, so this
        // is never reached with either set.
        return AgentVerdict::Refuse(if read.removes {
            Refusal::Delete
        } else if read.discloses {
            Refusal::Disclosure
        } else {
            Refusal::Unreadable
        });
    }

    match statement_count(engine, sql, read.statements) {
        0 => return AgentVerdict::Refuse(Refusal::NoStatement),
        1 => {}
        _ => return AgentVerdict::Refuse(Refusal::MoreThanOne),
    }

    // `classify` rates a command that is never sent a read, so only the parse
    // tells it from one that is.
    if read.unsent {
        return AgentVerdict::Refuse(Refusal::NotSent);
    }

    for (kind, refusal) in [
        (Destructive::Drop, Refusal::Drop),
        (Destructive::Truncate, Refusal::Truncate),
    ] {
        if verdict.destructive.contains(&kind) {
            return AgentVerdict::Refuse(refusal);
        }
    }
    // A filtered `DELETE` is a plain write to the verdict, so only the walk
    // that read it knows it removed rows; an unfiltered one carries its kind
    // as well.
    if read.removes || verdict.destructive.contains(&Destructive::UnfilteredDelete) {
        return AgentVerdict::Refuse(Refusal::Delete);
    }
    if read.alters {
        return AgentVerdict::Refuse(Refusal::Alter);
    }

    if read.scripts {
        return AgentVerdict::Refuse(Refusal::Script);
    }
    if read.discloses {
        return AgentVerdict::Refuse(Refusal::Disclosure);
    }

    // Judged by the statements' own kinds and by the tokens, never by
    // `leaves`: that answers nothing on an engine that keeps no session, and
    // it flags a plain read of session state, which a read is.
    if read.session || assigns_a_user_variable(engine, sql) {
        return AgentVerdict::Refuse(Refusal::Session);
    }

    if verdict.mode == Mode::Full {
        return AgentVerdict::Refuse(Refusal::NeedsFull);
    }

    // Never a silenced kind: an agent has no confirmations of its own and
    // borrows none of a human's.
    match gate(&verdict, mode, &[]) {
        Some(Stop::Upgrade(needed)) => {
            return AgentVerdict::Refuse(Refusal::NeedsMode { needed, held: mode });
        }
        // Steps 1 and 4 took every kind these answer for, so none is reached;
        // each is refused as what it names rather than panicking.
        Some(Stop::Confirm(Destructive::Drop)) => return AgentVerdict::Refuse(Refusal::Drop),
        Some(Stop::Confirm(Destructive::Truncate)) => {
            return AgentVerdict::Refuse(Refusal::Truncate);
        }
        Some(Stop::Confirm(Destructive::UnfilteredDelete)) => {
            return AgentVerdict::Refuse(Refusal::Delete);
        }
        Some(Stop::Confirm(Destructive::Unreadable) | Stop::RunOnce) => {
            return AgentVerdict::Refuse(Refusal::Unreadable);
        }
        None => {}
    }

    if verdict.mode == Mode::ReadWrite {
        // ponytail: `REPLACE INTO` and `INSERT OR REPLACE` delete a conflicting
        // row before they insert, and classify as the `INSERT` they parse as,
        // so they are approved like one. The human reads the statement before
        // approving it (D-23). The upgrade path is a removal flag on a
        // replacing insert in the walk, which would refuse them as `Delete`.
        return AgentVerdict::Approve(verdict, read.expiry);
    }
    // The egress list, read by the same scan as the deny-list but never by the
    // human path, so DBDelve's own file opens stay reads, and DuckDB's quoted
    // paths, which no function name marks, ask in every mode. A call the
    // engine's built-in list does not name asks at Read-write and Full, where
    // no engine holds; at Read-only the hold, where there is one, stops a write
    // and a read runs.
    if super::calls_listed(engine, sql, engine.spec().sql.egress_functions, &[])
        || reads_a_bare_path(engine, sql)
        || (mode >= Mode::ReadWrite && read.unlisted_call)
    {
        return AgentVerdict::Approve(verdict, None);
    }
    AgentVerdict::Run
}

/// What an approvable statement does, which is what the approval says first.
/// The three kinds are gate steps 9, 10 and 11, one to one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApprovalKind {
    /// It writes (step 9).
    Write,
    /// It reads a file, a URL, a program or another server (step 10).
    EgressRead,
    /// It calls a function its engine does not list, which may write (step 11).
    UnlistedCall,
}

/// The kind of a statement [`agent_gate`] approved with `verdict`. It asks the
/// predicates step 10 asks rather than restating them, so what the dialog says
/// cannot contradict the gate: a write is the step-9 verdict at Read-write, and
/// of the reads the gate approves the ones that are not egress are the ones
/// that call an unlisted function.
pub(crate) fn approval_kind(engine: Engine, sql: &str, verdict: &Verdict) -> ApprovalKind {
    if verdict.mode == Mode::ReadWrite {
        ApprovalKind::Write
    } else if super::calls_listed(engine, sql, engine.spec().sql.egress_functions, &[])
        || reads_a_bare_path(engine, sql)
    {
        ApprovalKind::EgressRead
    } else {
        ApprovalKind::UnlistedCall
    }
}

/// The statements `sql` parses to on `engine`, or none where it is not SQL or
/// does not parse. The typed parse decides, as the gate's does, so a word in a
/// comment or a string literal cannot stand in for the statement.
fn typed_statements(engine: Engine, sql: &str) -> Vec<Statement> {
    dialect(engine)
        .and_then(|dialect| Parser::parse_sql(dialect.as_ref(), sql).ok())
        .unwrap_or_default()
}

/// Whether `sql` is a Postgres `CREATE INDEX CONCURRENTLY`. Only Postgres
/// leaves an invalid index behind when a concurrent build stops, which is what
/// the answer to a stopped write tells the agent about.
pub(crate) fn creates_index_concurrently(engine: Engine, sql: &str) -> bool {
    engine == Engine::Postgres
        && typed_statements(engine, sql).iter().any(
            |statement| matches!(statement, Statement::CreateIndex(index) if index.concurrently),
        )
}

/// Whether the server's row count for `sql` is the rows it changed. A count of
/// zero is also what a command with no row count reports (see
/// [`crate::db::QueryResult::rows_affected`]), so it means "no rows" only for
/// these two statements. `DELETE` and `MERGE` would count too, and are not
/// listed because [`agent_gate`] approves neither: every `DELETE` is refused,
/// and a `MERGE` needs Full.
pub(crate) fn counts_rows(engine: Engine, sql: &str) -> bool {
    matches!(
        typed_statements(engine, sql).as_slice(),
        [Statement::Insert(_) | Statement::Update(_)]
    )
}

/// Whether `sql`, on an engine that reads a quoted string in a table position
/// as a file or a URL (`SELECT * FROM 'data.csv'`), holds a string literal or a
/// `"`-quoted word with a `.`, `/` or `\` in it. False on every other engine.
/// A text that does not tokenize answers true: it cannot be shown to read no
/// path. `:` is not a trigger, so a timestamp literal does not ask.
///
/// ponytail: any dotted literal asks, wherever it sits, so `WHERE v = '1.2'`
/// on DuckDB is approved though it reads no file; the corpus records it. This
/// is a superset of the positions DuckDB reads a path from (`FROM`, `JOIN`, a
/// comma join, a subquery anywhere), which is the safe direction. The upgrade
/// path is a table-position reading over every table factor of the parse,
/// which is a second reading of DuckDB's grammar kept in step by hand.
fn reads_a_bare_path(engine: Engine, sql: &str) -> bool {
    if !engine.spec().sql.bare_path_reads {
        return false;
    }
    let Some(dialect) = dialect(engine) else {
        return false;
    };
    let Ok(tokens) = Tokenizer::new(dialect.as_ref(), sql).tokenize() else {
        return true;
    };
    tokens.iter().any(|token| {
        let text = match token {
            Token::SingleQuotedString(text)
            | Token::DoubleQuotedString(text)
            | Token::TripleSingleQuotedString(text)
            | Token::TripleDoubleQuotedString(text)
            | Token::EscapedStringLiteral(text)
            | Token::UnicodeStringLiteral(text)
            | Token::NationalStringLiteral(text) => text,
            Token::DollarQuotedString(quoted) => &quoted.value,
            Token::Word(word) if word.quote_style == Some('"') => &word.value,
            _ => return false,
        };
        text.contains(['.', '/', '\\'])
    })
}

/// Whether `sql` assigns a user variable with `:=` (`SELECT @x := 1`), the
/// same session change as `SET @x = 1` in another spelling. Read off the
/// tokens the server will run, so one inside an executable comment counts.
/// The variable is an `@`-prefixed name, however the dialect's tokenizer
/// spells it; a named argument (`f(a := 1)`) has no `@` and is not one.
fn assigns_a_user_variable(engine: Engine, sql: &str) -> bool {
    let Some(dialect) = dialect(engine) else {
        return false;
    };
    let tokens = executed_tokens(engine, dialect.as_ref(), sql)
        .unwrap_or_else(|| Tokenizer::new(dialect.as_ref(), sql).tokenize());
    // A submission the tokenizer cannot read was refused as unreadable before
    // this is asked.
    let Ok(tokens) = tokens else {
        return false;
    };
    let tokens: Vec<&Token> = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect();
    tokens.windows(2).any(|pair| {
        matches!(pair, [Token::Word(variable), Token::Assignment] if variable.value.starts_with('@'))
    }) || tokens
        .windows(3)
        .any(|triple| matches!(triple, [Token::AtSign, Token::Word(_), Token::Assignment]))
}

/// How many statements `sql` holds, as the more of the two readings of it.
///
/// Both have to count one. `typed` is the count of the parse `classify`
/// judged, MariaDB's executable comments expanded: it reads `BEGIN; ...;
/// COMMIT` as three statements where the grammar reads one transaction block,
/// and finds a statement a comment carries. The grammar's ranges are counted
/// where the dialect's own lexer finds a real token in them, which leaves a
/// trailing comment, MySQL's `#` among them, uncounted. A statement wholly
/// inside an executable comment is one to `typed` and none to the lexer, and
/// is counted as that one.
fn statement_count(engine: Engine, sql: &str, typed: usize) -> usize {
    let Some(dialect) = dialect(engine) else {
        return typed;
    };
    let ranges = Buffer::for_engine(engine, sql)
        .statements()
        .iter()
        .filter(|range| holds_a_token(dialect.as_ref(), &sql[(*range).clone()]))
        .count();
    if typed == 0 { 0 } else { typed.max(ranges) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Syntax;
    use crate::sql::classify;

    fn sql_engines() -> impl Iterator<Item = Engine> {
        Engine::ALL
            .into_iter()
            .filter(|engine| engine.syntax() == Syntax::Sql)
    }

    fn refused(refusal: Refusal) -> AgentVerdict {
        AgentVerdict::Refuse(refusal)
    }

    fn needs_read_write() -> AgentVerdict {
        refused(Refusal::NeedsMode {
            needed: Mode::ReadWrite,
            held: Mode::ReadOnly,
        })
    }

    fn redis(text: &str, mode: Mode) -> AgentVerdict {
        agent_gate(Engine::Redis, text, mode)
    }

    /// `text` answers `expected` in every mode.
    fn redis_always(text: &str, expected: AgentVerdict) {
        for mode in Mode::ALL {
            assert_eq!(redis(text, mode), expected, "{mode:?}: {text}");
        }
    }

    fn postgres(text: &str, mode: Mode) -> AgentVerdict {
        agent_gate(Engine::Postgres, text, mode)
    }

    /// `text` answers `expected` in every mode on `engine`.
    fn always(engine: Engine, text: &str, expected: AgentVerdict) {
        for mode in Mode::ALL {
            assert_eq!(
                agent_gate(engine, text, mode),
                expected,
                "{engine:?} {mode:?}: {text}"
            );
        }
    }

    /// Each approvable statement the dialog has to name, with the kind it is.
    const APPROVABLE: &[(Engine, Mode, &str, ApprovalKind)] = &[
        (
            Engine::Postgres,
            Mode::ReadWrite,
            "UPDATE t SET a = 1",
            ApprovalKind::Write,
        ),
        (
            Engine::Sqlite,
            Mode::ReadWrite,
            "INSERT INTO t VALUES (1)",
            ApprovalKind::Write,
        ),
        (
            Engine::Postgres,
            Mode::ReadOnly,
            "SELECT * FROM pg_read_file('x')",
            ApprovalKind::EgressRead,
        ),
        (
            Engine::MySql,
            Mode::ReadOnly,
            "SELECT * FROM load_file('x')",
            ApprovalKind::EgressRead,
        ),
        (
            Engine::DuckDb,
            Mode::ReadOnly,
            "SELECT * FROM 'data.csv'",
            ApprovalKind::EgressRead,
        ),
        (
            Engine::DuckDb,
            Mode::ReadOnly,
            "SELECT * FROM glob('x')",
            ApprovalKind::EgressRead,
        ),
        (
            Engine::Postgres,
            Mode::ReadWrite,
            "SELECT my_udf(a) FROM t",
            ApprovalKind::UnlistedCall,
        ),
        (
            Engine::Sqlite,
            Mode::ReadWrite,
            "SELECT my_udf(1)",
            ApprovalKind::UnlistedCall,
        ),
    ];

    #[test]
    fn approval_kind_names_what_each_approvable_statement_does() {
        for &(engine, mode, sql, kind) in APPROVABLE {
            let AgentVerdict::Approve(verdict, _) = agent_gate(engine, sql, mode) else {
                panic!("{engine:?} {mode:?} {sql}: the gate does not approve it");
            };
            assert_eq!(
                approval_kind(engine, sql, &verdict),
                kind,
                "{engine:?} {mode:?}: {sql}"
            );
        }
    }

    #[test]
    fn approval_kind_never_contradicts_the_gate() {
        let corpus = APPROVABLE
            .iter()
            .map(|&(_, _, sql, _)| sql)
            .chain(["CREATE INDEX i ON t (a)"]);
        for sql in corpus {
            for engine in Engine::ALL {
                for mode in Mode::ALL {
                    if let AgentVerdict::Approve(verdict, _) = agent_gate(engine, sql, mode) {
                        assert_eq!(
                            approval_kind(engine, sql, &verdict) == ApprovalKind::Write,
                            verdict.mode == Mode::ReadWrite,
                            "{engine:?} {mode:?}: {sql}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn creates_index_concurrently_reads_the_parse() {
        for sql in [
            "CREATE INDEX CONCURRENTLY i ON t (a)",
            "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS i ON t (a)",
        ] {
            assert!(creates_index_concurrently(Engine::Postgres, sql), "{sql}");
        }
        for sql in [
            "CREATE INDEX i ON t (a)",
            "/* CONCURRENTLY */ CREATE INDEX i ON t (a)",
            "SELECT 'CREATE INDEX CONCURRENTLY i ON t (a)'",
            "CREATE INDEX CONCURRENTLY (",
        ] {
            assert!(!creates_index_concurrently(Engine::Postgres, sql), "{sql}");
        }
        for (engine, sql) in [
            (Engine::MySql, "CREATE INDEX CONCURRENTLY i ON t (a)"),
            (Engine::Sqlite, "CREATE INDEX CONCURRENTLY i ON t (a)"),
            (Engine::MongoDb, "db.t.createIndex({a: 1})"),
            (Engine::Redis, "SET k v"),
        ] {
            assert!(
                !creates_index_concurrently(engine, sql),
                "{engine:?}: {sql}"
            );
        }
    }

    #[test]
    fn counts_rows_only_for_statements_that_change_rows() {
        for engine in [
            Engine::Postgres,
            Engine::MySql,
            Engine::Sqlite,
            Engine::DuckDb,
        ] {
            for sql in ["INSERT INTO t VALUES (1)", "UPDATE t SET a = 1"] {
                assert!(counts_rows(engine, sql), "{engine:?}: {sql}");
            }
        }
        // Not counted because no agent can run them: every `DELETE` is refused,
        // and a `MERGE` needs Full, which an agent is refused too.
        for engine in Engine::ALL {
            for mode in Mode::ALL {
                for sql in [
                    "DELETE FROM t WHERE a = 1",
                    "MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN DELETE",
                ] {
                    assert!(
                        matches!(agent_gate(engine, sql, mode), AgentVerdict::Refuse(_)),
                        "{engine:?} {mode:?}: {sql}"
                    );
                }
            }
        }
        for (engine, sql) in [
            (Engine::Postgres, "CREATE INDEX i ON t (a)"),
            (Engine::Postgres, "CREATE TABLE u (a int)"),
            (Engine::Postgres, "ALTER TABLE t ADD COLUMN b int"),
            (Engine::Postgres, "SELECT 1"),
            (Engine::Postgres, "UPDATE ("),
            (Engine::MongoDb, "db.t.insertOne({a: 1})"),
            (Engine::Redis, "SET k v"),
        ] {
            assert!(!counts_rows(engine, sql), "{engine:?}: {sql}");
        }
    }

    #[test]
    fn a_listed_egress_call_asks_first_in_every_mode() {
        for text in [
            "SELECT pg_read_file('x')",
            "SELECT * FROM pg_ls_dir('.')",
            "SELECT * FROM pg_logdir_ls()",
            "SELECT pg_catalog.pg_read_file('x')",
            "SELECT a FROM t WHERE b IN (SELECT pg_read_file('x'))",
        ] {
            always(
                Engine::Postgres,
                text,
                AgentVerdict::Approve(Verdict::READ, None),
            );
        }
        for (engine, text) in [
            (Engine::MySql, "SELECT load_file('/etc/hosts')"),
            (Engine::MariaDb, "SELECT load_file('/etc/hosts')"),
            (Engine::DuckDb, "SELECT * FROM read_csv('x.csv')"),
            (Engine::DuckDb, "SELECT * FROM read_xlsx('x.xlsx')"),
            (Engine::ClickHouse, "SELECT * FROM url('http://h/x', CSV)"),
            (Engine::ClickHouse, "SELECT * FROM file('x.csv')"),
            (Engine::ClickHouse, "SELECT * FROM remote('h', db.t)"),
            (
                Engine::ClickHouse,
                "SELECT * FROM executable('s', CSV, 'a String')",
            ),
            (Engine::ClickHouse, "SELECT * FROM eval('SELECT 1')"),
        ] {
            always(engine, text, AgentVerdict::Approve(Verdict::READ, None));
        }
    }

    #[test]
    fn the_human_path_still_reads_an_egress_call_as_a_read() {
        for (engine, text) in [
            (Engine::Postgres, "SELECT pg_read_file('x')"),
            (Engine::Postgres, "SELECT * FROM pg_logdir_ls()"),
            (Engine::DuckDb, "SELECT * FROM read_csv('x.csv')"),
            (Engine::DuckDb, "SELECT * FROM 'x.csv'"),
            (Engine::ClickHouse, "SELECT * FROM url('http://h/x', CSV)"),
        ] {
            assert_eq!(classify(engine, text), Verdict::READ, "{engine:?}: {text}");
        }
    }

    /// The three writers answer `NeedsFull` because `HOLD_IGNORING_CALLS` also
    /// holds them at Full; `pg_file_sync` is only listed, so it is approvable.
    #[test]
    fn the_adminpack_file_functions_never_run_for_an_agent() {
        let sync = "SELECT pg_file_sync('x')";
        assert_eq!(classify(Engine::Postgres, sync).mode, Mode::ReadWrite);
        assert_eq!(postgres(sync, Mode::ReadOnly), needs_read_write());
        for mode in [Mode::ReadWrite, Mode::Full] {
            assert_eq!(
                postgres(sync, mode),
                AgentVerdict::Approve(Verdict::WRITE, None),
                "{mode:?}"
            );
        }
        for text in [
            "SELECT pg_file_write('x', 'abc', false)",
            "SELECT pg_file_rename('x', 'y')",
            "SELECT pg_file_unlink('x')",
        ] {
            assert_eq!(classify(Engine::Postgres, text).mode, Mode::Full, "{text}");
            always(Engine::Postgres, text, refused(Refusal::NeedsFull));
        }
    }

    #[test]
    fn a_quoted_path_in_any_duckdb_table_position_asks_first() {
        for text in [
            "SELECT * FROM 'data.csv'",
            "SELECT * FROM \"data.csv\"",
            "SELECT * FROM 'https://example.com/x.parquet'",
            "SELECT * FROM 'a.csv' a, 'b.csv' b",
            "SELECT * FROM t JOIN 'b.csv' b ON t.id = b.id",
            "SELECT * FROM t WHERE id IN (SELECT id FROM 'b.csv')",
            // The accepted false positive, recorded in the corpus.
            "SELECT * FROM t WHERE v = '1.2'",
        ] {
            always(
                Engine::DuckDb,
                text,
                AgentVerdict::Approve(Verdict::READ, None),
            );
        }
        for text in [
            "SELECT * FROM t WHERE ts = '2024-01-02 03:04:05'",
            "SELECT lower(a) FROM t",
        ] {
            always(Engine::DuckDb, text, AgentVerdict::Run);
        }
        // No other engine reads a path out of a table position.
        always(
            Engine::Postgres,
            "SELECT * FROM t WHERE v = 'data.csv'",
            AgentVerdict::Run,
        );
    }

    #[test]
    fn an_unlisted_function_asks_at_read_write_and_full() {
        // A listed built-in runs wherever it stands, and a column named like a
        // file function is no call.
        for text in [
            "SELECT lower(a) FROM t",
            "SELECT t.pg_read_file FROM t",
            "SELECT pg_read_file FROM t",
            // Field notation calls a one-argument function without a
            // parenthesis, and telling it from a column needs the catalog, so
            // it is an accepted residual.
            "SELECT t.f FROM t",
        ] {
            always(Engine::Postgres, text, AgentVerdict::Run);
        }
        for text in [
            "SELECT my_udf(a) FROM t",
            "SELECT * FROM my_tf()",
            "SELECT a FROM t WHERE my_udf(a) = 1",
            "EXPLAIN SELECT my_udf(a) FROM t",
            "SELECT pg_catalog.lower(a) FROM t",
            "SELECT \"lower\"(a) FROM t",
        ] {
            assert_eq!(postgres(text, Mode::ReadOnly), AgentVerdict::Run, "{text}");
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert_eq!(
                    postgres(text, mode),
                    AgentVerdict::Approve(Verdict::READ, None),
                    "{mode:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn clickhouse_matches_a_function_name_as_spelled() {
        let clickhouse = &Engine::ClickHouse.spec().sql;
        assert!(clickhouse.case_sensitive_functions);
        for engine in sql_engines().filter(|engine| *engine != Engine::ClickHouse) {
            assert!(!engine.spec().sql.case_sensitive_functions, "{engine:?}");
        }
        // A user `todate` can sit beside the built-in `toDate`, so a
        // differently cased spelling is a different function.
        let listed: Vec<&str> = clickhouse.builtin_functions.lines().collect();
        for name in ["merge", "cluster", "clusterAllReplicas", "dictionary"] {
            assert!(!listed.contains(&name), "{name}");
        }
        let ask = |text: &str, mode| agent_gate(Engine::ClickHouse, text, mode);
        for mode in [Mode::ReadWrite, Mode::Full] {
            assert_eq!(
                ask("SELECT todate(a) FROM t", mode),
                AgentVerdict::Approve(Verdict::READ, None),
                "{mode:?}"
            );
        }
        assert_eq!(
            ask("SELECT todate(a) FROM t", Mode::ReadOnly),
            AgentVerdict::Run
        );
        for text in [
            "SELECT toDate(a) FROM t",
            "SELECT lower(a) FROM t",
            "SELECT LOWER(a) FROM t",
            "SELECT * FROM numbers(10)",
        ] {
            always(Engine::ClickHouse, text, AgentVerdict::Run);
        }
    }

    #[test]
    fn egress_comes_after_every_refusal_and_the_write_approval() {
        assert_eq!(
            postgres("INSERT INTO t SELECT pg_read_file('x')", Mode::ReadOnly),
            needs_read_write()
        );
        assert_eq!(
            postgres("INSERT INTO t SELECT pg_read_file('x')", Mode::ReadWrite),
            AgentVerdict::Approve(Verdict::WRITE, None)
        );
        for text in [
            "SELECT nextval('s'), pg_read_file('x')",
            "SELECT pg_read_file('x'), nextval('s')",
        ] {
            assert_eq!(postgres(text, Mode::ReadOnly), needs_read_write(), "{text}");
            assert_eq!(
                postgres(text, Mode::ReadWrite),
                AgentVerdict::Approve(Verdict::WRITE, None),
                "{text}"
            );
        }
        for text in [
            "SELECT lower(a), pg_read_file('x') FROM t",
            "SELECT pg_read_file('x'), lower(a) FROM t",
        ] {
            always(
                Engine::Postgres,
                text,
                AgentVerdict::Approve(Verdict::READ, None),
            );
        }
        for mode in Mode::ALL {
            assert_eq!(
                postgres("DROP TABLE t; SELECT pg_read_file('x')", mode),
                refused(Refusal::MoreThanOne),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn every_function_list_is_lower_case() {
        for engine in Engine::ALL {
            let spec = &engine.spec().sql;
            for name in spec.denied_functions.iter().chain(spec.egress_functions) {
                assert_eq!(*name, name.to_lowercase(), "{engine:?}");
            }
            if !spec.case_sensitive_functions {
                for name in listed_names(engine) {
                    assert_eq!(name, name.to_lowercase(), "{engine:?}");
                }
            }
        }
    }

    /// The names of `engine`'s built-in list: every line that is not a comment.
    fn listed_names(engine: Engine) -> impl Iterator<Item = &'static str> {
        engine
            .spec()
            .sql
            .builtin_functions
            .lines()
            .filter(|line| !line.starts_with('#'))
    }

    #[test]
    fn no_built_in_list_names_a_denied_or_egress_function() {
        for engine in sql_engines() {
            let names: Vec<&str> = listed_names(engine).collect();
            assert!(names.contains(&"lower"), "{engine:?}");
            let spec = &engine.spec().sql;
            for name in &names {
                assert!(
                    !name.is_empty() && *name == name.trim(),
                    "{engine:?}: {name:?} is not a bare name"
                );
                let lower = name.to_lowercase();
                assert!(
                    !spec.denied_functions.contains(&lower.as_str())
                        && !spec.egress_functions.contains(&lower.as_str()),
                    "{engine:?}: {name} is on the built-in list and on a deny or egress list"
                );
            }
        }
        for engine in Engine::ALL
            .into_iter()
            .filter(|e| e.syntax() != Syntax::Sql)
        {
            assert_eq!(listed_names(engine).count(), 0, "{engine:?}");
        }
        assert_eq!(
            Engine::Turso.spec().sql.builtin_functions,
            Engine::Sqlite.spec().sql.builtin_functions
        );
    }

    /// The shapes of a call to a function the engine does not list, in every
    /// clause the visitor has to reach, with a qualified and a quoted name.
    /// Some do not parse on every dialect; those are refused, which is not `Run`
    /// either.
    const UNLISTED_CALLS: [&str; 22] = [
        "SELECT delvetest.my_udf()",
        "SELECT my_udf()",
        "SELECT * FROM delvetest.my_udf()",
        "SELECT 1 WHERE delvetest.my_udf() = 1",
        "VALUES (delvetest.my_udf())",
        "SELECT * FROM TABLE(DELVETEST.MY_TF())",
        "SELECT * FROM TABLE(DELVETEST.MY_TF()) AS T",
        "SELECT COUNT(*) FROM TABLE(DELVETEST.MY_TF()) x WHERE 1=0",
        "WITH t AS (SELECT * FROM TABLE(DELVETEST.MY_TF()) x) SELECT * FROM t",
        "SET DELVETEST.VV = (SELECT COUNT(*) FROM TABLE(DELVETEST.MY_TF()) x)",
        "SELECT myschema.upper(x) FROM t",
        "SELECT \"UPPER\"(x) FROM t",
        "SELECT count(my_udf()) FROM t",
        "SELECT 1 FROM t ORDER BY my_udf()",
        "SELECT 1 FROM t GROUP BY my_udf()",
        "SELECT * FROM t JOIN u ON my_udf() = 1",
        "SELECT CAST(my_udf() AS INT)",
        "SELECT CASE WHEN my_udf() = 1 THEN 1 END",
        "SELECT * FROM t WHERE x IN (SELECT my_udf())",
        "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.a = my_udf())",
        "SELECT 1 UNION SELECT my_udf()",
        "EXPLAIN ANALYZE SELECT my_udf()",
    ];

    #[test]
    fn every_sql_engine_asks_before_a_call_it_does_not_list() {
        for engine in sql_engines() {
            for text in UNLISTED_CALLS {
                for mode in [Mode::ReadWrite, Mode::Full] {
                    assert_ne!(
                        agent_gate(engine, text, mode),
                        AgentVerdict::Run,
                        "{engine:?} {mode:?}: {text}"
                    );
                }
            }
            for text in [
                "SELECT my_udf(a) FROM t",
                "SELECT a FROM t WHERE my_udf(a) = 1",
                "SELECT a FROM t ORDER BY my_udf(a)",
            ] {
                // Generic's Read-only already reads the call as a write, so it
                // is refused there rather than run.
                if engine == Engine::Generic {
                    for mode in Mode::ALL {
                        assert_ne!(agent_gate(engine, text, mode), AgentVerdict::Run);
                    }
                    continue;
                }
                assert_eq!(
                    agent_gate(engine, text, Mode::ReadOnly),
                    AgentVerdict::Run,
                    "{engine:?}: {text}"
                );
                for mode in [Mode::ReadWrite, Mode::Full] {
                    assert_eq!(
                        agent_gate(engine, text, mode),
                        AgentVerdict::Approve(Verdict::READ, None),
                        "{engine:?} {mode:?}: {text}"
                    );
                }
            }
        }
        // The visitor reads the expanded tokens of a MySQL executable comment.
        for engine in [Engine::MySql, Engine::MariaDb] {
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert_ne!(
                    agent_gate(engine, "SELECT 1 /*!, my_udf() */", mode),
                    AgentVerdict::Run,
                    "{engine:?} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn redis_commands_the_editor_never_sends_are_refused() {
        for text in [
            "SELECT 1",
            "select 1",
            "SUBSCRIBE n",
            "PSUBSCRIBE n*",
            "SSUBSCRIBE s",
            "MONITOR",
            "SYNC",
            "PSYNC ? -1",
            "HELLO 2",
            "RESET",
            "QUIT",
            "CLIENT REPLY OFF",
            "client reply off",
        ] {
            redis_always(text, refused(Refusal::NotSent));
        }
    }

    #[test]
    fn every_redis_removal_is_refused_as_a_delete() {
        for text in [
            "DEL k",
            "UNLINK k",
            "GETDEL k",
            "DELEX k",
            "HDEL h f",
            "HGETDEL h FIELDS 1 f",
            "LPOP l",
            "RPOP l",
            "BLPOP l 1",
            "BRPOP l 1",
            "LMPOP 1 l LEFT",
            "BLMPOP 1 1 l LEFT",
            "LREM l 0 v",
            "LTRIM l 0 0",
            "SREM s m",
            "SPOP s",
            "ZREM z m",
            "ZPOPMIN z",
            "ZPOPMAX z",
            "BZPOPMIN z 1",
            "BZPOPMAX z 1",
            "ZMPOP 1 z MIN",
            "BZMPOP 1 1 z MIN",
            "ZREMRANGEBYRANK z 0 1",
            "ZREMRANGEBYSCORE z 0 1",
            "ZREMRANGEBYLEX z - +",
            "XDEL s 1-1",
            "XTRIM s MAXLEN 0",
            "XACKDEL s g IDS 1 1-1",
            "XDELEX s IDS 1 1-1",
            "JSON.DEL d $",
            "JSON.FORGET d $",
            "JSON.CLEAR d $",
            "JSON.ARRPOP d $",
            "JSON.ARRTRIM d $ 0 1",
            "TS.DEL t 0 1",
            "TS.DELETERULE a b",
            "CF.DEL c i",
            "FT.ALIASDEL a",
            "FT.DICTDEL d t",
            "FT.SUGDEL k s",
            "VREM v e",
            "TDIGEST.RESET t",
            "XGROUP DESTROY s g",
            "XGROUP DELCONSUMER s g c",
            "MIGRATE h 6379 k 0 1000",
            "EXPIRE k 0",
            "EXPIRE k -1",
            "EXPIRE k 010",
            "PEXPIRE k 0",
            "HEXPIRE k 0 FIELDS 1 f",
            "HPEXPIRE k 0 FIELDS 1 f",
            "del k",
            "XADD s MAXLEN 0 * f v",
            "XADD s MINID 9999999999999-0 * f v",
            "xadd s maxlen ~ 0 limit 10 * f v",
            "HSETEX h EX 0 FIELDS 1 f v",
            "HGETEX h PX 0 FIELDS 1 f",
        ] {
            redis_always(text, refused(Refusal::Delete));
        }
    }

    #[test]
    fn redis_scripts_are_refused_including_the_read_only_forms() {
        for text in [
            "EVAL \"return 1\" 0",
            "EVALSHA abc 0",
            "FCALL f 0",
            "EVAL_RO \"return 1\" 0",
            "EVALSHA_RO abc 0",
            "FCALL_RO f 0",
            "SCRIPT LOAD \"return 1\"",
            "FUNCTION LOAD \"#!lua name=x\"",
        ] {
            redis_always(text, refused(Refusal::Script));
        }
    }

    #[test]
    fn a_redis_transaction_or_two_commands_is_refused() {
        redis_always("MULTI\nSET k v\nEXEC", refused(Refusal::MoreThanOne));
        redis_always("GET a\nGET b", refused(Refusal::MoreThanOne));
        redis_always("MONITOR\nGET a", refused(Refusal::MoreThanOne));
        for text in [
            "MULTI", "EXEC", "DISCARD", "WATCH k", "AUTH u p", "READONLY",
        ] {
            redis_always(text, refused(Refusal::Session));
        }
        redis_always("CLIENT SETNAME me", refused(Refusal::Session));
        redis_always("", refused(Refusal::NoStatement));
        redis_always("# note", refused(Refusal::NoStatement));
    }

    #[test]
    fn redis_overwrites_and_other_writes_are_approvable() {
        for text in [
            "SET k v",
            "HSET h f v",
            "PERSIST k",
            "RENAME a b",
            "SINTERSTORE d a b",
            "COPY a b REPLACE",
            "LMOVE a b LEFT RIGHT",
            "SMOVE a b m",
            "XADD s * f v",
            "GETEX k PERSIST",
            "SET k v EX 0",
            "SETEX k 0 v",
            "HGETEX h FIELDS 1 f",
        ] {
            assert_eq!(redis(text, Mode::ReadOnly), needs_read_write(), "{text}");
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert_eq!(
                    redis(text, mode),
                    AgentVerdict::Approve(Verdict::WRITE, None),
                    "{mode:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn a_redis_expiry_of_a_key_is_approved_and_named_and_refused_at_read_only() {
        for text in [
            "SET k v EXAT 1",
            "SET k v PXAT 1",
            "set k v exat 1",
            "SET k v EX 10",
            "SET k v PX 10",
            "GETEX k EX 10",
            "GETEX k EXAT 1",
            "GETEX k PXAT 1",
            "EXPIRE k 10",
            "PEXPIRE k 10",
            "EXPIRE k 10 NX",
            "EXPIREAT k 1",
            "PEXPIREAT k 1",
            "expireat k 4102444800",
            "SETEX k 10 v",
            "PSETEX k 10 v",
            "setex k 10 v",
        ] {
            assert_eq!(redis(text, Mode::ReadOnly), needs_read_write(), "{text}");
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert_eq!(
                    redis(text, mode),
                    AgentVerdict::Approve(Verdict::WRITE, Some(Expiry::Key)),
                    "{mode:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn a_redis_expiry_of_hash_fields_is_approved_and_named_for_the_fields() {
        for text in [
            "HEXPIRE h 10 FIELDS 1 f",
            "HPEXPIRE h 10 FIELDS 1 f",
            "HEXPIREAT h 1 FIELDS 1 f",
            "HPEXPIREAT h 1 FIELDS 1 f",
            "HSETEX h EX 10 FIELDS 1 f v",
            "HSETEX h FXX EXAT 1 FIELDS 1 f v",
            "HSETEX h PXAT 1 FIELDS 1 f v",
            "HGETEX h PX 1 FIELDS 1 f",
            "HGETEX h EXAT 1 FIELDS 1 f",
            "hgetex h pxat 1 fields 1 f",
        ] {
            assert_eq!(redis(text, Mode::ReadOnly), needs_read_write(), "{text}");
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert_eq!(
                    redis(text, mode),
                    AgentVerdict::Approve(Verdict::WRITE, Some(Expiry::Field)),
                    "{mode:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn an_expiry_approval_says_what_is_removed_and_when() {
        let AgentVerdict::Approve(_, Some(expiry)) = redis("SET k v EXAT 1", Mode::ReadWrite)
        else {
            panic!("an absolute expiry is approved with its expiry named");
        };
        let text = expiry.to_string();
        assert_eq!(
            text,
            "Sets an expiry: the key is removed when the time passes. A time already past removes it at once."
        );
        for advice in ["try", "should", "please", "instead", "upgrade"] {
            assert!(!text.contains(advice), "{advice}: {text}");
        }

        let AgentVerdict::Approve(_, Some(expiry)) =
            redis("HSETEX h EX 10 FIELDS 1 f v", Mode::ReadWrite)
        else {
            panic!("a hash field expiry is approved with its expiry named");
        };
        let text = expiry.to_string();
        assert_eq!(
            text,
            "Sets an expiry: the named fields are removed when the time passes. A time already past removes them at once."
        );
        for advice in ["try", "should", "please", "instead", "upgrade"] {
            assert!(!text.contains(advice), "{advice}: {text}");
        }
    }

    /// `removes` reaches only the agent's gate, so the human's verdict for
    /// these is the table's write.
    #[test]
    fn the_human_path_reads_a_redis_trim_or_a_fixed_time_expiry_as_a_write() {
        for text in [
            "XADD s MAXLEN 0 * f v",
            "SET k v EXAT 1",
            "SET k v PXAT 1",
            "GETEX k EXAT 1",
            "GETEX k PXAT 1",
            "HSETEX h EXAT 1 FIELDS 1 f v",
            "HGETEX h PXAT 1 FIELDS 1 f",
            "HSETEX h EX 0 FIELDS 1 f v",
            "HGETEX h PX 0 FIELDS 1 f",
            "set k v exat 1",
        ] {
            assert_eq!(classify(Engine::Redis, text), Verdict::WRITE, "{text}");
        }
    }

    #[test]
    fn redis_commands_that_return_credentials_are_refused() {
        for text in [
            "CONFIG GET requirepass",
            "CONFIG GET masterauth",
            "CONFIG GET *",
            "config get maxmemory",
            "CLIENT LIST",
            "CLIENT LIST TYPE normal",
            "ACL LIST",
            "acl list",
            "ACL GETUSER u",
            "acl getuser u",
        ] {
            redis_always(text, refused(Refusal::Disclosure));
        }
        for text in [
            "CONFIG HELP",
            "CLIENT INFO",
            "CLIENT GETNAME",
            "ACL WHOAMI",
            "ACL USERS",
            "ACL LOG",
        ] {
            redis_always(text, AgentVerdict::Run);
        }
        // Only the agent path reads the disclosure: the human's verdict stays a read.
        for text in ["ACL LIST", "ACL GETUSER u"] {
            assert_eq!(
                super::super::classify(Engine::Redis, text),
                Verdict::READ,
                "{text}"
            );
        }
    }

    #[test]
    fn an_unreadable_statement_seen_to_disclose_is_refused_as_a_disclosure() {
        mongodb_always("db.currentOp()", refused(Refusal::Disclosure));
        redis_always("CONFIG GET x\nFROBNICATE", refused(Refusal::Disclosure));
        // Delete is asked first, then Disclosure, then Unreadable.
        mongodb_always("db.c.remove({})", refused(Refusal::Delete));
        redis_always("FROBNICATE a", refused(Refusal::Unreadable));
        // A database-level `aggregate` is no method DBDelve reads, so it is
        // refused as unreadable and never runs.
        mongodb_always(
            r#"db.getSiblingDB("admin").aggregate([{$currentOp: {allUsers: true}}])"#,
            refused(Refusal::Unreadable),
        );
    }

    #[test]
    fn redis_reads_run_and_drops_are_refused_by_name() {
        for text in ["GET k", "KEYS *", "SCAN 0", "TTL k"] {
            redis_always(text, AgentVerdict::Run);
        }
        for text in [
            "FLUSHDB",
            "FLUSHALL",
            "CONFIG SET maxmemory 1",
            "SHUTDOWN",
            "FUNCTION DELETE f",
        ] {
            redis_always(text, refused(Refusal::Drop));
        }
    }

    fn mongodb(text: &str, mode: Mode) -> AgentVerdict {
        agent_gate(Engine::MongoDb, text, mode)
    }

    /// `text` answers `expected` in every mode.
    fn mongodb_always(text: &str, expected: AgentVerdict) {
        for mode in Mode::ALL {
            assert_eq!(mongodb(text, mode), expected, "{mode:?}: {text}");
        }
    }

    const MONGODB_REMOVE_CALLS: [&str; 6] = [
        "db.c.remove({})",
        "db.c.remove({a: 1})",
        "db.c.remove({a: 1}, true)",
        "db.c.remove({a: 1}, {justOne: true})",
        r#"db.getCollection("c").remove({})"#,
        r#"db.getSiblingDB("x").c.remove({})"#,
    ];

    #[test]
    fn every_mongodb_removal_is_refused_as_a_delete() {
        for text in [
            "db.c.deleteOne({_id: 1})",
            "db.c.deleteMany({a: 1})",
            "db.c.deleteMany({})",
            "db.c.findOneAndDelete({a: 1})",
            r#"db.runCommand({delete: "c", deletes: [{q: {a: 1}, limit: 1}]})"#,
            r#"db.adminCommand({delete: "c", deletes: [{q: {a: 1}, limit: 0}]})"#,
            r#"db.runCommand({findAndModify: "c", query: {a: 1}, remove: true})"#,
        ]
        .into_iter()
        .chain(MONGODB_REMOVE_CALLS)
        {
            mongodb_always(text, refused(Refusal::Delete));
        }
        // The human path still cannot read a `remove`: only the agent path
        // names what it is seen to do.
        for text in MONGODB_REMOVE_CALLS {
            assert_eq!(
                super::super::classify(Engine::MongoDb, text).destructive,
                [Destructive::Unreadable],
                "{text}"
            );
        }
    }

    #[test]
    fn mongodb_javascript_is_refused_as_a_script() {
        for text in [
            r#"db.c.find({$where: "1"})"#,
            r#"db.c.find({$expr: {$function: {body: "1", args: [], lang: "js"}}})"#,
            r#"db.c.aggregate([{$group: {_id: 1, n: {$accumulator: {init: "1", accumulate: "1", accumulateArgs: [], merge: "1", lang: "js"}}}}])"#,
        ] {
            mongodb_always(text, refused(Refusal::Script));
        }
    }

    #[test]
    fn mongodb_commands_that_disclose_are_refused() {
        for text in [
            "db.adminCommand({currentOp: 1})",
            "db.adminCommand({currentOp: 1, $all: true})",
            "db.runCommand({currentOp: 1})",
            "db.adminCommand('currentOp')",
            "db.getSiblingDB('admin').adminCommand({currentOp: 1})",
            "db.runCommand({explain: {currentOp: 1}})",
            "db.adminCommand({aggregate: 1, pipeline: [{$currentOp: {allUsers: true}}], cursor: {}})",
            "db.c.aggregate([{$currentOp: {}}])",
            "db.runCommand({connectionStatus: 1, showPrivileges: true})",
            "db.runCommand({connectionStatus: 1, showPrivileges: 1})",
            "db.adminCommand({connectionStatus: 1, showPrivileges: true})",
            "db.currentOp()",
            r#"db.getSiblingDB("admin").currentOp()"#,
        ] {
            mongodb_always(text, refused(Refusal::Disclosure));
        }
        for text in [
            "db.runCommand({connectionStatus: 1})",
            "db.runCommand({connectionStatus: 1, showPrivileges: false})",
            "db.c.find({currentOp: 1})",
            "db.runCommand({ping: 1, currentOp: 1})",
            "db.runCommand({ping: 1})",
        ] {
            mongodb_always(text, AgentVerdict::Run);
        }
        // Only the agent path reads the disclosure: the human's verdict stays a read.
        for text in [
            "db.adminCommand({currentOp: 1})",
            "db.runCommand({connectionStatus: 1, showPrivileges: true})",
        ] {
            assert_eq!(
                super::super::classify(Engine::MongoDb, text),
                Verdict::READ,
                "{text}"
            );
        }
    }

    #[test]
    fn two_mongodb_calls_are_more_than_one_statement() {
        mongodb_always("db.c.find(); db.c.find()", refused(Refusal::MoreThanOne));
        mongodb_always(
            "db.c.deleteMany({}); db.c.find()",
            refused(Refusal::MoreThanOne),
        );
        mongodb_always("", refused(Refusal::NoStatement));
        mongodb_always("// nothing", refused(Refusal::NoStatement));
    }

    #[test]
    fn mongodb_writes_that_remove_nothing_are_approvable() {
        for text in [
            "db.c.insertOne({a: 1})",
            "db.c.insertMany([{a: 1}])",
            "db.c.updateOne({a: 1}, {$set: {a: 2}})",
            "db.c.updateMany({a: 1}, {$set: {a: 2}})",
            "db.c.replaceOne({a: 1}, {a: 2})",
            "db.c.findOneAndUpdate({a: 1}, {$set: {a: 2}})",
            "db.c.findOneAndReplace({a: 1}, {a: 2})",
            "db.c.createIndex({a: 1})",
            r#"db.c.aggregate([{$match: {a: 1}}, {$merge: {into: "x"}}])"#,
            r#"db.c.renameCollection("x")"#,
            r#"db.c.renameCollection("x", false)"#,
            r#"db.runCommand({findAndModify: "c", query: {a: 1}, update: {$set: {a: 2}}})"#,
        ] {
            assert_eq!(mongodb(text, Mode::ReadOnly), needs_read_write(), "{text}");
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert_eq!(
                    mongodb(text, mode),
                    AgentVerdict::Approve(Verdict::WRITE, None),
                    "{mode:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn mongodb_reads_run_and_drops_are_refused_by_name() {
        for text in [
            "db.c.find({a: 1})",
            "db.c.countDocuments({})",
            "db.c.aggregate([{$match: {a: 1}}])",
            "show collections",
        ] {
            mongodb_always(text, AgentVerdict::Run);
        }
        for text in [
            "db.c.drop()",
            "db.dropDatabase()",
            r#"db.c.aggregate([{$out: "x"}])"#,
            r#"db.c.renameCollection("x", true)"#,
        ] {
            mongodb_always(text, refused(Refusal::Drop));
        }
    }

    #[test]
    fn a_plain_select_runs_on_every_engine_and_mode() {
        for engine in sql_engines() {
            for sql in ["SELECT a FROM t", "SELECT lower(a) FROM t"] {
                always(engine, sql, AgentVerdict::Run);
            }
        }
    }

    #[test]
    fn a_select_calling_a_listed_function_never_runs() {
        for engine in sql_engines() {
            let Some(name) = engine.spec().sql.denied_functions.first() else {
                continue;
            };
            let sql = format!("SELECT {name}()");
            assert_eq!(
                agent_gate(engine, &sql, Mode::ReadOnly),
                needs_read_write(),
                "{engine:?}: {sql}"
            );
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert!(
                    matches!(agent_gate(engine, &sql, mode), AgentVerdict::Approve(..)),
                    "{engine:?} {mode:?}: {sql}"
                );
            }
        }
    }

    #[test]
    fn an_update_on_a_read_write_profile_is_approved_not_run() {
        let sql = "UPDATE t SET a = 1 WHERE id = 1";
        for engine in sql_engines() {
            assert_eq!(
                agent_gate(engine, sql, Mode::ReadOnly),
                needs_read_write(),
                "{engine:?}"
            );
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert_eq!(
                    agent_gate(engine, sql, mode),
                    AgentVerdict::Approve(Verdict::WRITE, None),
                    "{engine:?} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn create_index_is_approvable_and_needs_read_write() {
        for engine in sql_engines() {
            for sql in [
                "CREATE INDEX i ON t (a)",
                "CREATE INDEX IF NOT EXISTS i ON t (a)",
            ] {
                assert_eq!(
                    agent_gate(engine, sql, Mode::ReadOnly),
                    needs_read_write(),
                    "{engine:?}: {sql}"
                );
                for mode in [Mode::ReadWrite, Mode::Full] {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        AgentVerdict::Approve(Verdict::WRITE, None),
                        "{engine:?} {mode:?}: {sql}"
                    );
                }
            }
        }
        let concurrently = "CREATE INDEX CONCURRENTLY i ON t (a)";
        for engine in sql_engines() {
            let held = agent_gate(engine, concurrently, Mode::ReadOnly);
            let approved = agent_gate(engine, concurrently, Mode::ReadWrite);
            // Only Postgres's dialect reads the keyword; another engine must
            // say it cannot, never run it.
            assert!(
                (held == needs_read_write()
                    && approved == AgentVerdict::Approve(Verdict::WRITE, None))
                    || held == refused(Refusal::Unreadable) && approved == held,
                "{engine:?}: {held:?} {approved:?}"
            );
        }
    }

    #[test]
    fn a_statement_needing_more_than_the_profile_is_refused_never_offered_an_upgrade() {
        for engine in sql_engines() {
            assert_eq!(
                agent_gate(engine, "INSERT INTO t (a) VALUES (1)", Mode::ReadOnly),
                needs_read_write(),
                "{engine:?}"
            );
        }
    }

    #[test]
    fn an_unreadable_statement_is_refused_with_or_without_a_server_hold() {
        for engine in sql_engines() {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(engine, "selec 1", mode),
                    refused(Refusal::Unreadable),
                    "{engine:?} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn a_silenced_kind_changes_nothing_for_an_agent() {
        for engine in sql_engines() {
            let sql = "DELETE FROM t";
            let human = super::super::classify(engine, sql);
            assert_eq!(
                gate(&human, Mode::Full, &[Destructive::UnfilteredDelete]),
                None,
                "{engine:?}"
            );
            assert_eq!(
                agent_gate(engine, sql, Mode::Full),
                refused(Refusal::Delete),
                "{engine:?}"
            );
        }
    }

    #[test]
    fn drop_and_truncate_are_refused_by_name_in_every_mode() {
        for engine in sql_engines() {
            for mode in Mode::ALL {
                for sql in ["DROP TABLE t", "drop table t", "DrOp TaBlE t"] {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        refused(Refusal::Drop),
                        "{engine:?} {mode:?}: {sql}"
                    );
                }
                for sql in ["TRUNCATE TABLE t", "truncate t"] {
                    let answer = agent_gate(engine, sql, mode);
                    // A dialect with no `TRUNCATE` cannot read it, and says so.
                    assert!(
                        answer == refused(Refusal::Truncate)
                            || answer == refused(Refusal::Unreadable),
                        "{engine:?} {mode:?}: {sql}: {answer:?}"
                    );
                }
            }
        }
        for mode in Mode::ALL {
            assert_eq!(
                agent_gate(Engine::Postgres, "truncate t", mode),
                refused(Refusal::Truncate)
            );
        }
    }

    #[test]
    fn full_class_statements_are_refused_on_every_profile() {
        for engine in sql_engines() {
            for sql in ["GRANT SELECT ON t TO u", "CALL p()"] {
                let answer = agent_gate(engine, sql, Mode::Full);
                assert!(
                    answer == refused(Refusal::NeedsFull) || answer == refused(Refusal::Unreadable),
                    "{engine:?}: {sql}: {answer:?}"
                );
            }
        }
        for sql in [
            "GRANT SELECT ON t TO u",
            "CALL p()",
            "COPY t TO '/tmp/x.csv'",
        ] {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(Engine::Postgres, sql, mode),
                    refused(Refusal::NeedsFull),
                    "{sql} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn empty_and_comment_only_text_is_no_statement() {
        for engine in sql_engines() {
            for sql in ["", "   ", "-- nothing", "/* x */"] {
                for mode in Mode::ALL {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        refused(Refusal::NoStatement),
                        "{engine:?} {mode:?}: {sql:?}"
                    );
                }
            }
            assert_eq!(
                agent_gate(engine, "SELECT 1", Mode::ReadOnly),
                AgentVerdict::Run
            );
        }
    }

    #[test]
    fn two_statements_are_refused_whatever_they_are() {
        for engine in sql_engines() {
            for mode in Mode::ALL {
                for sql in ["SELECT 1; SELECT 2", "DROP TABLE a; DROP TABLE b"] {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        refused(Refusal::MoreThanOne),
                        "{engine:?} {mode:?}: {sql}"
                    );
                }
                assert_eq!(
                    agent_gate(engine, "SELECT 1; garbage", mode),
                    refused(Refusal::Unreadable),
                    "{engine:?} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn a_comment_beside_one_statement_is_still_one_statement() {
        for engine in sql_engines() {
            for sql in ["SELECT 1; -- c", "-- c\nSELECT 1", "/* c */ SELECT 1"] {
                for mode in Mode::ALL {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        AgentVerdict::Run,
                        "{engine:?} {mode:?}: {sql:?}"
                    );
                }
            }
        }
        for engine in [Engine::MySql, Engine::MariaDb] {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(engine, "SELECT 1 # c", mode),
                    AgentVerdict::Run,
                    "{engine:?} {mode:?}"
                );
            }
        }
    }

    /// Every shape of `DELETE` on the engine whose dialect reads it, each with
    /// the text that makes the walk reach it somewhere other than the top.
    const DELETES: &[(Engine, &str)] = &[
        (Engine::Postgres, "DELETE FROM t WHERE id = 1"),
        (Engine::Postgres, "DELETE FROM t"),
        (
            Engine::Postgres,
            "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM d",
        ),
        (
            Engine::Postgres,
            "WITH s AS (SELECT 1) DELETE FROM t WHERE id IN (SELECT * FROM s)",
        ),
        (
            Engine::Postgres,
            "EXPLAIN ANALYZE DELETE FROM t WHERE id = 1",
        ),
        (
            Engine::Postgres,
            "EXPLAIN (ANALYZE TRUE) DELETE FROM t WHERE id = 1",
        ),
        (Engine::MariaDb, "ANALYZE DELETE FROM t WHERE id = 1"),
        (Engine::SqlServer, "IF 1 = 1 DELETE FROM t WHERE id = 1"),
        (
            Engine::SqlServer,
            "IF 1 = 1 SELECT 1 ELSE DELETE FROM t WHERE id = 1",
        ),
        (Engine::Snowflake, "BEGIN DELETE FROM t WHERE id = 1; END"),
        (Engine::ClickHouse, "ALTER TABLE t DELETE WHERE id = 1"),
        (Engine::ClickHouse, "DELETE FROM t WHERE id = 1"),
    ];

    #[test]
    fn every_shape_of_delete_is_refused() {
        for (engine, sql) in DELETES {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(*engine, sql, mode),
                    refused(Refusal::Delete),
                    "{engine:?} {mode:?}: {sql}"
                );
            }
        }
        // A one-row delete by key is not softened for naming one row.
        for engine in sql_engines() {
            let one = agent_gate(engine, "DELETE FROM t WHERE id = 1", Mode::ReadWrite);
            let all = agent_gate(engine, "DELETE FROM t", Mode::ReadWrite);
            assert_eq!(one, refused(Refusal::Delete), "{engine:?}");
            assert_eq!(one, all, "{engine:?}");
        }
    }

    #[test]
    fn a_plain_explain_runs_and_explain_analyze_takes_the_inner_verdict() {
        let postgres = |sql, mode| agent_gate(Engine::Postgres, sql, mode);
        for mode in Mode::ALL {
            assert_eq!(
                postgres("EXPLAIN DELETE FROM t WHERE id = 1", mode),
                AgentVerdict::Run,
                "{mode:?}"
            );
            assert_eq!(
                postgres("EXPLAIN ANALYZE SELECT a FROM t", mode),
                AgentVerdict::Run,
                "{mode:?}"
            );
        }
        let update = "EXPLAIN ANALYZE UPDATE t SET a = 1 WHERE id = 1";
        assert_eq!(
            postgres(update, Mode::ReadWrite),
            AgentVerdict::Approve(Verdict::WRITE, None)
        );
        assert_eq!(postgres(update, Mode::ReadOnly), needs_read_write());
    }

    #[test]
    fn alter_beyond_add_column_is_refused_by_name() {
        for sql in [
            "ALTER TABLE t DROP COLUMN b",
            "ALTER TABLE t RENAME TO u",
            "ALTER TABLE t ALTER COLUMN a TYPE TEXT",
            "alter table t drop column b",
            "ALTER TABLE t ADD COLUMN b INT, DROP COLUMN c",
        ] {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(Engine::Postgres, sql, mode),
                    refused(Refusal::Alter),
                    "{mode:?}: {sql}"
                );
            }
        }
        let add = "ALTER TABLE t ADD COLUMN b INT";
        for engine in sql_engines() {
            assert_eq!(
                agent_gate(engine, add, Mode::ReadWrite),
                AgentVerdict::Approve(Verdict::WRITE, None),
                "{engine:?}"
            );
        }
        // A ClickHouse mutation amounts to the statement it is written as.
        assert_eq!(
            agent_gate(
                Engine::ClickHouse,
                "ALTER TABLE t UPDATE a = 1 WHERE id = 1",
                Mode::ReadWrite
            ),
            AgentVerdict::Approve(Verdict::WRITE, None)
        );
    }

    #[test]
    fn a_statement_inside_an_executable_comment_is_counted() {
        for mode in Mode::ALL {
            for (sql, refusal) in [
                ("SELECT 1 /*M!100100 ; SELECT 2 */", Refusal::MoreThanOne),
                (
                    "SELECT 1 /*M!100100 ; DELETE FROM t WHERE id = 1 */",
                    Refusal::MoreThanOne,
                ),
                ("/*M!100100 DELETE FROM t WHERE id = 1 */", Refusal::Delete),
            ] {
                assert_eq!(
                    agent_gate(Engine::MariaDb, sql, mode),
                    refused(refusal),
                    "{mode:?}: {sql}"
                );
            }
        }
    }

    #[test]
    fn a_transaction_block_is_more_than_one_statement() {
        let sql = "BEGIN; DELETE FROM t WHERE id = 1; COMMIT";
        for engine in sql_engines() {
            for mode in Mode::ALL {
                let answer = agent_gate(engine, sql, mode);
                // A dialect that cannot read the block says so.
                assert!(
                    answer == refused(Refusal::MoreThanOne)
                        || answer == refused(Refusal::Unreadable),
                    "{engine:?} {mode:?}: {answer:?}"
                );
            }
        }
        assert_eq!(
            agent_gate(Engine::Postgres, sql, Mode::Full),
            refused(Refusal::MoreThanOne)
        );
    }

    /// The statements the classifier has no arm for, or cannot parse. None of
    /// them may be run or approved on any engine in any mode.
    const NO_ARM: &[&str] = &[
        "PREPARE p AS SELECT 1",
        "EXECUTE p",
        "DEALLOCATE p",
        "DO $$ BEGIN DELETE FROM t; END $$",
        "LOAD DATA INFILE 'x.csv' INTO TABLE t",
        "SELECT a INTO OUTFILE '/tmp/x' FROM t",
        "SELECT a FROM t INTO OUTFILE '/tmp/x'",
        "DECLARE c CURSOR FOR SELECT 1",
        "FETCH NEXT FROM c",
        "LISTEN c",
        "NOTIFY c",
        "LOCK TABLE t",
        "REFRESH MATERIALIZED VIEW v",
        "REINDEX TABLE t",
        "CLUSTER t",
        "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1' LANGUAGE SQL",
        "CREATE TRIGGER g AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
        "CREATE EXTENSION x",
        "ATTACH 'x.db' AS x",
        "INSTALL httpfs",
        "LOAD httpfs",
        "EXPORT DATABASE '/tmp/x'",
        "OPTIMIZE TABLE t FINAL",
        "KILL 1",
        "SET GLOBAL max_connections = 1",
        "XA START 'x'",
    ];

    #[test]
    fn statements_with_no_arm_of_their_own_are_never_run_or_approved() {
        for engine in sql_engines() {
            for sql in NO_ARM {
                for mode in Mode::ALL {
                    let answer = agent_gate(engine, sql, mode);
                    assert!(
                        matches!(answer, AgentVerdict::Refuse(_)),
                        "{engine:?} {mode:?}: {sql}: {answer:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn session_statements_are_refused_whatever_the_mode() {
        for sql in [
            "BEGIN",
            "START TRANSACTION READ WRITE",
            "COMMIT",
            "ROLLBACK",
            "SET search_path = other",
            "SET ROLE other",
            "SET SESSION TRANSACTION READ WRITE",
            "CREATE TEMPORARY TABLE u (a INT)",
        ] {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(Engine::Postgres, sql, mode),
                    refused(Refusal::Session),
                    "{mode:?}: {sql}"
                );
            }
        }
        // Stateless engines keep no session, and an agent's `SET` is refused
        // there all the same.
        for engine in [Engine::Snowflake, Engine::ClickHouse] {
            for sql in ["SET x = 1", "USE other"] {
                for mode in Mode::ALL {
                    let answer = agent_gate(engine, sql, mode);
                    assert!(
                        answer == refused(Refusal::Session)
                            || answer == refused(Refusal::Unreadable),
                        "{engine:?} {mode:?}: {sql}: {answer:?}"
                    );
                }
            }
        }
        for engine in [Engine::MySql, Engine::MariaDb] {
            for sql in ["USE other", "SELECT @x := 1", "SELECT 1 INTO @x"] {
                for mode in Mode::ALL {
                    let answer = agent_gate(engine, sql, mode);
                    assert!(
                        answer == refused(Refusal::Session)
                            || answer == refused(Refusal::Unreadable),
                        "{engine:?} {mode:?}: {sql}: {answer:?}"
                    );
                }
            }
        }
        for engine in sql_engines() {
            for sql in [
                "BEGIN",
                "COMMIT",
                "SET search_path = other",
                "SET x = 1",
                "USE other",
                "SELECT @x := 1",
                "SELECT 1 INTO @x",
                "SET GLOBAL max_connections = 1",
            ] {
                for mode in Mode::ALL {
                    let answer = agent_gate(engine, sql, mode);
                    assert!(
                        matches!(answer, AgentVerdict::Refuse(_)),
                        "{engine:?} {mode:?}: {sql}: {answer:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_savepoint_family_is_refused_as_a_session_statement() {
        for sql in [
            "SAVEPOINT a",
            "RELEASE SAVEPOINT a",
            "ROLLBACK TO SAVEPOINT a",
        ] {
            for engine in sql_engines() {
                for mode in Mode::ALL {
                    let answer = agent_gate(engine, sql, mode);
                    let reads_it = matches!(
                        engine,
                        Engine::Postgres
                            | Engine::MySql
                            | Engine::MariaDb
                            | Engine::Sqlite
                            | Engine::DuckDb
                    );
                    if reads_it {
                        assert_eq!(
                            answer,
                            refused(Refusal::Session),
                            "{engine:?} {mode:?}: {sql}"
                        );
                    } else {
                        assert!(
                            answer == refused(Refusal::Session)
                                || answer == refused(Refusal::Unreadable),
                            "{engine:?} {mode:?}: {sql}: {answer:?}"
                        );
                    }
                }
            }
        }
    }

    /// A temporary view shadows the table of the same name for the rest of the
    /// session, so a later approved write would hit the view, not the table.
    #[test]
    fn a_temporary_view_is_refused_as_a_session_statement() {
        for engine in sql_engines() {
            for text in [
                "CREATE TEMP VIEW v AS SELECT 1",
                "CREATE OR REPLACE TEMP VIEW v AS SELECT 1",
                "CREATE TEMPORARY VIEW v AS SELECT a FROM t",
            ] {
                always(engine, text, refused(Refusal::Session));
            }
        }
    }

    /// A temporary table or variable that the `SELECT` writes under a set
    /// operation lasts the session just as the bare `SELECT ... INTO` already
    /// refused does.
    #[test]
    fn a_select_into_a_temporary_target_under_a_set_operation_is_refused_as_a_session_statement() {
        for engine in sql_engines() {
            for text in [
                "SELECT * INTO TEMP u FROM t UNION ALL SELECT * FROM w",
                "SELECT * INTO TEMP u FROM t EXCEPT SELECT * FROM w",
                "(SELECT * INTO TEMP u FROM t) UNION ALL SELECT * FROM w",
            ] {
                always(engine, text, refused(Refusal::Session));
            }
        }
        always(
            Engine::SqlServer,
            "SELECT * INTO #u FROM t UNION ALL SELECT * FROM w",
            refused(Refusal::Session),
        );
        for engine in [Engine::MySql, Engine::MariaDb] {
            always(
                engine,
                "SELECT a INTO @x FROM t UNION SELECT a FROM w",
                refused(Refusal::Session),
            );
        }
    }

    /// The walker reads only the target, so a permanent target and a plain
    /// read keep their answers.
    #[test]
    fn a_view_or_a_set_operation_with_no_temporary_target_keeps_its_answer() {
        for engine in sql_engines() {
            for text in [
                "CREATE VIEW v AS SELECT a FROM t",
                "SELECT * INTO u FROM t UNION ALL SELECT * FROM w",
            ] {
                assert!(
                    matches!(
                        agent_gate(engine, text, Mode::ReadWrite),
                        AgentVerdict::Approve(..)
                    ),
                    "{engine:?}: {text}"
                );
            }
            always(
                engine,
                "SELECT a FROM t UNION ALL SELECT a FROM w",
                AgentVerdict::Run,
            );
            assert_eq!(
                classify(engine, "CREATE TEMP VIEW v AS SELECT 1"),
                classify(engine, "CREATE VIEW v AS SELECT 1"),
                "{engine:?}"
            );
        }
    }

    #[test]
    fn a_read_of_session_state_still_runs() {
        for mode in Mode::ALL {
            for (engine, sql) in [
                (Engine::Postgres, "SELECT currval('s')"),
                (Engine::MySql, "SELECT last_insert_id()"),
                (Engine::Postgres, "SELECT make_interval(days := 10)"),
            ] {
                assert_eq!(
                    agent_gate(engine, sql, mode),
                    AgentVerdict::Run,
                    "{engine:?} {mode:?}: {sql}"
                );
            }
        }
    }

    #[test]
    fn every_refusal_names_its_rule_in_its_own_words() {
        let all = [
            (Refusal::NoStatement, "there is no statement to run"),
            (Refusal::MoreThanOne, "more than one statement"),
            (Refusal::Unreadable, "DBDelve can't read this statement"),
            (Refusal::NotSent, "DBDelve never sends this command"),
            (Refusal::Drop, "DROP is refused for agent statements"),
            (
                Refusal::Truncate,
                "TRUNCATE is refused for agent statements",
            ),
            (
                Refusal::Delete,
                "deleting data is refused for agent statements",
            ),
            (
                Refusal::Alter,
                "ALTER other than ADD COLUMN is refused for agent statements",
            ),
            (
                Refusal::Session,
                "statements that change the session are refused for agent statements",
            ),
            (
                Refusal::Script,
                "server-side scripts are refused for agent statements",
            ),
            (
                Refusal::Disclosure,
                "commands that return credentials or other clients' details are refused for agent statements",
            ),
            (
                Refusal::NeedsFull,
                "statements that need Full are refused for agent statements",
            ),
            (
                Refusal::NeedsMode {
                    needed: Mode::ReadWrite,
                    held: Mode::ReadOnly,
                },
                "needs Read-write; the profile is Read-only",
            ),
        ];
        let mut seen = Vec::new();
        for (refusal, text) in all {
            let said = refusal.to_string();
            assert_eq!(said, text);
            assert!(!seen.contains(&said), "{said} is said twice");
            for advice in ["upgrade", "try", "instead", "please", "should"] {
                assert!(
                    !said.to_lowercase().contains(advice),
                    "{said} gives advice: {advice}"
                );
            }
            seen.push(said);
        }
        assert_eq!(seen.len(), 13);
    }

    #[test]
    fn a_cell_with_two_reasons_has_one_defined_answer() {
        let postgres = |sql, mode| agent_gate(Engine::Postgres, sql, mode);
        assert_eq!(
            postgres("DROP TABLE a; DROP TABLE b", Mode::Full),
            refused(Refusal::MoreThanOne)
        );
        assert_eq!(
            postgres("SELECT 1; garbage", Mode::Full),
            refused(Refusal::Unreadable)
        );
        assert_eq!(
            postgres("DELETE FROM t WHERE id = 1", Mode::ReadOnly),
            refused(Refusal::Delete)
        );
        assert_eq!(
            postgres("ALTER TABLE t ADD COLUMN b INT, DROP COLUMN c", Mode::Full),
            refused(Refusal::Alter)
        );
        assert_eq!(
            postgres("GRANT SELECT ON t TO u", Mode::Full),
            refused(Refusal::NeedsFull)
        );
    }

    #[test]
    fn the_same_question_gets_the_same_answer() {
        for (engine, case, text) in crate::sql_snapshots::agent::sql_cells() {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(engine, &text, mode),
                    agent_gate(engine, &text, mode),
                    "{engine:?} {mode:?} {case}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn replacing_a_row_is_approvable() {
        for (engine, sql) in [
            (Engine::MySql, "REPLACE INTO t (a) VALUES (1)"),
            (Engine::MariaDb, "REPLACE INTO t (a) VALUES (1)"),
            (Engine::Sqlite, "INSERT OR REPLACE INTO t (a) VALUES (1)"),
        ] {
            assert_eq!(
                agent_gate(engine, sql, Mode::ReadWrite),
                AgentVerdict::Approve(Verdict::WRITE, None),
                "{engine:?}: {sql}"
            );
        }
    }
}

use std::ops::Range;

use super::{check, fragment, refusal, visible};
use crate::{
    db::Engine,
    sql::{self, Buffer},
};

const BUFFERS: [(&str, &str); 12] = [
    ("two statements", "SELECT 1; SELECT 2"),
    ("go and a counted go", "SELECT 1;\nGO\nSELECT 2\nGO 2\n"),
    (
        "go in a string and a comment",
        "SELECT 'GO'\n-- GO\nSELECT 2",
    ),
    ("final go without a newline", "SELECT 1\nGO"),
    (
        "begin end block",
        "IF 1=1 BEGIN DELETE FROM t; DELETE FROM u; END",
    ),
    (
        "variable and temp table across a go",
        "DECLARE @x int = 1;\nSELECT @x\nGO\nSELECT * FROM #tmp",
    ),
    ("only a comment", "/* only a comment */"),
    ("go inside a string", "SELECT 'a\nGO\nb'\nGO\nSELECT 2"),
    (
        "go inside a block comment",
        "/*\nGO\n*/\nSELECT 1\nGO\nSELECT 2",
    ),
    (
        "trailing comment and quoted semicolon",
        "SELECT 1 -- trailing\n;\n\nSELECT 'a;b'",
    ),
    ("mongosh", "db.a.find({})\ndb.b.find({})"),
    ("redis-cli", "GET a\nSET b 1"),
];

fn ranges(out: &mut String, engine: Engine, case: &str, sql: &str, ranges: &[Range<usize>]) {
    if ranges.is_empty() {
        fragment(out, engine, case, None);
    }
    for range in ranges {
        fragment(
            out,
            engine,
            &format!("{case} {range:?}"),
            Some(&sql[range.clone()]),
        );
    }
}

fn result(out: &mut String, engine: Engine, case: &str, result: Result<&str, String>) {
    match result {
        Ok(text) => fragment(out, engine, case, Some(text)),
        Err(message) => refusal(out, engine, case, &message),
    }
}

#[test]
fn statement_units_per_engine() {
    let mut out = String::new();
    for (name, sql) in BUFFERS {
        out.push_str(&format!("\n== {name}: |{}|\n", visible(sql)));
        for engine in Engine::ALL {
            let buffer = Buffer::for_engine(engine, sql);
            ranges(&mut out, engine, "statement", sql, buffer.statements());
            let queued = sql::queued_statements(engine, sql, None);
            ranges(&mut out, engine, "queued", sql, &queued);
            ranges(
                &mut out,
                engine,
                "queued in 0..9",
                sql,
                &sql::queued_statements(engine, sql, Some(0..9)),
            );
            fragment(
                &mut out,
                engine,
                "expected_sets",
                Some(&sql::expected_sets(engine, sql).to_string()),
            );
            ranges(
                &mut out,
                engine,
                "split_reads",
                sql,
                &sql::split_reads(engine, sql, &queued, 0..sql.len()),
            );
            result(&mut out, engine, "one_batch", sql::one_batch(engine, sql));
            result(
                &mut out,
                engine,
                "batch_counts",
                sql::batch_counts(engine, sql).map(|()| "ok"),
            );
            result(
                &mut out,
                engine,
                "batch_repeats 0",
                sql::batch_repeats(engine, sql, 0).map(|()| "ok"),
            );
            let (second, place) = match sql.find("SELECT 2") {
                Some(offset) => (offset, "at SELECT 2"),
                None => (sql.len(), "at the end"),
            };
            result(
                &mut out,
                engine,
                &format!("batch_repeats {second} {place}"),
                sql::batch_repeats(engine, sql, second).map(|()| "ok"),
            );
        }
    }
    check("sessions_units", &out);
}

const STATEMENTS: [&str; 27] = [
    "SELECT * FROM t",
    "SELECT a, b FROM s.t",
    "SELECT nextval('s')",
    "SELECT @@IDENTITY",
    "SET x = 1",
    "USE db",
    "PRAGMA x = 1",
    "BEGIN",
    "COMMIT",
    "SELECT 1; SELECT 2",
    "INSERT INTO t VALUES (1) RETURNING id",
    "CREATE TABLE t (a int)",
    "EXPLAIN SELECT 1",
    "EXPLAIN ANALYZE SELECT 1",
    "EXPLAIN (ANALYZE off) SELECT 1",
    "EXPLAIN (FORMAT JSON) SELECT 1",
    "EXPLAIN FORMAT=TREE SELECT 1",
    "EXPLAIN QUERY PLAN SELECT 1",
    "ANALYZE SELECT 1",
    "ANALYZE FORMAT=JSON SELECT 1",
    "ANALYZE TABLE t",
    "db.a.find({})",
    "GET k",
    "MULTI",
    "EXEC",
    "DISCARD",
    "CLIENT SETNAME me",
];

#[test]
fn typed_statements_per_engine() {
    let mut out = String::new();
    for sql in STATEMENTS {
        for engine in Engine::ALL {
            let verdict = sql::classify(engine, sql);
            let record = format!(
                "classify={:?} {:?} rerunnable={} stoppable={} plain_read={} leaves={:?} \
                 answers_with_rows={} table_read={:?} typed_explain={:?} explainable={:?}",
                verdict.mode,
                verdict.destructive,
                sql::rerunnable(engine, sql),
                sql::stoppable(engine, sql),
                sql::plain_read(engine, sql),
                sql::leaves(engine, sql),
                sql::answers_with_rows(engine, sql),
                sql::table_read(engine, sql),
                sql::typed_explain(engine, sql),
                sql::explainable(engine, sql),
            );
            fragment(&mut out, engine, sql, Some(&record));
        }
    }
    check("sessions_reads", &out);
}

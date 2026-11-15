use super::{check, statement_with};
use crate::{
    db::{Engine, Syntax, Target},
    mql,
    sql::{self, Destructive, Mode},
};

/// A `DraftDelete` of `accounts` by the `_id` text the grid would hold.
fn mongo_delete(id: &str) -> sql::DraftDelete {
    sql::DraftDelete {
        schema: "dbdelve_dev".to_owned(),
        table: "accounts".to_owned(),
        keys: vec![("_id".to_owned(), id.to_owned())],
        null_keys: Vec::new(),
        types: vec![("_id".to_owned(), "objectId".to_owned())],
    }
}

fn mongo_update(id: &str, name: &str) -> crate::result_grid::PendingRow {
    crate::result_grid::PendingRow {
        schema: "dbdelve_dev".to_owned(),
        table: "accounts".to_owned(),
        sets: vec![(
            "name".to_owned(),
            crate::result_grid::NewValue::Value(name.to_owned().into()),
        )],
        keys: vec![("_id".to_owned(), id.to_owned())],
        types: vec![
            ("_id".to_owned(), "objectId".to_owned()),
            ("name".to_owned(), "string".to_owned()),
        ],
        ..Default::default()
    }
}

fn mongo_insert(columns: &[(&str, Option<&str>)]) -> sql::DraftInsert {
    sql::DraftInsert {
        schema: "dbdelve_dev".to_owned(),
        table: "accounts".to_owned(),
        columns: columns
            .iter()
            .map(|(field, value)| ((*field).to_owned(), value.map(str::to_owned)))
            .collect(),
        types: vec![("name".to_owned(), "string".to_owned())],
    }
}

/// The text reaches the gates exactly as written: no generator is called.
fn corpus(out: &mut String, engine: Engine, case: &str, text: &str) {
    corpus_with(out, engine, case, text, "");
}

fn corpus_with(out: &mut String, engine: Engine, case: &str, text: &str, more: &str) {
    let verdict = sql::classify(engine, text);
    let lists: [(&str, &[Destructive]); 5] = [
        ("none", &[]),
        ("drop", &[Destructive::Drop]),
        ("truncate", &[Destructive::Truncate]),
        ("unfiltered", &[Destructive::UnfilteredDelete]),
        (
            "all3",
            &[
                Destructive::Drop,
                Destructive::Truncate,
                Destructive::UnfilteredDelete,
            ],
        ),
    ];
    let mut extra = String::new();
    for (label, list) in lists {
        let [ro, rw, full] = Mode::ALL.map(|mode| sql::gate(&verdict, mode, list));
        extra.push_str(&format!(
            " confirm[{label}]=ro:{ro:?},rw:{rw:?},full:{full:?}"
        ));
    }
    let keys: [(&str, &[&str]); 4] = [
        ("id", &["id"]),
        ("id+Region", &["id", "Region"]),
        ("Region", &["Region"]),
        ("none", &[]),
    ];
    for (label, list) in keys {
        extra.push_str(&format!(
            " matches_key[{label}]={}",
            sql::delete_matches_key_on(engine, text, list)
        ));
    }
    extra.push_str(more);
    statement_with(out, engine, case, Some(text), &extra);
}

fn sql_engines() -> impl Iterator<Item = Engine> {
    Engine::ALL
        .into_iter()
        .filter(|engine| engine.syntax() == Syntax::Sql)
}

#[test]
fn sql_gate_corpus_per_engine() {
    let mut out = String::new();
    for engine in sql_engines() {
        let q = engine.qualified("public", "accounts");
        let id = engine.quote_identifier("id");
        let name = engine.quote_identifier("Name");
        let region = engine.quote_identifier("Region");
        let other = engine.qualified("public", "other");
        let upd = format!("UPDATE {q} SET {name} = 'x' WHERE {id} = '7'");
        let del = format!("DELETE FROM {q} WHERE {id} = '7'");
        let set = |value: &str| format!("UPDATE {q} SET {name} = {value} WHERE {id} = '7'");
        let cases: Vec<(&str, String)> = vec![
            ("update", upd.clone()),
            ("delete", del.clone()),
            ("insert", format!("INSERT INTO {q} ({id}) VALUES ('7')")),
            ("select", format!("SELECT * FROM {q}")),
            ("bracketed update", format!("BEGIN;\n{upd};\nCOMMIT;")),
            ("bracketed two", format!("BEGIN;\n{upd};\n{del};\nCOMMIT;")),
            ("update then drop", format!("{upd}; DROP TABLE {q}")),
            ("update then update", format!("{upd}; {upd}")),
            (
                "update then unfiltered delete",
                format!("{upd}; DELETE FROM {q}"),
            ),
            ("delete then truncate", format!("{del}; TRUNCATE TABLE {q}")),
            ("select then drop", format!("SELECT 1; DROP TABLE {q}")),
            ("select then select", format!("SELECT * FROM {q}; SELECT 2")),
            (
                "comment hides the second",
                format!("SELECT 1 -- ; DROP TABLE {q}"),
            ),
            (
                "block comment around the first",
                format!("/* c */ SELECT * FROM {q}"),
            ),
            ("unclosed bracket", format!("BEGIN;\n{upd};")),
            ("commit alone", "COMMIT;".into()),
            (
                "nested brackets",
                format!("BEGIN;\nBEGIN;\n{upd};\nCOMMIT;\nCOMMIT;"),
            ),
            (
                "drop inside bracket",
                format!("BEGIN;\nDROP TABLE {q};\nCOMMIT;"),
            ),
            (
                "cte delete",
                format!("WITH d AS (DELETE FROM {q} RETURNING *) SELECT * FROM d"),
            ),
            (
                "cte delete, no returning",
                format!("WITH d AS (SELECT 1) DELETE FROM {q}"),
            ),
            (
                "cte update",
                format!("WITH d AS (UPDATE {q} SET a = 1 RETURNING *) SELECT 1"),
            ),
            (
                "cte insert",
                format!("WITH d AS (INSERT INTO {q} ({id}) VALUES ('1') RETURNING *) SELECT 1"),
            ),
            (
                "cte benign",
                "WITH d AS (SELECT 1 AS a) SELECT * FROM d".into(),
            ),
            ("select into", format!("SELECT * INTO new_t FROM {q}")),
            ("select for update", format!("SELECT * FROM {q} FOR UPDATE")),
            ("explain analyze delete", format!("EXPLAIN ANALYZE {del}")),
            (
                "unterminated",
                format!("UPDATE {q} SET {name} = 'oops WHERE {id} = '7'"),
            ),
            (
                "unterminated national",
                format!("UPDATE {q} SET {name} = N'oops WHERE {id} = '7'"),
            ),
            ("stray quote", set("'it''s''")),
            ("bad hex", set("0xZZ")),
            ("adjacent strings", set("'a' 'b'")),
            ("escaped quote", set(r"'a\'b'")),
            ("trailing backslash", set(r"'a\'")),
            ("dollar quote", set("$$x$$")),
            ("concatenation", set("'a' || 'b'")),
            ("function", set("now()")),
            ("subquery", set("(SELECT 1)")),
            ("arithmetic", set("1 + 1")),
            ("control character", set("'a\u{0}b'")),
            ("update no where", format!("UPDATE {q} SET {name} = 'x'")),
            ("update or", format!("{upd} OR 1 = 1")),
            (
                "update range",
                format!("UPDATE {q} SET {name} = 'x' WHERE {id} > 5"),
            ),
            (
                "update in",
                format!("UPDATE {q} SET {name} = 'x' WHERE {id} IN (1, 2)"),
            ),
            (
                "update twice",
                format!("UPDATE {q} SET {name} = 'x', {name} = 'y' WHERE {id} = '7'"),
            ),
            (
                "update from",
                format!("UPDATE {q} SET {name} = 'x' FROM other WHERE {id} = '7'"),
            ),
            ("update returning", format!("{upd} RETURNING *")),
            (
                "insert default values",
                format!("INSERT INTO {q} DEFAULT VALUES"),
            ),
            (
                "insert select",
                format!("INSERT INTO {q} SELECT * FROM {q}"),
            ),
            (
                "insert returning",
                format!("INSERT INTO {q} ({id}) VALUES ('7') RETURNING *"),
            ),
            (
                "insert upsert",
                format!("INSERT INTO {q} ({id}) VALUES ('7') ON CONFLICT DO UPDATE SET a = 1"),
            ),
            (
                "insert two rows",
                format!("INSERT INTO {q} ({id}) VALUES ('7'), ('8')"),
            ),
            ("truncate", format!("TRUNCATE TABLE {q}")),
            ("drop", format!("DROP TABLE {q}")),
            ("drop database", "DROP DATABASE accounts".into()),
            ("alter", format!("ALTER TABLE {q} ADD c int")),
            ("create", "CREATE TABLE t (a int)".into()),
            ("grant", format!("GRANT ALL ON {q} TO u")),
            ("delete no where", format!("DELETE FROM {q}")),
            ("delete always true", format!("DELETE FROM {q} WHERE 1 = 1")),
            ("delete or", format!("{del} OR {id} = '8'")),
            (
                "delete two keys, one column",
                format!("DELETE FROM {q} WHERE {id} = '7' AND {id} = '8'"),
            ),
            ("delete in", format!("DELETE FROM {q} WHERE {id} IN (7)")),
            ("delete range", format!("DELETE FROM {q} WHERE {id} > 7")),
            ("delete limit", format!("{del} LIMIT 1")),
            (
                "delete using",
                format!("DELETE FROM {q} USING other WHERE {id} = '7'"),
            ),
            (
                "delete composite",
                format!("DELETE FROM {q} WHERE {id} = '7' AND {region} = 'eu'"),
            ),
            (
                "delete extra column",
                format!("DELETE FROM {q} WHERE {id} = '7' AND {name} = 'x'"),
            ),
            (
                "delete other table",
                format!("DELETE FROM {other} WHERE {id} = '7'"),
            ),
            ("delete returning", format!("{del} RETURNING *")),
        ];
        for (case, text) in cases {
            corpus(&mut out, engine, case, &text);
        }
    }
    check("gates_sql", &out);
}

/// What a draft's apply asks of a text, besides the gate: how many `DELETE`s it
/// names on the grid's table, for a key of `id` and for a composite of `id` and
/// `Region` (half of which an `id`-only `DELETE` names), and how many
/// statements the gate recovers from it.
fn draft_more(engine: Engine, text: &str) -> String {
    let key = |keys: &[&str]| sql::draft_deletes_match(engine, text, keys, "public", "accounts");
    format!(
        " draft_deletes_match[id]={:?} draft_deletes_match[id+Region]={:?} statements={:?}",
        key(&["id"]),
        key(&["id", "Region"]),
        sql::generated_writes(engine, text).map(|statements| statements.len()),
    )
}

#[test]
fn draft_gate_corpus_per_engine() {
    use crate::import_rows::Value;

    let mut out = String::new();
    let nine = Value::Text("9".into());
    for engine in sql_engines() {
        let q = engine.qualified("public", "accounts");
        let id = engine.quote_identifier("id");
        let name = engine.quote_identifier("Name");
        let region = engine.quote_identifier("Region");
        let other = engine.qualified("public", "other");
        let upd = format!("UPDATE {q} SET {name} = 'x' WHERE {id} = '8'");
        let del = format!("DELETE FROM {q} WHERE {id} = '7'");
        let ins = format!("INSERT INTO {q} ({id}, {name}) VALUES ('9', NULL)");
        let cte_delete = format!("WITH d AS (DELETE FROM {q} WHERE {id} = '8' RETURNING *)");
        let joined = |parts: &[&str]| {
            parts
                .iter()
                .map(|part| match part.starts_with("--") {
                    true => (*part).to_owned(),
                    false => format!("{part};"),
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let bracket = |body: &str| format!("BEGIN;\n{body}\nCOMMIT;");

        let admitted: Vec<(&str, Vec<&str>)> = vec![
            ("delete update insert", vec![&del, &upd, &ins]),
            ("delete delete", vec![&del, &del]),
            ("insert insert", vec![&ins, &ins]),
            ("update insert", vec![&upd, &ins]),
            ("delete insert", vec![&del, &ins]),
        ];
        let mut refused: Vec<(String, String)> = Vec::new();
        let mut refuse = |case: &str, parts: &[&str]| {
            let body = joined(parts);
            refused.push((case.to_owned(), body.clone()));
            refused.push((format!("{case}, bracketed"), bracket(&body)));
        };
        let cte_update = format!("{cte_delete} UPDATE {q} SET {name} = 'x' WHERE {id} = '8'");
        let two_rows = format!("INSERT INTO {q} ({id}) VALUES ('9'), ('10')");
        let from_select = format!("INSERT INTO {q} SELECT * FROM {q}");
        let returning = format!("{ins} RETURNING *");
        let or_delete = format!("{del} OR 1 = 1");
        let no_where = format!("DELETE FROM {q}");
        let delete_returning = format!("{del} RETURNING *");
        let cte_select = format!("{cte_delete} SELECT 1");
        refuse("update beside a cte delete", &[&upd, &cte_select]);
        refuse("cte delete update beside insert", &[&cte_update, &ins]);
        refuse("cte delete update beside update", &[&cte_update, &upd]);
        refuse("delete beside two-row insert", &[&del, &two_rows]);
        refuse("delete beside insert select", &[&del, &from_select]);
        refuse("delete beside insert returning", &[&del, &returning]);
        refuse("delete, comment, insert", &[&del, "-- note", &ins]);
        refuse(
            "delete, executable comment, insert",
            &[&del, "/*! SELECT 1 */", &ins],
        );
        refuse("delete or, beside insert", &[&or_delete, &ins]);
        refuse("delete no where, beside insert", &[&no_where, &ins]);
        refuse(
            "delete returning, beside insert",
            &[&delete_returning, &ins],
        );
        let skip = sql::insert_rows(
            engine,
            "public",
            "accounts",
            &[("id", "int")],
            &[vec![Some(&nine)]],
            true,
        );
        if let Ok(skip) = &skip {
            refuse("delete beside skip-tail insert", &[&del, skip]);
        }
        refused.push((
            "bracket never closed".into(),
            format!("BEGIN;\n{};", joined(&[&del, &ins])),
        ));

        // The key-level cases: the shape is a draft's, and only the key check
        // can tell that it is not the grid's.
        let elsewhere = format!("DELETE FROM {other} WHERE {id} = '7'");
        let half = format!("DELETE FROM {q} WHERE {region} = 'eu'");
        let mut key_level: Vec<(&str, String)> = Vec::new();
        for (case, delete) in [
            ("delete of another table", &elsewhere),
            ("delete by half a key", &half),
        ] {
            let body = joined(&[delete, &ins]);
            key_level.push((case, body.clone()));
            key_level.push((case, bracket(&body)));
        }

        for (case, parts) in &admitted {
            let body = joined(parts);
            for (case, text) in [
                (case.to_string(), body.clone()),
                (format!("{case}, bracketed"), bracket(&body)),
            ] {
                corpus_with(&mut out, engine, &case, &text, &draft_more(engine, &text));
                if Target::built_in(engine).row_edits() {
                    let verdict = sql::classify(engine, &text);
                    assert!(
                        sql::is_generated_write_on(engine, &text),
                        "{engine:?} {case}: {text}"
                    );
                    assert_eq!(
                        (verdict.mode, verdict.destructive.is_empty()),
                        (Mode::ReadWrite, true),
                        "{engine:?} {case}"
                    );
                    assert_eq!(
                        sql::gate(&verdict, Mode::ReadOnly, &[]),
                        Some(sql::Stop::Upgrade(Mode::ReadWrite))
                    );
                    assert_eq!(sql::gate(&verdict, Mode::ReadWrite, &[]), None);
                    assert_eq!(sql::gate(&verdict, Mode::Full, &[]), None);
                } else if engine == Engine::ClickHouse {
                    assert!(
                        !sql::is_generated_write_on(engine, &text),
                        "{engine:?} {case}: {text}"
                    );
                }
            }
        }
        for (case, text) in &refused {
            corpus_with(&mut out, engine, case, text, &draft_more(engine, text));
            assert!(
                !sql::is_generated_write_on(engine, text),
                "{engine:?} {case}: {text}"
            );
        }
        for (case, text) in &key_level {
            corpus_with(&mut out, engine, case, text, &draft_more(engine, text));
            if Target::built_in(engine).row_edits() {
                for keys in [&["id"][..], &["id", "Region"]] {
                    assert_eq!(
                        sql::draft_deletes_match(engine, text, keys, "public", "accounts"),
                        None,
                        "{engine:?} {case}: {text}"
                    );
                }
            }
        }
    }
    check("gates_draft", &out);
}

#[test]
fn dialect_corpus_per_engine() {
    let texts: &[&str] = &[
        "SELECT `a` FROM t",
        "SELECT [a] FROM t",
        "SELECT \"a\" FROM t",
        r"SELECT 'a\'b'",
        r"SELECT 'a\\'",
        "SELECT $$x$$",
        r"SELECT E'a\nb'",
        "SELECT TOP 5 * FROM t",
        "SELECT * FROM t LIMIT 5",
        "SELECT * FROM t LIMIT 5, 10",
        "SELECT * FROM t OFFSET 1 ROWS FETCH NEXT 5 ROWS ONLY",
        "SELECT TOP 1 * INTO x FROM t",
        "INSERT INTO t VALUES ('1') ON DUPLICATE KEY UPDATE a = 1",
        "INSERT INTO t VALUES ('1') ON CONFLICT DO NOTHING",
        "INSERT OR REPLACE INTO t VALUES ('1')",
        "MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN DELETE",
        "PRAGMA table_info(t)",
        "SET @x = 1",
        "SHOW TABLES",
        "EXEC sp_who",
        "CALL p()",
        "USE db",
        "COPY t FROM '/x'",
        "ATTACH 'x' AS y",
        "INSTALL httpfs",
        "SELECT a::int FROM t",
        "SELECT /*+ hint */ 1",
        "BEGIN",
        "BEGIN TRANSACTION",
        "START TRANSACTION",
        "COMMIT",
        "ROLLBACK",
        "SET TRANSACTION READ ONLY",
        "ANALYZE SELECT 1",
        "ANALYZE UPDATE t SET a = 1",
        "ANALYZE DELETE FROM t",
        "ANALYZE TABLE t",
        "ANALYZE FORMAT=JSON DELETE FROM t",
        "ANALYZE FORMAT=XML SELECT 1",
        "EXPLAIN ANALYZE DELETE FROM t WHERE a = 1",
        "EXPLAIN SELECT 1",
        "EXPLAIN FORMAT=JSON UPDATE t SET a = 1",
        "SELECT 1\nGO\nDROP TABLE t",
        "GO",
        "SELECT 1\nGO 5",
        "SELECT 1; SELECT 2",
        "SELECT 1;",
        ";",
        "-- only a comment",
        "",
        "SELECT 1 FROM t WHERE a = 'x\\'",
        "SELECT \"a\\\"b\" FROM t",
        "SELECT `a``b` FROM t",
        "SELECT 'a''b'",
        "SELECT 1 /* c */",
        "SELECT /* a */ /* b */ 1",
        "UPDATE t SET a = 1 WHERE b = 'x\\'",
        "DELETE FROM t WHERE a = 'x\\'",
        "DELETE FROM t WHERE a = 'x\\' OR 1 = 1 -- '",
        "UPDATE t SET a = 'a\\' , b = 'b' WHERE c = 'y'",
        "INSERT INTO t VALUES ('a\\', 'b')",
    ];
    let mut out = String::new();
    for engine in sql_engines() {
        for text in texts {
            corpus(&mut out, engine, &format!("{text:?}"), text);
        }
    }
    check("gates_dialects", &out);
}

#[test]
fn unreadable_corpus_per_engine() {
    let long = format!("SELECT 1 FROM\n{}", "x".repeat(5000));
    let texts: &[(&str, &str)] = &[
        ("typo", "SELEC 1"),
        ("dangling from", "SELECT * FROM"),
        ("unterminated quote", "'unterminated"),
        ("bare update", "UPDATE"),
        ("bare drop", "DROP"),
        ("one unreadable among readable", "SELECT 1; SELEC 2"),
        ("delete then garbage", "DELETE FROM t; garbage"),
        ("lone backtick", "`"),
        ("nul in literal", "SELECT 'a\u{0}b'"),
        ("bare analyze", "ANALYZE"),
        ("analyze garbage", "ANALYZE garbage"),
    ];
    // Where the server lexes otherwise than sqlparser's tokenizer, or the
    // first keyword decides.
    let clickhouse: &[(&str, &str)] = &[
        ("hash comment", "# c\nSELECT 1"),
        ("byte order mark", "\u{feff}SELECT 1"),
        ("unknown first word", "FROBNICATE t"),
        ("unreadable read", "SELECT 1 FORMAT JSON"),
        ("unreadable backup", "BACKUP TABLE t TO Disk('b', 'x')"),
        (
            "carriage return in a line comment",
            "-- c\rBACKUP TABLE t TO Disk('b', 'x')",
        ),
        (
            "backslash in a quoted name",
            "SELECT `a\\`` PARALLEL WITH BACKUP TABLE t TO Disk('b', 'x') -- `",
        ),
        (
            "dollar in a bare word",
            "SELECT a$$ PARALLEL WITH BACKUP TABLE t TO Disk('b', 'x') --$$",
        ),
        (
            "zero-width space",
            "SELECT 1 PARALLEL\u{200b}WITH BACKUP TABLE t TO Disk('b', 'x')",
        ),
    ];
    let mut out = String::new();
    for engine in sql_engines() {
        let more = format!(" holds_read_only={}", engine.holds_read_only());
        for (case, text) in texts {
            corpus_with(&mut out, engine, case, text, &more);
        }
        corpus_with(&mut out, engine, "long line", &long, &more);
        if engine == Engine::ClickHouse {
            for (case, text) in clickhouse {
                corpus_with(&mut out, engine, case, text, &more);
            }
        }
    }
    check("gates_unreadable", &out);
}

/// ClickHouse's own corpus: the statements sqlparser reads in its dialect, the
/// ones it cannot and the first keyword places, and the ones that write
/// outside the database.
#[test]
fn clickhouse_gate_corpus() {
    let backup = "BACKUP TABLE t TO Disk('b', 'x')";
    let mut texts: Vec<String> = [
        "SELECT * FROM t FINAL PREWHERE a = 1 LIMIT 1 BY a",
        "SELECT a FROM t ARRAY JOIN arr AS a",
        "SELECT arrayMap(x -> x + 1, [1, 2])",
        "WITH 1 AS x SELECT x",
        "SELECT number FROM system.numbers SETTINGS max_threads = 1",
        "SELECT number FROM system.numbers FORMAT JSON",
        "SHOW TABLES",
        "DESCRIBE TABLE t",
        "EXISTS t",
        "USE analytics",
        "SET max_threads = 1",
        "CREATE TEMPORARY TABLE t (a Int32) ENGINE = Memory",
        "INSERT INTO t SELECT * FROM s",
        "CREATE TABLE t2 AS SELECT * FROM t",
        "SELECT 1; DROP TABLE t",
        "/* c */ BACKUP TABLE t TO Disk('b', 'x')",
        "-- c\nRESTORE TABLE t FROM Disk('b', 'x')",
        "SET readonly = 0",
        "SELECT 1 SETTINGS readonly = 0",
        "SYSTEM SHUTDOWN",
        "KILL QUERY WHERE 1",
        "OPTIMIZE TABLE t FINAL",
        "#c\nSELECT 1",
        "SELECT 1 INTO OUTFILE 'x'",
        "SELECT 1 PARALLEL WITH SELECT 2",
        "CREATE TABLE t2 (a Int32) ENGINE = MergeTree ORDER BY a",
        "CREATE TABLE t2 (a Int32) ENGINE = URL('http://example.test/x', CSV)",
        "CREATE TABLE t2 ENGINE = File(CSV) AS SELECT 1",
    ]
    .map(str::to_owned)
    .into();
    for target in [
        "url('http://example.test/x', CSV, 'a Int32')",
        "s3('http://example.test/b/x.csv')",
        "file('x.csv', 'CSV', 'a Int32')",
        "hdfs('hdfs://example.test/x', 'CSV', 'a Int32')",
        "remote('h', db.t)",
        "mysql('h:3306', 'db', 't', 'u', 'p')",
        "postgresql('h:5432', 'db', 't', 'u', 'p')",
    ] {
        texts.push(format!("INSERT INTO FUNCTION {target} SELECT * FROM t"));
    }
    texts.push("INSERT INTO TABLE FUNCTION file('x.csv') VALUES (1)".into());
    for alter in ["UPDATE a = 1 WHERE b = 2", "DELETE WHERE b = 2"] {
        texts.extend([
            format!("ALTER TABLE t {alter}"),
            format!("ALTER TABLE t {alter} SETTINGS mutations_sync = 2"),
            format!("ALTER TABLE t {alter}, DROP COLUMN c"),
            format!("ALTER TABLE t ON CLUSTER c {alter}"),
        ]);
    }
    let floor = [
        "SELECT", "WITH", "SHOW", "DESCRIBE", "DESC", "EXISTS", "EXPLAIN", "CHECK", "WATCH", "USE",
        "SET", "INSERT", "DELETE", "UPDATE", "ALTER", "CREATE", "DROP", "TRUNCATE", "RENAME",
        "EXCHANGE", "ATTACH", "DETACH", "UNDROP", "MOVE", "OPTIMIZE", "SYSTEM", "KILL", "GRANT",
        "REVOKE", "BACKUP", "RESTORE",
    ];
    for word in floor {
        for lead in ["", "-- c\n", "/* c */ "] {
            texts.push(format!("{lead}{word} garbage ) ("));
        }
    }
    texts.push(format!("SELECT a$$ PARALLEL WITH {backup} --$$"));
    // Where the server lexes otherwise than the tokenizer, and the tokenizer's
    // error: what Read-write may not run without Full.
    let backup = "BACKUP TABLE t TO Disk('backups', 'review-poc')";
    let restore = "RESTORE TABLE t FROM Disk('backups', 'review-poc')";
    texts.extend([
        format!("SELECT 1 // '\nPARALLEL WITH {backup} -- '"),
        format!("SELECT 1 // '\nPARALLEL WITH {restore} -- '"),
        format!("{backup} // '"),
        format!("{restore} // '"),
        format!("SELECT 1 AS a$$ PARALLEL WITH {backup} --$$"),
        "SET DEFAULT ROLE NONE TO CURRENT_USER".into(),
        "CREATE TABLE t2 ON CLUSTER c (a Int32) ENGINE = MergeTree ORDER BY a".into(),
        "CREATE TABLE relay (x UInt8) ENGINE = Distributed('one_shard_cluster', 'db', 'target') \
         AS SELECT 1"
            .into(),
        "CREATE TABLE t2 (a Int32) ENGINE = KeeperMap('/k')".into(),
    ]);

    let mut out = String::new();
    for text in &texts {
        corpus(&mut out, Engine::ClickHouse, &format!("{text:?}"), text);
    }
    check("gates_clickhouse", &out);
}

/// ClickHouse's rows named by every value: the write gate, the delete readout
/// against every column and against some, and the exactly-one guard, each read
/// from the text exactly as written.
#[test]
fn clickhouse_row_terms_corpus() {
    let engine = Engine::ClickHouse;
    let t = "`db`.`t`";
    let terms = "`k` = '1' AND toString(`f`) = '1.5' AND `n` IS NULL";
    let set = "`a` = 'new'";
    let sync = "SETTINGS mutations_sync = 2";
    let alter =
        |set: &str, filter: &str| format!("ALTER TABLE {t} UPDATE {set} WHERE {filter} {sync}");
    let update = |set: &str, filter: &str| format!("UPDATE {t} SET {set} WHERE {filter}");
    let delete = |filter: &str| format!("DELETE FROM {t} WHERE {filter}");
    let cases: Vec<(&str, String)> = vec![
        ("update, every term", update(set, terms)),
        ("alter update, every term", alter(set, terms)),
        ("delete, every term", delete(terms)),
        ("update, terminated", format!("{};", update(set, terms))),
        (
            "alter update, terminated",
            format!("{};", alter(set, terms)),
        ),
        ("update to null", update("`a` = NULL", terms)),
        (
            "alter update, two sets",
            alter("`a` = 'new', `b` = NULL", terms),
        ),
        (
            "delete, a subset",
            delete("`k` = '1' AND toString(`f`) = '1.5'"),
        ),
        (
            "delete, null terms only",
            delete("`k` IS NULL AND `f` IS NULL AND `n` IS NULL"),
        ),
        (
            "delete, null terms first",
            delete("`n` IS NULL AND `k` = '1' AND toString(`f`) = '1.5'"),
        ),
        (
            "two alter updates",
            format!("{};\n{};", alter(set, terms), alter("`b` = 'x'", terms)),
        ),
        (
            "alter update and update",
            format!("{};\n{};", alter(set, terms), update("`b` = 'x'", terms)),
        ),
        // Refused: terms the guard could not count as one row.
        (
            "delete, qualified column in toString",
            delete("toString(`t`.`f`) = '1.5'"),
        ),
        (
            "delete, toString with a time zone",
            delete("toString(`f`, 'UTC') = '1.5'"),
        ),
        ("delete, another function", delete("lower(`k`) = 'x'")),
        ("delete, is not null", delete("`n` IS NOT NULL")),
        ("delete, or", delete("`k` = '1' OR `n` IS NULL")),
        ("update, or", update(set, "`k` = '1' OR `n` IS NULL")),
        ("alter update, or", alter(set, "`k` = '1' OR `n` IS NULL")),
        (
            "delete, toString of a hex literal",
            delete("toString(`f`) = 0x41"),
        ),
        ("delete, quoted toString", delete("`toString`(`f`) = '1.5'")),
        (
            "delete, lower-case tostring",
            delete("tostring(`f`) = '1.5'"),
        ),
        (
            "delete, qualified toString",
            delete("db.toString(`f`) = '1.5'"),
        ),
        (
            "delete, toString distinct",
            delete("toString(DISTINCT `f`) = '1.5'"),
        ),
        (
            "delete, toString of a group",
            delete("toString((`f`)) = '1.5'"),
        ),
        ("delete, grouped is null", delete("(`n`) IS NULL")),
        ("delete, isNull", delete("isNull(`n`) = '1'")),
        ("delete, equals null", delete("`n` = NULL")),
        ("delete, is null compared", delete("`n` IS NULL = '1'")),
        (
            "delete, a column twice",
            delete("`k` = '1' AND `k` IS NULL"),
        ),
        (
            "delete, subquery",
            delete("`k` = (SELECT max(`k`) FROM `db`.`t`)"),
        ),
        (
            "delete, in subquery",
            delete("`k` IN (SELECT `k` FROM `db`.`t`)"),
        ),
        ("delete, no where", format!("DELETE FROM {t}")),
        (
            "delete, settings",
            format!("{} SETTINGS mutations_sync = 2", delete(terms)),
        ),
        (
            "delete, on cluster",
            format!("DELETE FROM {t} ON CLUSTER c WHERE {terms}"),
        ),
        (
            "delete, in partition",
            format!("DELETE FROM {t} IN PARTITION 'p' WHERE {terms}"),
        ),
        (
            "update, settings",
            format!("{} SETTINGS mutations_sync = 2", update(set, terms)),
        ),
        ("update, no where", format!("UPDATE {t} SET {set}")),
        ("update, function", update("`a` = now()", terms)),
        ("update, subquery", update("`a` = (SELECT 1)", terms)),
        (
            "update, a column set twice",
            update("`a` = '1', `a` = '2'", terms),
        ),
        ("update, default", update("`a` = DEFAULT", terms)),
        (
            "update, aliased",
            format!("UPDATE {t} AS x SET {set} WHERE {terms}"),
        ),
        // Refused: anything after `ALTER` but the one spelling.
        (
            "alter update, second command",
            format!("ALTER TABLE {t} UPDATE {set} WHERE {terms}, UPDATE `b` = 'x' WHERE 1 {sync}"),
        ),
        (
            "alter update, then delete command",
            format!("ALTER TABLE {t} UPDATE {set} WHERE {terms}, DELETE WHERE 1 {sync}"),
        ),
        (
            "alter update, on cluster",
            format!("ALTER TABLE {t} ON CLUSTER c UPDATE {set} WHERE {terms} {sync}"),
        ),
        (
            "alter update, in partition",
            format!("ALTER TABLE {t} UPDATE {set} IN PARTITION 'p' WHERE {terms} {sync}"),
        ),
        (
            "alter update, no settings",
            format!("ALTER TABLE {t} UPDATE {set} WHERE {terms}"),
        ),
        (
            "alter update, asynchronous",
            format!("ALTER TABLE {t} UPDATE {set} WHERE {terms} SETTINGS mutations_sync = 0"),
        ),
        (
            "alter update, this replica only",
            format!("ALTER TABLE {t} UPDATE {set} WHERE {terms} SETTINGS mutations_sync = 1"),
        ),
        (
            "alter update, quoted setting",
            format!("ALTER TABLE {t} UPDATE {set} WHERE {terms} SETTINGS mutations_sync = '2'"),
        ),
        (
            "alter update, another setting",
            format!("{}, max_threads = 1", alter(set, terms)),
        ),
        (
            "alter update, settings twice",
            format!("{} {sync}", alter(set, terms)),
        ),
        (
            "alter update, lower case",
            alter(set, terms).replacen("ALTER TABLE", "alter table", 1),
        ),
        (
            "alter update, two spaces",
            alter(set, terms).replacen("ALTER TABLE", "ALTER  TABLE", 1),
        ),
        (
            "alter update, comment between words",
            alter(set, terms).replacen("ALTER TABLE", "ALTER /* c */ TABLE", 1),
        ),
        (
            "alter update, comment in the predicate",
            alter(set, &format!("{terms} /* c */")),
        ),
        (
            "alter update, nested comment",
            alter(set, "`k` = '1' /* /* */ AND `n` IS NULL */"),
        ),
        (
            "alter update, hash comment",
            alter(set, "`k` = '1' # c\n AND `n` IS NULL"),
        ),
        (
            "alter update, no where",
            format!("ALTER TABLE {t} UPDATE {set} {sync}"),
        ),
        (
            "alter update, subquery",
            alter(set, "`k` IN (SELECT `k` FROM `db`.`t`)"),
        ),
        ("alter update, function", alter("`a` = now()", terms)),
        ("alter update, heredoc", alter("`a` = $$x$$", terms)),
        (
            "alter delete",
            format!("ALTER TABLE {t} DELETE WHERE {terms} {sync}"),
        ),
        (
            "alter update, then drop",
            format!("{}; DROP TABLE {t}", alter(set, terms)),
        ),
        (
            "alter update and delete",
            format!("{};\n{};", alter(set, terms), delete(terms)),
        ),
        (
            "alter modify",
            format!("ALTER TABLE {t} MODIFY COLUMN `a` String"),
        ),
        (
            "backslash before a quote in a name",
            delete("`k\\` = '1' AND `n` IS NULL -- `"),
        ),
        ("delete, line comment", format!("{} -- c", delete(terms))),
        (
            "delete, nested comment",
            delete("`k` = '1' /* /* */ AND `n` IS NULL */"),
        ),
    ];
    let mut out = String::new();
    for (case, text) in &cases {
        let extra = format!(
            " matches_key[k+f+n]={} matches_key[k+f]={} row_guard={}",
            sql::delete_matches_key_on(engine, text, &["k", "f", "n"]),
            sql::delete_matches_key_on(engine, text, &["k", "f"]),
            match sql::row_guard(engine, text) {
                Some(guard) => format!("|{guard}|"),
                None => "(none)".into(),
            },
        );
        statement_with(&mut out, engine, case, Some(text), &extra);
    }
    check("gates_clickhouse_rows", &out);
}

#[test]
fn mongo_and_redis_gate_corpus() {
    let c = r#"db.getCollection("accounts")"#;
    let mongo: Vec<String> = [
        "{c}.find({})",
        "{c}.find({}).limit(1000)",
        "{c}.find({}).sort({\"id\": 1})",
        "{c}.aggregate([{\"$match\": {}}])",
        "{c}.insertOne({\"a\": 1})",
        "{c}.insertMany([{\"a\": 1}, {\"a\": 2}])",
        "{c}.updateOne({_id: 1}, {$set: {\"a\": 1}});",
        "{c}.deleteOne({_id: 1})",
        "{c}.find({}); {c}.drop()",
        "{c}.find({})\n{c}.drop()",
        "{c}.insertOne({\"a\": 1}); {c}.insertOne({\"a\": 2})",
        "{c}.updateOne({_id: 1}, {$set: {\"a\": 1}}); {c}.deleteMany({})",
        "{c}.aggregate([{\"$out\": \"copy\"}])",
        "{c}.aggregate([{\"$merge\": {\"into\": \"copy\"}}])",
        "{c}.aggregate([{\"$match\": {}}, {\"$out\": \"x\"}])",
        "{c}.aggregate([{\"$lookup\": {\"from\": \"o\", \"pipeline\": [{\"$out\": \"y\"}], \"as\": \"z\"}}])",
        "{c}.find({\"$where\": \"sleep(1000)\"})",
        "{c}.aggregate([{\"$function\": {\"body\": \"x\", \"args\": [], \"lang\": \"js\"}}])",
        "{c}.find({",
        "{c}.find({}",
        "{c}.insertOne({\"a\": })",
        "{c}.insertOne({\"a\": ObjectId(\"zz\")})",
        "{c}.insertOne({\"a\": NumberLong(\"x\")})",
        "{c}.updateOne({_id: 1}, {$set: {\"a\": 1}}",
        "",
        "{c}.insertOne({})",
        "{c}.insertOne({\"$set\": 1})",
        "{c}.insertMany([])",
        "{c}.updateOne({}, {$set: {\"a\": 1}})",
        "{c}.updateOne({_id: 1}, {$unset: {\"a\": 1}})",
        "{c}.updateOne({_id: 1}, {$set: {\"a\": 1}}, {upsert: true})",
        "{c}.updateMany({_id: 1}, {$set: {\"a\": 1}})",
        "{c}.deleteOne({})",
        "{c}.deleteMany({})",
        "{c}.deleteOne({_id: {\"$gt\": 1}})",
        "{c}.deleteOne({_id: 1, \"a\": 2})",
        "{c}.deleteOne({\"a\": 2})",
        "{c}.drop()",
        "{c}.remove({})",
        "db.dropDatabase()",
        "{c}.createIndex({\"a\": 1})",
        "{c}.renameCollection(\"x\")",
        "db.runCommand({drop: \"accounts\"})",
        "{c}.bulkWrite([])",
        "{c}.findOneAndDelete({})",
    ]
    .iter()
    .map(|text| text.replace("{c}", c))
    .collect();
    let redis: &[&str] = &[
        "GET k",
        "SCAN 0 MATCH * COUNT 1000",
        "TYPE k",
        "TTL k",
        "HGETALL k",
        "GET k\nDEL k",
        "GET k; DEL k",
        "SCAN 0 MATCH a*; FLUSHALL",
        "GET k\n\nFLUSHALL",
        "DEL k",
        "UNLINK k",
        "FLUSHALL",
        "FLUSHDB",
        "FLUSHALL ASYNC",
        "EVAL \"return redis.call('flushall')\" 0",
        "EVALSHA abc 0",
        "SCRIPT FLUSH",
        "CONFIG SET dir /tmp",
        "SHUTDOWN",
        "DEBUG SLEEP 5",
        "KEYS *",
        "SLAVEOF NO ONE",
        "RENAME a b",
        "MOVE k 1",
        "SELECT 1",
        "SET k v NX EX 30",
        "SET k v",
        "SET k v XX KEEPTTL GET",
        "HSET h f v",
        "HSETNX h f v",
        "HDEL h f",
        "SADD s m",
        "SREM s m",
        "ZADD z NX 1 m",
        "ZADD z XX CH 1 m",
        "ZREM z m",
        "RPUSH l v",
        "LSET l 0 v",
        "EXPIRE k 10",
        "PERSIST k",
        "RENAMENX a b",
        "WATCH l\nLPOS l v COUNT 0 MAXLEN 1\nMULTI\nLSET l 0 v\nEXEC",
        "LPOS l v COUNT 0 MAXLEN 1\nMULTI\nLSET l 0 v\nEXEC",
        "MULTI\nDEL k\nEXEC",
        "MULTI\nEXEC",
        "EXEC",
        "SET k \"unterminated",
        "SET",
        "GET",
        "SET k v NX EX abc",
        "SET k v NX EX 0",
        "HSET h f",
        "ZADD z NX abc m",
        "FROBNICATE",
        "set k v nx",
        "",
        "LRANGE l 0 -1",
        "GET k \u{0}",
        "SET k v NX EX 30\nSET k2 v NX EX 30",
        "SET k v NX EX 30; DEL k",
        "UNLINK k j",
        "HDEL h f g",
    ];
    let mut out = String::new();
    for text in &mongo {
        corpus(&mut out, Engine::MongoDb, &format!("{text:?}"), text);
    }
    for text in redis {
        corpus(&mut out, Engine::Redis, &format!("{text:?}"), text);
    }
    check("gates_nonsql", &out);
}

/// The MongoDB draft: every shape the write gate admits beyond the four it
/// admitted before, and every widened form it refuses. A record carries what
/// the gate says and what the key check reads on `accounts` by `_id`, so a
/// text the gate admits and the key check cannot count shows here.
#[test]
fn mongo_draft_gate_corpus() {
    let c = r#"db.getCollection("accounts")"#;
    let mut out = String::new();
    let mut record = |case: &str, text: &str, admitted: bool| {
        assert_eq!(
            sql::is_generated_write_on(Engine::MongoDb, text),
            admitted,
            "{case}: {text}"
        );
        let counted = mql::draft_deletes_match(text, &["_id"], "accounts");
        corpus_with(
            &mut out,
            Engine::MongoDb,
            case,
            text,
            &format!(" draft_deletes_match={counted:?}"),
        );
    };
    let delete = |n: u8| mongo_delete(&format!("ObjectId('65a4f1c0ffffffffffffff{n:02x}')"));
    let update = |n: u8, name: &str| {
        mongo_update(&format!("ObjectId('65a4f1c0ffffffffffffff{n:02x}')"), name)
    };
    let insert = |name: &str| mongo_insert(&[("name", Some(name))]);
    let written = |deletes: &[_], updates: &[_], inserts: &[_]| {
        mql::draft_batch(deletes, updates, inserts).expect("a draft that is written")
    };
    for (case, text) in [
        (
            "draft: deletes, updates, inserts",
            written(
                &[delete(1), delete(2)],
                &[update(3, "a"), update(4, "b")],
                &[insert("c"), insert("d")],
            ),
        ),
        (
            "draft: marks only",
            written(&[delete(1), delete(2)], &[], &[]),
        ),
        (
            "draft: adds only",
            written(&[], &[], &[insert("c"), insert("d")]),
        ),
        (
            "draft: marks and adds",
            written(&[delete(1)], &[], &[insert("c")]),
        ),
        (
            "draft: edits and adds",
            written(&[], &[update(3, "a")], &[insert("c")]),
        ),
        ("draft: one mark", written(&[delete(1)], &[], &[])),
        ("draft: one edit", written(&[], &[update(3, "a")], &[])),
        ("draft: one add", written(&[], &[], &[insert("c")])),
    ] {
        record(case, &text, true);
    }
    for (case, text) in [
        (
            "two deletes by id",
            "{c}.deleteOne({_id: 1});\n{c}.deleteOne({_id: 2});",
        ),
        (
            "two inserts",
            "{c}.insertOne({\"a\": 1});\n{c}.insertOne({\"a\": 2});",
        ),
        (
            "delete then insert",
            "{c}.deleteOne({_id: 1});\n{c}.insertOne({\"a\": 1});",
        ),
    ] {
        record(case, &text.replace("{c}", c), true);
    }
    for (case, text) in [
        (
            "insert before delete",
            "{c}.insertOne({\"a\": 1});\n{c}.deleteOne({_id: 1});",
        ),
        (
            "update before delete",
            "{c}.updateOne({_id: 1}, {$set: {\"a\": 1}});\n{c}.deleteOne({_id: 2});",
        ),
        (
            "insert before update",
            "{c}.insertOne({\"a\": 1});\n{c}.updateOne({_id: 1}, {$set: {\"a\": 1}});",
        ),
        (
            "delete then deleteMany",
            "{c}.deleteOne({_id: 1});\n{c}.deleteMany({});",
        ),
        (
            "delete then updateMany",
            "{c}.deleteOne({_id: 1});\n{c}.updateMany({_id: 1}, {$set: {\"a\": 1}});",
        ),
        (
            "delete then replaceOne",
            "{c}.deleteOne({_id: 1});\n{c}.replaceOne({_id: 2}, {\"a\": 1});",
        ),
        (
            "delete then findOneAndDelete",
            "{c}.deleteOne({_id: 1});\n{c}.findOneAndDelete({_id: 2});",
        ),
        (
            "delete then insertMany",
            "{c}.deleteOne({_id: 1});\n{c}.insertMany([{\"a\": 1}]);",
        ),
        (
            "delete then a second key",
            "{c}.deleteOne({_id: 1});\n{c}.deleteOne({_id: 2, \"a\": 1});",
        ),
        (
            "delete then a filter without id",
            "{c}.deleteOne({_id: 1});\n{c}.deleteOne({\"a\": 1});",
        ),
        (
            "delete then an empty filter",
            "{c}.deleteOne({_id: 1});\n{c}.deleteOne({});",
        ),
        (
            "delete then an operator filter",
            "{c}.deleteOne({_id: 1});\n{c}.deleteOne({_id: {\"$ne\": 1}});",
        ),
        (
            "delete then a regex id",
            "{c}.deleteOne({_id: 1});\n{c}.deleteOne({_id: /./});",
        ),
        (
            "delete then an option",
            "{c}.deleteOne({_id: 1});\n{c}.deleteOne({_id: 2}, {collation: {}});",
        ),
        (
            "delete then an upsert",
            "{c}.deleteOne({_id: 1});\n{c}.updateOne({_id: 2}, {$set: {\"a\": 1}}, {upsert: true});",
        ),
        (
            "delete then an unset",
            "{c}.deleteOne({_id: 1});\n{c}.updateOne({_id: 2}, {$unset: {\"a\": 1}});",
        ),
        (
            "delete then a second collection",
            "{c}.deleteOne({_id: 1});\ndb.getCollection(\"other\").deleteOne({_id: 2});",
        ),
        (
            "delete then a sibling database",
            "{c}.deleteOne({_id: 1});\ndb.getSiblingDB(\"x\").accounts.deleteOne({_id: 2});",
        ),
        (
            "a cursor before a delete",
            "{c}.find({}).limit(1);\n{c}.deleteOne({_id: 1});",
        ),
        (
            "delete then a read",
            "{c}.deleteOne({_id: 1});\n{c}.find({});",
        ),
        (
            "delete then dropDatabase",
            "{c}.deleteOne({_id: 1});\ndb.dropDatabase();",
        ),
        (
            "delete then an inserted $where",
            "{c}.deleteOne({_id: 1});\n{c}.insertOne({\"$where\": \"1\"});",
        ),
        (
            "delete then an inserted $set",
            "{c}.deleteOne({_id: 1});\n{c}.insertOne({\"a\": {\"$set\": {\"b\": 1}}});",
        ),
        (
            "delete then an empty insert",
            "{c}.deleteOne({_id: 1});\n{c}.insertOne({});",
        ),
        (
            "delete then an updated $where",
            "{c}.deleteOne({_id: 1});\n{c}.updateOne({_id: 2}, {$set: {\"a\": {\"$where\": \"1\"}}});",
        ),
        (
            "delete then text that does not parse",
            "{c}.deleteOne({_id: 1});\n{c}.insertOne({\"a\": ",
        ),
    ] {
        record(case, &text.replace("{c}", c), false);
    }
    check("gates_mongo_draft", &out);
}

/// What a keyless draft's apply asks of a text besides the gate: how many
/// `DELETE`s it names against the key of every column of the table
/// (`bird`, `place`) and against a keyed table's `id`, and how many statements
/// the gate recovers from it.
fn keyless_more(engine: Engine, text: &str) -> String {
    let key = |keys: &[&str]| sql::draft_deletes_match(engine, text, keys, "public", "accounts");
    format!(
        " draft_deletes_match[bird+place]={:?} draft_deletes_match[id]={:?} statements={:?}",
        key(&["bird", "place"]),
        key(&["id"]),
        sql::generated_writes(engine, text).map(|statements| statements.len()),
    )
}

/// A row named by every value, a NULL as `IS NULL`, on every SQL engine: what
/// the gate admits where `null_terms` (or ClickHouse's `row_terms`) is set and
/// refuses elsewhere, and the shapes it refuses everywhere.
#[test]
fn keyless_gate_corpus_per_engine() {
    let mut out = String::new();
    for engine in sql_engines() {
        let q = engine.qualified("public", "accounts");
        let id = engine.quote_identifier("id");
        let bird = engine.quote_identifier("bird");
        let place = engine.quote_identifier("place");
        let every = format!("{bird} = 'wren' AND {place} IS NULL");
        let delete = |predicate: &str| format!("DELETE FROM {q} WHERE {predicate}");
        let ins = format!("INSERT INTO {q} ({bird}, {place}) VALUES ('tit', NULL)");
        let cases: Vec<(&str, String)> = vec![
            ("delete every column, a null", delete(&every)),
            (
                "delete every column, no null",
                delete(&format!("{bird} = 'wren' AND {place} = 'oak'")),
            ),
            (
                "delete every column, other order",
                delete(&format!("{place} IS NULL AND {bird} = 'wren'")),
            ),
            (
                "delete every column, an empty string",
                delete(&format!("{bird} = 'wren' AND {place} = ''")),
            ),
            (
                "delete fewer columns than the row",
                delete(&format!("{bird} = 'wren'")),
            ),
            ("delete a null alone", delete(&format!("{place} IS NULL"))),
            (
                "delete or",
                delete(&format!("{bird} = 'wren' OR {place} IS NULL")),
            ),
            (
                "delete subquery",
                delete(&format!("{bird} = (SELECT 'wren') AND {place} IS NULL")),
            ),
            (
                "delete is not null",
                delete(&format!("{bird} = 'wren' AND {place} IS NOT NULL")),
            ),
            (
                "delete is null twice",
                delete(&format!("{place} IS NULL AND {place} IS NULL")),
            ),
            (
                "delete an extra null term on a keyed table",
                delete(&format!("{id} = '7' AND {place} IS NULL")),
            ),
            (
                "update every column, a null",
                format!("UPDATE {q} SET {bird} = 'finch' WHERE {every}"),
            ),
            (
                "update to null, every column",
                format!("UPDATE {q} SET {place} = NULL WHERE {every}"),
            ),
            (
                "draft, delete every column then insert",
                format!("{};\n{};", delete(&every), ins),
            ),
            (
                "draft, bracketed",
                format!("BEGIN;\n{};\n{};\nCOMMIT;", delete(&every), ins),
            ),
        ];
        for (case, text) in cases {
            let more = keyless_more(engine, &text);
            corpus_with(&mut out, engine, case, &text, &more);
        }
    }

    // A plugin that speaks Postgres's dialect reads the same gate through
    // `generated_writes`, so what it is asked to run is what is admitted here.
    let q = Engine::Postgres.qualified("public", "accounts");
    let text = format!(r#"DELETE FROM {q} WHERE "bird" = 'wren' AND "place" IS NULL"#);
    out.push_str(&format!(
        "\n[plugin speaking postgres] every-column delete => generated_writes={:?}\n",
        sql::generated_writes(Engine::Postgres, &text)
    ));
    check("gates_keyless", &out);
}

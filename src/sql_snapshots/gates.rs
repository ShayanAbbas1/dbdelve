use super::{check, statement_with};
use crate::{
    db::{Engine, Syntax},
    sql::{self, Destructive, Mode},
};

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
    let mut out = String::new();
    for engine in sql_engines() {
        let more = format!(" holds_read_only={}", engine.holds_read_only());
        for (case, text) in texts {
            corpus_with(&mut out, engine, case, text, &more);
        }
        corpus_with(&mut out, engine, "long line", &long, &more);
    }
    check("gates_unreadable", &out);
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

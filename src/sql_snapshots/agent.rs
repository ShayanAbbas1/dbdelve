//! What `sql::agent_gate` answers for each kind of statement an agent could
//! send, recorded per SQL engine and per mode, so every answer is one line of a
//! `.snap` read once and held. A record is the answer in Read-only, Read-write
//! and Full, in that order, for the text exactly as written.
//!
//! A `.snap` accepted by regeneration alone reviews nothing: the explicit
//! tests in `sql::agent` pin each cell the success criteria name, and the
//! invariants below hold over every cell recorded here.

use super::{check, fragment, visible};
use crate::{
    db::{Engine, ExplainMode, Syntax},
    sql::{self, AgentVerdict, Mode},
};

/// Stands for the engine's own first deny-listed function, called with no
/// arguments, in a kind below. An engine that lists none records `(none)`.
const DENIED_CALL: &str = "<the engine's first deny-listed function>";

/// The kinds, in the order they are recorded: a case name and the text sent.
const SQL_KINDS: &[(&str, &str)] = &[
    ("plain select", "SELECT a FROM t"),
    ("scalar function", "SELECT lower(a) FROM t"),
    ("user-defined function", "SELECT my_udf(a) FROM t"),
    ("deny-listed call", DENIED_CALL),
    ("insert", "INSERT INTO t (a) VALUES (1)"),
    ("update", "UPDATE t SET a = 1 WHERE id = 1"),
    ("update without where", "UPDATE t SET a = 1"),
    ("create table", "CREATE TABLE u (a INT)"),
    ("create view", "CREATE VIEW v AS SELECT a FROM t"),
    ("create schema", "CREATE SCHEMA s"),
    ("create index", "CREATE INDEX i ON t (a)"),
    (
        "create index if not exists",
        "CREATE INDEX IF NOT EXISTS i ON t (a)",
    ),
    (
        "create index concurrently",
        "CREATE INDEX CONCURRENTLY i ON t (a)",
    ),
    ("comment", "COMMENT ON TABLE t IS 'x'"),
    ("analyze", "ANALYZE t"),
    ("vacuum", "VACUUM t"),
    ("add column", "ALTER TABLE t ADD COLUMN b INT"),
    ("filtered delete", "DELETE FROM t WHERE id = 1"),
    ("unfiltered delete", "DELETE FROM t"),
    ("drop", "DROP TABLE t"),
    ("drop, lower case", "drop table t"),
    ("drop, mixed case", "DrOp TaBlE t"),
    ("truncate", "TRUNCATE TABLE t"),
    ("truncate, lower case", "truncate t"),
    ("grant", "GRANT SELECT ON t TO u"),
    ("call", "CALL p()"),
    ("copy to a file", "COPY t TO '/tmp/x.csv'"),
    ("show tables", "SHOW TABLES"),
    ("describe", "DESCRIBE t"),
    ("plain explain", "EXPLAIN SELECT a FROM t"),
    (
        "delete in a cte",
        "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM d",
    ),
    (
        "delete after a cte",
        "WITH s AS (SELECT 1) DELETE FROM t WHERE id IN (SELECT * FROM s)",
    ),
    (
        "plain explain of a delete",
        "EXPLAIN DELETE FROM t WHERE id = 1",
    ),
    (
        "explain analyze of a delete",
        "EXPLAIN ANALYZE DELETE FROM t WHERE id = 1",
    ),
    (
        "explain with analyze on, of a delete",
        "EXPLAIN (ANALYZE TRUE) DELETE FROM t WHERE id = 1",
    ),
    (
        "explain analyze of an update",
        "EXPLAIN ANALYZE UPDATE t SET a = 1 WHERE id = 1",
    ),
    (
        "explain analyze of a select",
        "EXPLAIN ANALYZE SELECT a FROM t",
    ),
    ("analyze of a delete", "ANALYZE DELETE FROM t WHERE id = 1"),
    ("delete in an if", "IF 1 = 1 DELETE FROM t WHERE id = 1"),
    (
        "delete in an else",
        "IF 1 = 1 SELECT 1 ELSE DELETE FROM t WHERE id = 1",
    ),
    ("delete in a block", "BEGIN DELETE FROM t WHERE id = 1; END"),
    ("delete as a mutation", "ALTER TABLE t DELETE WHERE id = 1"),
    (
        "update as a mutation",
        "ALTER TABLE t UPDATE a = 1 WHERE id = 1",
    ),
    ("drop a column", "ALTER TABLE t DROP COLUMN b"),
    ("rename a table", "ALTER TABLE t RENAME TO u"),
    (
        "change a column type",
        "ALTER TABLE t ALTER COLUMN a TYPE TEXT",
    ),
    ("drop a column, lower case", "alter table t drop column b"),
    (
        "add a column and drop one",
        "ALTER TABLE t ADD COLUMN b INT, DROP COLUMN c",
    ),
    ("alter with no operation", "ALTER TABLE t"),
    ("drop with no object", "DROP TABLE"),
    (
        "a second statement in an executable comment",
        "SELECT 1 /*M!100100 ; SELECT 2 */",
    ),
    (
        "a delete as a second statement in an executable comment",
        "SELECT 1 /*M!100100 ; DELETE FROM t WHERE id = 1 */",
    ),
    (
        "a delete wholly in an executable comment",
        "/*M!100100 DELETE FROM t WHERE id = 1 */",
    ),
    (
        "a transaction block",
        "BEGIN; DELETE FROM t WHERE id = 1; COMMIT",
    ),
    (
        "merge that deletes",
        "MERGE INTO t USING u ON t.id = u.id WHEN MATCHED THEN DELETE",
    ),
    (
        "create or replace table",
        "CREATE OR REPLACE TABLE t (a INT)",
    ),
    (
        "insert overwrite",
        "INSERT OVERWRITE TABLE t SELECT * FROM u",
    ),
    ("select into a table", "SELECT * INTO u FROM t"),
    ("copy to the client", "COPY t TO STDOUT"),
    ("prepare", "PREPARE p AS SELECT 1"),
    ("execute", "EXECUTE p"),
    ("deallocate", "DEALLOCATE p"),
    ("do block", "DO $$ BEGIN DELETE FROM t; END $$"),
    ("load data", "LOAD DATA INFILE 'x.csv' INTO TABLE t"),
    (
        "select into outfile",
        "SELECT a INTO OUTFILE '/tmp/x' FROM t",
    ),
    (
        "select into outfile at the end",
        "SELECT a FROM t INTO OUTFILE '/tmp/x'",
    ),
    ("declare a cursor", "DECLARE c CURSOR FOR SELECT 1"),
    ("fetch from a cursor", "FETCH NEXT FROM c"),
    ("listen", "LISTEN c"),
    ("notify", "NOTIFY c"),
    ("lock a table", "LOCK TABLE t"),
    ("refresh a materialized view", "REFRESH MATERIALIZED VIEW v"),
    ("reindex", "REINDEX TABLE t"),
    ("cluster", "CLUSTER t"),
    (
        "create a function",
        "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1' LANGUAGE SQL",
    ),
    (
        "create a trigger",
        "CREATE TRIGGER g AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
    ),
    ("create an extension", "CREATE EXTENSION x"),
    ("attach a database", "ATTACH 'x.db' AS x"),
    ("install an extension", "INSTALL httpfs"),
    ("load an extension", "LOAD httpfs"),
    ("export a database", "EXPORT DATABASE '/tmp/x'"),
    ("optimize a table", "OPTIMIZE TABLE t FINAL"),
    ("kill a session", "KILL 1"),
    ("xa transaction", "XA START 'x'"),
    ("begin", "BEGIN"),
    (
        "start a read-write transaction",
        "START TRANSACTION READ WRITE",
    ),
    ("commit", "COMMIT"),
    ("rollback", "ROLLBACK"),
    ("set the search path", "SET search_path = other"),
    ("set a role", "SET ROLE other"),
    (
        "set the session transaction",
        "SET SESSION TRANSACTION READ WRITE",
    ),
    ("set a variable", "SET x = 1"),
    (
        "set a server-wide setting",
        "SET GLOBAL max_connections = 1",
    ),
    ("use a database", "USE other"),
    (
        "create a temporary table",
        "CREATE TEMPORARY TABLE u (a INT)",
    ),
    ("create a temporary view", "CREATE TEMP VIEW v AS SELECT 1"),
    (
        "create or replace a temporary view",
        "CREATE OR REPLACE TEMP VIEW v AS SELECT 1",
    ),
    (
        "select into a temporary table under a union",
        "SELECT * INTO TEMP u FROM t UNION ALL SELECT * FROM w",
    ),
    ("assign a user variable", "SELECT @x := 1"),
    ("select into a user variable", "SELECT 1 INTO @x"),
    ("savepoint", "SAVEPOINT a"),
    ("release a savepoint", "RELEASE SAVEPOINT a"),
    ("roll back to a savepoint", "ROLLBACK TO SAVEPOINT a"),
    ("read a sequence's current value", "SELECT currval('s')"),
    ("read the last insert id", "SELECT last_insert_id()"),
    ("named argument", "SELECT make_interval(days := 10)"),
    ("replace into", "REPLACE INTO t (a) VALUES (1)"),
    (
        "insert or replace",
        "INSERT OR REPLACE INTO t (a) VALUES (1)",
    ),
    ("select for update", "SELECT a FROM t FOR UPDATE"),
    (
        "insert on conflict",
        "INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING",
    ),
    ("pragma read", "PRAGMA table_info(t)"),
    ("empty", ""),
    ("whitespace only", "   "),
    ("line comment only", "-- nothing"),
    ("block comment only", "/* x */"),
    ("two reads", "SELECT 1; SELECT 2"),
    ("two drops", "DROP TABLE a; DROP TABLE b"),
    ("read then garbage", "SELECT 1; garbage"),
    ("a typo", "selec 1"),
    ("read, then a line comment", "SELECT 1; -- c"),
    ("line comment, then a read", "-- c\nSELECT 1"),
    ("block comment, then a read", "/* c */ SELECT 1"),
    ("read, then a hash comment", "SELECT 1 # c"),
];

/// The kinds of the engines that are not SQL, in the order they are recorded:
/// the engine, a case name and the text sent. A call is one command on Redis
/// and one mongosh statement on MongoDB.
const NONSQL_KINDS: &[(Engine, &str, &str)] = &[
    (Engine::MongoDb, "find", "db.c.find({a: 1})"),
    (Engine::MongoDb, "count", "db.c.countDocuments({})"),
    (
        Engine::MongoDb,
        "aggregate",
        "db.c.aggregate([{$match: {a: 1}}])",
    ),
    (Engine::MongoDb, "show collections", "show collections"),
    (Engine::MongoDb, "show databases", "show dbs"),
    (Engine::MongoDb, "list indexes", "db.c.getIndexes()"),
    (
        Engine::MongoDb,
        "run a read command",
        "db.runCommand({ping: 1})",
    ),
    (Engine::MongoDb, "insert one", "db.c.insertOne({a: 1})"),
    (Engine::MongoDb, "insert many", "db.c.insertMany([{a: 1}])"),
    (
        Engine::MongoDb,
        "update one",
        "db.c.updateOne({a: 1}, {$set: {a: 2}})",
    ),
    (
        Engine::MongoDb,
        "update many",
        "db.c.updateMany({a: 1}, {$set: {a: 2}})",
    ),
    (
        Engine::MongoDb,
        "update many without a filter",
        "db.c.updateMany({}, {$set: {a: 2}})",
    ),
    (
        Engine::MongoDb,
        "replace one",
        "db.c.replaceOne({a: 1}, {a: 2})",
    ),
    (
        Engine::MongoDb,
        "find one and update",
        "db.c.findOneAndUpdate({a: 1}, {$set: {a: 2}})",
    ),
    (
        Engine::MongoDb,
        "find one and replace",
        "db.c.findOneAndReplace({a: 1}, {a: 2})",
    ),
    (
        Engine::MongoDb,
        "create an index",
        "db.c.createIndex({a: 1})",
    ),
    (
        Engine::MongoDb,
        "aggregate into another collection",
        "db.c.aggregate([{$match: {a: 1}}, {$merge: {into: \"x\"}}])",
    ),
    (
        Engine::MongoDb,
        "rename a collection",
        "db.c.renameCollection(\"x\")",
    ),
    (
        Engine::MongoDb,
        "rename without dropping the target",
        "db.c.renameCollection(\"x\", false)",
    ),
    (
        Engine::MongoDb,
        "find and modify with an update",
        "db.runCommand({findAndModify: \"c\", query: {a: 1}, update: {$set: {a: 2}}})",
    ),
    (
        Engine::MongoDb,
        "find and modify with remove false",
        "db.runCommand({findAndModify: \"c\", query: {a: 1}, remove: false, update: {$set: {a: 2}}})",
    ),
    (
        Engine::MongoDb,
        "create a collection",
        "db.createCollection(\"x\")",
    ),
    (Engine::MongoDb, "delete one", "db.c.deleteOne({_id: 1})"),
    (Engine::MongoDb, "delete many", "db.c.deleteMany({a: 1})"),
    (
        Engine::MongoDb,
        "delete many without a filter",
        "db.c.deleteMany({})",
    ),
    (
        Engine::MongoDb,
        "find one and delete",
        "db.c.findOneAndDelete({a: 1})",
    ),
    (
        Engine::MongoDb,
        "delete command",
        "db.runCommand({delete: \"c\", deletes: [{q: {a: 1}, limit: 1}]})",
    ),
    (
        Engine::MongoDb,
        "delete command on admin",
        "db.adminCommand({delete: \"c\", deletes: [{q: {a: 1}, limit: 0}]})",
    ),
    (
        Engine::MongoDb,
        "find and modify that removes",
        "db.runCommand({findAndModify: \"c\", query: {a: 1}, remove: true})",
    ),
    (Engine::MongoDb, "legacy remove", "db.c.remove({})"),
    (
        Engine::MongoDb,
        "legacy remove with a filter",
        "db.c.remove({a: 1})",
    ),
    (
        Engine::MongoDb,
        "legacy remove, one document",
        "db.c.remove({a: 1}, true)",
    ),
    (
        Engine::MongoDb,
        "legacy remove with options",
        "db.c.remove({a: 1}, {justOne: true})",
    ),
    (
        Engine::MongoDb,
        "legacy remove on a named collection",
        "db.getCollection(\"c\").remove({})",
    ),
    (
        Engine::MongoDb,
        "legacy remove on a sibling database",
        "db.getSiblingDB(\"x\").c.remove({})",
    ),
    (
        Engine::MongoDb,
        "legacy remove, then a read",
        "db.c.remove({}); db.c.find()",
    ),
    (Engine::MongoDb, "drop a collection", "db.c.drop()"),
    (Engine::MongoDb, "drop a database", "db.dropDatabase()"),
    (Engine::MongoDb, "drop an index", "db.c.dropIndex(\"i\")"),
    (
        Engine::MongoDb,
        "aggregate out",
        "db.c.aggregate([{$out: \"x\"}])",
    ),
    (
        Engine::MongoDb,
        "rename and drop the target",
        "db.c.renameCollection(\"x\", true)",
    ),
    (
        Engine::MongoDb,
        "a where clause",
        "db.c.find({$where: \"1\"})",
    ),
    (
        Engine::MongoDb,
        "a function in an expression",
        "db.c.find({$expr: {$function: {body: \"1\", args: [], lang: \"js\"}}})",
    ),
    (
        Engine::MongoDb,
        "an accumulator",
        "db.c.aggregate([{$group: {_id: 1, n: {$accumulator: {init: \"1\", accumulate: \"1\", accumulateArgs: [], merge: \"1\", lang: \"js\"}}}}])",
    ),
    (
        Engine::MongoDb,
        "a where clause in a chained call",
        "db.c.find({a: 1}).sort({$where: \"1\"})",
    ),
    (
        Engine::MongoDb,
        "current op",
        "db.adminCommand({currentOp: 1})",
    ),
    (
        Engine::MongoDb,
        "current op of every client",
        "db.adminCommand({currentOp: 1, $all: true})",
    ),
    (
        Engine::MongoDb,
        "current op through runCommand",
        "db.runCommand({currentOp: 1})",
    ),
    (
        Engine::MongoDb,
        "current op stage",
        "db.adminCommand({aggregate: 1, pipeline: [{$currentOp: {allUsers: true}}], cursor: {}})",
    ),
    (Engine::MongoDb, "current op helper", "db.currentOp()"),
    (
        Engine::MongoDb,
        "current op stage on the database",
        "db.getSiblingDB(\"admin\").aggregate([{$currentOp: {allUsers: true}}])",
    ),
    (
        Engine::MongoDb,
        "connection status with privileges",
        "db.runCommand({connectionStatus: 1, showPrivileges: true})",
    ),
    (
        Engine::MongoDb,
        "connection status",
        "db.runCommand({connectionStatus: 1})",
    ),
    (
        Engine::MongoDb,
        "connection status, privileges off",
        "db.runCommand({connectionStatus: 1, showPrivileges: false})",
    ),
    (
        Engine::MongoDb,
        "a field named currentOp",
        "db.c.find({currentOp: 1})",
    ),
    (Engine::MongoDb, "two reads", "db.c.find(); db.c.find()"),
    (
        Engine::MongoDb,
        "two deletes",
        "db.c.deleteMany({}); db.c.deleteMany({})",
    ),
    (Engine::MongoDb, "empty", ""),
    (Engine::MongoDb, "a comment only", "// nothing"),
    (
        Engine::MongoDb,
        "a method DBDelve does not know",
        "db.c.bulkWrite([])",
    ),
    (Engine::MongoDb, "garbage", "db.c.find("),
    (Engine::Redis, "read a key", "GET k"),
    (Engine::Redis, "read keys", "KEYS *"),
    (Engine::Redis, "scan", "SCAN 0"),
    (Engine::Redis, "ttl", "TTL k"),
    (Engine::Redis, "hash read", "HGETALL h"),
    (Engine::Redis, "config help", "CONFIG HELP"),
    (Engine::Redis, "client info", "CLIENT INFO"),
    (Engine::Redis, "client getname", "CLIENT GETNAME"),
    (Engine::Redis, "acl list", "ACL LIST"),
    (Engine::Redis, "select a database", "SELECT 1"),
    (Engine::Redis, "subscribe", "SUBSCRIBE n"),
    (Engine::Redis, "pattern subscribe", "PSUBSCRIBE n*"),
    (Engine::Redis, "monitor", "MONITOR"),
    (Engine::Redis, "sync", "SYNC"),
    (Engine::Redis, "hello", "HELLO 2"),
    (Engine::Redis, "reset", "RESET"),
    (Engine::Redis, "quit", "QUIT"),
    (Engine::Redis, "client reply", "CLIENT REPLY OFF"),
    (Engine::Redis, "monitor then a read", "MONITOR\nGET a"),
    (Engine::Redis, "set", "SET k v"),
    (Engine::Redis, "hash set", "HSET h f v"),
    (Engine::Redis, "expire", "EXPIRE k 10"),
    (Engine::Redis, "persist", "PERSIST k"),
    (Engine::Redis, "expire at a past time", "EXPIREAT k 1"),
    (Engine::Redis, "expire at in milliseconds", "PEXPIREAT k 1"),
    (Engine::Redis, "stream add", "XADD s * f v"),
    (Engine::Redis, "set with a timeout", "SET k v EX 10"),
    (
        Engine::Redis,
        "set with a timeout the server rejects",
        "SET k v EX 0",
    ),
    (Engine::Redis, "get and expire", "GETEX k EX 10"),
    (
        Engine::Redis,
        "hash field expire",
        "HEXPIRE h 10 FIELDS 1 f",
    ),
    (
        Engine::Redis,
        "hash field expire at a fixed time in milliseconds",
        "HPEXPIREAT h 1 FIELDS 1 f",
    ),
    (
        Engine::Redis,
        "hash set with a timeout",
        "HSETEX h EX 10 FIELDS 1 f v",
    ),
    (
        Engine::Redis,
        "hash get and expire",
        "HGETEX h EX 10 FIELDS 1 f",
    ),
    (
        Engine::Redis,
        "set with an expiry in seconds",
        "SETEX k 10 v",
    ),
    (
        Engine::Redis,
        "set with an expiry in milliseconds",
        "PSETEX k 10 v",
    ),
    (Engine::Redis, "rename", "RENAME a b"),
    (Engine::Redis, "store an intersection", "SINTERSTORE d a b"),
    (Engine::Redis, "copy over", "COPY a b REPLACE"),
    (Engine::Redis, "list move", "LMOVE a b LEFT RIGHT"),
    (Engine::Redis, "set move", "SMOVE a b m"),
    (Engine::Redis, "delete", "DEL k"),
    (Engine::Redis, "delete, lower case", "del k"),
    (Engine::Redis, "unlink", "UNLINK k"),
    (Engine::Redis, "get and delete", "GETDEL k"),
    (Engine::Redis, "hash delete", "HDEL h f"),
    (Engine::Redis, "list pop", "LPOP l"),
    (Engine::Redis, "blocking list pop", "BLPOP l 1"),
    (Engine::Redis, "list remove", "LREM l 0 v"),
    (Engine::Redis, "list trim", "LTRIM l 0 0"),
    (Engine::Redis, "set remove", "SREM s m"),
    (Engine::Redis, "set pop", "SPOP s"),
    (Engine::Redis, "sorted set remove", "ZREM z m"),
    (Engine::Redis, "sorted set pop", "ZPOPMIN z"),
    (
        Engine::Redis,
        "sorted set remove range",
        "ZREMRANGEBYRANK z 0 1",
    ),
    (Engine::Redis, "stream delete", "XDEL s 1-1"),
    (Engine::Redis, "stream trim", "XTRIM s MAXLEN 0"),
    (
        Engine::Redis,
        "stream add that trims",
        "XADD s MAXLEN 0 * f v",
    ),
    (
        Engine::Redis,
        "stream add that trims by id",
        "XADD s MINID 9999999999999-0 * f v",
    ),
    (Engine::Redis, "json delete", "JSON.DEL d $"),
    (Engine::Redis, "time series delete", "TS.DEL t 0 1"),
    (
        Engine::Redis,
        "destroy a consumer group",
        "XGROUP DESTROY s g",
    ),
    (
        Engine::Redis,
        "delete a consumer",
        "XGROUP DELCONSUMER s g c",
    ),
    (Engine::Redis, "migrate", "MIGRATE h 6379 k 0 1000"),
    (Engine::Redis, "expire at zero", "EXPIRE k 0"),
    (Engine::Redis, "expire at minus one", "EXPIRE k -1"),
    (Engine::Redis, "expire with a leading zero", "EXPIRE k 010"),
    (
        Engine::Redis,
        "expire in milliseconds at zero",
        "PEXPIRE k 0",
    ),
    (
        Engine::Redis,
        "hash field expire at zero",
        "HEXPIRE k 0 FIELDS 1 f",
    ),
    (Engine::Redis, "set at a fixed time", "SET k v EXAT 1"),
    (
        Engine::Redis,
        "set at a fixed time in milliseconds",
        "SET k v PXAT 1",
    ),
    (
        Engine::Redis,
        "get and expire at a fixed time",
        "GETEX k EXAT 1",
    ),
    (
        Engine::Redis,
        "hash set at a fixed time",
        "HSETEX h EXAT 1 FIELDS 1 f v",
    ),
    (
        Engine::Redis,
        "hash get and expire at a fixed time",
        "HGETEX h PXAT 1 FIELDS 1 f",
    ),
    (
        Engine::Redis,
        "hash set expiring at zero",
        "HSETEX h EX 0 FIELDS 1 f v",
    ),
    (
        Engine::Redis,
        "hash get expiring at zero",
        "HGETEX h PX 0 FIELDS 1 f",
    ),
    (Engine::Redis, "eval", "EVAL \"return 1\" 0"),
    (Engine::Redis, "eval sha", "EVALSHA abc 0"),
    (Engine::Redis, "fcall", "FCALL f 0"),
    (
        Engine::Redis,
        "eval, read-only form",
        "EVAL_RO \"return 1\" 0",
    ),
    (
        Engine::Redis,
        "eval sha, read-only form",
        "EVALSHA_RO abc 0",
    ),
    (Engine::Redis, "fcall, read-only form", "FCALL_RO f 0"),
    (Engine::Redis, "load a script", "SCRIPT LOAD \"return 1\""),
    (
        Engine::Redis,
        "load a function",
        "FUNCTION LOAD \"#!lua name=x\"",
    ),
    (
        Engine::Redis,
        "config get a password",
        "CONFIG GET requirepass",
    ),
    (Engine::Redis, "config get everything", "CONFIG GET *"),
    (
        Engine::Redis,
        "config get, lower case",
        "config get maxmemory",
    ),
    (Engine::Redis, "client list", "CLIENT LIST"),
    (Engine::Redis, "acl getuser", "ACL GETUSER u"),
    (Engine::Redis, "acl whoami", "ACL WHOAMI"),
    (Engine::Redis, "acl users", "ACL USERS"),
    (Engine::Redis, "acl log", "ACL LOG"),
    (
        Engine::Redis,
        "config get, then an unknown command",
        "CONFIG GET x\nFROBNICATE",
    ),
    (Engine::Redis, "flush a database", "FLUSHDB"),
    (Engine::Redis, "flush everything", "FLUSHALL"),
    (Engine::Redis, "config set", "CONFIG SET maxmemory 1"),
    (Engine::Redis, "shutdown", "SHUTDOWN"),
    (Engine::Redis, "delete a function", "FUNCTION DELETE f"),
    (Engine::Redis, "multi", "MULTI"),
    (Engine::Redis, "exec", "EXEC"),
    (Engine::Redis, "discard", "DISCARD"),
    (Engine::Redis, "watch", "WATCH k"),
    (Engine::Redis, "auth", "AUTH u p"),
    (Engine::Redis, "client setname", "CLIENT SETNAME me"),
    (Engine::Redis, "a transaction", "MULTI\nSET k v\nEXEC"),
    (Engine::Redis, "two reads", "GET a\nGET b"),
    (Engine::Redis, "two deletes", "DEL a\nDEL b"),
    (Engine::Redis, "empty", ""),
    (Engine::Redis, "a comment only", "# note"),
    (Engine::Redis, "a comment, then a read", "# note\nGET a"),
    (Engine::Redis, "an unknown command", "FROBNICATE a"),
    (Engine::Redis, "an unclosed quote", "SET k \"open"),
    (Engine::Redis, "a read, then garbage", "GET a\nFROBNICATE"),
];

/// The text of one kind on one engine, `None` where the engine has none: the
/// deny-listed call of an engine that lists no function.
fn text_of(engine: Engine, sql: &str) -> Option<String> {
    if sql != DENIED_CALL {
        return Some(sql.to_owned());
    }
    engine
        .spec()
        .sql
        .denied_functions
        .first()
        .map(|name| format!("SELECT {name}()"))
}

fn sql_engines() -> impl Iterator<Item = Engine> {
    Engine::ALL
        .into_iter()
        .filter(|engine| engine.syntax() == Syntax::Sql)
}

/// Every `(engine, case, text)` the corpus records, in the order it records
/// them: `Engine::ALL`, then the kinds, skipping what an engine has no text for.
pub(crate) fn sql_cells() -> Vec<(Engine, String, String)> {
    sql_engines()
        .flat_map(|engine| {
            SQL_KINDS.iter().filter_map(move |(case, sql)| {
                text_of(engine, sql).map(|text| (engine, (*case).to_owned(), text))
            })
        })
        .collect()
}

/// Every `(engine, case, text)` the non-SQL corpus records, in the order it
/// records them.
pub(crate) fn nonsql_cells() -> Vec<(Engine, String, String)> {
    NONSQL_KINDS
        .iter()
        .map(|(engine, case, text)| (*engine, (*case).to_owned(), (*text).to_owned()))
        .collect()
}

/// The shapes of DuckDB's bare-path reads (D-03), recorded on every engine: a
/// quoted string in each table position, then the two statements whose
/// literal is no path and the one that is dotted without being one, which
/// DuckDB asks about all the same.
const BARE_PATHS: &[(&str, &str)] = &[
    ("a quoted path", "SELECT * FROM 'data.csv'"),
    ("a double-quoted path", "SELECT * FROM \"data.csv\""),
    ("a url", "SELECT * FROM 'https://example.com/x.parquet'"),
    ("a comma join", "SELECT * FROM 'a.csv' a, 'b.csv' b"),
    ("a join", "SELECT * FROM t JOIN 'b.csv' b ON t.id = b.id"),
    (
        "a subquery",
        "SELECT * FROM t WHERE id IN (SELECT id FROM 'b.csv')",
    ),
    (
        "a dotted literal that is no path",
        "SELECT * FROM t WHERE v = '1.2'",
    ),
    (
        "a timestamp literal",
        "SELECT * FROM t WHERE ts = '2024-01-02 03:04:05'",
    ),
];

/// Every `(engine, case, text)` the egress corpus records, in the order it
/// records them: for each SQL engine, `SELECT * FROM {name}('x')` for each name
/// on its egress list, then the bare-path shapes on every engine, MongoDB and
/// Redis included, as whatever their parsers make of them. An engine with no
/// list is recorded as `(no egress list)` by [`agent_gate_corpus_for_egress`].
pub(crate) fn egress_cells() -> Vec<(Engine, String, String)> {
    Engine::ALL
        .into_iter()
        .flat_map(|engine| {
            let calls = if engine.syntax() == Syntax::Sql {
                engine.spec().sql.egress_functions
            } else {
                &[]
            };
            calls
                .iter()
                .map(move |name| {
                    (
                        engine,
                        format!("call {name}"),
                        format!("SELECT * FROM {name}('x')"),
                    )
                })
                .chain(
                    BARE_PATHS
                        .iter()
                        .map(move |(case, text)| (engine, (*case).to_owned(), (*text).to_owned())),
                )
        })
        .collect()
}

/// The three answers for `text`, in Read-only, Read-write and Full.
fn answers(engine: Engine, text: &str) -> String {
    let [ro, rw, full] = Mode::ALL.map(|mode| answer(&sql::agent_gate(engine, text, mode)));
    format!("ro={ro} rw={rw} full={full}")
}

fn answer(verdict: &AgentVerdict) -> String {
    match verdict {
        AgentVerdict::Run => "run".to_owned(),
        AgentVerdict::Approve(verdict, None) => format!("approve({:?})", verdict.mode),
        AgentVerdict::Approve(verdict, Some(expiry)) => {
            format!("approve({:?}, {expiry:?})", verdict.mode)
        }
        AgentVerdict::Refuse(refusal) => format!("refuse({refusal:?})"),
    }
}

#[test]
fn agent_gate_corpus_for_sql_engines() {
    let mut out = String::new();
    for engine in sql_engines() {
        for (case, sql) in SQL_KINDS {
            let Some(text) = text_of(engine, sql) else {
                fragment(&mut out, engine, case, None);
                continue;
            };
            fragment(
                &mut out,
                engine,
                &format!("{case}: {}", visible(&text)),
                Some(&answers(engine, &text)),
            );
        }
    }
    check("agent_gate_sql", &out);
}

#[test]
fn agent_gate_corpus_for_mongodb_and_redis() {
    let mut out = String::new();
    for (engine, case, text) in nonsql_cells() {
        fragment(
            &mut out,
            engine,
            &format!("{case}: {}", visible(&text)),
            Some(&answers(engine, &text)),
        );
    }
    check("agent_gate_nonsql", &out);
}

#[test]
fn agent_gate_corpus_for_egress() {
    let mut out = String::new();
    for engine in Engine::ALL {
        if engine.syntax() == Syntax::Sql && engine.spec().sql.egress_functions.is_empty() {
            fragment(&mut out, engine, "(no egress list)", None);
        }
        for (recorded, case, text) in egress_cells() {
            if recorded != engine {
                continue;
            }
            fragment(
                &mut out,
                engine,
                &format!("{case}: {}", visible(&text)),
                Some(&answers(engine, &text)),
            );
        }
    }
    check("agent_gate_egress", &out);
}

/// What an agent's `explain_query` composes around: a case name, the statement
/// it wraps, and what the gate owes the composition. SQL engines.
const EXPLAIN_SQL_KINDS: &[(&str, &str, Owed)] = &[
    ("read", "SELECT a FROM t", Owed::Run),
    ("update", "UPDATE t SET a = 1 WHERE id = 1", Owed::NotRun),
    ("delete", "DELETE FROM t WHERE id = 1", Owed::Refused),
    (
        "writable cte",
        "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING id) SELECT id FROM d",
        Owed::NotRun,
    ),
];

/// The same on MongoDB, whose plan is a suffix on the cursor.
const EXPLAIN_MONGO_KINDS: &[(&str, &str, Owed)] = &[
    ("read", "db.c.find({a: 1})", Owed::Run),
    (
        "update",
        "db.c.updateOne({a: 1}, {$set: {a: 2}})",
        Owed::NotRun,
    ),
    ("delete", "db.c.deleteMany({a: 1})", Owed::Refused),
    (
        "merge aggregate",
        r#"db.c.aggregate([{$match: {a: 1}}, {$merge: {into: "x"}}])"#,
        Owed::NotRun,
    ),
    (
        "where find",
        r#"db.c.find({$where: "this.a > 1"})"#,
        Owed::NotRun,
    ),
];

/// What the gate owes the explain of a statement, in every mode.
#[derive(Clone, Copy)]
enum Owed {
    Run,
    /// Anything but `Run`: a write waits for the human or is refused.
    NotRun,
    Refused,
}

#[test]
fn agent_gate_corpus_for_composed_explains() {
    let mut out = String::new();
    let mut mongo_suffixes = [false; 2];
    for engine in Engine::ALL {
        let kinds = match engine.syntax() {
            Syntax::Sql => EXPLAIN_SQL_KINDS,
            Syntax::Mongo => EXPLAIN_MONGO_KINDS,
            Syntax::Redis => &[],
        };
        for mode in ExplainMode::ALL {
            for (kind, sql, owed) in kinds {
                // The composition `explain_query` makes, never a hand-written EXPLAIN.
                let Some(text) = engine.explain(mode, sql) else {
                    fragment(&mut out, engine, &format!("{mode:?} of {kind}"), None);
                    continue;
                };
                fragment(
                    &mut out,
                    engine,
                    &format!("{mode:?} of {kind}: {}", visible(&text)),
                    Some(&answers(engine, &text)),
                );
                for gate_mode in Mode::ALL {
                    let verdict = sql::agent_gate(engine, &text, gate_mode);
                    let context = format!("{engine:?} {gate_mode:?} {mode:?} of {kind}: {text:?}");
                    match (owed, mode) {
                        (Owed::Run, _) => assert_eq!(verdict, AgentVerdict::Run, "{context}"),
                        // A plain plan executes nothing, so the plan of a write is
                        // judged as the plan it is (`agent_gate_sql.snap` pins
                        // `plain explain of a delete` as a run); only ANALYZE runs it.
                        (_, ExplainMode::Plan) => {}
                        (Owed::NotRun, ExplainMode::Analyze) => {
                            assert_ne!(verdict, AgentVerdict::Run, "{context}")
                        }
                        (Owed::Refused, ExplainMode::Analyze) => {
                            assert!(matches!(verdict, AgentVerdict::Refuse(_)), "{context}")
                        }
                    }
                }
                if engine.syntax() == Syntax::Mongo {
                    mongo_suffixes[0] |= text.contains(r#".explain("queryPlanner")"#);
                    mongo_suffixes[1] |= text.contains(r#".explain("executionStats")"#);
                }
            }
        }
    }
    assert_eq!(
        mongo_suffixes,
        [true, true],
        "MongoDB's two explain suffixes are both composed"
    );
    check("agent_gate_explain", &out);
}

/// Every cell of the corpus, SQL and not, and the egress cells.
fn all_cells() -> Vec<(Engine, String, String)> {
    let mut cells = sql_cells();
    cells.extend(nonsql_cells());
    cells.extend(egress_cells());
    cells
}

#[test]
fn every_engine_has_cells() {
    let cells = all_cells();
    for engine in Engine::ALL {
        assert!(
            cells.iter().any(|(recorded, ..)| *recorded == engine),
            "{engine:?} has no recorded cell"
        );
    }
}

#[test]
fn every_run_or_approve_cell_is_one_the_mode_already_allows() {
    for (engine, case, text) in all_cells() {
        let verdict = sql::classify(engine, &text);
        for mode in Mode::ALL {
            match sql::agent_gate(engine, &text, mode) {
                AgentVerdict::Run | AgentVerdict::Approve(..) => assert_eq!(
                    sql::gate(&verdict, mode, &[]),
                    None,
                    "{engine:?} {mode:?} {case}: {text:?}"
                ),
                AgentVerdict::Refuse(_) => {}
            }
        }
    }
}

#[test]
fn every_run_cell_is_a_rerunnable_read() {
    for (engine, case, text) in all_cells() {
        for mode in Mode::ALL {
            if sql::agent_gate(engine, &text, mode) == AgentVerdict::Run {
                assert!(
                    sql::rerunnable(engine, &text),
                    "{engine:?} {mode:?} {case}: {text:?}"
                );
            }
        }
    }
}

#[test]
fn the_full_column_equals_the_read_write_column() {
    for (engine, case, text) in all_cells() {
        assert_eq!(
            sql::agent_gate(engine, &text, Mode::Full),
            sql::agent_gate(engine, &text, Mode::ReadWrite),
            "{engine:?} {case}: {text:?}"
        );
    }
}

/// What an agent reads for a refusal is the rule that refused and nothing else:
/// no advice, no way round it.
fn reason(refusal: &sql::Refusal) -> String {
    refusal.to_string().to_lowercase()
}

#[test]
fn every_refusal_in_the_corpus_states_a_rule_and_gives_no_advice() {
    for (engine, case, text) in all_cells() {
        for mode in Mode::ALL {
            let AgentVerdict::Refuse(refusal) = sql::agent_gate(engine, &text, mode) else {
                continue;
            };
            let reason = reason(&refusal);
            assert!(!reason.is_empty(), "{engine:?} {mode:?} {case}");
            for advice in ["upgrade", "try", "instead", "please", "should"] {
                assert!(
                    !reason.contains(advice),
                    "{engine:?} {mode:?} {case}: {reason}"
                );
            }
        }
    }
}

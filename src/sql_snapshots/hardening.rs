//! The verdict for every call the classifier hardening reclassifies, recorded per
//! engine, so each change reads as a one-line diff in the `.snap` rather than as
//! a new file. A record is `classify` and `gate` for the text exactly as
//! written, on every engine, which is what shows a call moving only where the
//! engine lists it.

use super::{check, fragment};
use crate::{
    db::Engine,
    sql::{self, Mode},
};

/// Function calls whose verdict the deny-list moves, one name and its arguments
/// each, written as a plain `SELECT` so the scalar position is the one under
/// test.
const CALLS: &[(&str, &str)] = &[
    (
        "set_config",
        "'default_transaction_read_only', 'off', false",
    ),
    ("nextval", "'s'"),
    ("setval", "'s', 10"),
    ("pg_advisory_lock", "1"),
    ("pg_advisory_lock_shared", "1"),
    ("pg_advisory_xact_lock", "1"),
    ("pg_advisory_xact_lock_shared", "1"),
    ("pg_try_advisory_lock", "1"),
    ("pg_try_advisory_lock_shared", "1"),
    ("pg_try_advisory_xact_lock", "1"),
    ("pg_try_advisory_xact_lock_shared", "1"),
    ("pg_advisory_unlock", "1"),
    ("pg_advisory_unlock_shared", "1"),
    ("pg_advisory_unlock_all", ""),
    ("pg_notify", "'c', 'p'"),
    ("pg_terminate_backend", "0"),
    ("pg_cancel_backend", "0"),
    ("pg_reload_conf", ""),
    ("pg_rotate_logfile", ""),
    ("pg_switch_wal", ""),
    ("pg_create_restore_point", "'x'"),
    ("pg_stat_reset", ""),
    ("pg_stat_reset_shared", "'bgwriter'"),
    ("pg_stat_reset_single_table_counters", "1"),
    ("pg_stat_reset_single_function_counters", "1"),
    ("pg_stat_reset_slru", "'CommitTs'"),
    ("pg_stat_reset_replication_slot", "'s'"),
    ("lo_create", "0"),
    ("lo_import", "'/etc/hostname'"),
    ("lo_export", "1, '/tmp/x'"),
    ("lo_unlink", "1"),
    ("lo_from_bytea", "1, 'abc'::bytea"),
    ("lo_put", "1, 0, 'abc'::bytea"),
    ("lo_creat", "-1"),
    ("lowrite", "0, 'abc'::bytea"),
    ("lo_truncate", "0, 1"),
    ("lo_truncate64", "0, 1"),
    ("lo_get", "1"),
    ("loread", "0, 10"),
    ("dblink", "'c', 'SELECT 1'"),
    ("dblink_exec", "'c', 'DELETE FROM t'"),
    ("dblink_connect", "'c', 'dbname=x'"),
    ("dblink_connect_u", "'c', 'dbname=x'"),
    ("dblink_send_query", "'c', 'SELECT 1'"),
    ("dblink_open", "'c', 'cur', 'SELECT 1'"),
    ("query_to_xml", "'select 1', true, false, ''"),
    ("query_to_xml_and_xmlschema", "'select 1', true, false, ''"),
    ("query_to_xmlschema", "'select 1', true, false, ''"),
    ("ts_stat", "'select to_tsvector(''a'')'"),
    (
        "ts_rewrite",
        "'a & b'::tsquery, 'select ''c''::tsquery, ''d''::tsquery'",
    ),
    ("crosstab", "'select ''a''::text, ''b''::text, ''c''::text'"),
    ("crosstab2", "'select 1, 2, 3'"),
    ("crosstab3", "'select 1, 2, 3'"),
    ("crosstab4", "'select 1, 2, 3'"),
    ("connectby", "'rel', 'k', 'p', '0', 0"),
    ("xpath_table", "'id', 'doc', 'rel', '/r/a', 'true'"),
    ("st_findextent", "'t', 'a'"),
    ("pg_file_write", "'x', 'abc', false"),
    ("pg_file_rename", "'x', 'y'"),
    ("pg_file_unlink", "'x'"),
    ("pg_file_sync", "'x'"),
    ("get_lock", "'k', 1"),
    ("release_lock", "'k'"),
    ("release_all_locks", ""),
    ("load_extension", "'x'"),
    ("query", "'select 1'"),
    ("json_execute_serialized_sql", "'{}'"),
    ("system$cancel_all_queries", "1"),
    ("system$cancel_query", "'q'"),
    ("system$abort_session", "1"),
    ("system$abort_transaction", "1"),
    ("getnextval", "seq1"),
    ("openquery", "srv, 'SELECT 1'"),
    ("openrowset", "'SQLNCLI', 'x', 'DELETE FROM t'"),
    ("opendatasource", "'SQLNCLI', 'x'"),
];

/// The same call in each position the parser can put one, and the controls that
/// must never move: a function nobody listed, and a projection calling nothing.
const SHAPES: &[(&str, &str)] = &[
    ("table position", "SELECT * FROM nextval('s')"),
    (
        "lateral table position",
        "SELECT * FROM t, LATERAL nextval('s') x",
    ),
    (
        "schema-qualified",
        "SELECT pg_catalog.set_config('a', 'b', false)",
    ),
    (
        "where subquery",
        "SELECT * FROM t WHERE id = (SELECT nextval('s'))",
    ),
    (
        "cte body",
        "WITH n AS (SELECT nextval('s') AS v) SELECT v FROM n",
    ),
    ("comment before paren", "SELECT nextval /* c */ ('s')"),
    (
        "two denied calls",
        "SELECT nextval('s'), set_config('a', 'b', false)",
    ),
    (
        "two denied calls reversed",
        "SELECT set_config('a', 'b', false), nextval('s')",
    ),
    (
        "openquery in table position",
        "SELECT * FROM OPENQUERY(srv, 'SELECT 1')",
    ),
    (
        "openrowset in table position",
        "SELECT * FROM OPENROWSET('SQLNCLI', 'x', 'DELETE FROM t')",
    ),
    (
        "opendatasource in table position",
        "SELECT * FROM OPENDATASOURCE('SQLNCLI', 'x')",
    ),
    (
        "table function wrapper",
        "SELECT * FROM TABLE(GETNEXTVAL(seq1))",
    ),
    ("sequence without parentheses", "SELECT s.nextval"),
    (
        "sequence without parentheses, qualified",
        "SELECT db.s.nextval FROM t",
    ),
    (
        "unicode-escaped name",
        r#"SELECT U&"set_c\006fnfig"('default_transaction_read_only', 'off', false)"#,
    ),
    (
        "unicode-escaped name, no escape sequence",
        r#"SELECT U&"set_config"('default_transaction_read_only', 'off', false)"#,
    ),
    (
        "unicode-escaped name, schema-qualified",
        r#"SELECT pg_catalog.U&"set_c\006fnfig"('a', 'b', false)"#,
    ),
    (
        "control spaced bit-and of a quoted column",
        r#"SELECT U & "x" FROM t"#,
    ),
    ("control bit-and of an unquoted column", "SELECT U&x FROM t"),
    ("control qualified column", "SELECT t.id FROM t"),
    (
        "control qualified column sharing no listed name",
        "SELECT t.nextvalue FROM t",
    ),
    ("control session read lastval", "SELECT lastval()"),
    ("control session read currval", "SELECT currval('s')"),
    ("control last_insert_id", "SELECT last_insert_id()"),
    ("control sleep", "SELECT sleep(1)"),
    ("control file reader", "SELECT * FROM read_csv('f.csv')"),
    ("control file string factor", "SELECT * FROM 'f.csv'"),
    (
        "control pg_read_file",
        "SELECT pg_read_file('/etc/hostname')",
    ),
    ("control pg_logdir_ls", "SELECT * FROM pg_logdir_ls()"),
    ("control load_file", "SELECT LOAD_FILE('/etc/hostname')"),
    ("control user-defined call", "SELECT my_function(1)"),
    ("control plain projection", "SELECT a, b FROM t"),
    (
        "sql-text executor carrying a disarm",
        "SELECT query_to_xml('select set_config(''default_transaction_read_only'', ''off'', false)', true, false, '')",
    ),
    (
        "sql-text executor in table position",
        "SELECT count(*) FROM ts_stat($q$select to_tsvector(set_config('default_transaction_read_only', 'off', false))$q$)",
    ),
    (
        "sql-text executor carrying a write",
        "SELECT query_to_xml('select lo_from_bytea(970972, ''abc''::bytea)', true, false, '')",
    ),
    (
        "sql-text executor with an unnamed text argument",
        "SELECT ts_rewrite('a & b'::tsquery, 'select set_config(''default_transaction_read_only'', ''off'', false)::tsquery, ''c''::tsquery')",
    ),
    (
        "sql-text executor with an unnamed text argument carrying a write",
        "SELECT ts_rewrite('a & b'::tsquery, 'select lo_from_bytea(970974, ''abc''::bytea)::text::tsquery, ''c''::tsquery')",
    ),
    (
        "extension sql-text executor carrying a disarm",
        "SELECT * FROM crosstab('select set_config(''default_transaction_read_only'', ''off'', false), ''a'', ''b''') AS t(r text, c text)",
    ),
    (
        "extension sql-text executor carrying a write",
        "SELECT * FROM crosstab('select 1::text, ''a''::text, lo_from_bytea(975001, ''abc''::bytea)::text') AS t(r text, c text)",
    ),
    (
        "extension sql-text executor splicing a relation name, carrying a write",
        "SELECT * FROM connectby('(select 1 as k, 0 as p, lo_from_bytea(975002, ''abc''::bytea) as o) q', 'k', 'p', '0', 0) AS t(k int, p int, level int)",
    ),
    (
        "extension sql-text executor splicing a relation and criteria, carrying a write",
        "SELECT * FROM xpath_table('id', 'doc', '(select 1 as id, (select lo_from_bytea(975003, ''abc''::bytea))::text as doc) s', '/r/a', 'true') AS t(id int, a text)",
    ),
    (
        "extension function splicing an identifier unescaped, carrying a write",
        "SELECT st_findextent('t', 'a\") AS extent FROM (select lo_from_bytea(975004, ''abc''::bytea), null::geometry AS a) AS q --')",
    ),
    (
        "control column named like an extension carrier",
        "SELECT t.crosstab FROM t",
    ),
    (
        "control denied name inside a literal with no executor",
        "SELECT 'select set_config(''default_transaction_read_only'', ''off'', false)'",
    ),
    (
        "duckdb text executor",
        "SELECT * FROM query('select nextval(''s'')')",
    ),
    (
        "duckdb text executor by name",
        "SELECT * FROM query_table('s')",
    ),
    (
        "duckdb serialized-plan executor",
        "SELECT * FROM json_execute_serialized_sql(json_serialize_sql('select nextval(''s'')'))",
    ),
    (
        "control column named like the duckdb text executor",
        "SELECT t.query FROM t",
    ),
    (
        "member form, snowflake sequence pseudo-column",
        "SELECT seq1.nextval",
    ),
    (
        "accepted false positive: column reference to a listed sequence name, stopped by the member form",
        "SELECT t.nextval FROM t",
    ),
    (
        "column reference to a listed name, not a member form",
        "SELECT t.setval FROM t",
    ),
    (
        "column reference to a listed lock name, not a member form",
        "SELECT x.get_lock FROM x",
    ),
    (
        "accepted false positive: listed name as a cte name with a column list",
        "WITH query(x) AS (SELECT 1) SELECT x FROM query",
    ),
    (
        "accepted false positive: listed name as a derived-table alias with a column list",
        "SELECT * FROM (SELECT 1, 2) AS query(a, b)",
    ),
];

fn record(out: &mut String, engine: Engine, case: &str, sql: &str) {
    let verdict = sql::classify(engine, sql);
    let [ro, rw, full] = Mode::ALL.map(|mode| sql::gate(&verdict, mode, &[]));
    let record = format!(
        "classify={:?} {:?} gate ro={ro:?} rw={rw:?} full={full:?}",
        verdict.mode, verdict.destructive,
    );
    fragment(out, engine, &format!("{case}: {sql}"), Some(&record));
}

#[test]
fn denied_function_corpus_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for (name, args) in CALLS {
            record(
                &mut out,
                engine,
                "scalar",
                &format!("SELECT {name}({args})"),
            );
        }
        for (case, sql) in SHAPES {
            record(&mut out, engine, case, sql);
        }
    }
    check("hardening_functions", &out);
}

/// Statements that change who the session is, or whether it can write, grouped
/// by the rule that decides them. Written once and run on every engine, so a
/// dialect that does not parse one shows `Unreadable` rather than nothing.
const SESSION_RULES: &[(&str, &str)] = &[
    ("identity change", "SET ROLE other"),
    ("identity change, local", "SET LOCAL ROLE other"),
    ("authorization change", "SET SESSION AUTHORIZATION other"),
    ("write access mode", "SET TRANSACTION READ WRITE"),
    (
        "write access mode, session keyword",
        "SET SESSION TRANSACTION READ WRITE",
    ),
    (
        "write access mode, characteristics",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
    ),
    (
        "write access mode after an isolation level",
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE",
    ),
    ("read access mode", "SET TRANSACTION READ ONLY"),
    (
        "read access mode, session keyword",
        "SET SESSION TRANSACTION READ ONLY",
    ),
    (
        "isolation level",
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    ),
    ("transaction opening, write", "BEGIN READ WRITE"),
    (
        "transaction opening, write, spelled out",
        "START TRANSACTION READ WRITE",
    ),
    (
        "transaction opening, write, with the noise word",
        "BEGIN TRANSACTION READ WRITE",
    ),
    ("transaction opening, read", "START TRANSACTION READ ONLY"),
    ("transaction opening", "BEGIN"),
    ("transaction opening, spelled out", "START TRANSACTION"),
    ("transaction commit", "COMMIT"),
    ("transaction rollback", "ROLLBACK"),
    ("savepoint", "SAVEPOINT a"),
    ("savepoint release", "RELEASE SAVEPOINT a"),
    ("bracketed read", "BEGIN; SELECT 1; COMMIT"),
    ("server-wide scope", "SET GLOBAL max_execution_time = 1"),
    (
        "server-wide transaction access mode",
        "SET GLOBAL TRANSACTION READ ONLY",
    ),
    (
        "server-wide transaction isolation level",
        "SET GLOBAL TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    ),
    (
        "persistent transaction access mode",
        "SET PERSIST TRANSACTION READ ONLY",
    ),
    (
        "persistent-only transaction access mode",
        "SET PERSIST_ONLY TRANSACTION READ ONLY",
    ),
    (
        "local transaction access mode",
        "SET LOCAL TRANSACTION READ ONLY",
    ),
    (
        "user variable beginning with the scope word",
        "SET @global = 1",
    ),
    (
        "server-wide scope, doubled-at prefix",
        "SET @@global.time_zone = 'UTC'",
    ),
    (
        "server-wide scope on a harmless name",
        "SET GLOBAL time_zone = 'UTC'",
    ),
    (
        "hold name, session prefix",
        "SET @@session.transaction_read_only = 0",
    ),
    ("hold name, doubled-at", "SET @@transaction_read_only = OFF"),
    (
        "hold name, session keyword",
        "SET SESSION transaction_read_only = 0",
    ),
    ("hold name", "SET transaction_read_only = off"),
    (
        "hold name, default",
        "SET default_transaction_read_only = off",
    ),
    ("hold name, older spelling", "SET tx_read_only = 0"),
    (
        "hold name, local",
        "SET LOCAL default_transaction_read_only = off",
    ),
    ("user variable", "SET @x = 1"),
    (
        "session setting, doubled-at",
        "SET @@session.time_zone = 'UTC'",
    ),
    (
        "session setting, doubled-at, bare",
        "SET @@time_zone = 'UTC'",
    ),
    ("time zone", "SET TIME ZONE 'UTC'"),
    ("unknown setting", "SET x = 1"),
    ("navigation setting", "SET search_path TO other"),
    ("navigation setting, equals", "SET search_path = other"),
    ("comfort setting", "SET statement_timeout = 1000"),
    ("comfort setting, time zone", "SET timezone = 'UTC'"),
    ("comfort setting, no count", "SET NOCOUNT ON"),
    ("comfort setting, auto-commit", "SET autocommit = 0"),
    ("comfort setting, time zone name", "SET time_zone = 'UTC'"),
];

/// A `SET` whose verdict is the maximum over its assignments, written both
/// ways round, and the same `SET` sitting inside the blocks the parser lets one
/// hide in. Each pair must record the same verdict.
const SESSION_SHAPES: &[(&str, &str)] = &[
    (
        "hold name after a user variable",
        "SET @x = 1, @@session.transaction_read_only = 0",
    ),
    (
        "hold name before a user variable",
        "SET @@session.transaction_read_only = 0, @x = 1",
    ),
    ("two user variables", "SET @x = 1, @y = 2"),
    ("user variable then unknown setting", "SET @x = 1, y = 2"),
    ("unknown setting then user variable", "SET y = 2, @x = 1"),
    ("two unknown settings", "SET x = 1, y = 2"),
    (
        "identity change in an IF block",
        "IF 1 = 1 THEN SET ROLE other; END IF",
    ),
    ("identity change in a T-SQL IF", "IF 1 = 1 SET ROLE other"),
    (
        "identity change in a scripting block",
        "BEGIN SET ROLE other; END",
    ),
    (
        "unknown setting in an IF block",
        "IF 1 = 1 THEN SET x = 1; END IF",
    ),
    ("unknown setting in a T-SQL IF", "IF 1 = 1 SET x = 1"),
    (
        "unknown setting in a scripting block",
        "BEGIN SET x = 1; END",
    ),
    (
        "navigation setting in an IF block",
        "IF 1 = 1 THEN SET search_path TO other; END IF",
    ),
    (
        "navigation setting in a scripting block",
        "BEGIN SET search_path TO other; END",
    ),
    (
        "write opening in a scripting block",
        "BEGIN START TRANSACTION READ WRITE; END",
    ),
    (
        "identity change under an analysing explain",
        "EXPLAIN ANALYZE SET ROLE other",
    ),
    (
        "identity change under a plain explain",
        "EXPLAIN SET ROLE other",
    ),
    (
        "two statements, hold name second",
        "SELECT 1; SET transaction_read_only = 0",
    ),
    (
        "two statements, user variable second",
        "SELECT 1; SET @x = 1",
    ),
    (
        "two statements, server-wide transaction second",
        "SELECT 1; SET GLOBAL TRANSACTION READ ONLY",
    ),
    (
        "two statements, server-wide transaction first",
        "SET GLOBAL TRANSACTION READ ONLY; SELECT 1",
    ),
    (
        "ordinary identifiers spelling the scope pair, no SET statement",
        "SELECT t.set global FROM t",
    ),
    (
        "server-wide transaction under a plain explain",
        "EXPLAIN SET GLOBAL TRANSACTION READ ONLY",
    ),
];

/// Settings that stay off every allow-list, on the engine that has them, and the
/// reason each is not navigation or comfort. A name here is stopped by the
/// default alone, so none of it is data a spec carries: the list exists to prove
/// the default holds for the names most likely to be added to an allow-list by
/// mistake.
pub(crate) const EXCLUDED_SETTINGS: &[(Engine, &[&str])] = &[
    (
        Engine::Postgres,
        &[
            // The hold itself and the identity changes.
            "default_transaction_read_only",
            "transaction_read_only",
            "role",
            "session_authorization",
            // Turn off the server's own checks.
            "session_replication_role",
            "row_security",
            "check_function_bodies",
            // Change how a quote or a backslash in the next statement is read.
            "standard_conforming_strings",
            "backslash_quote",
            "escape_string_warning",
            "client_encoding",
            // Change what a statement means or when it is durable.
            "synchronous_commit",
            "transform_null_equals",
            "array_nulls",
        ],
    ),
    (
        Engine::MySql,
        &[
            "transaction_read_only",
            "tx_read_only",
            "read_only",
            "super_read_only",
            "sql_mode",
            "character_set_client",
            "character_set_connection",
            "collation_connection",
            "foreign_key_checks",
            "unique_checks",
            "sql_log_bin",
            "sql_safe_updates",
        ],
    ),
    (
        Engine::MariaDb,
        &[
            "transaction_read_only",
            "tx_read_only",
            "read_only",
            "super_read_only",
            "sql_mode",
            "character_set_client",
            "character_set_connection",
            "collation_connection",
            "foreign_key_checks",
            "unique_checks",
            "sql_log_bin",
            "sql_safe_updates",
        ],
    ),
    (
        Engine::DuckDb,
        &[
            // One-way: once off, DBDelve's own file opening stays off.
            "enable_external_access",
            "lock_configuration",
            "allowed_directories",
            "allowed_paths",
            "autoinstall_known_extensions",
            "autoload_known_extensions",
            "allow_unsigned_extensions",
            "allow_community_extensions",
            "disabled_filesystems",
            "custom_extension_repository",
            "temp_directory",
            "secret_directory",
            "allow_persistent_secrets",
            "access_mode",
        ],
    ),
    (
        Engine::SqlServer,
        &[
            "IDENTITY_INSERT t",
            "IMPLICIT_TRANSACTIONS",
            // Read at parse time, and changes how a double quote is read.
            "QUOTED_IDENTIFIER",
            "NOEXEC",
            "PARSEONLY",
            "FMTONLY",
            "SHOWPLAN_ALL",
            "FORCEPLAN",
            "CONTEXT_INFO",
            "OFFSETS SELECT",
            "FIPS_FLAGGER",
            "REMOTE_PROC_TRANSACTIONS",
            "CURSOR_CLOSE_ON_COMMIT",
            "ANSI_DEFAULTS",
            "ANSI_NULL_DFLT_ON",
            "QUERY_GOVERNOR_COST_LIMIT",
        ],
    ),
    // The embedded file engines configure through `PRAGMA`, which is a different
    // statement type and already needs Full, so a `SET` there is a statement the
    // server rejects. The stateless engine keeps no session for one to change.
    (Engine::Sqlite, &["foreign_keys", "journal_mode"]),
    (Engine::Turso, &["foreign_keys", "journal_mode"]),
    (Engine::Snowflake, &["search_path", "timezone"]),
];

/// `SET NAMES` for charsets either side of the single-byte line: the ones a
/// statement can still be lexed in, and the multibyte ones whose trail bytes can
/// swallow a backslash and leave the parser's reading of a quote behind the
/// server's.
const CHARSETS: &[&str] = &[
    "DEFAULT",
    "utf8mb4",
    "UTF8MB4",
    "utf8mb3",
    "utf8",
    "latin1",
    "ascii",
    "binary",
    "'utf8mb4'",
    "gbk",
    "big5",
    "sjis",
    "cp932",
    "gb18030",
    "ucs2",
    "utf16",
];

/// The statement that assigns a setting, spelled the way the engine's own
/// dialect assigns one.
pub(crate) fn set_statement(engine: Engine, name: &str) -> String {
    match engine {
        Engine::SqlServer => format!("SET {name} ON"),
        _ => format!("SET {name} = 1"),
    }
}

#[test]
fn session_setting_corpus_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for (case, sql) in SESSION_RULES.iter().chain(SESSION_SHAPES) {
            record(&mut out, engine, case, sql);
        }
        for charset in CHARSETS {
            record(&mut out, engine, "charset", &format!("SET NAMES {charset}"));
        }
        for name in engine.spec().sql.plain_settings {
            record(
                &mut out,
                engine,
                "allow-listed",
                &set_statement(engine, name),
            );
            record(
                &mut out,
                engine,
                "allow-listed, server-wide",
                &format!("SET GLOBAL {name} = 1"),
            );
        }
        for (owner, names) in EXCLUDED_SETTINGS {
            if *owner != engine {
                continue;
            }
            for name in *names {
                record(
                    &mut out,
                    engine,
                    "not allow-listed",
                    &set_statement(engine, name),
                );
            }
        }
    }
    check("hardening_settings", &out);
}

/// Comment forms the MySQL family reads differently from the rest, and the
/// controls that must not move. Every executable-comment case is a live server
/// observation (MariaDB 11.4 executes the `M!` marker and MySQL 8.4 ignores it),
/// so the sentence says what the server does with the text, not what the parser
/// makes of it.
const EXECUTABLE_COMMENTS: &[(&str, &str)] = &[
    (
        "executable comment alone, destructive",
        "/*M!100100 DROP TABLE t */",
    ),
    (
        "executable comment alone, no version",
        "/*M!DROP TABLE t */",
    ),
    (
        "executable comment alone, version glued to the code",
        "/*M!100100DROP TABLE t */",
    ),
    (
        "executable comment alone, five digits",
        "/*M!10010 DROP TABLE t */",
    ),
    (
        "executable comment alone, line break after the version",
        "/*M!100100\nDROP TABLE t */",
    ),
    (
        "executable comment after an ordinary statement",
        "SELECT 1; /*M!100100 DROP TABLE t */",
    ),
    (
        "executable comment after an ordinary statement, own line",
        "SELECT 1;\n/*M!100100 DROP TABLE t */",
    ),
    (
        "executable comment holding a truncate",
        "/*M!100100 TRUNCATE TABLE t */",
    ),
    (
        "executable comment holding an unfiltered delete",
        "/*M!100100 DELETE FROM t */",
    ),
    (
        "executable comment holding a write",
        "/*M!100100 INSERT INTO t VALUES (1) */",
    ),
    (
        "executable comment holding a read",
        "/*M!100100 SELECT 1 */",
    ),
    (
        "executable comment holding the write access mode",
        "/*M!100100 SET SESSION TRANSACTION READ WRITE */",
    ),
    (
        "executable comment holding the write access mode after a statement",
        "SELECT 1; /*M!100100 SET SESSION TRANSACTION READ WRITE */",
    ),
    (
        "executable comment holding the hold name",
        "/*M!100100 SET SESSION transaction_read_only = 0 */",
    ),
    (
        "executable comment holding a denied call",
        "/*M!100100 SELECT get_lock('k', 1) */",
    ),
    (
        "fragment that changes the statement around it",
        "SELECT 1 /*M!100100 INTO @y */ FROM t",
    ),
    (
        "fragment that only extends the statement around it",
        "SELECT 2 /*M!100100 +1 */",
    ),
    (
        "executable comment fused with a bare statement",
        "/*M!100100 DROP TABLE t */ SELECT 1",
    ),
    (
        "executable comment, lower-case marker",
        "/*m!100100 DROP TABLE t */",
    ),
    (
        "executable comment with a multi-byte character in the body",
        "/*M!100100 SELECT 'é' */",
    ),
    (
        "executable comment with a multi-byte character where the digits end",
        "/*M!100100é DROP TABLE t */",
    ),
    (
        "executable comment with a multi-byte character straight after the marker",
        "/*M!é DROP TABLE t */",
    ),
    (
        "executable comment with full-width digits",
        "/*M!１００１００ DROP TABLE t */",
    ),
    ("executable comment, empty", "/*M!*/"),
    (
        "four-digit version, a plain comment on the server",
        "/*M!1234 DROP TABLE t */",
    ),
    ("space before the digits", "/*M! 100100 DROP TABLE t */"),
    ("bang before the letter", "/*!M100100 DROP TABLE t */"),
    ("the form both servers execute", "/*!50000 DROP TABLE t */"),
    (
        "the form only the older server executes",
        "/*!50700 DROP TABLE t */",
    ),
    (
        "marker inside a string literal",
        "SELECT '/*M!100100 DROP TABLE t */'",
    ),
    (
        "marker after a line comment",
        "-- /*M!100100 DROP TABLE t */\nSELECT 1",
    ),
    (
        "marker after a hash comment",
        "# /*M!100100 DROP TABLE t */\nSELECT 1",
    ),
    (
        "marker inside an ordinary block comment",
        "/* /*M!100100 DROP TABLE t */ SELECT 1",
    ),
    ("unterminated executable comment", "/*M!100100 DROP TABLE t"),
    (
        "nested executable comment",
        "/*M!100100 /*M!100100 SELECT 7 */ */",
    ),
    (
        "dump header, the version no server reaches",
        "/*M!999999\\- enable the sandbox mode */",
    ),
    ("ordinary block comment before a read", "/* c */ SELECT 1"),
];

/// Submissions the typed parser reads as no statement at all, which the
/// zero-statement cross-check looks at: the forms that must stay silent reads,
/// and the comment residue the pinned tokenizer reads as real tokens. The
/// existing dialect corpus holds no hash-comment or nested-comment text, so
/// this is the only place a carve-out that stopped clearing them would show.
const NOTHING_TO_RUN: &[(&str, &str)] = &[
    ("empty", ""),
    ("whitespace only", "  \n\t"),
    ("lone semicolon", ";"),
    ("two semicolons", ";;"),
    ("semicolon then comment", "; -- c"),
    ("line comment", "-- c"),
    ("block comment", "/* c */"),
    ("block comment after a line comment", "-- a\n/* b */"),
    ("hash line comment", "# c"),
    ("hash line comment, line break", "# c\n"),
    ("hash line comment after a block comment", "/* a */ # c"),
    ("nested block comment", "/* /* n */ */"),
    (
        "nested block comment hiding a statement",
        "/* /* n */ DROP TABLE t */",
    ),
    ("batch separator only", "GO"),
];

#[test]
fn executable_comment_corpus_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for (case, sql) in EXECUTABLE_COMMENTS.iter().chain(NOTHING_TO_RUN) {
            record(&mut out, engine, case, sql);
        }
    }
    check("hardening_comments", &out);
}

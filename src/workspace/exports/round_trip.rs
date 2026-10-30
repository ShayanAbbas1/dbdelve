//! Whether a file of `INSERT`s puts back what it was exported from: a throwaway
//! table per engine, a column of every type it has, exported through the real
//! path, emptied, run back and read again cell by cell.
//!
//! What a cell's text cannot carry is left out of the fixtures, each with a
//! comment beside the column, since a value the grid spells the same either
//! way would pass here and still be gone. Snowflake and Turso are not covered.

use super::*;
use crate::db::{ConnectionConfig, Fetch};

const AWKWARD_TEXTS: [&str; 14] = [
    "it's a \"quote\"",
    r"C:\temp\new",
    r"ends with \",
    "line one\nline two\r\nline three\ttabbed",
    "Zoë 李 🐉",
    "",
    "NULL",
    "; -- no comment /* here */",
    r"\N",
    "0xABCD",
    "x'ABCD'",
    "'",
    "''",
    r"\'",
];

const FIRST_TEXT_ID: usize = 100;

/// A table of the test's own, dropped when the test ends, however it ends.
struct Throwaway {
    connection: Connection,
    engine: Engine,
    schema: &'static str,
    name: String,
    directory: PathBuf,
    drops: Vec<String>,
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        for statement in &self.drops {
            let _ = self
                .connection
                .query(statement, &CancelToken::default(), Fetch::default());
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl Throwaway {
    fn new(connection: Connection, engine: Engine, schema: &'static str) -> Self {
        let name = format!("dbdelve_export_{}_{}", engine.spec().id, std::process::id());
        let directory = std::env::temp_dir().join(&name);
        _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let mut table = Self {
            connection,
            engine,
            schema,
            name,
            directory,
            drops: Vec::new(),
        };
        table.drop_after("DROP TABLE IF EXISTS {table}");
        table
    }

    fn drop_after(&mut self, statement: &str) {
        let statement = self.named(statement);
        self.drops.push(statement);
    }

    fn named(&self, sql: &str) -> String {
        sql.replace("{table}", &self.engine.qualified(self.schema, &self.name))
            .replace("{name}", &self.name)
            .replace("{schema}", self.schema)
    }

    fn run(&self, sql: &str) -> QueryResult {
        let sql = self.named(sql);
        self.connection
            .query(&sql, &CancelToken::default(), Fetch::default())
            .unwrap_or_else(|error| panic!("{:?}: `{sql}` failed: {error}", self.engine))
    }

    /// Read as a relation tab reads it, which on SQL Server is the only way
    /// into a table holding a type its driver cannot decode.
    fn rows(&self) -> QueryResult {
        let sql = self.named("SELECT * FROM {table} ORDER BY id");
        self.connection
            .generated(&sql, &CancelToken::default(), None, None)
            .unwrap_or_else(|error| panic!("{:?}: `{sql}` failed: {error}", self.engine))
    }

    fn seed_awkward_texts(&self, column: &str) {
        for (index, text) in AWKWARD_TEXTS.iter().enumerate() {
            self.run(&format!(
                "INSERT INTO {{table}} (id, {column}) VALUES ({}, {})",
                FIRST_TEXT_ID + index,
                self.engine.quote_literal(text)
            ));
        }
    }

    /// The file a sidebar export of the whole table would write.
    fn exported(&self) -> Result<(usize, String), Ended> {
        let path = self.directory.join("rows.sql");
        let refetch = relation_refetch(self.engine, self.schema, &self.name, "", &[], &[])
            .expect("a relation of this name is read in one query");
        let target = InsertTarget {
            relation: Some((self.schema.into(), self.name.clone())),
            columns: None,
        };
        let written = export_every_row(
            &self.connection,
            &refetch,
            &target,
            false,
            self.engine,
            &path,
            &ExportProgress::default(),
        )?;
        Ok((written, std::fs::read_to_string(&path).unwrap()))
    }

    /// Every statement of `file` run, each failure said rather than stopping
    /// at the first, since one bad value only costs its row.
    fn replay(&self, file: &str) -> Vec<String> {
        let buffer = sql::Buffer::for_engine(self.engine, file);
        buffer
            .statements()
            .iter()
            .filter_map(|range| {
                let statement = &file[range.clone()];
                let ran =
                    self.connection
                        .query(statement, &CancelToken::default(), Fetch::default());
                ran.err()
                    .map(|error| format!("`{statement}` failed: {error}"))
            })
            .collect()
    }
}

/// Every cell of `before` against `after`, matched on the first column.
fn differences(before: &QueryResult, after: &QueryResult) -> Vec<String> {
    let mut differences = Vec::new();
    if after.rows.len() != before.rows.len() {
        differences.push(format!(
            "{} rows came back of {}",
            after.rows.len(),
            before.rows.len()
        ));
    }
    let key = |row: &Row| row.get(0).map(str::to_string);
    for row in &before.rows {
        let Some(back) = after.rows.iter().find(|back| key(back) == key(row)) else {
            differences.push(format!("row {:?} did not come back", key(row)));
            continue;
        };
        for (index, column) in before.columns.iter().enumerate() {
            if row.get(index) != back.get(index) {
                differences.push(format!(
                    "row {:?}, {} ({:?}): {:?} came back as {:?}",
                    key(row),
                    column.name,
                    column.data_type,
                    row.get(index),
                    back.get(index),
                ));
            }
        }
    }
    differences
}

/// `exact` is a read keyed and ordered by id, for what a cell's text does not
/// show: a value the grid spells the same either way is caught only by a
/// column that spells it exactly.
fn assert_runs_back(table: &Throwaway, exact: &str) {
    let (before, exact_before) = (table.rows(), table.run(exact));
    let (written, file) = table.exported().expect("the export finishes");
    assert_eq!(written, before.rows.len(), "{:?}", table.engine);
    table.run("TRUNCATE TABLE {table}");
    let statements = sql::Buffer::for_engine(table.engine, &file)
        .statements()
        .len();
    assert_eq!(statements, written, "{:?}: {file}", table.engine);
    let mut problems = table.replay(&file);
    problems.extend(differences(&before, &table.rows()));
    problems.extend(differences(&exact_before, &table.run(exact)));
    assert!(
        problems.is_empty(),
        "{:?}:\n{}",
        table.engine,
        problems.join("\n")
    );
}

fn assert_awkward_texts_read_as_written(table: &Throwaway, column: usize) {
    let read = table.rows();
    for (index, text) in AWKWARD_TEXTS.iter().enumerate() {
        let id = (FIRST_TEXT_ID + index).to_string();
        let row = read
            .rows
            .iter()
            .find(|row| row.get(0) == Some(id.as_str()))
            .unwrap_or_else(|| panic!("{:?}: no row {id}", table.engine));
        assert_eq!(row.get(column), Some(*text), "{:?}", table.engine);
    }
}

/// A file with nothing to say is an empty one, and it says so as a success.
fn assert_an_empty_table_exports_an_empty_file(table: &Throwaway) {
    assert_eq!(
        table.exported(),
        Ok((0, String::new())),
        "{:?}",
        table.engine
    );
}

fn open(variable: &str) -> Connection {
    let url = std::env::var(variable).unwrap_or_else(|_| panic!("{variable} is required"));
    let config = ConnectionConfig::from_url(&url).expect("the URL should parse");
    Connection::open(config).expect("connection should open")
}

fn postgres() -> Connection {
    let variable =
        |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    let url = format!(
        "postgres://{}:{}@{}:{}/{}?sslmode=disable",
        variable("PGUSER"),
        variable("PGPASSWORD"),
        variable("PGHOST"),
        variable("PGPORT"),
        variable("PGDATABASE"),
    );
    let config = ConnectionConfig::from_url(&url).expect("the URL should parse");
    Connection::open(config).expect("connection should open")
}

#[test]
#[ignore = "needs the dev Postgres"]
fn live_a_relation_exported_as_sql_runs_back_on_postgres() {
    let mut table = Throwaway::new(postgres(), Engine::Postgres, "public");
    // `shape` holds no SRID: the grid's WKT has no way to say one, so a column
    // that does not fix its own, as `spot` and `globe` do, comes back with 0.
    table.drop_after("DROP TYPE IF EXISTS \"{schema}\".\"{name}_mood\"");
    table.run("CREATE TYPE \"{schema}\".\"{name}_mood\" AS ENUM ('sad', 'it''s ok', 'a,b')");
    table.run(
        "CREATE TABLE {table} (
            id int PRIMARY KEY,
            small smallint, whole int, big bigint,
            exact numeric(60, 30), free numeric,
            single real, doubled double precision,
            flag boolean, bits bit(8), varbits varbit(16),
            body text, label varchar(50), letter char(3),
            day date, clock time, clock_tz timetz, moment timestamp, instant timestamptz,
            span interval, ident uuid, doc json, docb jsonb, bytes bytea,
            mood \"{schema}\".\"{name}_mood\",
            ints int[], texts text[], stamps timestamp[], blobs bytea[],
            address inet, network cidr, mac macaddr, cost money,
            plane point, reach int4range, words tsvector, markup xml,
            shape geometry, spot geometry(Point, 4326), globe geography
        )",
    );
    assert_an_empty_table_exports_an_empty_file(&table);
    table.run(
        "INSERT INTO {table} VALUES (
            1, 32767, 2147483647, 9223372036854775807,
            123456789012345678901234567890.123456789012345678901234567890, 1e1000,
            3.4028235e38, 1.7976931348623157e308,
            true, B'10100101', B'101',
            'plain', 'label', 'abc',
            '2024-02-29', '23:59:59.999999', '12:34:56+05:30', '2024-02-29 12:34:56.789012',
            '2024-03-10 12:34:56.789012+05:30',
            '1 year 2 mons 3 days 04:05:06.789', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11',
            '{\"a\": [1, 2.50, null], \"q\": \"it''s\\n\"}',
            '{\"a\": [1, 2.50, null], \"q\": \"it''s\\n\"}',
            '\\x00ff27275c0a', 'it''s ok',
            ARRAY[1, NULL, 3], ARRAY['a,b', 'q\"uote', 'it''s', NULL, 'NULL', 'back\\slash', ''],
            ARRAY['2024-01-02 03:04:05'::timestamp], ARRAY['\\xdead'::bytea],
            '192.168.0.1/24', '10.0.0.0/8', '08:00:2b:01:02:03', '$1,234.56',
            '(1.5,2.5)', '[1,5)', 'a:1 it''s:2', '<a>it''s &amp; b</a>',
            ST_GeomFromText('LINESTRING(1 2, 3 4)'),
            ST_GeomFromEWKT('SRID=4326;POINT(1.5 2.5)'),
            ST_GeogFromText('SRID=4326;POINT(10 20)')
        ), (
            2, -32768, -2147483648, -9223372036854775808,
            -0.000000000000000000000000000001, 'NaN',
            'NaN', 'Infinity',
            false, B'00000000', B'',
            '', '', '   ',
            'infinity', '00:00:00', '00:00:00+00', 'infinity', '-infinity',
            '-178956970 years -8 mons -6 hours', '00000000-0000-0000-0000-000000000000',
            '[]', '{}', '', 'a,b', '{}', '{}', '{}', '{}',
            '::1', '::/0', '00:00:00:00:00:00', '-$92,233,720,368,547,758.08',
            '(-1,0)', 'empty', '', '<a/>',
            ST_GeomFromText('POLYGON EMPTY'), NULL, NULL
        ), (
            3, 0, 0, 0, 0, 0, 'Infinity', '-Infinity', NULL, NULL, NULL,
            NULL, NULL, NULL, '0001-01-01 BC', NULL, NULL, '0001-01-01 00:00:00 BC', NULL,
            NULL, NULL, NULL, NULL, NULL, 'sad', ARRAY[]::int[], ARRAY[NULL]::text[], NULL,
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL
        )",
    );
    table.run(
        "INSERT INTO {table} (id, single, doubled) VALUES (4, 1.17549e-38, 5e-324), (5, NULL, NULL)",
    );
    table.seed_awkward_texts("body");
    assert_awkward_texts_read_as_written(&table, 11);
    assert_runs_back(
        &table,
        "SELECT id, ST_AsEWKT(shape), ST_AsEWKT(spot), ST_AsEWKT(globe) FROM {table} ORDER BY id",
    );
}

/// `single` holds only what six digits can say: the text protocol prints a
/// `FLOAT` no further, so `1.2345678` runs back as `1.23457`.
const MYSQL_COLUMNS: &str = "
    id INT PRIMARY KEY,
    tiny TINYINT, flag BOOLEAN, small SMALLINT, medium MEDIUMINT, whole INT,
    big BIGINT, ubig BIGINT UNSIGNED,
    exact DECIMAL(65, 30), single FLOAT, doubled DOUBLE,
    bits BIT(8), wide_bits BIT(64), yr YEAR,
    body TEXT, label VARCHAR(100), letter CHAR(3), long_body LONGTEXT,
    day DATE, clock TIME(6), moment DATETIME(6), stamp TIMESTAMP(6) NULL,
    doc JSON, kind ENUM('a', 'b,c', 'it''s'), tags SET('x', 'y', 'z'),
    bytes VARBINARY(32), fixed BINARY(4), blobby BLOB,
    shape GEOMETRY, spot POINT";

fn mysql_rows(engine: Engine, table: &Throwaway, extra: [&str; 4]) {
    let [first, second, third, fourth] = extra;
    let srid = match engine {
        Engine::MariaDb => "ST_GeomFromText('POINT(1.5 2.5)', 4326)",
        _ => "ST_GeomFromText('POINT(1.5 2.5)', 4326, 'axis-order=long-lat')",
    };
    table.run(&format!(
        "INSERT INTO {{table}} VALUES (
            1, 127, TRUE, 32767, 8388607, 2147483647,
            9223372036854775807, 18446744073709551615,
            12345678901234567890123456789012345.123456789012345678901234567890,
            1.5, 1.7976931348623157e308,
            b'10100101', b'1111111111111111111111111111111111111111111111111111111111111111',
            2155,
            'plain', 'label', 'abc', 'long',
            '2024-02-29', '-838:59:59.000000', '2024-02-29 12:34:56.789012',
            '2024-02-29 12:34:56.789012',
            '{{\"a\": [1, 2.5, null], \"q\": \"it''s\\\\n\"}}', 'it''s', 'x,z',
            x'00FF27275C0A', x'DEADBEEF', x'00FF',
            ST_GeomFromText('LINESTRING(1 2, 3 4)'), {srid}{first}
        ), (
            2, -128, FALSE, -32768, -8388608, -2147483648,
            -9223372036854775808, 0,
            -0.000000000000000000000000000001,
            -2.5, 5e-324,
            b'00000000', b'0', 1901,
            '', '', '', '',
            '1000-01-01', '838:59:59.000000', '1000-01-01 00:00:00.000000',
            '1970-01-02 00:00:00.000001',
            '[]', 'b,c', '',
            x'', x'', x'',
            ST_GeomFromText('POINT(0 0)'), NULL{second}
        ), (
            3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL{third}
        ), (
            4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'1', b'1', 0,
            'x', 'x', 'x', 'x', '9999-12-31', '00:00:00', '9999-12-31 23:59:59.999999', NULL,
            '{{}}', 'a', 'x,y,z', x'41', x'41424344', x'41', NULL, NULL{fourth}
        )"
    ));
}

#[test]
#[ignore = "needs the dev MySQL"]
fn live_a_relation_exported_as_sql_runs_back_on_mysql() {
    let engine = Engine::MySql;
    let table = Throwaway::new(open("dbdelve_MYSQL_URL"), engine, "dbdelve_dev");
    table.run(&format!("CREATE TABLE {{table}} ({MYSQL_COLUMNS})"));
    assert_an_empty_table_exports_an_empty_file(&table);
    mysql_rows(engine, &table, ["", "", "", ""]);
    table.seed_awkward_texts("body");
    assert_awkward_texts_read_as_written(&table, 14);
    assert_runs_back(
        &table,
        "SELECT id, ST_AsText(shape), ST_SRID(shape), ST_SRID(spot), HEX(bytes), \
         HEX(bits), HEX(wide_bits), CAST(single AS DOUBLE) FROM {table} ORDER BY id",
    );
}

#[test]
#[ignore = "needs the dev MariaDB"]
fn live_a_relation_exported_as_sql_runs_back_on_mariadb() {
    let engine = Engine::MariaDb;
    let table = Throwaway::new(open("dbdelve_MARIADB_URL"), engine, "dbdelve_dev");
    table.run(&format!(
        "CREATE TABLE {{table}} ({MYSQL_COLUMNS}, ident UUID, v6 INET6, v4 INET4)"
    ));
    assert_an_empty_table_exports_an_empty_file(&table);
    mysql_rows(
        engine,
        &table,
        [
            ", 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '2001:db8::1', '192.168.0.1'",
            ", '00000000-0000-0000-0000-000000000000', '::', '0.0.0.0'",
            ", NULL, NULL, NULL",
            ", NULL, NULL, NULL",
        ],
    );
    table.seed_awkward_texts("body");
    assert_awkward_texts_read_as_written(&table, 14);
    assert_runs_back(
        &table,
        "SELECT id, ST_AsText(shape), ST_SRID(shape), ST_SRID(spot), HEX(bytes), \
         HEX(bits), HEX(wide_bits) FROM {table} ORDER BY id",
    );
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn live_a_relation_exported_as_sql_runs_back_on_sql_server() {
    // The driver decodes `money` to an `f64`, so `cost` stays below where that
    // holds four decimals: `1000000000000.0001` runs back without its last
    // digit, and the type's extremes round past its range and fail to.
    //
    // `shape` holds no SRID and `variant` only text: the relation's read turns
    // both to text first, so another SRID comes back 0 and any other base type
    // comes back `nvarchar`.
    let table = Throwaway::new(open("dbdelve_MSSQL_URL"), Engine::SqlServer, "dbo");
    table.run(
        "CREATE TABLE {table} (
            id int PRIMARY KEY,
            tiny tinyint, small smallint, whole int, big bigint,
            exact decimal(38, 10), huge numeric(38, 0),
            single real, doubled float, flag bit,
            cost money, small_cost smallmoney,
            body nvarchar(max), label nvarchar(100), ansi varchar(100), letter char(3),
            wide_letter nchar(3), old_text text, old_ntext ntext,
            day date, clock time(7), old datetime, small_old smalldatetime,
            moment datetime2(7), instant datetimeoffset(7),
            ident uniqueidentifier, bytes varbinary(max), fixed binary(4), short varbinary(16),
            picture image, markup xml, variant sql_variant,
            shape geometry, globe geography, path hierarchyid
        )",
    );
    assert_an_empty_table_exports_an_empty_file(&table);
    table.run(
        "INSERT INTO {table} VALUES (
            1, 255, 32767, 2147483647, 9223372036854775807,
            1234567890123456789012345678.0123456789, 99999999999999999999999999999999999999,
            CAST(3.4028235E+38 AS real), 1.7976931348623157E+308, 1,
            123456789.1234, 214748.3647,
            N'plain', N'label', 'ansi', 'abc', N'李小龙', 'old', N'old 李',
            '2024-02-29', '23:59:59.9999999', '2024-02-29 12:34:56.997',
            '2024-02-29 12:34:00', '2024-02-29 12:34:56.1234567',
            '2024-02-29 12:34:56.1234567 +05:30',
            'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11', 0x00FF27275C0A, 0xDEADBEEF, 0x00,
            0x00FF, N'<a>it''s &amp; b 李</a>', N'variant',
            geometry::STGeomFromText('LINESTRING(1 2, 3 4)', 0),
            geography::STGeomFromText('POINT(10 20)', 4326),
            hierarchyid::Parse('/1/2/')
        ), (
            2, 0, -32768, -2147483648, -9223372036854775808,
            -0.0000000001, -99999999999999999999999999999999999999,
            -1.5, 5E-324, 0,
            -123456789.1234, -214748.3648,
            N'', N'', '', '', N'', '', N'',
            '0001-01-01', '00:00:00', '1753-01-01 00:00:00',
            '1900-01-01 00:00:00', '0001-01-01 00:00:00',
            '0001-01-01 00:00:00 -14:00',
            '00000000-0000-0000-0000-000000000000', 0x, 0x00000000, 0x,
            0x, N'<a/>', NULL, NULL, NULL, NULL
        ), (
            3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL
        ), (
            4, NULL, NULL, NULL, NULL, NULL, NULL, 0, 0, NULL, 0, 0,
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, '9999-12-31', NULL,
            '9999-12-31 23:59:59.997', '2079-06-06 23:59:00', '9999-12-31 23:59:59.9999999',
            '9999-12-31 23:59:59.9999999 +14:00',
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL
        )",
    );
    table.seed_awkward_texts("body");
    assert_awkward_texts_read_as_written(&table, 12);
    assert_runs_back(
        &table,
        "SELECT id, shape.STSrid AS shape_srid, globe.STSrid AS globe_srid, \
         CAST(SQL_VARIANT_PROPERTY(variant, 'BaseType') AS nvarchar(40)) AS variant_type \
         FROM {table} ORDER BY id",
    );
}

#[test]
#[ignore = "needs the dev ClickHouse"]
fn live_a_relation_exported_as_sql_runs_back_on_clickhouse() {
    // Every string here is UTF-8: bytes that are not read as `0x…`, and the text
    // of that is what `plain` would run back as, while a `FixedString` refuses
    // it for its length and an `AggregateFunction` state for its bytes.
    //
    // `loose` and `either` hold strings: a cell does not say which of their types
    // it is, so a number in one comes back as the string that spells it.
    let table = Throwaway::new(
        open("dbdelve_CLICKHOUSE_URL"),
        Engine::ClickHouse,
        "dbdelve_dev",
    );
    table.run(
        "CREATE TABLE {table} (
            id Int32,
            i8 Nullable(Int8), i64 Nullable(Int64), u64 Nullable(UInt64),
            i128 Nullable(Int128), u256 Nullable(UInt256), i256 Int256,
            exact Nullable(Decimal(38, 18)), wide Nullable(Decimal(76, 38)),
            f32 Nullable(Float32), f64 Nullable(Float64), flag Nullable(Bool),
            body Nullable(String), plain String, low LowCardinality(String),
            low_null LowCardinality(Nullable(String)), fixed Nullable(FixedString(4)),
            day Nullable(Date), day32 Nullable(Date32), moment Nullable(DateTime),
            precise Nullable(DateTime64(3)), tokyo Nullable(DateTime64(6, 'Asia/Tokyo')),
            ident Nullable(UUID), v4 Nullable(IPv4), v6 Nullable(IPv6),
            kind Nullable(Enum8('a' = 1, 'it''s' = 2)),
            ints Array(Int32), texts Array(String), maybe Array(Nullable(String)),
            grid Array(Array(Int32)), counts Map(String, Int32), labels Map(String, String),
            pair Tuple(Int32, String), named Tuple(a Int32, b String),
            spot Point, doc JSON, loose Dynamic, either Variant(Int64, String)
        ) ENGINE = MergeTree ORDER BY id",
    );
    assert_an_empty_table_exports_an_empty_file(&table);
    table.run(
        r#"INSERT INTO {table} VALUES (
            1, 127, 9223372036854775807, 18446744073709551615,
            170141183460469231731687303715884105727,
            115792089237316195423570985008687907853269984665640564039457584007913129639935,
            57896044618658097711785492504343953926634992332820282019728792003956564819967,
            12345678901234567890.123456789012345678,
            12345678901234567890123456789012345678.12345678901234567890123456789012345678,
            3.4028235e38, 1.7976931348623157e308, true,
            'plain', 'plain', 'low', 'low', 'abcd',
            '2024-02-29', '2024-02-29', '2024-02-29 12:34:56', '2024-02-29 12:34:56.789',
            '2024-02-29 12:34:56.789012',
            'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '192.168.0.1', '2001:db8::1', 'it''s',
            [1, -2], ['a,b', 'it''s', 'q"uote', 'back\\slash', 'line\none', '', 'NULL'],
            ['x', NULL, 'NULL'], [[1, 2], [], [3]],
            map('a', 1, 'it''s', 2), map('k', 'v''s', 'n', 'line\nbreak'),
            (1, 'it''s'), (2, 'b\\c'),
            (1.5, 2.5), '{"a": [1, 2.5], "q": "it''s\\n"}', 'loose', 'seven'
        ), (
            2, -128, -9223372036854775808, 0,
            -170141183460469231731687303715884105728, 0,
            -57896044618658097711785492504343953926634992332820282019728792003956564819968,
            -0.000000000000000001, 0, nan, -inf, false,
            '', '', '', '', 'ab',
            '1970-01-01', '1900-01-01', '1970-01-01 00:00:00', '1900-01-01 00:00:00.000',
            '1970-01-01 00:00:00.000000',
            '00000000-0000-0000-0000-000000000000', '0.0.0.0', '::', 'a',
            [], [], [], [], map(), map(), (0, ''), (0, ''),
            (0, 0), '{}', 'twelve', 'text'
        ), (
            3, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, inf, NULL, NULL,
            NULL, 'x', 'x', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
            [], [], [], [], map(), map(), (0, ''), (0, ''), (0, 0), '{}', NULL, NULL
        )"#,
    );
    table.seed_awkward_texts("body");
    assert_awkward_texts_read_as_written(&table, 12);
    assert_runs_back(
        &table,
        "SELECT id, hex(fixed), hex(plain), dynamicType(loose), variantType(either) FROM {table} ORDER BY id",
    );
}

#[test]
fn a_relation_exported_as_sql_runs_back_on_duckdb() {
    let directory =
        std::env::temp_dir().join(format!("dbdelve-export-duckdb-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("rows.duckdb");
    drop(::duckdb::Connection::open(&path).unwrap());
    let connection = Connection::open(ConnectionConfig::duckdb(
        path.to_string_lossy().into_owned(),
        0,
    ))
    .unwrap();
    // Nested values are held to what Arrow's text keeps and a cast reads back:
    // it does not quote, so `['a,b']` runs back as two items and `['NULL']` as a
    // NULL, a quote or comma in a struct's text fails the cast, a `BLOB` in a
    // list loses its bytes, and neither a `MAP` nor a `UNION` is read at all.
    let table = Throwaway::new(connection, Engine::DuckDb, "main");
    table.run(
        "CREATE TABLE {table} (
            id INTEGER PRIMARY KEY,
            tiny TINYINT, big BIGINT, ubig UBIGINT, huge HUGEINT, uhuge UHUGEINT,
            exact DECIMAL(38, 10), single FLOAT, doubled DOUBLE, flag BOOLEAN,
            body VARCHAR, day DATE, clock TIME, moment TIMESTAMP, second TIMESTAMP_S,
            milli TIMESTAMP_MS, nano TIMESTAMP_NS, instant TIMESTAMPTZ, clock_tz TIMETZ,
            ident UUID, bytes BLOB, bits BIT, span INTERVAL, kind ENUM('a', 'it''s'),
            ints INTEGER[], words VARCHAR[], triple INTEGER[3],
            record STRUCT(a INTEGER, b VARCHAR)
        )",
    );
    assert_an_empty_table_exports_an_empty_file(&table);
    table.run(
        "INSERT INTO {table} VALUES (
            1, 127, 9223372036854775807, 18446744073709551615,
            170141183460469231731687303715884105727,
            340282366920938463463374607431768211455,
            1234567890123456789012345678.0123456789,
            3.4028235e38, 1.7976931348623157e308, true,
            'plain', '2024-02-29', '23:59:59.999999', '2024-02-29 12:34:56.789012',
            '2024-02-29 12:34:56', '2024-02-29 12:34:56.789', '2024-02-29 12:34:56.789012345',
            '2024-03-10 12:34:56.789012+05:30', '12:34:56+05:30',
            'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '\\x00\\xFF''\\x5C\\x0A'::BLOB, '101',
            INTERVAL '1 year 2 months 3 days 04:05:06.789', 'it''s',
            [1, NULL, 3], ['plain', 'words'],
            [1, 2, 3], {'a': 1, 'b': 'plain'}
        ), (
            2, -128, -9223372036854775808, 0,
            -170141183460469231731687303715884105728, 0,
            -0.0000000001, 'NaN', 'Infinity', false,
            '', '-infinity', '00:00:00', 'infinity',
            '1970-01-01 00:00:00', '1970-01-01 00:00:00', '1970-01-01 00:00:00',
            'infinity', '00:00:00+00',
            '00000000-0000-0000-0000-000000000000', ''::BLOB, '0',
            INTERVAL '-1 day', 'a',
            [], [], [0, 0, 0], {'a': NULL, 'b': NULL}
        ), (
            3, NULL, NULL, NULL, NULL, NULL, NULL, '-Infinity', 5e-324, NULL,
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL
        ), (
            4, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL
        )",
    );
    table.seed_awkward_texts("body");
    assert_awkward_texts_read_as_written(&table, 10);
    assert_runs_back(&table, "SELECT id, hex(bytes) FROM {table} ORDER BY id");
    drop(table);
    _ = std::fs::remove_dir_all(&directory);
}

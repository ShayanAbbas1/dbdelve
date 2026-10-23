-- Build a fresh file with `duckdb dev/dbdelve_dev.duckdb < dev/duckdb/001-dbdelve-demo.sql`.
-- Not idempotent: it expects the file not to exist, so remove it first.

CREATE TABLE accounts (
    id INTEGER PRIMARY KEY,
    external_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    email TEXT,
    plan TEXT NOT NULL CHECK (plan IN ('free', 'team', 'enterprise')),
    balance DECIMAL(18, 2) NOT NULL,
    active BOOLEAN NOT NULL,
    tags JSON NOT NULL,
    metadata JSON NOT NULL,
    created_at TIMESTAMP NOT NULL
);

INSERT INTO accounts (
    id,
    external_id,
    name,
    email,
    plan,
    balance,
    active,
    tags,
    metadata,
    created_at
) VALUES
    (
        1,
        '018f1f6e-7c2a-7000-8000-000000000001',
        'Ada Lovelace',
        'ada@example.test',
        'enterprise',
        125000.50,
        TRUE,
        '["founder","priority"]',
        '{"timezone":"Europe/London","features":{"audit":true,"seats":250}}',
        '2024-01-15 09:30:00'
    ),
    (
        2,
        '018f1f6e-7c2a-7000-8000-000000000002',
        'Grace Hopper',
        'grace@example.test',
        'team',
        8192.00,
        TRUE,
        '["compiler","navy"]',
        '{"timezone":"America/New_York","languages":["COBOL","English"]}',
        '2024-02-29 12:00:00'
    ),
    (
        3,
        '018f1f6e-7c2a-7000-8000-000000000003',
        'Edsger Dijkstra',
        NULL,
        'free',
        -0.01,
        FALSE,
        '[]',
        '{"note":"Simplicity is prerequisite for reliability."}',
        '2024-03-10 18:45:12.123456'
    ),
    (
        4,
        '018f1f6e-7c2a-7000-8000-000000000004',
        '李小龍',
        'bruce.lee@example.test',
        'team',
        42.42,
        TRUE,
        '["unicode","香港"]',
        '{"display_name":"李小龍","emoji":"🐉","rtl":"مرحبا"}',
        '2024-04-01 00:00:00'
    ),
    (
        5,
        '018f1f6e-7c2a-7000-8000-000000000005',
        'Quotes ''n'' Backslashes \\',
        'escaping@example.test',
        'free',
        0.00,
        TRUE,
        '["quotes","backslash"]',
        '{"sql":"SELECT ''not a delimiter;'';","path":"C:\\\\demo\\\\file"}',
        '2024-05-05 05:05:05'
    );

-- Five thousand rows, so scrolling, sorting and the row-count chip have
-- something to work against rather than a grid that fits on screen.
CREATE TABLE measurements (
    id INTEGER PRIMARY KEY,
    recorded_at TIMESTAMP NOT NULL,
    sensor TEXT NOT NULL,
    temperature_c DOUBLE,
    pressure_kpa DECIMAL(8, 3),
    healthy BOOLEAN NOT NULL,
    samples JSON NOT NULL,
    payload JSON NOT NULL
);

INSERT INTO measurements
SELECT
    sample,
    TIMESTAMP '2025-01-01 00:00:00' + to_seconds(sample * 15),
    'sensor-' || printf('%02d', (sample - 1) % 24 + 1),
    -- A null every 97th row, so the grid's null rendering is reachable by
    -- scrolling rather than only by writing a query for it.
    CASE WHEN sample % 97 = 0 THEN NULL ELSE 18.0 + (sample % 150) / 10.0 END,
    98.000 + (sample % 700) / 1000.0,
    sample % 113 <> 0,
    json_array(sample % 10, sample % 20, sample % 30),
    json_object(
        'sequence', sample,
        'firmware', 'v' || (1 + sample % 3) || '.' || (sample % 10),
        'flags', json_array(sample % 2 = 0, sample % 5 = 0)
    )
FROM range(1, 5001) AS series (sample);

-- Values far larger than a cell can show, and values a cell would misread:
-- embedded newlines, an embedded semicolon, a JSON null, and raw bytes.
CREATE TABLE documents (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    body TEXT,
    document JSON,
    binary_value BLOB
);

INSERT INTO documents VALUES (
    1,
    'Multiline text',
    -- A plain string literal has no backslash escapes, so the newlines are
    -- built rather than written.
    'first line' || chr(10) || 'second line' || chr(10) || 'third line; with a semicolon',
    '{"kind":"short","nested":{"null_value":null}}',
    '\x00\x01\x02\x03\xFE\xFF'::BLOB
);

INSERT INTO documents
SELECT
    2,
    'Large values',
    repeat('DBDelve keeps the complete value while the grid clips visually. ', 2048),
    json_object(
        'kind', 'large',
        'values', (SELECT json_group_array(json_object('index', value, 'square', value * value)) FROM range(1, 501) AS series (value))
    ),
    repeat('\xDE\xAD\xBE\xEF'::BLOB, 4096);

-- Geometry columns (`point`, `boundary`) are PostGIS-only; DuckDB's spatial
-- extension is separate and out of scope.
CREATE TABLE locations (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

INSERT INTO locations (id, name) VALUES
    (1, 'San Francisco'),
    (2, 'Null Island');

-- Keys worth following. `orders` is both ends of the problem at once: a
-- composite primary key, and a single-column foreign key into `accounts`, so
-- the simple case and the parent of the hard case are one table.
CREATE TABLE orders (
    account_id INTEGER NOT NULL REFERENCES accounts (id),
    number INTEGER NOT NULL,
    placed_at TIMESTAMP NOT NULL,
    total DECIMAL(18, 2) NOT NULL,
    PRIMARY KEY (account_id, number)
);

INSERT INTO orders VALUES
    (1, 1001, '2024-06-01 10:00:00', 4500.00),
    (1, 1002, '2024-06-08 11:30:00', 125.75),
    (2, 2001, '2024-06-12 16:15:00', 890.10);

-- The composite foreign key, which the catalog has to report as one key over
-- two columns rather than two keys of one column each.
CREATE TABLE order_items (
    id INTEGER PRIMARY KEY,
    order_account_id INTEGER NOT NULL,
    order_number INTEGER NOT NULL,
    description TEXT NOT NULL,
    quantity INTEGER NOT NULL,
    FOREIGN KEY (order_account_id, order_number) REFERENCES orders (account_id, number)
);

INSERT INTO order_items VALUES
    (1, 1, 1001, 'Analytical engine time', 3),
    (2, 1, 1002, 'Punch card stock', 500),
    (3, 2, 2001, 'Compiler seat', 1);

CREATE SCHEMA analytics;

-- DuckDB refuses a foreign key across schemas, so there is no cross-schema
-- fixture of that kind; this is a second schema for the tree to list, keyed to
-- `accounts` by value only.
CREATE TABLE analytics.account_scores (
    account_id INTEGER PRIMARY KEY,
    score DOUBLE NOT NULL
);

INSERT INTO analytics.account_scores VALUES (1, 99.5), (2, 87.25), (3, 12.0);

CREATE VIEW account_overview AS
SELECT
    plan,
    count(*) AS accounts,
    sum(balance) AS total_balance,
    count(CASE WHEN active THEN 1 END) AS active_accounts
FROM accounts
GROUP BY plan;

-- account_label is a Postgres/MySQL stored function and deactivate_account a
-- stored procedure; DuckDB has neither (macros are not catalogued as routines
-- here), so there is nothing to port.

-- A million rows and two hundred columns: the fixtures the grid's paging and
-- horizontal scrolling are measured against, not the hand-written rows above.

CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    amount DECIMAL(18, 2) NOT NULL,
    occurred_at TIMESTAMP NOT NULL
);

INSERT INTO events
SELECT i,
       (i % 6) + 1,
       CASE i % 6
           WHEN 0 THEN 'click'
           WHEN 1 THEN 'view'
           WHEN 2 THEN 'purchase'
           WHEN 3 THEN 'refund'
           WHEN 4 THEN 'signup'
           ELSE 'churn'
       END,
       (i % 100000) / 100.0,
       TIMESTAMP '2024-01-01 00:00:00' + to_seconds(i)
FROM range(1, 1000001) AS n (i);

CREATE TABLE wide_metrics (
    id INTEGER PRIMARY KEY,
    c001 INTEGER NOT NULL,
    c002 DOUBLE NOT NULL,
    c003 TEXT NOT NULL,
    c004 INTEGER NOT NULL,
    c005 DOUBLE NOT NULL,
    c006 TEXT NOT NULL,
    c007 INTEGER NOT NULL,
    c008 DOUBLE NOT NULL,
    c009 TEXT NOT NULL,
    c010 INTEGER NOT NULL,
    c011 DOUBLE NOT NULL,
    c012 TEXT NOT NULL,
    c013 INTEGER NOT NULL,
    c014 DOUBLE NOT NULL,
    c015 TEXT NOT NULL,
    c016 INTEGER NOT NULL,
    c017 DOUBLE NOT NULL,
    c018 TEXT NOT NULL,
    c019 INTEGER NOT NULL,
    c020 DOUBLE NOT NULL,
    c021 TEXT NOT NULL,
    c022 INTEGER NOT NULL,
    c023 DOUBLE NOT NULL,
    c024 TEXT NOT NULL,
    c025 INTEGER NOT NULL,
    c026 DOUBLE NOT NULL,
    c027 TEXT NOT NULL,
    c028 INTEGER NOT NULL,
    c029 DOUBLE NOT NULL,
    c030 TEXT NOT NULL,
    c031 INTEGER NOT NULL,
    c032 DOUBLE NOT NULL,
    c033 TEXT NOT NULL,
    c034 INTEGER NOT NULL,
    c035 DOUBLE NOT NULL,
    c036 TEXT NOT NULL,
    c037 INTEGER NOT NULL,
    c038 DOUBLE NOT NULL,
    c039 TEXT NOT NULL,
    c040 INTEGER NOT NULL,
    c041 DOUBLE NOT NULL,
    c042 TEXT NOT NULL,
    c043 INTEGER NOT NULL,
    c044 DOUBLE NOT NULL,
    c045 TEXT NOT NULL,
    c046 INTEGER NOT NULL,
    c047 DOUBLE NOT NULL,
    c048 TEXT NOT NULL,
    c049 INTEGER NOT NULL,
    c050 DOUBLE NOT NULL,
    c051 TEXT NOT NULL,
    c052 INTEGER NOT NULL,
    c053 DOUBLE NOT NULL,
    c054 TEXT NOT NULL,
    c055 INTEGER NOT NULL,
    c056 DOUBLE NOT NULL,
    c057 TEXT NOT NULL,
    c058 INTEGER NOT NULL,
    c059 DOUBLE NOT NULL,
    c060 TEXT NOT NULL,
    c061 INTEGER NOT NULL,
    c062 DOUBLE NOT NULL,
    c063 TEXT NOT NULL,
    c064 INTEGER NOT NULL,
    c065 DOUBLE NOT NULL,
    c066 TEXT NOT NULL,
    c067 INTEGER NOT NULL,
    c068 DOUBLE NOT NULL,
    c069 TEXT NOT NULL,
    c070 INTEGER NOT NULL,
    c071 DOUBLE NOT NULL,
    c072 TEXT NOT NULL,
    c073 INTEGER NOT NULL,
    c074 DOUBLE NOT NULL,
    c075 TEXT NOT NULL,
    c076 INTEGER NOT NULL,
    c077 DOUBLE NOT NULL,
    c078 TEXT NOT NULL,
    c079 INTEGER NOT NULL,
    c080 DOUBLE NOT NULL,
    c081 TEXT NOT NULL,
    c082 INTEGER NOT NULL,
    c083 DOUBLE NOT NULL,
    c084 TEXT NOT NULL,
    c085 INTEGER NOT NULL,
    c086 DOUBLE NOT NULL,
    c087 TEXT NOT NULL,
    c088 INTEGER NOT NULL,
    c089 DOUBLE NOT NULL,
    c090 TEXT NOT NULL,
    c091 INTEGER NOT NULL,
    c092 DOUBLE NOT NULL,
    c093 TEXT NOT NULL,
    c094 INTEGER NOT NULL,
    c095 DOUBLE NOT NULL,
    c096 TEXT NOT NULL,
    c097 INTEGER NOT NULL,
    c098 DOUBLE NOT NULL,
    c099 TEXT NOT NULL,
    c100 INTEGER NOT NULL,
    c101 DOUBLE NOT NULL,
    c102 TEXT NOT NULL,
    c103 INTEGER NOT NULL,
    c104 DOUBLE NOT NULL,
    c105 TEXT NOT NULL,
    c106 INTEGER NOT NULL,
    c107 DOUBLE NOT NULL,
    c108 TEXT NOT NULL,
    c109 INTEGER NOT NULL,
    c110 DOUBLE NOT NULL,
    c111 TEXT NOT NULL,
    c112 INTEGER NOT NULL,
    c113 DOUBLE NOT NULL,
    c114 TEXT NOT NULL,
    c115 INTEGER NOT NULL,
    c116 DOUBLE NOT NULL,
    c117 TEXT NOT NULL,
    c118 INTEGER NOT NULL,
    c119 DOUBLE NOT NULL,
    c120 TEXT NOT NULL,
    c121 INTEGER NOT NULL,
    c122 DOUBLE NOT NULL,
    c123 TEXT NOT NULL,
    c124 INTEGER NOT NULL,
    c125 DOUBLE NOT NULL,
    c126 TEXT NOT NULL,
    c127 INTEGER NOT NULL,
    c128 DOUBLE NOT NULL,
    c129 TEXT NOT NULL,
    c130 INTEGER NOT NULL,
    c131 DOUBLE NOT NULL,
    c132 TEXT NOT NULL,
    c133 INTEGER NOT NULL,
    c134 DOUBLE NOT NULL,
    c135 TEXT NOT NULL,
    c136 INTEGER NOT NULL,
    c137 DOUBLE NOT NULL,
    c138 TEXT NOT NULL,
    c139 INTEGER NOT NULL,
    c140 DOUBLE NOT NULL,
    c141 TEXT NOT NULL,
    c142 INTEGER NOT NULL,
    c143 DOUBLE NOT NULL,
    c144 TEXT NOT NULL,
    c145 INTEGER NOT NULL,
    c146 DOUBLE NOT NULL,
    c147 TEXT NOT NULL,
    c148 INTEGER NOT NULL,
    c149 DOUBLE NOT NULL,
    c150 TEXT NOT NULL,
    c151 INTEGER NOT NULL,
    c152 DOUBLE NOT NULL,
    c153 TEXT NOT NULL,
    c154 INTEGER NOT NULL,
    c155 DOUBLE NOT NULL,
    c156 TEXT NOT NULL,
    c157 INTEGER NOT NULL,
    c158 DOUBLE NOT NULL,
    c159 TEXT NOT NULL,
    c160 INTEGER NOT NULL,
    c161 DOUBLE NOT NULL,
    c162 TEXT NOT NULL,
    c163 INTEGER NOT NULL,
    c164 DOUBLE NOT NULL,
    c165 TEXT NOT NULL,
    c166 INTEGER NOT NULL,
    c167 DOUBLE NOT NULL,
    c168 TEXT NOT NULL,
    c169 INTEGER NOT NULL,
    c170 DOUBLE NOT NULL,
    c171 TEXT NOT NULL,
    c172 INTEGER NOT NULL,
    c173 DOUBLE NOT NULL,
    c174 TEXT NOT NULL,
    c175 INTEGER NOT NULL,
    c176 DOUBLE NOT NULL,
    c177 TEXT NOT NULL,
    c178 INTEGER NOT NULL,
    c179 DOUBLE NOT NULL,
    c180 TEXT NOT NULL,
    c181 INTEGER NOT NULL,
    c182 DOUBLE NOT NULL,
    c183 TEXT NOT NULL,
    c184 INTEGER NOT NULL,
    c185 DOUBLE NOT NULL,
    c186 TEXT NOT NULL,
    c187 INTEGER NOT NULL,
    c188 DOUBLE NOT NULL,
    c189 TEXT NOT NULL,
    c190 INTEGER NOT NULL,
    c191 DOUBLE NOT NULL,
    c192 TEXT NOT NULL,
    c193 INTEGER NOT NULL,
    c194 DOUBLE NOT NULL,
    c195 TEXT NOT NULL,
    c196 INTEGER NOT NULL,
    c197 DOUBLE NOT NULL,
    c198 TEXT NOT NULL,
    c199 INTEGER NOT NULL
);

INSERT INTO wide_metrics
SELECT i,
       i * 1 + 1,
       i * 0.5 + 2,
       'c003-' || i,
       i * 4 + 1,
       i * 0.5 + 5,
       'c006-' || i,
       i * 7 + 1,
       i * 0.5 + 8,
       'c009-' || i,
       i * 10 + 1,
       i * 0.5 + 11,
       'c012-' || i,
       i * 13 + 1,
       i * 0.5 + 14,
       'c015-' || i,
       i * 16 + 1,
       i * 0.5 + 17,
       'c018-' || i,
       i * 19 + 1,
       i * 0.5 + 20,
       'c021-' || i,
       i * 22 + 1,
       i * 0.5 + 23,
       'c024-' || i,
       i * 25 + 1,
       i * 0.5 + 26,
       'c027-' || i,
       i * 28 + 1,
       i * 0.5 + 29,
       'c030-' || i,
       i * 31 + 1,
       i * 0.5 + 32,
       'c033-' || i,
       i * 34 + 1,
       i * 0.5 + 35,
       'c036-' || i,
       i * 37 + 1,
       i * 0.5 + 38,
       'c039-' || i,
       i * 40 + 1,
       i * 0.5 + 41,
       'c042-' || i,
       i * 43 + 1,
       i * 0.5 + 44,
       'c045-' || i,
       i * 46 + 1,
       i * 0.5 + 47,
       'c048-' || i,
       i * 49 + 1,
       i * 0.5 + 50,
       'c051-' || i,
       i * 52 + 1,
       i * 0.5 + 53,
       'c054-' || i,
       i * 55 + 1,
       i * 0.5 + 56,
       'c057-' || i,
       i * 58 + 1,
       i * 0.5 + 59,
       'c060-' || i,
       i * 61 + 1,
       i * 0.5 + 62,
       'c063-' || i,
       i * 64 + 1,
       i * 0.5 + 65,
       'c066-' || i,
       i * 67 + 1,
       i * 0.5 + 68,
       'c069-' || i,
       i * 70 + 1,
       i * 0.5 + 71,
       'c072-' || i,
       i * 73 + 1,
       i * 0.5 + 74,
       'c075-' || i,
       i * 76 + 1,
       i * 0.5 + 77,
       'c078-' || i,
       i * 79 + 1,
       i * 0.5 + 80,
       'c081-' || i,
       i * 82 + 1,
       i * 0.5 + 83,
       'c084-' || i,
       i * 85 + 1,
       i * 0.5 + 86,
       'c087-' || i,
       i * 88 + 1,
       i * 0.5 + 89,
       'c090-' || i,
       i * 91 + 1,
       i * 0.5 + 92,
       'c093-' || i,
       i * 94 + 1,
       i * 0.5 + 95,
       'c096-' || i,
       i * 97 + 1,
       i * 0.5 + 98,
       'c099-' || i,
       i * 100 + 1,
       i * 0.5 + 101,
       'c102-' || i,
       i * 103 + 1,
       i * 0.5 + 104,
       'c105-' || i,
       i * 106 + 1,
       i * 0.5 + 107,
       'c108-' || i,
       i * 109 + 1,
       i * 0.5 + 110,
       'c111-' || i,
       i * 112 + 1,
       i * 0.5 + 113,
       'c114-' || i,
       i * 115 + 1,
       i * 0.5 + 116,
       'c117-' || i,
       i * 118 + 1,
       i * 0.5 + 119,
       'c120-' || i,
       i * 121 + 1,
       i * 0.5 + 122,
       'c123-' || i,
       i * 124 + 1,
       i * 0.5 + 125,
       'c126-' || i,
       i * 127 + 1,
       i * 0.5 + 128,
       'c129-' || i,
       i * 130 + 1,
       i * 0.5 + 131,
       'c132-' || i,
       i * 133 + 1,
       i * 0.5 + 134,
       'c135-' || i,
       i * 136 + 1,
       i * 0.5 + 137,
       'c138-' || i,
       i * 139 + 1,
       i * 0.5 + 140,
       'c141-' || i,
       i * 142 + 1,
       i * 0.5 + 143,
       'c144-' || i,
       i * 145 + 1,
       i * 0.5 + 146,
       'c147-' || i,
       i * 148 + 1,
       i * 0.5 + 149,
       'c150-' || i,
       i * 151 + 1,
       i * 0.5 + 152,
       'c153-' || i,
       i * 154 + 1,
       i * 0.5 + 155,
       'c156-' || i,
       i * 157 + 1,
       i * 0.5 + 158,
       'c159-' || i,
       i * 160 + 1,
       i * 0.5 + 161,
       'c162-' || i,
       i * 163 + 1,
       i * 0.5 + 164,
       'c165-' || i,
       i * 166 + 1,
       i * 0.5 + 167,
       'c168-' || i,
       i * 169 + 1,
       i * 0.5 + 170,
       'c171-' || i,
       i * 172 + 1,
       i * 0.5 + 173,
       'c174-' || i,
       i * 175 + 1,
       i * 0.5 + 176,
       'c177-' || i,
       i * 178 + 1,
       i * 0.5 + 179,
       'c180-' || i,
       i * 181 + 1,
       i * 0.5 + 182,
       'c183-' || i,
       i * 184 + 1,
       i * 0.5 + 185,
       'c186-' || i,
       i * 187 + 1,
       i * 0.5 + 188,
       'c189-' || i,
       i * 190 + 1,
       i * 0.5 + 191,
       'c192-' || i,
       i * 193 + 1,
       i * 0.5 + 194,
       'c195-' || i,
       i * 196 + 1,
       i * 0.5 + 197,
       'c198-' || i,
       i * 199 + 1
FROM range(1, 26) AS n (i);

-- Types only DuckDB has, for checking how the client renders them. Nested
-- values hold NULLs at every level, since that is where a renderer slips.
CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');

CREATE TABLE type_gallery (
    id INTEGER PRIMARY KEY,
    label TEXT NOT NULL,
    int_list INTEGER[],
    text_list VARCHAR[],
    fixed_array INTEGER[3],
    point STRUCT(x DOUBLE, y DOUBLE),
    profile STRUCT(name VARCHAR, tags VARCHAR[], address STRUCT(city VARCHAR, zip VARCHAR)),
    scores MAP(VARCHAR, INTEGER),
    nested_map MAP(VARCHAR, INTEGER[]),
    list_of_structs STRUCT(k VARCHAR, v INTEGER)[],
    list_of_lists INTEGER[][],
    current_mood mood,
    uid UUID,
    doc JSON,
    huge HUGEINT,
    uhuge UHUGEINT,
    u8 UTINYINT,
    u16 USMALLINT,
    u32 UINTEGER,
    u64 UBIGINT,
    i8 TINYINT,
    i16 SMALLINT,
    dec_small DECIMAL(4, 1),
    dec_wide DECIMAL(38, 10),
    span INTERVAL,
    ts_tz TIMESTAMPTZ,
    ts_ns TIMESTAMP_NS,
    ts_ms TIMESTAMP_MS,
    ts_s TIMESTAMP_S,
    clock TIME,
    clock_tz TIMETZ,
    day DATE,
    raw BLOB,
    flags BIT,
    big VARINT,
    ratio FLOAT,
    precise DOUBLE
);

INSERT INTO type_gallery VALUES
    (
        1, 'typical',
        [1, 2, 3], ['a', 'b'], [1, 2, 3],
        {'x': 1.5, 'y': -2.5},
        {'name': 'Ada', 'tags': ['founder', 'priority'], 'address': {'city': 'London', 'zip': 'N1'}},
        MAP {'a': 1, 'b': 2},
        MAP {'evens': [2, 4], 'odds': [1, 3]},
        [{'k': 'one', 'v': 1}, {'k': 'two', 'v': 2}],
        [[1, 2], [3], []],
        'happy',
        '018f1f6e-7c2a-7000-8000-000000000001',
        '{"a":[1,2,{"b":null}]}',
        170141183460469231731687303715884105727,
        340282366920938463463374607431768211455,
        255, 65535, 4294967295, 18446744073709551615,
        -128, -32768,
        -123.4,
        1234567890123456789012345678.1234567890,
        INTERVAL '1 year 2 months 3 days 04:05:06.789',
        '2024-06-15 12:34:56.789012+02',
        '2024-06-15 12:34:56.123456789',
        '2024-06-15 12:34:56.123',
        '2024-06-15 12:34:56',
        '23:59:59.999999',
        '12:34:56+05:30',
        '2024-02-29',
        '\x00\x01\xFE\xFF'::BLOB,
        '101010011'::BIT,
        123456789012345678901234567890::VARINT,
        1.1,
        0.1 + 0.2
    ),
    (
        2, 'NULLs inside nested values',
        [1, NULL, 3], [NULL, 'b'], [NULL, NULL, 3],
        {'x': NULL, 'y': 2.0},
        {'name': NULL, 'tags': [NULL, 'x'], 'address': {'city': NULL, 'zip': NULL}},
        MAP {'a': NULL, 'b': 2},
        MAP {'none': NULL, 'some': [NULL, 1]},
        [{'k': NULL, 'v': 1}, NULL],
        [[NULL], NULL, [1, NULL]],
        'sad',
        'ffffffff-ffff-ffff-ffff-ffffffffffff',
        'null',
        -170141183460469231731687303715884105728,
        0,
        0, 0, 0, 0,
        127, 32767,
        99.9,
        -0.0000000001,
        INTERVAL '-5 days',
        '1970-01-01 00:00:00+00',
        '1970-01-01 00:00:00.000000001',
        '1970-01-01 00:00:00.001',
        '1970-01-01 00:00:01',
        '00:00:00',
        '00:00:00+00',
        '0001-01-01',
        ''::BLOB,
        '0'::BIT,
        -123456789012345678901234567890::VARINT,
        'NaN'::FLOAT,
        'Infinity'::DOUBLE
    ),
    (
        3, 'infinite dates',
        [], [], NULL,
        NULL, NULL, MAP {}, NULL, [], [[]],
        'ok',
        '00000000-0000-0000-0000-000000000000',
        '"just a string"',
        NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
        INTERVAL '0 seconds',
        'infinity', '-infinity', 'infinity', '-infinity',
        NULL, NULL,
        'infinity',
        NULL, NULL, NULL, '-Infinity'::FLOAT, '-Infinity'::DOUBLE
    ),
    (
        4, 'minus infinity date',
        NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
        NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
        NULL, NULL, NULL, NULL, NULL, NULL,
        '-infinity',
        NULL, NULL, NULL, NULL, NULL
    );

-- Two tables with no primary key, so the client's choice of row identity is
-- visible: `tally` is keyless (made with CREATE TABLE AS, which carries no
-- constraints), `contacts` has only a unique index to go on.
CREATE TABLE tally AS
SELECT i AS n, 'row-' || i AS label FROM range(1, 11) AS t (i);

CREATE TABLE contacts (
    email TEXT NOT NULL,
    display_name TEXT
);

CREATE UNIQUE INDEX contacts_email_key ON contacts (email);

INSERT INTO contacts VALUES
    ('ada@example.test', 'Ada'),
    ('grace@example.test', 'Grace'),
    ('edsger@example.test', NULL);

-- A plain secondary index, so the Structure tab has one that is not a key.
CREATE INDEX events_account_id_idx ON events (account_id);

CREATE TABLE accounts (
    id INTEGER PRIMARY KEY,
    external_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    email TEXT,
    plan TEXT NOT NULL CHECK (plan IN ('free', 'team', 'enterprise')),
    balance NUMERIC NOT NULL,
    active BOOLEAN NOT NULL,
    tags TEXT NOT NULL,
    metadata TEXT NOT NULL,
    created_at TEXT NOT NULL
);

INSERT INTO accounts (
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
    recorded_at TEXT NOT NULL,
    sensor TEXT NOT NULL,
    temperature_c REAL,
    pressure_kpa NUMERIC,
    healthy BOOLEAN NOT NULL,
    samples TEXT NOT NULL,
    payload TEXT NOT NULL
);

WITH RECURSIVE series (sample) AS (
    SELECT 1
    UNION ALL
    SELECT sample + 1 FROM series WHERE sample < 5000
)
INSERT INTO measurements
SELECT
    sample,
    strftime('%Y-%m-%d %H:%M:%f', '2025-01-01 00:00:00', (sample * 15) || ' seconds'),
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
FROM series;

-- Values far larger than a cell can show, and values a cell would misread:
-- embedded newlines, an embedded semicolon, a JSON null, and raw bytes. The
-- blob is what exercises the `x'...'` rendering.
CREATE TABLE documents (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    body TEXT,
    document TEXT,
    binary_value BLOB
);

INSERT INTO documents VALUES (
    1,
    'Multiline text',
    -- SQLite has no backslash escapes in a string literal, so the newlines are
    -- built rather than written.
    'first line' || char(10) || 'second line' || char(10) || 'third line; with a semicolon',
    '{"kind":"short","nested":{"null_value":null}}',
    x'00010203FEFF'
);

-- SQLite has no `repeat`, so a run of N copies is `hex(zeroblob(N))` -- 2N
-- zero characters -- with each '00' replaced by the text to repeat.
WITH RECURSIVE series (value) AS (
    SELECT 1
    UNION ALL
    SELECT value + 1 FROM series WHERE value < 500
)
INSERT INTO documents
SELECT
    2,
    'Large values',
    replace(hex(zeroblob(2048)), '00', 'DBDelve keeps the complete value while the grid clips visually. '),
    json_object(
        'kind', 'large',
        'values', json_group_array(json_object('index', value, 'square', value * value))
    ),
    unhex(replace(hex(zeroblob(4096)), '00', 'deadbeef'))
FROM series;

-- Geometry columns (`point`, `boundary`) are PostGIS-only; SQLite has no
-- built-in geometry type (SpatiaLite is a separate extension, out of scope).
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
    placed_at TEXT NOT NULL,
    total NUMERIC NOT NULL,
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

-- There is deliberately no cross-schema fixture here. A SQLite foreign key may
-- not reference an attached database, so the parent of every key is always in
-- the database the key itself lives in -- the case Postgres and MySQL cover
-- cannot be written at all against SQLite, rather than merely being omitted.

CREATE VIEW account_overview AS
SELECT
    plan,
    count(*) AS accounts,
    sum(balance) AS total_balance,
    count(CASE WHEN active THEN 1 END) AS active_accounts
FROM accounts
GROUP BY plan;

-- account_label is a Postgres/MySQL stored function; SQLite has no stored
-- functions, so there is nothing to port here.
-- `deactivate_account` is a Postgres/MySQL stored procedure, and has nowhere to
-- go here for the same reason.

-- The engine conformance suite's fixture (src/db/conformance.rs): one table,
-- the same shape in every seed. `whole` is above 2^53 on purpose. Ids 900 and
-- up belong to the suite's write scenarios.

CREATE TABLE conformance (
    id INTEGER PRIMARY KEY,
    label TEXT NOT NULL,
    whole INTEGER,
    fraction NUMERIC,
    ratio REAL,
    flag BOOLEAN,
    day DATE,
    moment DATETIME,
    bytes BLOB
);

INSERT INTO conformance (id, label, whole, fraction, ratio, flag, day, moment, bytes) VALUES
    (1, 'alpha', 9007199254740993, 12.50, 0.5, 1, '2024-02-29', '2024-02-29 12:34:56', X'00FF'),
    (2, 'Zoë 李 🐉 ''quoted''', -42, -0.01, -1.25, 0, '1999-12-31', '1999-12-31 23:59:59', X'DEADBEEF'),
    (3, 'nulls', NULL, NULL, NULL, NULL, NULL, NULL, NULL);

-- A million rows and two hundred columns: the fixtures the grid's paging and
-- horizontal scrolling are measured against, not the hand-written rows above.

CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    amount NUMERIC NOT NULL,
    occurred_at TEXT NOT NULL
);

INSERT INTO events (account_id, kind, amount, occurred_at)
WITH RECURSIVE n(i) AS (
    SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000000
)
SELECT (i % 6) + 1,
       CASE i % 6
           WHEN 0 THEN 'click'
           WHEN 1 THEN 'view'
           WHEN 2 THEN 'purchase'
           WHEN 3 THEN 'refund'
           WHEN 4 THEN 'signup'
           ELSE 'churn'
       END,
       (i % 100000) / 100.0,
       datetime('2024-01-01 00:00:00', '+' || i || ' seconds')
FROM n;

CREATE TABLE wide_metrics (
    id INTEGER PRIMARY KEY,
    c001 INTEGER NOT NULL,
    c002 REAL NOT NULL,
    c003 TEXT NOT NULL,
    c004 INTEGER NOT NULL,
    c005 REAL NOT NULL,
    c006 TEXT NOT NULL,
    c007 INTEGER NOT NULL,
    c008 REAL NOT NULL,
    c009 TEXT NOT NULL,
    c010 INTEGER NOT NULL,
    c011 REAL NOT NULL,
    c012 TEXT NOT NULL,
    c013 INTEGER NOT NULL,
    c014 REAL NOT NULL,
    c015 TEXT NOT NULL,
    c016 INTEGER NOT NULL,
    c017 REAL NOT NULL,
    c018 TEXT NOT NULL,
    c019 INTEGER NOT NULL,
    c020 REAL NOT NULL,
    c021 TEXT NOT NULL,
    c022 INTEGER NOT NULL,
    c023 REAL NOT NULL,
    c024 TEXT NOT NULL,
    c025 INTEGER NOT NULL,
    c026 REAL NOT NULL,
    c027 TEXT NOT NULL,
    c028 INTEGER NOT NULL,
    c029 REAL NOT NULL,
    c030 TEXT NOT NULL,
    c031 INTEGER NOT NULL,
    c032 REAL NOT NULL,
    c033 TEXT NOT NULL,
    c034 INTEGER NOT NULL,
    c035 REAL NOT NULL,
    c036 TEXT NOT NULL,
    c037 INTEGER NOT NULL,
    c038 REAL NOT NULL,
    c039 TEXT NOT NULL,
    c040 INTEGER NOT NULL,
    c041 REAL NOT NULL,
    c042 TEXT NOT NULL,
    c043 INTEGER NOT NULL,
    c044 REAL NOT NULL,
    c045 TEXT NOT NULL,
    c046 INTEGER NOT NULL,
    c047 REAL NOT NULL,
    c048 TEXT NOT NULL,
    c049 INTEGER NOT NULL,
    c050 REAL NOT NULL,
    c051 TEXT NOT NULL,
    c052 INTEGER NOT NULL,
    c053 REAL NOT NULL,
    c054 TEXT NOT NULL,
    c055 INTEGER NOT NULL,
    c056 REAL NOT NULL,
    c057 TEXT NOT NULL,
    c058 INTEGER NOT NULL,
    c059 REAL NOT NULL,
    c060 TEXT NOT NULL,
    c061 INTEGER NOT NULL,
    c062 REAL NOT NULL,
    c063 TEXT NOT NULL,
    c064 INTEGER NOT NULL,
    c065 REAL NOT NULL,
    c066 TEXT NOT NULL,
    c067 INTEGER NOT NULL,
    c068 REAL NOT NULL,
    c069 TEXT NOT NULL,
    c070 INTEGER NOT NULL,
    c071 REAL NOT NULL,
    c072 TEXT NOT NULL,
    c073 INTEGER NOT NULL,
    c074 REAL NOT NULL,
    c075 TEXT NOT NULL,
    c076 INTEGER NOT NULL,
    c077 REAL NOT NULL,
    c078 TEXT NOT NULL,
    c079 INTEGER NOT NULL,
    c080 REAL NOT NULL,
    c081 TEXT NOT NULL,
    c082 INTEGER NOT NULL,
    c083 REAL NOT NULL,
    c084 TEXT NOT NULL,
    c085 INTEGER NOT NULL,
    c086 REAL NOT NULL,
    c087 TEXT NOT NULL,
    c088 INTEGER NOT NULL,
    c089 REAL NOT NULL,
    c090 TEXT NOT NULL,
    c091 INTEGER NOT NULL,
    c092 REAL NOT NULL,
    c093 TEXT NOT NULL,
    c094 INTEGER NOT NULL,
    c095 REAL NOT NULL,
    c096 TEXT NOT NULL,
    c097 INTEGER NOT NULL,
    c098 REAL NOT NULL,
    c099 TEXT NOT NULL,
    c100 INTEGER NOT NULL,
    c101 REAL NOT NULL,
    c102 TEXT NOT NULL,
    c103 INTEGER NOT NULL,
    c104 REAL NOT NULL,
    c105 TEXT NOT NULL,
    c106 INTEGER NOT NULL,
    c107 REAL NOT NULL,
    c108 TEXT NOT NULL,
    c109 INTEGER NOT NULL,
    c110 REAL NOT NULL,
    c111 TEXT NOT NULL,
    c112 INTEGER NOT NULL,
    c113 REAL NOT NULL,
    c114 TEXT NOT NULL,
    c115 INTEGER NOT NULL,
    c116 REAL NOT NULL,
    c117 TEXT NOT NULL,
    c118 INTEGER NOT NULL,
    c119 REAL NOT NULL,
    c120 TEXT NOT NULL,
    c121 INTEGER NOT NULL,
    c122 REAL NOT NULL,
    c123 TEXT NOT NULL,
    c124 INTEGER NOT NULL,
    c125 REAL NOT NULL,
    c126 TEXT NOT NULL,
    c127 INTEGER NOT NULL,
    c128 REAL NOT NULL,
    c129 TEXT NOT NULL,
    c130 INTEGER NOT NULL,
    c131 REAL NOT NULL,
    c132 TEXT NOT NULL,
    c133 INTEGER NOT NULL,
    c134 REAL NOT NULL,
    c135 TEXT NOT NULL,
    c136 INTEGER NOT NULL,
    c137 REAL NOT NULL,
    c138 TEXT NOT NULL,
    c139 INTEGER NOT NULL,
    c140 REAL NOT NULL,
    c141 TEXT NOT NULL,
    c142 INTEGER NOT NULL,
    c143 REAL NOT NULL,
    c144 TEXT NOT NULL,
    c145 INTEGER NOT NULL,
    c146 REAL NOT NULL,
    c147 TEXT NOT NULL,
    c148 INTEGER NOT NULL,
    c149 REAL NOT NULL,
    c150 TEXT NOT NULL,
    c151 INTEGER NOT NULL,
    c152 REAL NOT NULL,
    c153 TEXT NOT NULL,
    c154 INTEGER NOT NULL,
    c155 REAL NOT NULL,
    c156 TEXT NOT NULL,
    c157 INTEGER NOT NULL,
    c158 REAL NOT NULL,
    c159 TEXT NOT NULL,
    c160 INTEGER NOT NULL,
    c161 REAL NOT NULL,
    c162 TEXT NOT NULL,
    c163 INTEGER NOT NULL,
    c164 REAL NOT NULL,
    c165 TEXT NOT NULL,
    c166 INTEGER NOT NULL,
    c167 REAL NOT NULL,
    c168 TEXT NOT NULL,
    c169 INTEGER NOT NULL,
    c170 REAL NOT NULL,
    c171 TEXT NOT NULL,
    c172 INTEGER NOT NULL,
    c173 REAL NOT NULL,
    c174 TEXT NOT NULL,
    c175 INTEGER NOT NULL,
    c176 REAL NOT NULL,
    c177 TEXT NOT NULL,
    c178 INTEGER NOT NULL,
    c179 REAL NOT NULL,
    c180 TEXT NOT NULL,
    c181 INTEGER NOT NULL,
    c182 REAL NOT NULL,
    c183 TEXT NOT NULL,
    c184 INTEGER NOT NULL,
    c185 REAL NOT NULL,
    c186 TEXT NOT NULL,
    c187 INTEGER NOT NULL,
    c188 REAL NOT NULL,
    c189 TEXT NOT NULL,
    c190 INTEGER NOT NULL,
    c191 REAL NOT NULL,
    c192 TEXT NOT NULL,
    c193 INTEGER NOT NULL,
    c194 REAL NOT NULL,
    c195 TEXT NOT NULL,
    c196 INTEGER NOT NULL,
    c197 REAL NOT NULL,
    c198 TEXT NOT NULL,
    c199 INTEGER NOT NULL
);

INSERT INTO wide_metrics (c001, c002, c003, c004, c005, c006, c007, c008, c009, c010, c011, c012, c013, c014, c015, c016, c017, c018, c019, c020, c021, c022, c023, c024, c025, c026, c027, c028, c029, c030, c031, c032, c033, c034, c035, c036, c037, c038, c039, c040, c041, c042, c043, c044, c045, c046, c047, c048, c049, c050, c051, c052, c053, c054, c055, c056, c057, c058, c059, c060, c061, c062, c063, c064, c065, c066, c067, c068, c069, c070, c071, c072, c073, c074, c075, c076, c077, c078, c079, c080, c081, c082, c083, c084, c085, c086, c087, c088, c089, c090, c091, c092, c093, c094, c095, c096, c097, c098, c099, c100, c101, c102, c103, c104, c105, c106, c107, c108, c109, c110, c111, c112, c113, c114, c115, c116, c117, c118, c119, c120, c121, c122, c123, c124, c125, c126, c127, c128, c129, c130, c131, c132, c133, c134, c135, c136, c137, c138, c139, c140, c141, c142, c143, c144, c145, c146, c147, c148, c149, c150, c151, c152, c153, c154, c155, c156, c157, c158, c159, c160, c161, c162, c163, c164, c165, c166, c167, c168, c169, c170, c171, c172, c173, c174, c175, c176, c177, c178, c179, c180, c181, c182, c183, c184, c185, c186, c187, c188, c189, c190, c191, c192, c193, c194, c195, c196, c197, c198, c199)
WITH RECURSIVE n(i) AS (
    SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 25
)
SELECT i * 1 + 1,
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
FROM n;

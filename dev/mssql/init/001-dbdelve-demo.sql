-- CHECK_POLICY is off because the app password is fixed by the dev seed
-- convention (matching the Postgres/MySQL containers) and it embeds the
-- login name, which SQL Server's default complexity policy refuses.
CREATE LOGIN dbdelve WITH PASSWORD = 'DBDelve_dev1', CHECK_POLICY = OFF;
GO

CREATE DATABASE dbdelve_dev;
GO

USE dbdelve_dev;
GO

CREATE USER dbdelve FOR LOGIN dbdelve;
ALTER ROLE db_owner ADD MEMBER dbdelve;
GO

-- A reference that crosses a schema boundary, so the catalog is forced to
-- report the schema of the referenced table and not just its name.
CREATE SCHEMA archive;
GO

CREATE TABLE accounts (
    id bigint IDENTITY(1,1) PRIMARY KEY,
    external_id uniqueidentifier NOT NULL UNIQUE,
    name nvarchar(200) NOT NULL,
    email nvarchar(200) NULL,
    [plan] nvarchar(20) NOT NULL CONSTRAINT CK_accounts_plan CHECK ([plan] IN (N'free', N'team', N'enterprise')),
    balance decimal(14, 2) NOT NULL,
    active bit NOT NULL,
    tags nvarchar(max) NOT NULL,
    metadata nvarchar(max) NOT NULL,
    created_at datetime2(6) NOT NULL
);

INSERT INTO accounts (
    external_id,
    name,
    email,
    [plan],
    balance,
    active,
    tags,
    metadata,
    created_at
) VALUES
    (
        '018f1f6e-7c2a-7000-8000-000000000001',
        N'Ada Lovelace',
        N'ada@example.test',
        N'enterprise',
        125000.50,
        1,
        N'["founder","priority"]',
        N'{"timezone":"Europe/London","features":{"audit":true,"seats":250}}',
        '2024-01-15 09:30:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000002',
        N'Grace Hopper',
        N'grace@example.test',
        N'team',
        8192.00,
        1,
        N'["compiler","navy"]',
        N'{"timezone":"America/New_York","languages":["COBOL","English"]}',
        '2024-02-29 12:00:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000003',
        N'Edsger Dijkstra',
        NULL,
        N'free',
        -0.01,
        0,
        N'[]',
        N'{"note":"Simplicity is prerequisite for reliability."}',
        '2024-03-10 18:45:12.123456'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000004',
        N'李小龍',
        N'bruce.lee@example.test',
        N'team',
        42.42,
        1,
        N'["unicode","香港"]',
        N'{"display_name":"李小龍","emoji":"🐉","rtl":"مرحبا"}',
        '2024-04-01 00:00:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000005',
        N'Quotes ''n'' Backslashes \\',
        N'escaping@example.test',
        N'free',
        0.00,
        1,
        N'["quotes","backslash"]',
        N'{"sql":"SELECT ''not a delimiter;'';","path":"C:\\demo\\file"}',
        '2024-05-05 05:05:05'
    );
GO

CREATE TABLE measurements (
    id bigint PRIMARY KEY,
    recorded_at datetime2(6) NOT NULL,
    sensor nvarchar(32) NOT NULL,
    temperature_c float NULL,
    pressure_kpa decimal(8, 3) NULL,
    healthy bit NOT NULL,
    samples nvarchar(max) NOT NULL,
    payload nvarchar(max) NOT NULL
);
GO

-- Five thousand rows, so scrolling, sorting and the row-count chip have
-- something to work against rather than a grid that fits on screen. The
-- default MAXRECURSION of 100 needs raising first, same reason as MySQL's
-- cte_max_recursion_depth.
WITH series (sample) AS (
    SELECT 1
    UNION ALL
    SELECT sample + 1 FROM series WHERE sample < 5000
)
INSERT INTO measurements (id, recorded_at, sensor, temperature_c, pressure_kpa, healthy, samples, payload)
SELECT
    sample,
    DATEADD(SECOND, sample * 15, '2025-01-01 00:00:00'),
    CONCAT('sensor-', RIGHT('0' + CAST(((sample - 1) % 24) + 1 AS varchar(10)), 2)),
    -- A null every 97th row, so the grid's null rendering is reachable by
    -- scrolling rather than only by writing a query for it.
    CASE WHEN sample % 97 = 0 THEN NULL ELSE 18.0 + (sample % 150) / 10.0 END,
    98.000 + (sample % 700) / 1000.0,
    IIF(sample % 113 <> 0, CAST(1 AS bit), CAST(0 AS bit)),
    CONCAT('[', sample % 10, ',', sample % 20, ',', sample % 30, ']'),
    CONCAT(
        '{"sequence":', sample,
        ',"firmware":"v', 1 + sample % 3, '.', sample % 10,
        '","flags":[', IIF(sample % 2 = 0, 'true', 'false'), ',', IIF(sample % 5 = 0, 'true', 'false'), ']}'
    )
FROM series
OPTION (MAXRECURSION 10000);
GO

-- Values far larger than a cell can show, and values a cell would misread:
-- embedded newlines, an embedded semicolon, a JSON null, and raw bytes.
CREATE TABLE documents (
    id int PRIMARY KEY,
    title nvarchar(200) NOT NULL,
    body nvarchar(max) NULL,
    document nvarchar(max) NULL,
    binary_value varbinary(max) NULL
);
GO

INSERT INTO documents VALUES (
    1,
    N'Multiline text',
    N'first line' + CHAR(10) + N'second line' + CHAR(10) + N'third line; with a semicolon',
    N'{"kind":"short","nested":{"null_value":null}}',
    CONVERT(varbinary(max), '00010203feff', 2)
);
GO

WITH series (value) AS (
    SELECT 1
    UNION ALL
    SELECT value + 1 FROM series WHERE value < 500
)
INSERT INTO documents (id, title, body, document, binary_value)
SELECT
    2,
    N'Large values',
    -- REPLICATE truncates at 8000/4000 chars unless the source is cast to
    -- (n)varchar(max) first; the literal alone is typed varchar(n).
    REPLICATE(CAST(N'DBDelve keeps the complete value while the grid clips visually. ' AS nvarchar(max)), 2048),
    -- STRING_AGG's result stays non-LOB (8000-byte cap) unless an argument is
    -- explicitly varchar(max); the concatenated values here are ~4KB.
    N'{"kind":"large","values":[' + STRING_AGG(CONCAT(CAST('{"index":' AS varchar(max)), value, ',"square":', value * value, '}'), ',') WITHIN GROUP (ORDER BY value) + ']}',
    CONVERT(varbinary(max), REPLICATE(CAST('deadbeef' AS varchar(max)), 4096), 2)
FROM series
OPTION (MAXRECURSION 1000);
GO

-- Geometry types (`geometry`/`geography`) are PostGIS-only in the seeds this
-- ports; MySQL geometry support is out of scope there too, so this stays
-- plain like the MySQL table.
CREATE TABLE locations (
    id int PRIMARY KEY,
    name nvarchar(200) NOT NULL
);

INSERT INTO locations (id, name) VALUES
    (1, N'San Francisco'),
    (2, N'Null Island');
GO

-- Keys worth following. `orders` is both ends of the problem at once: a
-- composite primary key, and a single-column foreign key into `accounts`, so
-- the simple case and the parent of the hard case are one table.
-- `placed_at` is `datetimeoffset` rather than `datetime2`, the one place this
-- seed reaches for a SQL-Server-only temporal type.
CREATE TABLE orders (
    account_id bigint NOT NULL,
    number int NOT NULL,
    placed_at datetimeoffset(0) NOT NULL,
    total decimal(14, 2) NOT NULL,
    PRIMARY KEY (account_id, number),
    FOREIGN KEY (account_id) REFERENCES accounts (id)
);

INSERT INTO orders VALUES
    (1, 1001, '2024-06-01 10:00:00 +00:00', 4500.00),
    (1, 1002, '2024-06-08 11:30:00 +00:00', 125.75),
    (2, 2001, '2024-06-12 16:15:00 +00:00', 890.10);
GO

-- The composite foreign key, which the catalog has to report as one key over
-- two columns rather than two keys of one column each.
CREATE TABLE order_items (
    id int PRIMARY KEY,
    order_account_id bigint NOT NULL,
    order_number int NOT NULL,
    description nvarchar(200) NOT NULL,
    quantity int NOT NULL,
    FOREIGN KEY (order_account_id, order_number) REFERENCES orders (account_id, number)
);

INSERT INTO order_items VALUES
    (1, 1, 1001, N'Analytical engine time', 3),
    (2, 1, 1002, N'Punch card stock', 500),
    (3, 2, 2001, N'Compiler seat', 1);
GO

CREATE TABLE archive.closed_accounts (
    id int PRIMARY KEY,
    account_id bigint NOT NULL REFERENCES dbo.accounts (id),
    closed_at datetime2(6) NOT NULL
);

INSERT INTO archive.closed_accounts VALUES
    (1, 3, '2024-07-01 00:00:00'),
    (2, 5, '2024-07-04 12:00:00');
GO

CREATE VIEW account_overview AS
SELECT
    [plan],
    COUNT(*) AS accounts,
    SUM(balance) AS total_balance,
    SUM(CASE WHEN active = 1 THEN 1 ELSE 0 END) AS active_accounts
FROM accounts
GROUP BY [plan];
GO

-- Routines the explorer can actually show. Extension-owned routines are
-- filtered out of the catalog, so without these the routine surface has
-- nothing to display against this database.
CREATE FUNCTION dbo.account_label (@account_id bigint)
RETURNS nvarchar(400)
AS
BEGIN
    DECLARE @label nvarchar(400);
    SELECT @label = CONCAT(name, N' (', [plan], N')')
    FROM accounts
    WHERE id = @account_id;
    RETURN @label;
END
GO

CREATE PROCEDURE dbo.deactivate_account @account_id bigint
AS
BEGIN
    UPDATE accounts SET active = 0 WHERE id = @account_id;
END
GO

-- Keys whose literal depends on the column's type: a binary key is a bare
-- `0x...`, and a `datetime` key has to read the same under a session's
-- `SET LANGUAGE british`, which reads `2024-01-02` as the first of February:
-- the other row.
CREATE TABLE blobs_by_hash (
    hash binary(16) PRIMARY KEY,
    label nvarchar(50) NOT NULL
);

INSERT INTO blobs_by_hash VALUES
    (0x000102030405060708090A0B0C0D0EFF, N'first'),
    (0xFF0E0D0C0B0A09080706050403020100, N'second');

CREATE TABLE readings_by_time (
    taken_at datetime PRIMARY KEY,
    reading decimal(6, 2) NOT NULL
);

INSERT INTO readings_by_time VALUES
    ('2024-01-02T03:04:05', 1.00),
    ('2024-02-01T03:04:05', 2.00);
GO

-- Types tiberius cannot decode, which a preview reads as text instead.
CREATE TABLE places (
    id int PRIMARY KEY,
    position geography NOT NULL,
    node hierarchyid NULL,
    extra sql_variant NULL
);

INSERT INTO places VALUES
    (1, geography::Point(51.5, -0.125, 4326), hierarchyid::Parse('/1/'), CAST(42 AS sql_variant)),
    (2, geography::Point(0, 0, 4326), hierarchyid::Parse('/1/2/'), CAST(N'text' AS sql_variant)),
    (3, geography::Point(-33.5, 151.25, 4326), NULL, NULL);
GO

-- The structure surface's harder cases in one place: an alias type, an
-- identity and a computed column, a filtered unique index with included
-- columns, both kinds of columnstore index and a cascading foreign key.
-- A filtered index refuses writes unless QUOTED_IDENTIFIER is on, and sqlcmd
-- leaves it off.
SET QUOTED_IDENTIFIER ON;
GO

CREATE TYPE dbo.sku FROM varchar(16) NOT NULL;
GO

CREATE TABLE products (
    id int IDENTITY(1,1) PRIMARY KEY,
    sku dbo.sku,
    name nvarchar(100) NOT NULL,
    price decimal(10, 2) NOT NULL,
    discontinued bit NOT NULL,
    price_with_tax AS (price * 1.2) PERSISTED
);

CREATE UNIQUE NONCLUSTERED INDEX ux_products_live_sku
    ON products (sku) INCLUDE (name, price) WHERE discontinued = 0;

CREATE NONCLUSTERED COLUMNSTORE INDEX ncci_products ON products (price, discontinued);

CREATE TABLE product_tags (
    product_id int NOT NULL,
    tag varchar(20) NOT NULL,
    PRIMARY KEY (product_id, tag),
    CONSTRAINT fk_product_tags_product FOREIGN KEY (product_id)
        REFERENCES products (id) ON DELETE CASCADE
);

INSERT INTO products (sku, name, price, discontinued) VALUES
    ('AE-001', N'Analytical engine', 9999.99, 0),
    ('PC-500', N'Punch cards', 12.50, 0),
    ('PC-500', N'Punch cards (old stock)', 10.00, 1);

INSERT INTO product_tags VALUES
    (1, 'hardware'),
    (2, 'consumable'),
    (3, 'consumable');

CREATE TABLE daily_totals (
    day date NOT NULL,
    total decimal(12, 2) NOT NULL
);

CREATE CLUSTERED COLUMNSTORE INDEX cci_daily_totals ON daily_totals;

INSERT INTO daily_totals VALUES
    ('2024-06-01', 4500.00),
    ('2024-06-08', 125.75),
    ('2024-06-12', 890.10);
GO

-- A million rows and two hundred columns: the fixtures the grid's paging and
-- horizontal scrolling are measured against, not the hand-written rows above.

CREATE TABLE events (
    id bigint IDENTITY(1,1) PRIMARY KEY,
    account_id bigint NOT NULL,
    kind nvarchar(16) NOT NULL,
    amount decimal(12, 2) NOT NULL,
    occurred_at datetime2(0) NOT NULL
);
GO

-- Six self-joined digits beat a recursive CTE here, same reason as the MySQL
-- seed: a million-row CTE materializes into a temp table before the insert
-- sees a single row.
CREATE TABLE seq10 (d tinyint NOT NULL);
INSERT INTO seq10 VALUES (0), (1), (2), (3), (4), (5), (6), (7), (8), (9);

INSERT INTO events (account_id, kind, amount, occurred_at)
SELECT (n % 6) + 1,
       CASE (n % 6) + 1
           WHEN 1 THEN N'click'
           WHEN 2 THEN N'view'
           WHEN 3 THEN N'purchase'
           WHEN 4 THEN N'refund'
           WHEN 5 THEN N'signup'
           ELSE N'churn'
       END,
       CAST(n % 100000 AS decimal(12, 2)) / 100,
       DATEADD(SECOND, n, '2024-01-01 00:00:00')
FROM (
    SELECT a.d + b.d * 10 + c.d * 100 + d.d * 1000 + e.d * 10000 + f.d * 100000 + 1 AS n
    FROM seq10 a, seq10 b, seq10 c, seq10 d, seq10 e, seq10 f
) AS s;

DROP TABLE seq10;
GO

CREATE TABLE wide_metrics (
    id bigint IDENTITY(1,1) PRIMARY KEY,
    c001 bigint NOT NULL,
    c002 float NOT NULL,
    c003 nvarchar(32) NOT NULL,
    c004 bigint NOT NULL,
    c005 float NOT NULL,
    c006 nvarchar(32) NOT NULL,
    c007 bigint NOT NULL,
    c008 float NOT NULL,
    c009 nvarchar(32) NOT NULL,
    c010 bigint NOT NULL,
    c011 float NOT NULL,
    c012 nvarchar(32) NOT NULL,
    c013 bigint NOT NULL,
    c014 float NOT NULL,
    c015 nvarchar(32) NOT NULL,
    c016 bigint NOT NULL,
    c017 float NOT NULL,
    c018 nvarchar(32) NOT NULL,
    c019 bigint NOT NULL,
    c020 float NOT NULL,
    c021 nvarchar(32) NOT NULL,
    c022 bigint NOT NULL,
    c023 float NOT NULL,
    c024 nvarchar(32) NOT NULL,
    c025 bigint NOT NULL,
    c026 float NOT NULL,
    c027 nvarchar(32) NOT NULL,
    c028 bigint NOT NULL,
    c029 float NOT NULL,
    c030 nvarchar(32) NOT NULL,
    c031 bigint NOT NULL,
    c032 float NOT NULL,
    c033 nvarchar(32) NOT NULL,
    c034 bigint NOT NULL,
    c035 float NOT NULL,
    c036 nvarchar(32) NOT NULL,
    c037 bigint NOT NULL,
    c038 float NOT NULL,
    c039 nvarchar(32) NOT NULL,
    c040 bigint NOT NULL,
    c041 float NOT NULL,
    c042 nvarchar(32) NOT NULL,
    c043 bigint NOT NULL,
    c044 float NOT NULL,
    c045 nvarchar(32) NOT NULL,
    c046 bigint NOT NULL,
    c047 float NOT NULL,
    c048 nvarchar(32) NOT NULL,
    c049 bigint NOT NULL,
    c050 float NOT NULL,
    c051 nvarchar(32) NOT NULL,
    c052 bigint NOT NULL,
    c053 float NOT NULL,
    c054 nvarchar(32) NOT NULL,
    c055 bigint NOT NULL,
    c056 float NOT NULL,
    c057 nvarchar(32) NOT NULL,
    c058 bigint NOT NULL,
    c059 float NOT NULL,
    c060 nvarchar(32) NOT NULL,
    c061 bigint NOT NULL,
    c062 float NOT NULL,
    c063 nvarchar(32) NOT NULL,
    c064 bigint NOT NULL,
    c065 float NOT NULL,
    c066 nvarchar(32) NOT NULL,
    c067 bigint NOT NULL,
    c068 float NOT NULL,
    c069 nvarchar(32) NOT NULL,
    c070 bigint NOT NULL,
    c071 float NOT NULL,
    c072 nvarchar(32) NOT NULL,
    c073 bigint NOT NULL,
    c074 float NOT NULL,
    c075 nvarchar(32) NOT NULL,
    c076 bigint NOT NULL,
    c077 float NOT NULL,
    c078 nvarchar(32) NOT NULL,
    c079 bigint NOT NULL,
    c080 float NOT NULL,
    c081 nvarchar(32) NOT NULL,
    c082 bigint NOT NULL,
    c083 float NOT NULL,
    c084 nvarchar(32) NOT NULL,
    c085 bigint NOT NULL,
    c086 float NOT NULL,
    c087 nvarchar(32) NOT NULL,
    c088 bigint NOT NULL,
    c089 float NOT NULL,
    c090 nvarchar(32) NOT NULL,
    c091 bigint NOT NULL,
    c092 float NOT NULL,
    c093 nvarchar(32) NOT NULL,
    c094 bigint NOT NULL,
    c095 float NOT NULL,
    c096 nvarchar(32) NOT NULL,
    c097 bigint NOT NULL,
    c098 float NOT NULL,
    c099 nvarchar(32) NOT NULL,
    c100 bigint NOT NULL,
    c101 float NOT NULL,
    c102 nvarchar(32) NOT NULL,
    c103 bigint NOT NULL,
    c104 float NOT NULL,
    c105 nvarchar(32) NOT NULL,
    c106 bigint NOT NULL,
    c107 float NOT NULL,
    c108 nvarchar(32) NOT NULL,
    c109 bigint NOT NULL,
    c110 float NOT NULL,
    c111 nvarchar(32) NOT NULL,
    c112 bigint NOT NULL,
    c113 float NOT NULL,
    c114 nvarchar(32) NOT NULL,
    c115 bigint NOT NULL,
    c116 float NOT NULL,
    c117 nvarchar(32) NOT NULL,
    c118 bigint NOT NULL,
    c119 float NOT NULL,
    c120 nvarchar(32) NOT NULL,
    c121 bigint NOT NULL,
    c122 float NOT NULL,
    c123 nvarchar(32) NOT NULL,
    c124 bigint NOT NULL,
    c125 float NOT NULL,
    c126 nvarchar(32) NOT NULL,
    c127 bigint NOT NULL,
    c128 float NOT NULL,
    c129 nvarchar(32) NOT NULL,
    c130 bigint NOT NULL,
    c131 float NOT NULL,
    c132 nvarchar(32) NOT NULL,
    c133 bigint NOT NULL,
    c134 float NOT NULL,
    c135 nvarchar(32) NOT NULL,
    c136 bigint NOT NULL,
    c137 float NOT NULL,
    c138 nvarchar(32) NOT NULL,
    c139 bigint NOT NULL,
    c140 float NOT NULL,
    c141 nvarchar(32) NOT NULL,
    c142 bigint NOT NULL,
    c143 float NOT NULL,
    c144 nvarchar(32) NOT NULL,
    c145 bigint NOT NULL,
    c146 float NOT NULL,
    c147 nvarchar(32) NOT NULL,
    c148 bigint NOT NULL,
    c149 float NOT NULL,
    c150 nvarchar(32) NOT NULL,
    c151 bigint NOT NULL,
    c152 float NOT NULL,
    c153 nvarchar(32) NOT NULL,
    c154 bigint NOT NULL,
    c155 float NOT NULL,
    c156 nvarchar(32) NOT NULL,
    c157 bigint NOT NULL,
    c158 float NOT NULL,
    c159 nvarchar(32) NOT NULL,
    c160 bigint NOT NULL,
    c161 float NOT NULL,
    c162 nvarchar(32) NOT NULL,
    c163 bigint NOT NULL,
    c164 float NOT NULL,
    c165 nvarchar(32) NOT NULL,
    c166 bigint NOT NULL,
    c167 float NOT NULL,
    c168 nvarchar(32) NOT NULL,
    c169 bigint NOT NULL,
    c170 float NOT NULL,
    c171 nvarchar(32) NOT NULL,
    c172 bigint NOT NULL,
    c173 float NOT NULL,
    c174 nvarchar(32) NOT NULL,
    c175 bigint NOT NULL,
    c176 float NOT NULL,
    c177 nvarchar(32) NOT NULL,
    c178 bigint NOT NULL,
    c179 float NOT NULL,
    c180 nvarchar(32) NOT NULL,
    c181 bigint NOT NULL,
    c182 float NOT NULL,
    c183 nvarchar(32) NOT NULL,
    c184 bigint NOT NULL,
    c185 float NOT NULL,
    c186 nvarchar(32) NOT NULL,
    c187 bigint NOT NULL,
    c188 float NOT NULL,
    c189 nvarchar(32) NOT NULL,
    c190 bigint NOT NULL,
    c191 float NOT NULL,
    c192 nvarchar(32) NOT NULL,
    c193 bigint NOT NULL,
    c194 float NOT NULL,
    c195 nvarchar(32) NOT NULL,
    c196 bigint NOT NULL,
    c197 float NOT NULL,
    c198 nvarchar(32) NOT NULL,
    c199 bigint NOT NULL
);
GO

WITH n (i) AS (
    SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 25
)
INSERT INTO wide_metrics (c001, c002, c003, c004, c005, c006, c007, c008, c009, c010, c011, c012, c013, c014, c015, c016, c017, c018, c019, c020, c021, c022, c023, c024, c025, c026, c027, c028, c029, c030, c031, c032, c033, c034, c035, c036, c037, c038, c039, c040, c041, c042, c043, c044, c045, c046, c047, c048, c049, c050, c051, c052, c053, c054, c055, c056, c057, c058, c059, c060, c061, c062, c063, c064, c065, c066, c067, c068, c069, c070, c071, c072, c073, c074, c075, c076, c077, c078, c079, c080, c081, c082, c083, c084, c085, c086, c087, c088, c089, c090, c091, c092, c093, c094, c095, c096, c097, c098, c099, c100, c101, c102, c103, c104, c105, c106, c107, c108, c109, c110, c111, c112, c113, c114, c115, c116, c117, c118, c119, c120, c121, c122, c123, c124, c125, c126, c127, c128, c129, c130, c131, c132, c133, c134, c135, c136, c137, c138, c139, c140, c141, c142, c143, c144, c145, c146, c147, c148, c149, c150, c151, c152, c153, c154, c155, c156, c157, c158, c159, c160, c161, c162, c163, c164, c165, c166, c167, c168, c169, c170, c171, c172, c173, c174, c175, c176, c177, c178, c179, c180, c181, c182, c183, c184, c185, c186, c187, c188, c189, c190, c191, c192, c193, c194, c195, c196, c197, c198, c199)
SELECT
       i * 1 + 1,
       i * 0.5 + 2,
       CONCAT('c003-', i),
       i * 4 + 1,
       i * 0.5 + 5,
       CONCAT('c006-', i),
       i * 7 + 1,
       i * 0.5 + 8,
       CONCAT('c009-', i),
       i * 10 + 1,
       i * 0.5 + 11,
       CONCAT('c012-', i),
       i * 13 + 1,
       i * 0.5 + 14,
       CONCAT('c015-', i),
       i * 16 + 1,
       i * 0.5 + 17,
       CONCAT('c018-', i),
       i * 19 + 1,
       i * 0.5 + 20,
       CONCAT('c021-', i),
       i * 22 + 1,
       i * 0.5 + 23,
       CONCAT('c024-', i),
       i * 25 + 1,
       i * 0.5 + 26,
       CONCAT('c027-', i),
       i * 28 + 1,
       i * 0.5 + 29,
       CONCAT('c030-', i),
       i * 31 + 1,
       i * 0.5 + 32,
       CONCAT('c033-', i),
       i * 34 + 1,
       i * 0.5 + 35,
       CONCAT('c036-', i),
       i * 37 + 1,
       i * 0.5 + 38,
       CONCAT('c039-', i),
       i * 40 + 1,
       i * 0.5 + 41,
       CONCAT('c042-', i),
       i * 43 + 1,
       i * 0.5 + 44,
       CONCAT('c045-', i),
       i * 46 + 1,
       i * 0.5 + 47,
       CONCAT('c048-', i),
       i * 49 + 1,
       i * 0.5 + 50,
       CONCAT('c051-', i),
       i * 52 + 1,
       i * 0.5 + 53,
       CONCAT('c054-', i),
       i * 55 + 1,
       i * 0.5 + 56,
       CONCAT('c057-', i),
       i * 58 + 1,
       i * 0.5 + 59,
       CONCAT('c060-', i),
       i * 61 + 1,
       i * 0.5 + 62,
       CONCAT('c063-', i),
       i * 64 + 1,
       i * 0.5 + 65,
       CONCAT('c066-', i),
       i * 67 + 1,
       i * 0.5 + 68,
       CONCAT('c069-', i),
       i * 70 + 1,
       i * 0.5 + 71,
       CONCAT('c072-', i),
       i * 73 + 1,
       i * 0.5 + 74,
       CONCAT('c075-', i),
       i * 76 + 1,
       i * 0.5 + 77,
       CONCAT('c078-', i),
       i * 79 + 1,
       i * 0.5 + 80,
       CONCAT('c081-', i),
       i * 82 + 1,
       i * 0.5 + 83,
       CONCAT('c084-', i),
       i * 85 + 1,
       i * 0.5 + 86,
       CONCAT('c087-', i),
       i * 88 + 1,
       i * 0.5 + 89,
       CONCAT('c090-', i),
       i * 91 + 1,
       i * 0.5 + 92,
       CONCAT('c093-', i),
       i * 94 + 1,
       i * 0.5 + 95,
       CONCAT('c096-', i),
       i * 97 + 1,
       i * 0.5 + 98,
       CONCAT('c099-', i),
       i * 100 + 1,
       i * 0.5 + 101,
       CONCAT('c102-', i),
       i * 103 + 1,
       i * 0.5 + 104,
       CONCAT('c105-', i),
       i * 106 + 1,
       i * 0.5 + 107,
       CONCAT('c108-', i),
       i * 109 + 1,
       i * 0.5 + 110,
       CONCAT('c111-', i),
       i * 112 + 1,
       i * 0.5 + 113,
       CONCAT('c114-', i),
       i * 115 + 1,
       i * 0.5 + 116,
       CONCAT('c117-', i),
       i * 118 + 1,
       i * 0.5 + 119,
       CONCAT('c120-', i),
       i * 121 + 1,
       i * 0.5 + 122,
       CONCAT('c123-', i),
       i * 124 + 1,
       i * 0.5 + 125,
       CONCAT('c126-', i),
       i * 127 + 1,
       i * 0.5 + 128,
       CONCAT('c129-', i),
       i * 130 + 1,
       i * 0.5 + 131,
       CONCAT('c132-', i),
       i * 133 + 1,
       i * 0.5 + 134,
       CONCAT('c135-', i),
       i * 136 + 1,
       i * 0.5 + 137,
       CONCAT('c138-', i),
       i * 139 + 1,
       i * 0.5 + 140,
       CONCAT('c141-', i),
       i * 142 + 1,
       i * 0.5 + 143,
       CONCAT('c144-', i),
       i * 145 + 1,
       i * 0.5 + 146,
       CONCAT('c147-', i),
       i * 148 + 1,
       i * 0.5 + 149,
       CONCAT('c150-', i),
       i * 151 + 1,
       i * 0.5 + 152,
       CONCAT('c153-', i),
       i * 154 + 1,
       i * 0.5 + 155,
       CONCAT('c156-', i),
       i * 157 + 1,
       i * 0.5 + 158,
       CONCAT('c159-', i),
       i * 160 + 1,
       i * 0.5 + 161,
       CONCAT('c162-', i),
       i * 163 + 1,
       i * 0.5 + 164,
       CONCAT('c165-', i),
       i * 166 + 1,
       i * 0.5 + 167,
       CONCAT('c168-', i),
       i * 169 + 1,
       i * 0.5 + 170,
       CONCAT('c171-', i),
       i * 172 + 1,
       i * 0.5 + 173,
       CONCAT('c174-', i),
       i * 175 + 1,
       i * 0.5 + 176,
       CONCAT('c177-', i),
       i * 178 + 1,
       i * 0.5 + 179,
       CONCAT('c180-', i),
       i * 181 + 1,
       i * 0.5 + 182,
       CONCAT('c183-', i),
       i * 184 + 1,
       i * 0.5 + 185,
       CONCAT('c186-', i),
       i * 187 + 1,
       i * 0.5 + 188,
       CONCAT('c189-', i),
       i * 190 + 1,
       i * 0.5 + 191,
       CONCAT('c192-', i),
       i * 193 + 1,
       i * 0.5 + 194,
       CONCAT('c195-', i),
       i * 196 + 1,
       i * 0.5 + 197,
       CONCAT('c198-', i),
       i * 199 + 1
FROM n
OPTION (MAXRECURSION 100);
GO

-- MariaDB's client defaults to utf8mb3 here and turns the emoji below into NULL.
SET NAMES utf8mb4;

CREATE TABLE accounts (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    external_id CHAR(36) NOT NULL UNIQUE,
    name TEXT NOT NULL,
    email TEXT,
    plan ENUM('free', 'team', 'enterprise') NOT NULL,
    balance DECIMAL(14, 2) NOT NULL,
    active BOOLEAN NOT NULL,
    tags JSON NOT NULL,
    metadata JSON NOT NULL,
    created_at DATETIME(6) NOT NULL
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
        JSON_ARRAY('founder', 'priority'),
        JSON_OBJECT('timezone', 'Europe/London', 'features', JSON_OBJECT('audit', TRUE, 'seats', 250)),
        '2024-01-15 09:30:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000002',
        'Grace Hopper',
        'grace@example.test',
        'team',
        8192.00,
        TRUE,
        JSON_ARRAY('compiler', 'navy'),
        JSON_OBJECT('timezone', 'America/New_York', 'languages', JSON_ARRAY('COBOL', 'English')),
        '2024-02-29 12:00:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000003',
        'Edsger Dijkstra',
        NULL,
        'free',
        -0.01,
        FALSE,
        JSON_ARRAY(),
        JSON_OBJECT('note', 'Simplicity is prerequisite for reliability.'),
        '2024-03-10 18:45:12.123456'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000004',
        '李小龍',
        'bruce.lee@example.test',
        'team',
        42.42,
        TRUE,
        JSON_ARRAY('unicode', '香港'),
        JSON_OBJECT('display_name', '李小龍', 'emoji', '🐉', 'rtl', 'مرحبا'),
        '2024-04-01 00:00:00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000005',
        'Quotes ''n'' Backslashes \\\\',
        'escaping@example.test',
        'free',
        0.00,
        TRUE,
        JSON_ARRAY('quotes', 'backslash'),
        JSON_OBJECT('sql', 'SELECT ''not a delimiter;'';', 'path', 'C:\\\\demo\\\\file'),
        '2024-05-05 05:05:05'
    );

-- Five thousand rows, so scrolling, sorting and the row-count chip have
-- something to work against rather than a grid that fits on screen.
CREATE TABLE measurements (
    id BIGINT PRIMARY KEY,
    recorded_at DATETIME(6) NOT NULL,
    sensor VARCHAR(32) NOT NULL,
    temperature_c DOUBLE,
    pressure_kpa DECIMAL(8, 3),
    healthy BOOLEAN NOT NULL,
    samples JSON NOT NULL,
    payload JSON NOT NULL
);

-- The `WITH` goes after `INSERT INTO`, which is the only place MySQL
-- accepts one on an `INSERT ... SELECT`.
INSERT INTO measurements
WITH RECURSIVE counter (n) AS (
    SELECT 0
    UNION ALL
    SELECT n + 1 FROM counter WHERE n < 99
),
-- 100 x 50 rows from a 100-step recursion, because the default recursion
-- ceilings differ between MySQL and MariaDB and no portable SET raises them.
series (sample) AS (
    SELECT hi.n * 50 + lo.n + 1 FROM counter AS hi JOIN counter AS lo ON lo.n < 50
)
SELECT
    sample,
    TIMESTAMPADD(SECOND, sample * 15, '2025-01-01 00:00:00'),
    CONCAT('sensor-', LPAD((sample - 1) % 24 + 1, 2, '0')),
    -- A null every 97th row, so the grid's null rendering is reachable by
    -- scrolling rather than only by writing a query for it.
    CASE WHEN sample % 97 = 0 THEN NULL ELSE 18.0 + (sample % 150) / 10.0 END,
    98.000 + (sample % 700) / 1000.0,
    sample % 113 <> 0,
    JSON_ARRAY(sample % 10, sample % 20, sample % 30),
    JSON_OBJECT(
        'sequence', sample,
        'firmware', CONCAT('v', 1 + sample % 3, '.', sample % 10),
        'flags', JSON_ARRAY(sample % 2 = 0, sample % 5 = 0)
    )
FROM series;

-- Values far larger than a cell can show, and values a cell would misread:
-- embedded newlines, an embedded semicolon, a JSON null, and raw bytes.
CREATE TABLE documents (
    id INT PRIMARY KEY,
    title TEXT NOT NULL,
    body LONGTEXT,
    document JSON,
    binary_value LONGBLOB
);

INSERT INTO documents VALUES (
    1,
    'Multiline text',
    'first line\nsecond line\nthird line; with a semicolon',
    JSON_OBJECT('kind', 'short', 'nested', JSON_OBJECT('null_value', JSON_EXTRACT('{"v": null}', '$.v'))),
    UNHEX('00010203feff')
);

-- The `WITH` goes after `INSERT INTO`, which is the only place MySQL
-- accepts one on an `INSERT ... SELECT`.
INSERT INTO documents
WITH RECURSIVE series (value) AS (
    SELECT 1
    UNION ALL
    SELECT value + 1 FROM series WHERE value < 500
)
SELECT
    2,
    'Large values',
    REPEAT('DBDelve keeps the complete value while the grid clips visually. ', 2048),
    JSON_OBJECT(
        'kind', 'large',
        'values', JSON_ARRAYAGG(JSON_OBJECT('index', value, 'square', value * value))
    ),
    UNHEX(REPEAT('deadbeef', 4096))
FROM series;

-- Geometry columns (`point`, `boundary`) are PostGIS-only; MySQL geometry
-- support is out of scope.
CREATE TABLE locations (
    id INT PRIMARY KEY,
    name TEXT NOT NULL
);

INSERT INTO locations (id, name) VALUES
    (1, 'San Francisco'),
    (2, 'Null Island');

-- Keys worth following. `orders` is both ends of the problem at once: a
-- composite primary key, and a single-column foreign key into `accounts`, so
-- the simple case and the parent of the hard case are one table.
CREATE TABLE orders (
    account_id BIGINT NOT NULL,
    number INT NOT NULL,
    placed_at DATETIME(6) NOT NULL,
    total DECIMAL(14, 2) NOT NULL,
    PRIMARY KEY (account_id, number),
    FOREIGN KEY (account_id) REFERENCES accounts (id)
);

INSERT INTO orders VALUES
    (1, 1001, '2024-06-01 10:00:00', 4500.00),
    (1, 1002, '2024-06-08 11:30:00', 125.75),
    (2, 2001, '2024-06-12 16:15:00', 890.10);

-- The composite foreign key, which the catalog has to report as one key over
-- two columns rather than two keys of one column each.
CREATE TABLE order_items (
    id INT PRIMARY KEY,
    order_account_id BIGINT NOT NULL,
    order_number INT NOT NULL,
    description TEXT NOT NULL,
    quantity INT NOT NULL,
    FOREIGN KEY (order_account_id, order_number) REFERENCES orders (account_id, number)
);

INSERT INTO order_items VALUES
    (1, 1, 1001, 'Analytical engine time', 3),
    (2, 1, 1002, 'Punch card stock', 500),
    (3, 2, 2001, 'Compiler seat', 1);

-- MySQL has no schemas inside a database, so the cross-schema case is a second
-- database. InnoDB takes a foreign key across databases, and
-- `information_schema` names the referenced schema either way -- which is the
-- thing the catalog query has to get right.
--
-- The entrypoint grants the app user `dbdelve` rights on MYSQL_DATABASE only, so
-- the second database needs its own grant or the app cannot read what it seeds.
CREATE DATABASE dbdelve_archive;

GRANT ALL PRIVILEGES ON dbdelve_archive.* TO 'dbdelve'@'%';

CREATE TABLE dbdelve_archive.closed_accounts (
    id INT PRIMARY KEY,
    account_id BIGINT NOT NULL,
    closed_at DATETIME(6) NOT NULL,
    FOREIGN KEY (account_id) REFERENCES dbdelve_dev.accounts (id)
);

INSERT INTO dbdelve_archive.closed_accounts VALUES
    (1, 3, '2024-07-01 00:00:00'),
    (2, 5, '2024-07-04 12:00:00');

CREATE VIEW account_overview AS
SELECT
    plan,
    COUNT(*) AS accounts,
    SUM(balance) AS total_balance,
    COUNT(CASE WHEN active THEN 1 END) AS active_accounts
FROM accounts
GROUP BY plan;

-- Routines the explorer can actually show. Extension-owned routines are
-- filtered out of the catalog, so without this the routine surface has
-- nothing to display against this database.
DELIMITER $$

CREATE FUNCTION account_label(account_id BIGINT) RETURNS TEXT
DETERMINISTIC
READS SQL DATA
BEGIN
    DECLARE label TEXT;
    SELECT CONCAT(name, ' (', plan, ')') INTO label
    FROM accounts
    WHERE id = account_id;
    RETURN label;
END$$

CREATE PROCEDURE deactivate_account(account_id BIGINT)
MODIFIES SQL DATA
BEGIN
    UPDATE accounts SET active = FALSE WHERE id = account_id;
END$$

DELIMITER ;

-- A million rows and two hundred columns: the fixtures the grid's paging and
-- horizontal scrolling are measured against, not the hand-written rows above.

CREATE TABLE events (
    id bigint NOT NULL AUTO_INCREMENT PRIMARY KEY,
    account_id bigint NOT NULL,
    kind varchar(16) NOT NULL,
    amount decimal(12, 2) NOT NULL,
    occurred_at datetime NOT NULL
);

-- Six self-joined digits beat a recursive CTE here: MySQL materialises the
-- whole CTE into a temp table before the insert sees a single row.
CREATE TABLE seq10 (d tinyint NOT NULL);
INSERT INTO seq10 VALUES (0), (1), (2), (3), (4), (5), (6), (7), (8), (9);

INSERT INTO events (account_id, kind, amount, occurred_at)
SELECT (n % 6) + 1,
       ELT((n % 6) + 1, 'click', 'view', 'purchase', 'refund', 'signup', 'churn'),
       (n % 100000) / 100,
       TIMESTAMPADD(SECOND, n, '2024-01-01 00:00:00')
FROM (
    SELECT a.d + b.d * 10 + c.d * 100 + d.d * 1000 + e.d * 10000 + f.d * 100000 + 1 AS n
    FROM seq10 a, seq10 b, seq10 c, seq10 d, seq10 e, seq10 f
) AS s;

DROP TABLE seq10;

-- ascii keeps the 66 varchar columns under MariaDB's 8126-byte InnoDB row limit,
-- which counts utf8mb4 at four bytes a character; the values are all ASCII.
CREATE TABLE wide_metrics (
    id bigint NOT NULL AUTO_INCREMENT PRIMARY KEY,
    c001 bigint NOT NULL,
    c002 double NOT NULL,
    c003 varchar(32) NOT NULL,
    c004 bigint NOT NULL,
    c005 double NOT NULL,
    c006 varchar(32) NOT NULL,
    c007 bigint NOT NULL,
    c008 double NOT NULL,
    c009 varchar(32) NOT NULL,
    c010 bigint NOT NULL,
    c011 double NOT NULL,
    c012 varchar(32) NOT NULL,
    c013 bigint NOT NULL,
    c014 double NOT NULL,
    c015 varchar(32) NOT NULL,
    c016 bigint NOT NULL,
    c017 double NOT NULL,
    c018 varchar(32) NOT NULL,
    c019 bigint NOT NULL,
    c020 double NOT NULL,
    c021 varchar(32) NOT NULL,
    c022 bigint NOT NULL,
    c023 double NOT NULL,
    c024 varchar(32) NOT NULL,
    c025 bigint NOT NULL,
    c026 double NOT NULL,
    c027 varchar(32) NOT NULL,
    c028 bigint NOT NULL,
    c029 double NOT NULL,
    c030 varchar(32) NOT NULL,
    c031 bigint NOT NULL,
    c032 double NOT NULL,
    c033 varchar(32) NOT NULL,
    c034 bigint NOT NULL,
    c035 double NOT NULL,
    c036 varchar(32) NOT NULL,
    c037 bigint NOT NULL,
    c038 double NOT NULL,
    c039 varchar(32) NOT NULL,
    c040 bigint NOT NULL,
    c041 double NOT NULL,
    c042 varchar(32) NOT NULL,
    c043 bigint NOT NULL,
    c044 double NOT NULL,
    c045 varchar(32) NOT NULL,
    c046 bigint NOT NULL,
    c047 double NOT NULL,
    c048 varchar(32) NOT NULL,
    c049 bigint NOT NULL,
    c050 double NOT NULL,
    c051 varchar(32) NOT NULL,
    c052 bigint NOT NULL,
    c053 double NOT NULL,
    c054 varchar(32) NOT NULL,
    c055 bigint NOT NULL,
    c056 double NOT NULL,
    c057 varchar(32) NOT NULL,
    c058 bigint NOT NULL,
    c059 double NOT NULL,
    c060 varchar(32) NOT NULL,
    c061 bigint NOT NULL,
    c062 double NOT NULL,
    c063 varchar(32) NOT NULL,
    c064 bigint NOT NULL,
    c065 double NOT NULL,
    c066 varchar(32) NOT NULL,
    c067 bigint NOT NULL,
    c068 double NOT NULL,
    c069 varchar(32) NOT NULL,
    c070 bigint NOT NULL,
    c071 double NOT NULL,
    c072 varchar(32) NOT NULL,
    c073 bigint NOT NULL,
    c074 double NOT NULL,
    c075 varchar(32) NOT NULL,
    c076 bigint NOT NULL,
    c077 double NOT NULL,
    c078 varchar(32) NOT NULL,
    c079 bigint NOT NULL,
    c080 double NOT NULL,
    c081 varchar(32) NOT NULL,
    c082 bigint NOT NULL,
    c083 double NOT NULL,
    c084 varchar(32) NOT NULL,
    c085 bigint NOT NULL,
    c086 double NOT NULL,
    c087 varchar(32) NOT NULL,
    c088 bigint NOT NULL,
    c089 double NOT NULL,
    c090 varchar(32) NOT NULL,
    c091 bigint NOT NULL,
    c092 double NOT NULL,
    c093 varchar(32) NOT NULL,
    c094 bigint NOT NULL,
    c095 double NOT NULL,
    c096 varchar(32) NOT NULL,
    c097 bigint NOT NULL,
    c098 double NOT NULL,
    c099 varchar(32) NOT NULL,
    c100 bigint NOT NULL,
    c101 double NOT NULL,
    c102 varchar(32) NOT NULL,
    c103 bigint NOT NULL,
    c104 double NOT NULL,
    c105 varchar(32) NOT NULL,
    c106 bigint NOT NULL,
    c107 double NOT NULL,
    c108 varchar(32) NOT NULL,
    c109 bigint NOT NULL,
    c110 double NOT NULL,
    c111 varchar(32) NOT NULL,
    c112 bigint NOT NULL,
    c113 double NOT NULL,
    c114 varchar(32) NOT NULL,
    c115 bigint NOT NULL,
    c116 double NOT NULL,
    c117 varchar(32) NOT NULL,
    c118 bigint NOT NULL,
    c119 double NOT NULL,
    c120 varchar(32) NOT NULL,
    c121 bigint NOT NULL,
    c122 double NOT NULL,
    c123 varchar(32) NOT NULL,
    c124 bigint NOT NULL,
    c125 double NOT NULL,
    c126 varchar(32) NOT NULL,
    c127 bigint NOT NULL,
    c128 double NOT NULL,
    c129 varchar(32) NOT NULL,
    c130 bigint NOT NULL,
    c131 double NOT NULL,
    c132 varchar(32) NOT NULL,
    c133 bigint NOT NULL,
    c134 double NOT NULL,
    c135 varchar(32) NOT NULL,
    c136 bigint NOT NULL,
    c137 double NOT NULL,
    c138 varchar(32) NOT NULL,
    c139 bigint NOT NULL,
    c140 double NOT NULL,
    c141 varchar(32) NOT NULL,
    c142 bigint NOT NULL,
    c143 double NOT NULL,
    c144 varchar(32) NOT NULL,
    c145 bigint NOT NULL,
    c146 double NOT NULL,
    c147 varchar(32) NOT NULL,
    c148 bigint NOT NULL,
    c149 double NOT NULL,
    c150 varchar(32) NOT NULL,
    c151 bigint NOT NULL,
    c152 double NOT NULL,
    c153 varchar(32) NOT NULL,
    c154 bigint NOT NULL,
    c155 double NOT NULL,
    c156 varchar(32) NOT NULL,
    c157 bigint NOT NULL,
    c158 double NOT NULL,
    c159 varchar(32) NOT NULL,
    c160 bigint NOT NULL,
    c161 double NOT NULL,
    c162 varchar(32) NOT NULL,
    c163 bigint NOT NULL,
    c164 double NOT NULL,
    c165 varchar(32) NOT NULL,
    c166 bigint NOT NULL,
    c167 double NOT NULL,
    c168 varchar(32) NOT NULL,
    c169 bigint NOT NULL,
    c170 double NOT NULL,
    c171 varchar(32) NOT NULL,
    c172 bigint NOT NULL,
    c173 double NOT NULL,
    c174 varchar(32) NOT NULL,
    c175 bigint NOT NULL,
    c176 double NOT NULL,
    c177 varchar(32) NOT NULL,
    c178 bigint NOT NULL,
    c179 double NOT NULL,
    c180 varchar(32) NOT NULL,
    c181 bigint NOT NULL,
    c182 double NOT NULL,
    c183 varchar(32) NOT NULL,
    c184 bigint NOT NULL,
    c185 double NOT NULL,
    c186 varchar(32) NOT NULL,
    c187 bigint NOT NULL,
    c188 double NOT NULL,
    c189 varchar(32) NOT NULL,
    c190 bigint NOT NULL,
    c191 double NOT NULL,
    c192 varchar(32) NOT NULL,
    c193 bigint NOT NULL,
    c194 double NOT NULL,
    c195 varchar(32) NOT NULL,
    c196 bigint NOT NULL,
    c197 double NOT NULL,
    c198 varchar(32) NOT NULL,
    c199 bigint NOT NULL
) DEFAULT CHARACTER SET ascii;

INSERT INTO wide_metrics (c001, c002, c003, c004, c005, c006, c007, c008, c009, c010, c011, c012, c013, c014, c015, c016, c017, c018, c019, c020, c021, c022, c023, c024, c025, c026, c027, c028, c029, c030, c031, c032, c033, c034, c035, c036, c037, c038, c039, c040, c041, c042, c043, c044, c045, c046, c047, c048, c049, c050, c051, c052, c053, c054, c055, c056, c057, c058, c059, c060, c061, c062, c063, c064, c065, c066, c067, c068, c069, c070, c071, c072, c073, c074, c075, c076, c077, c078, c079, c080, c081, c082, c083, c084, c085, c086, c087, c088, c089, c090, c091, c092, c093, c094, c095, c096, c097, c098, c099, c100, c101, c102, c103, c104, c105, c106, c107, c108, c109, c110, c111, c112, c113, c114, c115, c116, c117, c118, c119, c120, c121, c122, c123, c124, c125, c126, c127, c128, c129, c130, c131, c132, c133, c134, c135, c136, c137, c138, c139, c140, c141, c142, c143, c144, c145, c146, c147, c148, c149, c150, c151, c152, c153, c154, c155, c156, c157, c158, c159, c160, c161, c162, c163, c164, c165, c166, c167, c168, c169, c170, c171, c172, c173, c174, c175, c176, c177, c178, c179, c180, c181, c182, c183, c184, c185, c186, c187, c188, c189, c190, c191, c192, c193, c194, c195, c196, c197, c198, c199)
WITH RECURSIVE n(i) AS (
    SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 25
)
SELECT i * 1 + 1,
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
FROM n;

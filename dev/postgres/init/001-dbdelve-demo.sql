CREATE EXTENSION IF NOT EXISTS postgis;

CREATE TYPE account_plan AS ENUM ('free', 'team', 'enterprise');

CREATE TABLE accounts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    external_id uuid NOT NULL UNIQUE,
    name text NOT NULL,
    email text,
    plan account_plan NOT NULL,
    balance numeric(14, 2) NOT NULL,
    active boolean NOT NULL,
    tags text[] NOT NULL,
    metadata jsonb NOT NULL,
    created_at timestamptz NOT NULL
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
        true,
        ARRAY['founder', 'priority'],
        '{"timezone":"Europe/London","features":{"audit":true,"seats":250}}',
        '2024-01-15 09:30:00+00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000002',
        'Grace Hopper',
        'grace@example.test',
        'team',
        8192.00,
        true,
        ARRAY['compiler', 'navy'],
        '{"timezone":"America/New_York","languages":["COBOL","English"]}',
        '2024-02-29 12:00:00+00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000003',
        'Edsger Dijkstra',
        NULL,
        'free',
        -0.01,
        false,
        ARRAY[]::text[],
        '{"note":"Simplicity is prerequisite for reliability."}',
        '2024-03-10 18:45:12.123456+00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000004',
        '李小龍',
        'bruce.lee@example.test',
        'team',
        42.42,
        true,
        ARRAY['unicode', '香港'],
        '{"display_name":"李小龍","emoji":"🐉","rtl":"مرحبا"}',
        '2024-04-01 00:00:00+00'
    ),
    (
        '018f1f6e-7c2a-7000-8000-000000000005',
        'Quotes ''n'' Backslashes \\',
        'escaping@example.test',
        'free',
        0.00,
        true,
        ARRAY['quotes', 'backslash'],
        '{"sql":"SELECT ''not a delimiter;'';","path":"C:\\\\demo\\\\file"}',
        '2024-05-05 05:05:05+00'
    );

CREATE TABLE measurements (
    id bigint PRIMARY KEY,
    recorded_at timestamptz NOT NULL,
    sensor text NOT NULL,
    temperature_c double precision,
    pressure_kpa numeric(8, 3),
    healthy boolean NOT NULL,
    samples integer[] NOT NULL,
    payload jsonb NOT NULL
);

INSERT INTO measurements
SELECT
    sample,
    '2025-01-01 00:00:00+00'::timestamptz + sample * interval '15 seconds',
    'sensor-' || lpad(((sample - 1) % 24 + 1)::text, 2, '0'),
    CASE WHEN sample % 97 = 0 THEN NULL ELSE 18.0 + (sample % 150) / 10.0 END,
    98.000 + (sample % 700) / 1000.0,
    sample % 113 <> 0,
    ARRAY[sample % 10, sample % 20, sample % 30],
    jsonb_build_object(
        'sequence', sample,
        'firmware', 'v' || (1 + sample % 3) || '.' || sample % 10,
        'flags', jsonb_build_array(sample % 2 = 0, sample % 5 = 0)
    )
FROM generate_series(1, 5000) AS sample;

CREATE TABLE documents (
    id integer PRIMARY KEY,
    title text NOT NULL,
    body text,
    document jsonb,
    binary_value bytea
);

INSERT INTO documents VALUES
    (
        1,
        'Multiline text',
        E'first line\nsecond line\nthird line; with a semicolon',
        '{"kind":"short","nested":{"null_value":null}}',
        decode('00010203feff', 'hex')
    ),
    (
        2,
        'Large values',
        repeat('DBDelve keeps the complete value while the grid clips visually. ', 2048),
        jsonb_build_object(
            'kind', 'large',
            'values', (
                SELECT jsonb_agg(jsonb_build_object('index', value, 'square', value * value))
                FROM generate_series(1, 500) AS value
            )
        ),
        decode(repeat('deadbeef', 4096), 'hex')
    );

CREATE TABLE locations (
    id integer PRIMARY KEY,
    name text NOT NULL,
    point geometry(Point, 4326),
    boundary geometry(Polygon, 4326)
);

INSERT INTO locations VALUES
    (
        1,
        'San Francisco',
        ST_SetSRID(ST_MakePoint(-122.4194, 37.7749), 4326),
        ST_GeomFromText(
            'POLYGON((-122.52 37.70,-122.35 37.70,-122.35 37.83,-122.52 37.83,-122.52 37.70))',
            4326
        )
    ),
    (
        2,
        'Null Island',
        ST_SetSRID(ST_MakePoint(0, 0), 4326),
        NULL
    );

-- Keys worth following. `orders` is both ends of the problem at once: a
-- composite primary key, and a single-column foreign key into `accounts`, so
-- the simple case and the parent of the hard case are one table.
CREATE TABLE orders (
    account_id bigint NOT NULL REFERENCES accounts (id),
    number integer NOT NULL,
    placed_at timestamptz NOT NULL,
    total numeric(14, 2) NOT NULL,
    PRIMARY KEY (account_id, number)
);

INSERT INTO orders VALUES
    (1, 1001, '2024-06-01 10:00:00+00', 4500.00),
    (1, 1002, '2024-06-08 11:30:00+00', 125.75),
    (2, 2001, '2024-06-12 16:15:00+00', 890.10);

-- The composite foreign key, which the catalog has to report as one key over
-- two columns rather than two keys of one column each.
CREATE TABLE order_items (
    id integer PRIMARY KEY,
    order_account_id bigint NOT NULL,
    order_number integer NOT NULL,
    description text NOT NULL,
    quantity integer NOT NULL,
    FOREIGN KEY (order_account_id, order_number) REFERENCES orders (account_id, number)
);

INSERT INTO order_items VALUES
    (1, 1, 1001, 'Analytical engine time', 3),
    (2, 1, 1002, 'Punch card stock', 500),
    (3, 2, 2001, 'Compiler seat', 1);

-- A reference that crosses a schema boundary, so the catalog is forced to
-- report the schema of the referenced table and not just its name.
CREATE SCHEMA archive;

CREATE TABLE archive.closed_accounts (
    id integer PRIMARY KEY,
    account_id bigint NOT NULL REFERENCES public.accounts (id),
    closed_at timestamptz NOT NULL
);

INSERT INTO archive.closed_accounts VALUES
    (1, 3, '2024-07-01 00:00:00+00'),
    (2, 5, '2024-07-04 12:00:00+00');

CREATE VIEW account_overview AS
SELECT
    plan,
    count(*) AS accounts,
    sum(balance) AS total_balance,
    count(*) FILTER (WHERE active) AS active_accounts
FROM accounts
GROUP BY plan;

-- Routines the explorer can actually show. Extension-owned routines are
-- filtered out of the catalog, so without these the routine surface has
-- nothing to display against this database.
CREATE FUNCTION account_label(account_id bigint) RETURNS text
LANGUAGE sql STABLE AS $$
    SELECT name || ' (' || plan::text || ')'
    FROM accounts
    WHERE id = account_id;
$$;

CREATE PROCEDURE deactivate_account(account_id bigint)
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE accounts SET active = false WHERE id = account_id;
END;
$$;

-- The engine conformance suite's fixture (src/db/conformance.rs): one table,
-- the same shape in every seed. `whole` is above 2^53 on purpose. Ids 900 and
-- up belong to the suite's write scenarios.

CREATE TABLE conformance (
    id integer PRIMARY KEY,
    label text NOT NULL,
    whole bigint,
    fraction numeric(10, 2),
    ratio double precision,
    flag boolean,
    day date,
    moment timestamp,
    bytes bytea
);

INSERT INTO conformance (id, label, whole, fraction, ratio, flag, day, moment, bytes) VALUES
    (1, 'alpha', 9007199254740993, 12.50, 0.5, true, '2024-02-29', '2024-02-29 12:34:56', '\x00ff'::bytea),
    (2, 'Zoë 李 🐉 ''quoted''', -42, -0.01, -1.25, false, '1999-12-31', '1999-12-31 23:59:59', '\xdeadbeef'::bytea),
    (3, 'nulls', NULL, NULL, NULL, NULL, NULL, NULL, NULL);

-- A million rows and two hundred columns: the fixtures the grid's paging and
-- horizontal scrolling are measured against, not the hand-written rows above.

CREATE TABLE events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id bigint NOT NULL,
    kind text NOT NULL,
    amount numeric(12, 2) NOT NULL,
    occurred_at timestamptz NOT NULL
);

INSERT INTO events (account_id, kind, amount, occurred_at)
SELECT (n % 6) + 1,
       (ARRAY['click', 'view', 'purchase', 'refund', 'signup', 'churn'])[(n % 6) + 1],
       round((n % 100000)::numeric / 100, 2),
       timestamptz '2024-01-01 00:00:00+00' + (n * interval '1 second')
FROM generate_series(1, 1000000) AS g(n);

CREATE TABLE wide_metrics (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    c001 bigint NOT NULL,
    c002 double precision NOT NULL,
    c003 text NOT NULL,
    c004 bigint NOT NULL,
    c005 double precision NOT NULL,
    c006 text NOT NULL,
    c007 bigint NOT NULL,
    c008 double precision NOT NULL,
    c009 text NOT NULL,
    c010 bigint NOT NULL,
    c011 double precision NOT NULL,
    c012 text NOT NULL,
    c013 bigint NOT NULL,
    c014 double precision NOT NULL,
    c015 text NOT NULL,
    c016 bigint NOT NULL,
    c017 double precision NOT NULL,
    c018 text NOT NULL,
    c019 bigint NOT NULL,
    c020 double precision NOT NULL,
    c021 text NOT NULL,
    c022 bigint NOT NULL,
    c023 double precision NOT NULL,
    c024 text NOT NULL,
    c025 bigint NOT NULL,
    c026 double precision NOT NULL,
    c027 text NOT NULL,
    c028 bigint NOT NULL,
    c029 double precision NOT NULL,
    c030 text NOT NULL,
    c031 bigint NOT NULL,
    c032 double precision NOT NULL,
    c033 text NOT NULL,
    c034 bigint NOT NULL,
    c035 double precision NOT NULL,
    c036 text NOT NULL,
    c037 bigint NOT NULL,
    c038 double precision NOT NULL,
    c039 text NOT NULL,
    c040 bigint NOT NULL,
    c041 double precision NOT NULL,
    c042 text NOT NULL,
    c043 bigint NOT NULL,
    c044 double precision NOT NULL,
    c045 text NOT NULL,
    c046 bigint NOT NULL,
    c047 double precision NOT NULL,
    c048 text NOT NULL,
    c049 bigint NOT NULL,
    c050 double precision NOT NULL,
    c051 text NOT NULL,
    c052 bigint NOT NULL,
    c053 double precision NOT NULL,
    c054 text NOT NULL,
    c055 bigint NOT NULL,
    c056 double precision NOT NULL,
    c057 text NOT NULL,
    c058 bigint NOT NULL,
    c059 double precision NOT NULL,
    c060 text NOT NULL,
    c061 bigint NOT NULL,
    c062 double precision NOT NULL,
    c063 text NOT NULL,
    c064 bigint NOT NULL,
    c065 double precision NOT NULL,
    c066 text NOT NULL,
    c067 bigint NOT NULL,
    c068 double precision NOT NULL,
    c069 text NOT NULL,
    c070 bigint NOT NULL,
    c071 double precision NOT NULL,
    c072 text NOT NULL,
    c073 bigint NOT NULL,
    c074 double precision NOT NULL,
    c075 text NOT NULL,
    c076 bigint NOT NULL,
    c077 double precision NOT NULL,
    c078 text NOT NULL,
    c079 bigint NOT NULL,
    c080 double precision NOT NULL,
    c081 text NOT NULL,
    c082 bigint NOT NULL,
    c083 double precision NOT NULL,
    c084 text NOT NULL,
    c085 bigint NOT NULL,
    c086 double precision NOT NULL,
    c087 text NOT NULL,
    c088 bigint NOT NULL,
    c089 double precision NOT NULL,
    c090 text NOT NULL,
    c091 bigint NOT NULL,
    c092 double precision NOT NULL,
    c093 text NOT NULL,
    c094 bigint NOT NULL,
    c095 double precision NOT NULL,
    c096 text NOT NULL,
    c097 bigint NOT NULL,
    c098 double precision NOT NULL,
    c099 text NOT NULL,
    c100 bigint NOT NULL,
    c101 double precision NOT NULL,
    c102 text NOT NULL,
    c103 bigint NOT NULL,
    c104 double precision NOT NULL,
    c105 text NOT NULL,
    c106 bigint NOT NULL,
    c107 double precision NOT NULL,
    c108 text NOT NULL,
    c109 bigint NOT NULL,
    c110 double precision NOT NULL,
    c111 text NOT NULL,
    c112 bigint NOT NULL,
    c113 double precision NOT NULL,
    c114 text NOT NULL,
    c115 bigint NOT NULL,
    c116 double precision NOT NULL,
    c117 text NOT NULL,
    c118 bigint NOT NULL,
    c119 double precision NOT NULL,
    c120 text NOT NULL,
    c121 bigint NOT NULL,
    c122 double precision NOT NULL,
    c123 text NOT NULL,
    c124 bigint NOT NULL,
    c125 double precision NOT NULL,
    c126 text NOT NULL,
    c127 bigint NOT NULL,
    c128 double precision NOT NULL,
    c129 text NOT NULL,
    c130 bigint NOT NULL,
    c131 double precision NOT NULL,
    c132 text NOT NULL,
    c133 bigint NOT NULL,
    c134 double precision NOT NULL,
    c135 text NOT NULL,
    c136 bigint NOT NULL,
    c137 double precision NOT NULL,
    c138 text NOT NULL,
    c139 bigint NOT NULL,
    c140 double precision NOT NULL,
    c141 text NOT NULL,
    c142 bigint NOT NULL,
    c143 double precision NOT NULL,
    c144 text NOT NULL,
    c145 bigint NOT NULL,
    c146 double precision NOT NULL,
    c147 text NOT NULL,
    c148 bigint NOT NULL,
    c149 double precision NOT NULL,
    c150 text NOT NULL,
    c151 bigint NOT NULL,
    c152 double precision NOT NULL,
    c153 text NOT NULL,
    c154 bigint NOT NULL,
    c155 double precision NOT NULL,
    c156 text NOT NULL,
    c157 bigint NOT NULL,
    c158 double precision NOT NULL,
    c159 text NOT NULL,
    c160 bigint NOT NULL,
    c161 double precision NOT NULL,
    c162 text NOT NULL,
    c163 bigint NOT NULL,
    c164 double precision NOT NULL,
    c165 text NOT NULL,
    c166 bigint NOT NULL,
    c167 double precision NOT NULL,
    c168 text NOT NULL,
    c169 bigint NOT NULL,
    c170 double precision NOT NULL,
    c171 text NOT NULL,
    c172 bigint NOT NULL,
    c173 double precision NOT NULL,
    c174 text NOT NULL,
    c175 bigint NOT NULL,
    c176 double precision NOT NULL,
    c177 text NOT NULL,
    c178 bigint NOT NULL,
    c179 double precision NOT NULL,
    c180 text NOT NULL,
    c181 bigint NOT NULL,
    c182 double precision NOT NULL,
    c183 text NOT NULL,
    c184 bigint NOT NULL,
    c185 double precision NOT NULL,
    c186 text NOT NULL,
    c187 bigint NOT NULL,
    c188 double precision NOT NULL,
    c189 text NOT NULL,
    c190 bigint NOT NULL,
    c191 double precision NOT NULL,
    c192 text NOT NULL,
    c193 bigint NOT NULL,
    c194 double precision NOT NULL,
    c195 text NOT NULL,
    c196 bigint NOT NULL,
    c197 double precision NOT NULL,
    c198 text NOT NULL,
    c199 bigint NOT NULL
);

INSERT INTO wide_metrics (c001, c002, c003, c004, c005, c006, c007, c008, c009, c010, c011, c012, c013, c014, c015, c016, c017, c018, c019, c020, c021, c022, c023, c024, c025, c026, c027, c028, c029, c030, c031, c032, c033, c034, c035, c036, c037, c038, c039, c040, c041, c042, c043, c044, c045, c046, c047, c048, c049, c050, c051, c052, c053, c054, c055, c056, c057, c058, c059, c060, c061, c062, c063, c064, c065, c066, c067, c068, c069, c070, c071, c072, c073, c074, c075, c076, c077, c078, c079, c080, c081, c082, c083, c084, c085, c086, c087, c088, c089, c090, c091, c092, c093, c094, c095, c096, c097, c098, c099, c100, c101, c102, c103, c104, c105, c106, c107, c108, c109, c110, c111, c112, c113, c114, c115, c116, c117, c118, c119, c120, c121, c122, c123, c124, c125, c126, c127, c128, c129, c130, c131, c132, c133, c134, c135, c136, c137, c138, c139, c140, c141, c142, c143, c144, c145, c146, c147, c148, c149, c150, c151, c152, c153, c154, c155, c156, c157, c158, c159, c160, c161, c162, c163, c164, c165, c166, c167, c168, c169, c170, c171, c172, c173, c174, c175, c176, c177, c178, c179, c180, c181, c182, c183, c184, c185, c186, c187, c188, c189, c190, c191, c192, c193, c194, c195, c196, c197, c198, c199)
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
FROM generate_series(1, 25) AS g(i);

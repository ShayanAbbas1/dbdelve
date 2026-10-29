-- Run by the image's entrypoint as the `dbdelve` user, in the order the file
-- is written. `type_gallery` is last on purpose: the healthcheck counts its
-- rows, so the container is healthy only once the whole seed has applied.

CREATE DATABASE IF NOT EXISTS analytics;

USE dbdelve_dev;

-- The engine conformance suite's fixture (src/db/conformance.rs): the same
-- shape in every seed. `whole` is above 2^53 on purpose. Ids 900 and up belong
-- to the suite's write scenarios.
CREATE TABLE conformance (
    id Int32,
    label String,
    whole Nullable(Int64),
    fraction Nullable(Decimal(10, 2)),
    ratio Nullable(Float64),
    flag Nullable(Bool),
    day Nullable(Date),
    moment Nullable(DateTime('UTC')),
    bytes Nullable(String)
) ENGINE = MergeTree ORDER BY id;

INSERT INTO conformance (id, label, whole, fraction, ratio, flag, day, moment, bytes) VALUES
    (1, 'alpha', 9007199254740993, 12.50, 0.5, true, '2024-02-29', '2024-02-29 12:34:56', unhex('00FF')),
    (2, 'Zoë 李 🐉 \'quoted\'', -42, -0.01, -1.25, false, '1999-12-31', '1999-12-31 23:59:59', unhex('DEADBEEF')),
    (3, 'nulls', NULL, NULL, NULL, NULL, NULL, NULL, NULL);

CREATE TABLE accounts (
    id UInt64,
    external_id UUID,
    name String,
    email Nullable(String),
    plan Enum8('free' = 1, 'team' = 2, 'enterprise' = 3),
    balance Decimal(14, 2),
    active Bool,
    created_at DateTime64(6, 'UTC')
) ENGINE = MergeTree ORDER BY id;

INSERT INTO accounts
SELECT
    number + 1,
    generateUUIDv4(),
    concat('Account ', toString(number + 1)),
    if(number % 5 = 0, NULL, concat('user', toString(number + 1), '@example.com')),
    CAST(number % 3 + 1, 'Enum8(\'free\' = 1, \'team\' = 2, \'enterprise\' = 3)'),
    toDecimal64((number * 37) % 100000, 2) / 100,
    number % 4 != 0,
    toDateTime64('2024-01-01 00:00:00', 6, 'UTC') + toIntervalSecond(number * 3600)
FROM numbers(50);

-- A million rows in a partitioned MergeTree: the fixture paging is measured
-- against.
CREATE TABLE events (
    id UInt64,
    account_id UInt64,
    kind LowCardinality(String),
    amount Decimal(12, 2),
    occurred_at DateTime('UTC')
) ENGINE = MergeTree PARTITION BY toYYYYMM(occurred_at) ORDER BY (account_id, occurred_at);

CREATE TABLE events_per_kind (
    kind LowCardinality(String),
    events UInt64
) ENGINE = SummingMergeTree ORDER BY kind;

CREATE MATERIALIZED VIEW events_per_kind_mv TO events_per_kind AS
SELECT kind, count() AS events FROM events GROUP BY kind;

INSERT INTO events
SELECT
    number + 1,
    number % 50 + 1,
    ['click', 'view', 'purchase', 'refund', 'signup', 'churn'][number % 6 + 1],
    toDecimal64(number % 100000, 2) / 100,
    toDateTime('2024-01-01 00:00:00', 'UTC') + toIntervalSecond(number)
FROM numbers(1000000);

CREATE VIEW active_accounts AS
SELECT id, name, plan, balance FROM accounts WHERE active;

CREATE TABLE plans (
    plan String,
    seats UInt32
) ENGINE = MergeTree ORDER BY plan;

INSERT INTO plans VALUES ('free', 1), ('team', 10), ('enterprise', 1000);

CREATE DICTIONARY plan_seats (
    plan String,
    seats UInt32
) PRIMARY KEY plan
SOURCE(CLICKHOUSE(TABLE 'plans' DB 'dbdelve_dev' USER 'dbdelve' PASSWORD 'dbdelve'))
LAYOUT(COMPLEX_KEY_HASHED())
LIFETIME(0);

CREATE TABLE scratch (
    id UInt32,
    note String
) ENGINE = Memory;

INSERT INTO scratch VALUES (1, 'in memory');

CREATE TABLE analytics.page_views (
    id UInt64,
    path String,
    viewed_at DateTime('UTC')
) ENGINE = MergeTree ORDER BY id;

INSERT INTO analytics.page_views
SELECT number + 1, concat('/page/', toString(number % 7)), toDateTime('2024-03-01 00:00:00', 'UTC') + toIntervalMinute(number)
FROM numbers(100);

-- For editing: the block settings lightweight updates need, set on one table
-- and left off the next, and the engines and projections that change what an
-- edit may do.
CREATE TABLE edit_lw (
    id UInt32,
    label String,
    qty Int32
) ENGINE = MergeTree ORDER BY id
SETTINGS enable_block_number_column = 1, enable_block_offset_column = 1;

CREATE TABLE edit_heavy (
    id UInt32,
    label String,
    qty Int32
) ENGINE = MergeTree ORDER BY id;

CREATE TABLE edit_proj (
    id UInt32,
    label String,
    qty Int32,
    PROJECTION by_label (SELECT label, sum(qty) GROUP BY label)
) ENGINE = MergeTree ORDER BY id;

CREATE TABLE edit_dupes (
    id UInt32,
    label String
) ENGINE = ReplacingMergeTree ORDER BY id;

CREATE TABLE edit_sum (
    id UInt32,
    total Int64
) ENGINE = SummingMergeTree ORDER BY id;

INSERT INTO edit_lw VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30);
INSERT INTO edit_heavy VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30);
INSERT INTO edit_proj VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30);
-- Two identical rows, in separate parts so the merge that would collapse them
-- has not run.
INSERT INTO edit_dupes VALUES (1, 'same');
INSERT INTO edit_dupes VALUES (1, 'same');
INSERT INTO edit_sum VALUES (1, 5), (2, 7);

-- A user the server holds to reads, and one who may select and insert but not
-- alter.
CREATE USER IF NOT EXISTS dbdelve_reader IDENTIFIED BY 'dbdelve' SETTINGS readonly = 1;
GRANT SELECT ON dbdelve_dev.* TO dbdelve_reader;
GRANT SELECT ON analytics.* TO dbdelve_reader;
GRANT SELECT ON system.* TO dbdelve_reader;

CREATE USER IF NOT EXISTS dbdelve_noalter IDENTIFIED BY 'dbdelve';
GRANT SELECT, INSERT ON dbdelve_dev.* TO dbdelve_noalter;
GRANT SELECT ON system.* TO dbdelve_noalter;

-- Every type the grid maps, two rows, with tab, quote and backslash nested in
-- strings where the escaping differs by depth.
CREATE TABLE type_gallery (
    id UInt32,
    label String,
    u8 UInt8,
    u64 UInt64,
    u128 UInt128,
    u256 UInt256,
    i8 Int8,
    i64 Int64,
    i128 Int128,
    i256 Int256,
    f32 Float32,
    f64 Float64,
    bf16 BFloat16,
    dec_small Decimal(4, 1),
    dec_wide Decimal(38, 10),
    flag Bool,
    day Date,
    day32 Date32,
    moment DateTime('UTC'),
    moment_tz DateTime('Asia/Tokyo'),
    moment64 DateTime64(3, 'UTC'),
    uid UUID,
    ip4 IPv4,
    ip6 IPv6,
    mood Enum8('sad' = 1, 'ok' = 2, 'happy' = 3),
    mood16 Enum16('low' = 1, 'high' = 1000),
    text String,
    fixed FixedString(4),
    raw String,
    maybe Nullable(String),
    low LowCardinality(String),
    low_maybe LowCardinality(Nullable(String)),
    ints Array(Int32),
    strings Array(String),
    maybes Array(Nullable(String)),
    pair Tuple(String, Int32),
    named Tuple(a String, b Array(String)),
    scores Map(String, Int32),
    labels Map(String, String),
    grid Array(Array(Int32)),
    point Point,
    nested Nested(k String, v Int32),
    variant Variant(String, Int32, Array(String)),
    dyn Dynamic,
    doc JSON,
    simple SimpleAggregateFunction(max, Int32),
    state AggregateFunction(sum, UInt64)
) ENGINE = MergeTree ORDER BY id;

INSERT INTO type_gallery SELECT
    1, 'typical',
    255, 18446744073709551615, toUInt128('340282366920938463463374607431768211455'),
    toUInt256('115792089237316195423570985008687907853269984665640564039457584007913129639935'),
    -128, -9223372036854775808, toInt128('-170141183460469231731687303715884105728'),
    toInt256('-57896044618658097711785492504343953926634992332820282019728792003956564819968'),
    1.5, 0.1, 1.5,
    -123.4, toDecimal128('1234567890123456789012345678.1234567890', 10),
    true, '2024-06-15', '1900-01-01',
    '2024-06-15 12:34:56', '2024-06-15 12:34:56', '2024-06-15 12:34:56.789',
    '018f1f6e-7c2a-7000-8000-000000000001', '192.168.0.1', '2001:db8::1',
    'happy', 'high',
    'tab\there \'quote\' back\\slash', 'ab', unhex('00FF00'), NULL, 'low', NULL,
    [1, 2, 3], ['a\tb', 'it\'s', 'back\\slash'], ['x', NULL],
    ('t\tab', 7), ('n', ['p\tq', 'r\\s']),
    map('a', 1, 'b', 2), map('k\tey', 'v\'al', 'bs', 'x\\y'),
    [[1, 2], [3], []],
    (1.5, -2.5),
    ['k1', 'k2'], [10, 20],
    'tab\there'::String, 'plain'::Dynamic,
    '{"a":[1,2,{"b":null}],"s":"t\\tab"}',
    7, sumState(toUInt64(5))
UNION ALL SELECT
    2, 'edges',
    0, 0, 0, 0,
    127, 9223372036854775807, toInt128('170141183460469231731687303715884105727'),
    toInt256('57896044618658097711785492504343953926634992332820282019728792003956564819967'),
    -inf, nan, -0.5,
    99.9, toDecimal128('-0.0000000001', 10),
    false, '1970-01-01', '2299-12-31',
    '1970-01-01 00:00:00', '1970-01-01 09:00:00', '1970-01-01 00:00:00.001',
    '00000000-0000-0000-0000-000000000000', '0.0.0.0', '::',
    'sad', 'low',
    '', '', '', 'present', '', 'in\\ner',
    [], [], [],
    ('', 0), ('', []),
    map(), map(),
    [],
    (0, 0),
    [], [],
    42::Int32, 7::Dynamic,
    '{}',
    -1, sumState(toUInt64(0));

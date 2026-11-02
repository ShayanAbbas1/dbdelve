-- The engine conformance suite's fixture (src/db/conformance.rs): one table,
-- the same shape in every seed. `whole` is above 2^53 on purpose. Ids 900 and
-- up belong to the suite's write scenarios.

CREATE TABLE conformance (
    id INTEGER NOT NULL PRIMARY KEY,
    label VARCHAR(100) NOT NULL,
    whole BIGINT,
    fraction DECIMAL(10, 2),
    ratio DOUBLE,
    flag BOOLEAN,
    day DATE,
    moment TIMESTAMP(0),
    bytes VARBINARY(8)
);

INSERT INTO conformance (id, label, whole, fraction, ratio, flag, day, moment, bytes) VALUES
    (1, 'alpha', 9007199254740993, 12.50, 0.5, TRUE, '2024-02-29', '2024-02-29 12:34:56', BX'00FF'),
    (2, 'Zoë 李 🐉 ''quoted''', -42, -0.01, -1.25, FALSE, '1999-12-31', '1999-12-31 23:59:59', BX'DEADBEEF'),
    (3, 'nulls', NULL, NULL, NULL, NULL, NULL, NULL, NULL);

-- A table the suite's INSERT scenario writes to and cleans up after.
CREATE TABLE scratch (
    id INTEGER NOT NULL PRIMARY KEY,
    label VARCHAR(100)
);

CREATE TABLE accounts (
    id INTEGER NOT NULL PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    email VARCHAR(100),
    balance DECIMAL(14, 2) NOT NULL,
    created_at TIMESTAMP NOT NULL
);

CREATE TABLE orders (
    id INTEGER NOT NULL PRIMARY KEY,
    account_id INTEGER NOT NULL,
    total DECIMAL(10, 2) NOT NULL,
    placed_on DATE NOT NULL,
    CONSTRAINT orders_account_fk FOREIGN KEY (account_id) REFERENCES accounts (id)
);

INSERT INTO accounts (id, name, email, balance, created_at) VALUES
    (1, 'Ada Lovelace', 'ada@example.test', 125000.50, '2024-01-15 09:30:00'),
    (2, 'Grace Hopper', 'grace@example.test', 98000.00, '2024-02-01 14:00:00'),
    (3, 'Edsger Dijkstra', NULL, 0.00, '2024-03-10 08:15:00');

INSERT INTO orders (id, account_id, total, placed_on) VALUES
    (1, 1, 19.99, '2024-04-01'),
    (2, 1, 250.00, '2024-04-02'),
    (3, 2, 5.25, '2024-04-03');

CREATE VIEW rich_accounts AS
    SELECT id, name, balance FROM accounts WHERE balance > 100000;

-- A materialized query table: Db2's own kind of object, which the catalog has
-- to name as something other than a plain table.
CREATE TABLE order_totals AS (
    SELECT account_id, SUM(total) AS spent, COUNT(*) AS orders
    FROM orders GROUP BY account_id
) DATA INITIALLY DEFERRED REFRESH DEFERRED;

REFRESH TABLE order_totals;

-- Last: the health check reads it, so a half-seeded container is not healthy.
CREATE TABLE dbdelve_seeded (done INTEGER NOT NULL);
INSERT INTO dbdelve_seeded VALUES (1);

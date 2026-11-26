-- Runs once, as SYS in the PDB the image creates, after APP_USER (dbdelve) has
-- been made. Everything is created in dbdelve's own schema so a connection as
-- that user sees it through the ALL_* views.
ALTER SESSION SET CURRENT_SCHEMA = dbdelve;

CREATE TABLE accounts (
    id        NUMBER(10) GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      VARCHAR2(60) NOT NULL,
    email     VARCHAR2(120),
    balance   NUMBER(12, 2) DEFAULT 0 NOT NULL,
    created   TIMESTAMP DEFAULT SYSTIMESTAMP NOT NULL
);

CREATE UNIQUE INDEX accounts_email_ux ON accounts (email);

INSERT INTO accounts (name, email, balance) VALUES ('Ada Lovelace', 'ada@example.test', 120.50);
INSERT INTO accounts (name, email, balance) VALUES ('Grace Hopper', 'grace@example.test', 75);
INSERT INTO accounts (name, email, balance) VALUES ('Alan Turing', NULL, 0);

CREATE TABLE orders (
    id          NUMBER(10) GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id  NUMBER(10) NOT NULL REFERENCES accounts (id),
    placed_on   DATE NOT NULL,
    note        CLOB
);

INSERT INTO orders (account_id, placed_on, note) VALUES (1, DATE '2024-01-02', 'first');
INSERT INTO orders (account_id, placed_on, note) VALUES (1, DATE '2024-02-03', NULL);

CREATE VIEW rich_accounts AS SELECT id, name FROM accounts WHERE balance > 100;

-- One column of each type the grid has to render, last, so the healthcheck's
-- row count proves the seed finished.
CREATE TABLE type_gallery (
    id        NUMBER(10) PRIMARY KEY,
    c_char    CHAR(4),
    c_varchar VARCHAR2(20),
    c_nvarchar NVARCHAR2(20),
    c_number  NUMBER,
    c_float   BINARY_DOUBLE,
    c_date    DATE,
    c_ts      TIMESTAMP(3),
    c_tstz    TIMESTAMP(3) WITH TIME ZONE,
    c_ids     INTERVAL DAY(2) TO SECOND(0),
    c_iym     INTERVAL YEAR(2) TO MONTH,
    c_raw     RAW(8),
    c_clob    CLOB,
    c_blob    BLOB
);

INSERT INTO type_gallery VALUES (
    1, 'ab', 'text', 'ünï', 3.14159, 2.5,
    DATE '2024-05-06', TIMESTAMP '2024-05-06 07:08:09.123',
    TIMESTAMP '2024-05-06 07:08:09.123 +02:00',
    INTERVAL '3 04:05:06' DAY TO SECOND, INTERVAL '1-2' YEAR TO MONTH,
    HEXTORAW('DEADBEEF'), 'a long text', UTL_RAW.CAST_TO_RAW('blob'));
INSERT INTO type_gallery (id) VALUES (2);
COMMIT;

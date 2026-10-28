# Development databases

With no saved connection and no `PG*` environment configured, the app opens
on its welcome surface, where New connection takes one of the development
database URLs listed in `AGENTS.md`.

The ports can be moved with `DBDELVE_POSTGRES_PORT`, `DBDELVE_MYSQL_PORT`,
`DBDELVE_MARIADB_PORT`, `DBDELVE_MSSQL_PORT`, `DBDELVE_REDIS_PORT`, `DBDELVE_MONGO_PORT` and
`DBDELVE_CLICKHOUSE_PORT`. The SQL Server image is
`linux/amd64` only, so on Apple silicon it runs under emulation and takes a
while to come up.

The ClickHouse container (`clickhouse-server:26.8`, built from
`dev/clickhouse/` with its seed baked in, so no bind mount records a worktree
path) serves the HTTP interface on 58123 and has no TLS listener, so the
TLS modes are tested offline only. The seed makes `conformance`, `accounts`,
`events` (partitioned, about a million rows), a view, a materialized view and
its target, a dictionary, the `analytics` database, the `edit_*` tables
(lightweight, heavy, projection, duplicates, summing), the `dbdelve_reader`
(`readonly = 1`) and `dbdelve_noalter` users, and `type_gallery` last: its
healthcheck counts the two rows of `type_gallery`, so a half-seeded container
is not healthy. The live tests are `cargo test clickhouse -- --include-ignored`.

The ClickHouse HTTP mock releases a completed execution's session before its
complete response becomes observable, including a zero-length body's headers.
Releasing after socket shutdown races the client's next request and invents a
`SESSION_IS_LOCKED`; delayed or stalled executions still hold their session
until they finish or are stopped.

Snowflake has no container. Its unit tests need nothing, and neither do its
mock tests: `snowflake/mock.rs` is a loopback HTTP server replaying responses
recorded from a real account (`dev/snowflake/fixtures`), with scripted
sequences, error statuses and truncated or stalled bodies for the failure
paths. A test build alone accepts an `http://` host, which is how a
connection reaches it; everywhere else the API is HTTPS. Its live tests are
`#[ignore]`d and read `DBDELVE_SNOWFLAKE_ACCOUNT`, `_USER`, `_PRIVATE_KEY` (an
absolute path) and `_DATABASE`, plus `_WAREHOUSE`, `_ROLE` and `_HOST` when set,
run with `cargo test snowflake -- --ignored`. The catalog test creates and
drops a `DBDELVE_TEST` schema. A recording that no longer matches what the
server says is re-recorded from a live account, with handles and request ids
replaced by placeholders.

SQLite has no server to connect to. Build the file once, then give the form its
absolute path:

```sh
sqlite3 dev/dbdelve_dev.db < dev/sqlite/001-dbdelve-demo.sql
```

The seed uses `unhex()`, so it needs sqlite3 3.41 or later.

DuckDB has no server either. Build the file once, then give the form its
absolute path; the `duckdb` CLI comes from `brew install duckdb` or duckdb.org:

```sh
rm -f dev/dbdelve_dev.duckdb && duckdb dev/dbdelve_dev.duckdb < dev/duckdb/001-dbdelve-demo.sql
```

The seed expects a fresh file, hence the `rm`. It ends with `type_gallery`, a
table of DuckDB-only types (nested LIST/STRUCT/MAP, ENUM, HUGEINT, infinite
dates) for checking how a cell renders them.

`dev/load.sh` rebuilds Postgres `events` about a hundred columns wide for load testing;
it is opt-in and CI never runs it (see `CONTRIBUTING.md`).

**The MySQL container reports itself healthy when its init script failed.**
`mysqladmin ping` does not care whether the seed applied, so a half-seeded
database looks exactly like a good one. Check a row count, not the status;
`live_the_development_database_is_fully_seeded` is that check. SQL Server's own
healthcheck gates the same row count behind the seed marker file, since
`accounts` fills early in the script and would otherwise look done before it
is; `dev/mssql/entrypoint.sh` writes the marker only once the whole seed
succeeds, so `docker compose up -d --wait` waits for the whole seed rather
than the first table. A seed that failed part way leaves objects behind with no
marker, so a restart that finds none drops the database and login first and
reseeds from scratch instead of colliding with what it left behind.

## Live tests and CI

The `live_` tests are `#[ignore]`d and read `PGHOST`, `PGPORT`, `PGDATABASE`,
`PGUSER`, `PGPASSWORD`, `dbdelve_MYSQL_URL`, `dbdelve_MARIADB_URL`,
`dbdelve_MSSQL_URL`, `dbdelve_REDIS_URL`, `dbdelve_MONGO_URL`, `dbdelve_CLICKHOUSE_URL`, `dbdelve_SQLITE_PATH`, `dbdelve_TURSO_PATH` and `dbdelve_SSH_CONFIG` (the
lowercase prefix is what they read; the last points `live_ssh_*` at the
config `dev/ssh/setup.sh` generates, an `ssh -F` rather than a connection
string); the `tests` job in `ci.yml` has the values
for the dev databases. It has no Snowflake account, so it skips the Snowflake
live tests but for the one that needs none; the mock tests stand in for them. CI lints on Linux, macOS and Windows, runs the live tests
on Linux, and runs the unit tests on Windows too, since the storage tests are
what exercise the `APPDATA` paths. `compose.yaml` and `dev/**` are in the
workflow's own path filters, so a change to a dev container or a seed script
runs CI too, not only a change under `src/`.

# ClickHouse

DBDelve speaks to ClickHouse over its HTTP interface, the one ClickHouse Cloud
and managed hosts expose. The native protocol (port 9000) is not supported; a
URL pointing at it fails saying the host did not answer as an HTTP server. This
page is what you need to know before filling in the form, and what behaves
differently from the other SQL engines.

## Connecting

Pick **ClickHouse** in the form's Engine dropdown. Host, port, database,
username, password and TLS. A blank port is 8123, or 8443 under Require and the
Verify modes. A URL works too:

```text
clickhouse://user:password@db.example.com:8443/analytics?sslmode=verify-full
```

The development container:

```text
clickhouse://dbdelve:dbdelve@127.0.0.1:58123/dbdelve_dev?sslmode=disable
```

A ClickHouse Cloud service that is idle wakes on the first query, so the
connect waits up to 30 seconds. Verify CA is checked as strictly as Verify
Full. Databases are the schemas the explorer lists.

## Read-only

A Read-only profile sends every request with `readonly=2`, which the server
enforces and a `SET` cannot lift. It does not stop `BACKUP` or `RESTORE`, so
DBDelve classifies those, and anything it cannot parse, by the statement's
first word. A `SELECT` from `url()`, `s3()` and the other table functions is a
read; `INSERT INTO FUNCTION` needs Full.

## Editing rows

ClickHouse has no unique key, so a row is named by all of its values as read,
and DBDelve runs a `COUNT(*)` with the identical condition before every
update or delete and writes only when exactly one row matches. Identical rows
(a `ReplacingMergeTree` duplicate not yet merged, say) are refused, as is a
row changed since you read it. Another client can still write between the count
and the write.

- Only MergeTree-family tables are editable. A row holding an
  `AggregateFunction`, `Variant`, `Dynamic` or `JSON` value, or bytes that are
  not text, cannot be matched and is read-only, and the grid says why.
- Key columns, `MATERIALIZED` columns and `AggregateFunction` columns cannot be
  set, and a `Map` cannot be edited, only inserted.
- An update is a lightweight `UPDATE` when the server and the table support it,
  else `ALTER TABLE … UPDATE … SETTINGS mutations_sync = 2`, which rewrites the
  parts holding the row. If the server refuses a lightweight update for want
  of the block columns, the error says so and names that spelling.
- There are no transactions. A batch stops at its first refusal and the edits
  before it stay; Import Rows keeps each chunk as it lands.

## Cancel and the row limit

Cancel and a read stopped at the row limit both end in `KILL QUERY` on the
server. The statement timeout is sent as `max_execution_time`, and DBDelve also
stops waiting a little after it. Mutations keep running on the server after a
Cancel; `KILL MUTATION` stops one.

## Explain

Explain runs `EXPLAIN` on a copy of the statement, which plans it and never
runs it. There is no analyze form.

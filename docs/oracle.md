# Oracle

DBDelve speaks to Oracle Database over Oracle's own Rust driver: pure Rust, no
Oracle Client or Instant Client to install. This page is what you need to know
before filling in the form.

## Connecting

Pick **Oracle** in the form's Engine dropdown. Host, port (1521 when blank),
the **service name** in the Database field, username, password and TLS. A URL
works too:

```text
oracle://scott:tiger@db.example.com:1521/ORCLPDB1
```

A SID cannot be used: connect to the service the database registers. Blank
passwords and usernames with `@` work.

## TLS

`Disable` is plaintext. `Prefer` tries TLS and falls back to plaintext when TLS
could not be set up; a refused login is never retried. `Require`, `Verify CA`
and `Verify Full` all verify the server certificate, which is stricter than
`require` in other clients. For a wallet, give its directory in the certificate
field: it holds `ewallet.pem`. A TLS connection through an SSH or SSM tunnel is
refused, because the certificate would be checked against the tunnel's address.

## What it does and does not do yet

Oracle is read-only for now: browse the schemas, run queries, read a table's
structure and Copy DDL. Editing cells, inserting and importing are not offered.

- A `DML` you run is **not committed**: run `COMMIT` to keep it. DBDelve never
  commits for you.
- A statement is sent as typed. A PL/SQL block cut from its final `;` gets it
  back, since a block needs one and plain SQL must not have one.
- An empty string is NULL in Oracle, so an empty cell is shown as NULL.
- **Cancel** stops the running call. On a TLS connection it closes the session
  instead, which rolls back an open transaction; you are told when that
  happens.
- The catalog hides Oracle's own schemas, which needs Oracle 12.1 or later.
- Types the grid cannot render yet (JSON, VECTOR, objects) show as `<TYPE>`.

The development container:

```text
oracle://dbdelve:dbdelve@127.0.0.1:51521/FREEPDB1?sslmode=disable
```

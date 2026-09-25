# SQL Server

DBDelve speaks to SQL Server 2017 or later (and Azure SQL) over TDS, signing in
with a SQL login: a username and a password. This page is what you need to know
before filling in the form, and what behaves differently from the other
engines.

## Connecting

Pick **SQL Server** on the form's chip row. The fields are the same as
Postgres's and MySQL's: host, port (blank is 1433), database, username,
password and TLS. A URL works too:

```text
mssql://user:password@db.example.com:1433/database?sslmode=require
sqlserver://user:password@db.example.com/database
```

Percent-encode anything in the userinfo that a URL reserves, such as `@` in a
username. `sslmode` and `sslrootcert` are the only parameters accepted.

Windows (integrated) authentication, Azure AD / Entra sign-in and named
instances reached through SQL Browser are not supported. For a named instance,
give its port.

The development container:

```text
mssql://dbdelve:DBDelve_dev1@127.0.0.1:51433/dbdelve_dev
```

## TLS

The five modes mean what they mean on Postgres, with two differences the
driver forces:

- **Disable still encrypts the login**, then sends everything after it in
  plaintext. SQL Server's own lowest setting works that way, so the password
  does not cross the wire in the clear unless the server cannot encrypt at all,
  which Disable and Prefer then accept.
- **Verify CA checks the hostname too**, which makes it the same as Verify
  Full. The driver cannot switch the hostname check off on its own, and a mode
  that checks more than asked is not a weaker one.

A root certificate named on the form replaces the system's trust store and has
to be a single certificate in a `.pem`, `.crt` or `.der` file; a bundle holding
several is refused. A server with a self-signed certificate (the development
container has one) connects under Prefer or Require and is refused by the two
verifying modes, which say which check failed.

## Things that behave differently here

- **Cancel closes the connection.** The driver has no way to send SQL Server's
  cancel signal, so Cancel stops a statement by closing its socket, and the
  server stops the batch when its client goes. That ends the session: an open
  transaction is rolled back and temporary tables and `SET` options are lost.
  DBDelve reconnects straight away and the error says so. The statement timeout
  works the same way, since SQL Server has no server-side one.
- **Every error inside a transaction rolls it back.** The session runs with
  `SET XACT_ABORT ON`, which is what keeps a multi-row edit all-or-nothing:
  without it a failed row lets the rest of the batch commit. T-SQL has no
  nested transactions, so a failing edit applied inside a transaction you
  opened rolls that transaction back too. `SET XACT_ABORT OFF` in your own SQL
  wins for the rest of the session.
- **Identifiers DBDelve writes are double-quoted**, `"dbo"."accounts"`, not
  bracketed. The SQL checker DBDelve runs before anything it generates does not
  read brackets. Your own SQL can use either. If you turn `QUOTED_IDENTIFIER`
  off, double-quoted names become strings and sorting from a header click stops
  meaning anything.
- **Header-click sorting on a query tab needs SQL the checker can read**, so a
  statement with `[brackets]`, `TOP` or `OFFSET … FETCH` is not sorted from a
  header. Table tabs sort, filter and page normally; their paging is written as
  `ORDER BY … OFFSET n ROWS FETCH NEXT m ROWS ONLY`, visible in the tab.
- **`USE` is moved back.** A profile is bound to one database, and everything
  DBDelve writes names objects by schema alone. A run that switches database is
  reported as an error, and the session is switched back.
- **Editing asks the server where a column came from**, with one extra round
  trip after a query returns (`sys.dm_exec_describe_first_result_set`). A
  result from one table with its whole primary key is editable; a view, a join,
  a temporary table, a table in another database, or a run returning more than
  one result set is not.
- **No row count for writes.** The driver reports how many rows a query
  returned but not how many an `UPDATE` changed.
- **Explain is not offered.** SQL Server's plans come from `SET SHOWPLAN_XML`,
  a session switch rather than a prefix DBDelve could put on a copy of your
  statement.
- **No regex filter.** SQL Server has none before 2025.
- **`DECLARE`, `WAITFOR` and other statements the checker does not know** need
  Full, as on every engine.
- **Read-only has no server-side backstop.** SQL Server has no session setting
  that refuses writes, so Read-only refuses what DBDelve cannot parse rather
  than offering to run it once.
- **`GO` is not a statement.** It is a separator `sqlcmd` and SSMS understand;
  the server does not, and DBDelve does not split on it.
- **`money` is exact below about 900 billion.** The driver hands it over as a
  floating-point number.

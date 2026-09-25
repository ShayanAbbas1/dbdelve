# SQL Server

DBDelve speaks to SQL Server 2017 or later (and Azure SQL) over TDS, signing in
with a SQL login: a username and a password. This page is what you need to know
before filling in the form, and what behaves differently from the other
engines.

## Connecting

Pick **SQL Server** in the form's Engine dropdown. The fields are the same as
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

**Prefer and Disable only retry without encryption after a failed TLS
handshake.** A refused login or a connect that times out fails the same way
whether or not encryption is tried again, so neither retries it.

## Things that behave differently here

- **Cancel closes the connection.** The driver has no way to send SQL Server's
  cancel signal, so Cancel stops a statement by closing its socket, and the
  server stops the batch when its client goes. That ends the session: an open
  transaction is rolled back and temporary tables and `SET` options are lost.
  DBDelve reconnects straight away and the error says so. The statement timeout
  works the same way, since SQL Server has no server-side one, and it bounds
  every round trip a run needs, including the ones DBDelve makes on its own
  before and after your statement.
  Cancel never stops a catalog load, and a statement queued behind one, or
  waiting for DBDelve to reconnect, is cancelled before it is sent rather than
  being made to run first. A Cancel
  that arrives after the statement already finished is reported as such.
- **DBDelve's own SQL runs under its own session options, not yours.** A
  preview, a grid edit or a catalog or structure query sets `XACT_ABORT ON`,
  `QUOTED_IDENTIFIER ON`, `ANSI_NULLS ON`, `ANSI_WARNINGS ON`,
  `IMPLICIT_TRANSACTIONS OFF`, `ROWCOUNT 0` and `DATEFORMAT ymd` for that one
  statement only, so whatever you `SET` yourself is exactly as you left it
  afterwards. Your own SQL runs as you typed it, and the check DBDelve makes
  before it (for the types below) compiles it under your options too; the one
  thing DBDelve sets at login is `XACT_ABORT ON`, and `SET XACT_ABORT OFF` in
  your own SQL wins for the rest of the session.
- **DBDelve only issues its own rollback for a transaction its own statement
  opened**, never one you had running already. T-SQL has no nested
  transactions, though, so `XACT_ABORT` can still end a transaction you opened
  earlier when one of your own statements fails inside it; when that happens
  the error says so, rather than leaving you to notice it is gone.
- **`sql_variant` and CLR-typed columns** (`geography`, `geometry`,
  `hierarchyid`) **are read as text in DBDelve's own previews** —
  `CONVERT(nvarchar(max), …)` for a CLR type, `CAST(… AS nvarchar(4000))` for
  `sql_variant` — and are never an edit target. That rewrite is something
  DBDelve does only to its own generated preview SQL, never to yours. A query
  you write is refused before it runs if one of these types would be in its
  first result set, with a message telling you how to cast it; further into a
  multi-result-set query the driver still cannot decode it, fails partway
  through, and the connection is reset — the error says the transaction and
  any temp tables went with it.
- **Identifiers DBDelve writes are double-quoted**, `"dbo"."accounts"`, not
  bracketed. The SQL checker DBDelve runs before anything it generates does not
  read brackets. Your own SQL can use either. If you turn `QUOTED_IDENTIFIER`
  off, double-quoted names become strings and sorting from a header click stops
  meaning anything.
- **Header-click sorting on a query tab needs SQL the checker can read**, so a
  statement with `[brackets]`, `TOP` or `OFFSET … FETCH` is not sorted from a
  header. Table tabs sort, filter and page normally, written as
  `ORDER BY … OFFSET n ROWS FETCH NEXT m ROWS ONLY` and visible in the tab. An
  unsorted page is ordered by `(SELECT NULL), <key>` — the primary key, or the
  first unique key if the table has none, or `(SELECT NULL)` alone if it has
  neither — so pages don't repeat or skip rows; the first page waits for the
  table's structure to load.
- **A line holding only `GO` separates batches, and it is never sent.** The
  statement under your cursor runs inside its whole batch. A selection has to
  hold exactly one batch; `GO` lines around it are dropped rather than sent. A
  count on it, `GO 5`, is refused, because DBDelve runs a batch once. Format
  Query leaves `GO` lines and `[bracketed]` names as you wrote them.
  `CREATE`/`ALTER PROCEDURE`/`FUNCTION`/`TRIGGER` runs to the next `GO` or the
  end of the buffer, and a `BEGIN … END` or `BEGIN TRY … END CATCH` block runs
  whole, however many `;`s it holds; `[bracketed]` names are read as names
  wherever DBDelve looks for a statement's boundaries.
- **`USE` is moved back.** A profile is bound to one database, and everything
  DBDelve writes names objects by schema alone. A run that switches database is
  reported as an error and the session is switched back, even when the
  statement carrying the `USE` itself failed.
- **Editing asks the server where a column came from**, with one extra round
  trip after a query returns (`sys.dm_exec_describe_first_result_set`). A
  result from one table with its whole primary key is editable; a view, a join,
  a temporary table, a table in another database, or a run returning more than
  one result set is not. A computed, identity or rowversion column shows in a
  result but is never an edit target, though it can still locate its row if it
  is part of the key.
- **Writes report the last statement's row count**, the way Postgres does:
  DBDelve asks for it with a follow-up `SELECT @@ROWCOUNT`, since the driver
  keeps its own row-count tokens to itself. That, and the check before each
  statement, means `@@ROWCOUNT` and `@@ERROR` in your next run describe
  DBDelve's queries, not your last statement (known limitation); read them in
  the same batch as the statement they follow.
- **A literal is spelled by the column's type**, in an edit or in the filter
  bar alike. DBDelve writes `N'…'` for a Unicode or unknown-type column and
  for any value holding a non-ASCII character, plain `'…'` for an ASCII value
  going into a char, varchar, text, numeric or date column (a `datetime` or
  `smalldatetime` as `'2024-01-02T03:04:05.000'`, the form no `SET LANGUAGE`
  or `DATEFORMAT` reads differently), and a bare `0x…` for a binary or
  rowversion column — which is what lets a row keyed by a binary or
  varbinary column be edited and deleted (a zero-length `0x` key is still
  refused), and lets a filter bar over one match by bare hex too. A column of
  a binary alias type is known only by its alias name and gets `N'…'`, so it
  matches nothing by value (known limitation). `LIKE`'s
  bracket escaping (`[%]`, `[_]`, `[[]`) is unchanged; only the final quoting
  follows the column's type. The filter bar's type comes from the tab's
  loaded structure: filtering before it loads writes `N'…'` until you apply
  the bar again (a tab restored from a saved session reopens with the filter
  exactly as it last ran), and following a
  foreign key filters by the referencing column's type, since the referenced
  table's structure is not loaded to ask.
- **The Structure tab shows more of an index, key or type, permissions
  allowing.** An index's text carries its `INCLUDE` columns, its filter
  predicate and whether it is a columnstore index; a foreign key's carries its
  `ON DELETE`/`ON UPDATE` actions and whether it is disabled or not trusted; a
  computed column's definition the login cannot see reads `AS <hidden>` rather
  than nothing; an alias type the login cannot see falls back to showing its
  base type.
- **Explain is not offered.** SQL Server's plans come from `SET SHOWPLAN_XML`,
  a session switch rather than a prefix DBDelve could put on a copy of your
  statement.
- **No regex filter.** SQL Server has none before 2025.
- **`DECLARE`, `WAITFOR` and other statements the checker does not know** need
  Full, as on every engine.
- **Read-only has no server-side backstop.** SQL Server has no session setting
  that refuses writes, so Read-only refuses what DBDelve cannot parse rather
  than offering to run it once.
- **An error from inside a procedure, trigger or function points at nothing in
  your buffer.** The line SQL Server reports is the routine's own, not your
  statement's, so DBDelve does not place it in the editor.
- **`money` is exact below about 900 billion**; the driver hands it over as a
  floating-point number. A nullable `smallmoney` column shows as `money`, and
  an empty or all-NULL nullable `smalldatetime` column shows as `datetime`,
  because the driver doesn't expose which of the pair a nullable column is
  (known limitation).
- **A `float` or `real` value far from 1 prints in exponent form**, `1E+308`,
  the way SQL Server itself would and a form T-SQL reads back as a literal.

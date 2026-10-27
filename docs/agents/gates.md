# SQL DBDelve writes, and the gates it passes

## The row limit and what DBDelve writes

**Never rewrite SQL behind the user's back.** No silent `LIMIT` injection, no
column projection, no reformatting on execute, and nothing at all on a
statement the user did not ask DBDelve to change. A client that silently
alters statements cannot be trusted with the statements that matter, which
is why "silently" is the word that carries the rule. A `LIMIT` is written
only into DBDelve-generated preview queries. A user's own statement runs
as typed; the row limit bounds it by keeping only that many rows and
dropping the rest as they arrive, never by changing the statement. A
read is stopped once it passes the limit instead of being drained only when
it is the submission's one statement (`sql::stoppable`: rerunnable, both
parsers count exactly one statement, and no `set_config`, advisory or named
lock or `@x :=` the session keeps) and stopping it undoes nothing. Postgres
cancels it only outside any transaction the user opened, since a cancel
aborts it, and only if `pg_stat_activity` shows the session's process running
this statement's exact text, never waiting on the side session to find
out; MySQL and MariaDB `KILL QUERY` anywhere, since a killed statement is
rolled back alone; SQLite interrupts it; DuckDB fetches no further; MongoDB
drops the cursor; ClickHouse closes the
response and `KILL QUERY`s it by its `query_id`; SQL Server
sends `SET ROWCOUNT limit+1` as a batch of its own ahead of it and always
`SET ROWCOUNT 0` after, reconnecting if that reset fails, unless a
`SET ROWCOUNT` of the user's own is in force, which that reset would erase.
Anything else is drained. Either way the limit is visible in the UI, and a capped
result says how many rows the statement returned, or that it was stopped.

DBDelve _does_ write SQL when the user asks it to, and only then, always
where the user can read it:

- On a view the server sorts, a header click asking for a sort splices the
  `ORDER BY` into the statement in the buffer, where it can be read, edited
  and undone; the statement that runs is the statement on screen. A view
  sorted in memory reorders the rows it holds and writes nothing;
  switching it to the server is an ask, and splices its keys the same way.
- Grid edits on a query tab are appended to the buffer
  (`sql::appended_statement`) and run from there. On an object tab, which
  has no buffer, the statement is shown in a review dialog before it runs,
  as it is on a query tab of an engine without unique keys
  (`Engine::reviews_query_edits`), whose writes run only behind the guard.
- Import Rows shows every statement it will send in its dialog's SQL view
  before any runs.
- **Explain** puts the engine's `EXPLAIN` prefix on a copy of the
  statement, never into the buffer.
- **Format Query** rewrites the buffer on command only, through
  `sqlformat`'s token-level reformatter (on MongoDB, `mql::format`'s
  bracket-depth layout over the tree-sitter tokens, refused unless the
  result reads back as the same statements). Never an AST round-trip: that
  regenerates the statement and drops every comment the user wrote.

Limits on what DBDelve may write. It never writes `DROP` or `TRUNCATE`
into a buffer or onto the wire, whatever the user asked for. It writes
`DELETE` only as the explicit deletion of one named row: by primary key (on
an engine without unique keys, by all of the row's values as read, and only
when exactly one row matches), from a direct ask, with the statement shown before it runs
(`sql::delete_row`). The one exception to both is the explorer's
right-click menu, whose Copy DROP and Copy TRUNCATE (SQLite's spelling is
`DELETE FROM`, MongoDB's `drop()` and `deleteMany({})`, Redis's Copy
TRUNCATE `FLUSHDB`, with no Copy DROP) put the text on the
clipboard and nothing else: no buffer, no run, no gate, so pasting and
running it is the user's own statement. And it never writes into
a statement it cannot parse whole: `sql::with_order_by` refuses rather than
guessing at a clause boundary, because a corrupted statement is worse than
an unsorted grid.

## The gates

**Generated SQL passes a whitelist gate, and there is one per path.**
`sql::is_generated_write` is the single gate every statement the grid writes
passes first. It admits exactly four shapes:

- a batch of `UPDATE`s, optionally bracketed by a `BEGIN`/`COMMIT` the gate
  can see closed;
- one `INSERT`, naming the columns it fills, of one or more rows whose
  values are each exactly one single-quoted literal (`N'…'` too, never two
  adjacent), a bare `0x…` hex literal or `NULL` (no subquery, function call
  or `SELECT` source), with no comment anywhere in it (MySQL runs
  `/*! … */`), optionally ending in exactly `ON CONFLICT DO NOTHING` or
  `ON DUPLICATE KEY UPDATE c = c` with `c` bare or backticked (Import Rows'
  Skip);
- one `DELETE` whose `WHERE` is a conjunction of equality predicates over
  distinct, unqualified columns against single-quoted literals (or a bare
  `0x…` hex literal, SQL Server's spelling of bytes): no `OR`, no
  other operator, no subquery, no function call, no CTE beside it, no
  `RETURNING`, no `LIMIT`, and nothing else in the submission. ClickHouse,
  which has no unique key, is the one exception to the terms: a row is named
  by every value, and a term may also be `toString(col) = '…'` (the only
  function call admitted, over a bare column) or `col IS NULL`;
- a submission that is exactly one lone `BEGIN`, `BEGIN TRANSACTION`,
  `COMMIT` or `ROLLBACK`, compared as text. These are the brackets of an
  import's transaction, which spans submissions on a connection of the
  import's own, and are never admitted beside anything else.

Because it is a whitelist, `DROP` and `TRUNCATE` are refused structurally,
anywhere in the tree, CTEs included (`sql::forbidden`), and so is every
`delete` outside that one shape (`sql::deletes_anything`, checked on the
`INSERT` and `UPDATE` arms). On an engine where a backslash escapes
(`Engine::backslash_escapes`), a submission with any literal holding a quote
after an odd run of backslashes is refused, in every arm
(`sql::parse_write`): the grammar ends the string there and the engine
does not.

`sql::is_generated_select` guards the filter bar, the one place user text is
spliced into DBDelve's statement: exactly one query, nothing destructive
under it, no delete at all. It admits no write and is not a way around
`sql::is_generated_write`. Do not add a path that bypasses either.

MongoDB has its own pair, reached through the same two entry points
(`sql::is_generated_write_on` and `sql::is_generated_select` dispatch on the
engine): `mql::is_generated_write` admits `updateOne`s whose filter is
exactly `{_id: <literal>}` and whose update is exactly a `$set` of literals,
one `insertOne` of a literal document, one `insertMany` of a non-empty
array of literal documents, and one `deleteOne` by `_id`; and
`mql::browse::is_generated_read` admits only the preview's own
`find(filter).sort().skip().limit()` (no `limit()` under no row limit) and the
status bar's `countDocuments`,
each written again from its parts and compared with the text,
with a filter that is one document running no server-side JavaScript
(`$where`, `$function`, `$accumulator`). Both read the tree-sitter parse, not
the builder's word, as with the `DELETE`'s shape below.

Redis has its own pair too. `redis_cmd::edit::is_generated_write` admits
only what the grid's edits, new rows and deleted rows write
(`RENAMENX`, `EXPIRE`, `PERSIST`, `SET … XX KEEPTTL GET` and `SET … NX`, `UNLINK`,
`HSET`, `HSETNX`, `HDEL`, `LSET`/`LREM`, `RPUSH`, `SADD`, `SREM`, `ZADD`,
`ZREM`, a list item's write: `WATCH`, an `LPOS` that checks the item
still is the one read, and a `MULTI`/`EXEC` bracket, and a collection's
JSON edit: `WATCH`, an `ECHO` of the JSON read, a bounded read
of the key (`HSCAN`/`SSCAN`, `LRANGE 0 100`, `ZSCAN`), and a `MULTI`/`EXEC` of writes of the key's own type), each
written again from its parts and compared with the text.
`redis_cmd::browse::is_generated_read` admits only the keys grid's
`SCAN 0 [MATCH p] [TYPE t] COUNT 1000`, with `TYPE` before `MATCH` too,
and `DBSIZE`. A key opened as a grid
runs through `classify` and `gate` like any typed read. A user's own
command is never one of these: `redis_cmd::classify` reads it by name and
subcommand from the `READS`, `WRITES` and `DROPS` tables, and a name in none
is `Unreadable`. The server has no read-only hold (`holds_read_only` is
false), so the tables are the Read-only boundary and must stay closed to
anything unlisted. `redis_cmd::refusal` is a separate list: the commands
that would take the session out from under DBDelve (`SELECT`, the
subscribe family, `MONITOR`, `SYNC`, `HELLO`, `RESET`, `QUIT`,
`CLIENT REPLY`) are never sent, in any mode.

**The `DELETE`'s shape is verified from the parse tree, not trusted because
`sql::delete_row` produced it.** A gate that trusts its caller is a comment;
the check lives in the gate rather than the generator precisely so the two
can disagree. `sql::delete_matches_key` answers the half the gate cannot,
whether the `WHERE` names exactly the row's key as a set. It is a readout,
not a second gate: it admits nothing, and a caller runs both.

**Multi-row deletion is not admitted.** When it is wanted, the path is the
`BEGIN`/`COMMIT` bracketing multi-row edits already use: one `DELETE` per
row, each naming its own key, never one predicate covering several.

A cell is editable only when DBDelve can name its row by primary key; when
it cannot, the grid stays read-only and says why, and never guesses at a
predicate. An `INSERT` has no existing row to name, so a table without a
primary key can be inserted into and not edited. That asymmetry is
deliberate and belongs in anything that documents either feature.

The one exception is an engine without unique keys (ClickHouse): a row is
named by all of its values as read, and a write runs only when exactly one row
matches. Before each `UPDATE`, `DELETE` or `ALTER` write, an exactly-one-row
`COUNT(*)` with the identical predicate (`sql::row_guard`) must return one. An
update on a table lacking the block columns is spelled `ALTER TABLE <t> UPDATE
<assignments> WHERE <predicate> SETTINGS mutations_sync = 2`, exactly, with
the setting last. A row that cannot be matched exactly stays read-only and says
why.

## Connection modes, per engine

The core of the mode check is in `AGENTS.md`, "Connection modes". The detail:

- **An unreadable statement on an engine with no server-side hold.**
  `Engine::holds_read_only` is false on SQLite, DuckDB, Snowflake and SQL
  Server, so a statement `sqlparser` cannot parse carries Read-write there, not
  Read-only, and the gate asks for that. A statement it parses keeps its own
  verdict. ClickHouse holds Read-only (`readonly=2`) but not for every
  statement, so what it cannot parse is read by its first keyword
  (`UnreadableFloor::FirstKeyword`): reads stay Read-only and run once on
  confirmation, writes need Read-write, DDL, server control and `BACKUP` and
  `RESTORE` need Full, and anything it does not know fails closed to
  Read-write.
- **`IF` conditions.** T-SQL's `IF … ELSE` and the `IF … END IF` Postgres, MySQL
  and Snowflake parse to the same variant. `condition_verdict` reads a plain
  name, value, comparison or `IS …` test as a read, and a subquery (`EXISTS`,
  `IN`, a bare subquery) as whatever `sql::classify` would give that query; a
  function call or anything else it does not recognise still needs Full.
- **`CycleMode`** (no default chord, "Cycle Access Level") steps Read-only,
  Read-write, Full and back through `choose_mode`, so it takes the titlebar
  menu's path and nothing else.
- **`Connection::set_read_only`** holds a Read-only session to reads on Postgres
  and MySQL, and on ClickHouse flags every request to carry `readonly=2`; SQLite has no such setting, Snowflake no session to hold it in,
  and SQL Server no session-level switch.

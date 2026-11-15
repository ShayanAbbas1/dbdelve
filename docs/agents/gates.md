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
A plugin is sent `stop` and stops sending rows, then ends the statement only
where it can prove that safe; the ODBC bridge never can, so it drains on its
own side (`plugins.md`). Anything else is drained. Either way the limit is visible in the UI, and a capped
result says how many rows the statement returned, or that it was stopped.

**Export fetches past the limit only on an explicit ask.** Export writes the rows a grid holds when they are its whole
result. When they are not (stopped at the limit or by a cancel, capped by a
snapshot, or one page of a relation), a dialog asks first, and Export All
Rows runs the statement again: a query tab's verbatim with no limit, a
relation tab's regenerated with neither limit nor offset and put through
`sql::is_generated_select` and `sql::paged` as its page was. It is offered
only where running it again changes nothing and reads what this session
would (`exports::refetch_refusal`): a lone `sql::plain_read` the gate lets
through as the mode stands, with nothing held in turn (`Profile::in_turn`),
because it runs on a connection of its own (`query_alongside`, or `apart`
with the Read-only hold), so the profile's stays free and Cancel stops only
it. Anything unsure is refused with its reason, leaving the rows shown. Rows
go from the feed to a sibling `.partial` file renamed onto the chosen path
only once whole, so a cancel or a failure leaves whatever was there.

DBDelve _does_ write SQL when the user asks it to, and only then; outside Row
mode, always where the user can read it before it runs:

- On a view the server sorts, a header click asking for a sort splices the
  `ORDER BY` into the statement in the buffer, where it can be read, edited
  and undone; the statement that runs is the statement on screen. A view
  sorted in memory reorders the rows it holds and writes nothing;
  switching it to the server is an ask, and splices its keys the same way.
- Grid edits on a query tab are appended to the buffer
  (`sql::appended_statement`) and run from there only where the target
  neither guards nor verifies them (`Target::reviews_query_apply`). On an
  object tab, which has no buffer, the statement is shown in a review dialog
  before it runs, as it is on a query tab of an engine without unique keys
  (`Engine::reviews_query_edits`), whose writes run only behind the guard, and
  on one whose apply is verified (`Target::verifies_applies`: the rows are
  re-read and compared and each write counted, which a copy left in the buffer
  would run without). A verified query-tab apply is not kept in the history for
  the same reason, and no edit of a verifying target is sent without its
  verification: `execute_unchecked` refuses one that arrives with none (`dbdelve
  refused to send an edit it could not verify.`), so a path added later that
  forgets to build one cannot send the text alone.
  The exception is a relation tab's view in Row mode, which has no buffer and
  no dialog: the row's `UPDATE` is sent as the row is left or saved
  (`Workspace::save_row`), and that is the user's own act. It is gated by
  `sql::is_generated_write_on`, mode-checked in `execute_and_then` as it is
  sent, read back by `sql::row_read` under `sql::is_generated_select`, written
  to history, and the last one is shown in the status line. On a target that
  verifies applies (`Target::verifies_applies`) the send is one verified step:
  the row is re-read and compared with the row as it was fetched before its
  `UPDATE`, the `UPDATE` must count one row, and the read-back is taken inside
  the same transaction. A row that drifted, or an `UPDATE` that counted any
  other number, applies nothing and the row keeps its edits. A Classic or
  Draft view sends nothing unread.
  The New row form's `INSERT` and a lone Delete row's `DELETE` are reviewed as
  before and run through the same verified apply on such a target: the
  `INSERT` must count one row, and the `DELETE` re-reads its row first and
  must count one.
- A relation tab's Draft apply on a target that verifies it
  (`Target::verifies_applies`: SQLite, DuckDB, Postgres, MySQL, MariaDB and SQL Server so
  far, widened an engine at a time as each one's arm of `Connection::apply_verified` lands) is not sent as the text
  the user read. The text is still what the user reads, and it is gated as it
  always was (`sql::is_generated_write_on`, `sql::draft_deletes_match`); the
  apply then cuts its statements out of that text (`sql::generated_writes`,
  which sees through the `BEGIN`/`COMMIT` brackets the text may hold) and sends
  them one at a time on one held session: a `BEGIN` of the apply's own
  (`Engine::transaction_start`, or `BEGIN` where one submission is already
  atomic), a `SELECT * … WHERE <the statement's own
  predicate> LIMIT 2` (two rows already prove it ambiguous; SQL Server's arm
  re-spells it as a preview is) for every `UPDATE` and `DELETE` (`sql::row_reread`, cut from the
  statement's own parse tree and admitted by `sql::is_generated_select`) before
  any write, the writes in the order the text holds them with each one's row
  count checked, then `COMMIT`, or `ROLLBACK` on any refusal or failure before
  it. Every re-read is compared whole, by column name and byte for byte, with
  the row as the grid fetched it (copied by value when the review was written);
  a row that differs or is gone writes nothing, keeps the draft and is named in
  the review. A statement that does not count exactly one row rolls the whole
  sequence back; a `DELETE` may count none only when its row is named by its
  key (`VerifiedStep::by_key`) and a second read of it finds it gone, which an
  earlier `DELETE` of the draft taking it along would leave, and a statement
  something skipped (a trigger returning without writing) leaves its row there.
  A row named by every value reads as gone once an earlier write changed one of
  them, so there a `DELETE` counting none is a miscount. Such a row is proven
  the one read only at its re-read: a trigger or a cascading foreign key fired
  by an earlier write of the draft can move another row onto exactly its
  values, and the write would count that row. So a keyless `UPDATE` or
  `DELETE` after any earlier write is refused, inside the apply's transaction
  and before any re-read (`db::rewrites::probe`, `KEYLESS_AFTER_A_WRITE`),
  when the catalog shows a trigger on its table or on a table written before
  it (or, on Postgres, a rule), or a foreign key from its table to one written
  before it with `ON DELETE` or `ON UPDATE` `CASCADE`, `SET NULL` or `SET
  DEFAULT`. SQLite, Postgres, MySQL, MariaDB and SQL Server read their
  catalogs; DuckDB has no triggers and its foreign keys never cascade; any
  other engine refuses. A first step, and a row named by its key, are never
  asked about, so multi-row keyless drafts on a plain table still apply. While a transaction
  the user opened is open on the connection (`Profile::in_transaction`, which
  counts MySQL's `SET autocommit = 0` and SQL Server's `SET
  IMPLICIT_TRANSACTIONS ON`) a verified apply is refused, at review and at Run
  (`session::APPLY_IN_TRANSACTION`), because its own `BEGIN` and `COMMIT`
  would end that transaction; the draft is kept. The arm asks the server too,
  under the lock it holds, before its `BEGIN` (`Sending::Open`): SQLite and
  DuckDB `is_autocommit`, MySQL and MariaDB `@@autocommit` and a `SET
  TRANSACTION` refused with 1568 inside one, SQL Server `@@TRANCOUNT` and
  `@@OPTIONS & 2`, Postgres the 25001 warning its `BEGIN` raises inside one.
  There is no `FOR UPDATE`: the pinned grammar cannot read it.
  Every re-read runs before any write, so a step's row waits from its re-read
  through the rest of the re-reads and the writes before its own, many round
  trips on a long draft. The isolation the apply runs under covers that
  window: Postgres opens it `REPEATABLE READ` (a write to a row changed since
  the snapshot fails with 40001), MySQL and MariaDB `SERIALIZABLE` (each
  re-read holds a shared lock on its rows to the `COMMIT`), SQL Server runs
  each re-read `REPEATABLE READ` (the same, by lock); SQLite has one writer and
  DuckDB fails a conflicting write. MongoDB has no transaction, so a document
  changed between its re-read and its write is overwritten. Targets that do not verify
  keep `Connection::generated`. On MySQL and MariaDB a table whose storage
  engine has no transactions (`information_schema.ENGINES.TRANSACTIONS`, so
  MyISAM, Aria, MEMORY) is not editable at all: the `ROLLBACK` would undo
  nothing, and a draft kept after a failure part way would send what already
  ran again.
- Import Rows shows every statement it will send in its dialog's SQL view
  before any runs.
- **Explain** puts the engine's `EXPLAIN` prefix on a copy of the
  statement, never into the buffer.
- **Transaction control** appends its own statement to the buffer and runs it
  from there (`Workspace::run_in_buffer`, `src/workspace/transactions.rs`).
  Turning auto-commit off opens the transaction at once, because a switch
  reading "off" over a session that is still autocommitting is wrong for as
  long as it takes to lose a row; Commit and Roll Back send `COMMIT;` and the
  next `BEGIN;` as one submission, because a gap between them is a statement
  that commits itself under a switch saying "off". `Engine::begins_transaction`
  spells the `BEGIN`, and is not `Engine::transaction_start`: that one answers
  whether a *generated* batch needs brackets, and says `None` for Postgres,
  where one submission is already one implicit transaction. Turning
  auto-commit back on while a transaction is open is refused rather than
  resolved, since dbdelve does not get to choose between committing the
  user's work and discarding it.
- **Format Query** rewrites the buffer on command only, through
  `sqlformat`'s token-level reformatter (on MongoDB, `mql::format`'s
  bracket-depth layout over the tree-sitter tokens, refused unless the
  result reads back as the same statements). Never an AST round-trip: that
  regenerates the statement and drops every comment the user wrote.

Limits on what DBDelve may write. It never writes `DROP` or `TRUNCATE`
into a buffer or onto the wire, whatever the user asked for. It writes
`DELETE` only as the explicit deletion of one named row: by primary key (on a
table with no primary key, by all of the row's values as read, a NULL as
`IS NULL`, and only when exactly one row matches), from a direct ask, with the statement shown before it runs
(`sql::delete_row`), or as one `DELETE` per row the user marked in a reviewed
draft, each naming that row's key and shown with the rest of the draft before it
runs. The one exception to both is the explorer's
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
passes first. It admits exactly five shapes:

- a batch of `UPDATE`s, optionally bracketed by a `BEGIN`/`COMMIT` the gate
  can see closed, each setting columns only to values DBDelve writes
  (`sql::literal_sets`): a single-quoted literal (`N'…'` too, never two
  adjacent), a bare `0x…`, `NULL`, `DEFAULT`, or the engine's own literal of
  bytes (`Engine::binary_literal`): `E'\\x…'` on Postgres, `X'…'` on MySQL,
  MariaDB and SQLite, `0x…` on SQL Server, hex digits in pairs and nothing
  else between the quotes, read off the text because the grammar reads
  `X'00' 'aa'` as one literal, with no comment anywhere in the batch (the
  grammar reads MySQL's `#` as an operator, so a `#` counts). No number, column, function call,
  concatenation or subquery is a value DBDelve sets. The predicate is not
  read on this arm (ClickHouse's, which names rows by every value, is);
  the bytes literal is admitted only as a set value, never in a key, a
  `DELETE` or an `INSERT`. Postgres's is an escape string so that
  `standard_conforming_strings` cannot change what it means, MySQL's is
  `X'…'` because `0x` has no empty spelling there, and SQL Server, whose
  server reads a bare `0x` as empty bytes, has empty bytes refused instead
  because the grammar does not read a bare `0x` as one literal;
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
  `RETURNING`, no `LIMIT`, and nothing else in the submission. A row with no
  primary key is named by every value, so a term may also be `col IS NULL`
  (never `IS NOT NULL`) on Postgres, MySQL, MariaDB, SQLite, DuckDB and SQL
  Server (`SqlSpec::null_terms`); ClickHouse, whose drafts are not verified,
  also admits `toString(col) = '…'` (the only function call admitted, over a
  bare column) under `SqlSpec::row_terms`. The `IS NULL` term is read through
  the spoken engine's spec, so a plugin that speaks one of those dialects is
  admitted it too (`generated_writes`); its driver holds each statement to
  exactly one row or saves nothing;
- a draft: more than one statement, optionally bracketed as the first shape
  is, each an `UPDATE`, an `INSERT` of exactly one row with no skip tail
  (otherwise as the second shape reads it) or a `DELETE` of the third shape,
  holding at least one `INSERT` or `DELETE`, with no comment anywhere in it
  (`sql::draft_sequence`). The count of `delete` nodes in the whole tree must
  equal the count of top-level statements that are such a `DELETE`, so one
  hidden in a CTE beside an `UPDATE` is refused. Admitted only on a spec whose
  grid writes a draft (`SqlSpec::drafts`: the six verified engines), so refused
  on Snowflake, Turso and Generic, and on ClickHouse, whose one-row guard a
  sequence cannot carry. A sequence's `UPDATE`s are read as a batch's are:
  every set value one the first shape admits (the bytes literal included), and
  the predicate not read on an engine that names a row by its key, so
  `sql::draft_deletes_match`, not the gate, is what holds an `UPDATE` to one
  row;
- a submission that is exactly one lone `BEGIN`, `BEGIN TRANSACTION`,
  `COMMIT` or `ROLLBACK`, compared as text. These are the brackets of an
  import's transaction, which spans submissions on a connection of the
  import's own, and are never admitted beside anything else.

Because it is a whitelist, `DROP` and `TRUNCATE` are refused structurally,
anywhere in the tree, CTEs included (`sql::forbidden`), and so is every
`delete` outside those shapes (`sql::deletes_anything`, checked on the
`INSERT` and `UPDATE` arms; the draft's count of `delete` nodes). On an engine where a backslash escapes
(`Engine::backslash_escapes`), a submission with any literal holding a quote
after an odd run of backslashes is refused, in every arm
(`sql::parse_gated`): the grammar ends the string there and the engine
does not. The grammar reads a backticked name on every engine, so one is
refused where the engine does not quote with a backtick (Postgres lexes it as
an operator character), wherever it holds a doubled backtick (the grammar
reads `` `x``y` `` as `x` aliased `y`), and on ClickHouse whenever it holds a
backslash, which the server reads as an escape and the grammar does not. The
filter gate parses the same way.

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

**Multi-row deletion by predicate is not admitted.** A reviewed draft may carry
one `DELETE` per marked row, each naming its own key and table, inside the
`BEGIN`/`COMMIT` bracketing multi-row edits already use, never one predicate
covering several. The gate reads each `DELETE`'s shape and cannot know the key
or the table, so `sql::draft_deletes_match` reads them out of the same parse
tree, and the draft is reviewed only when both hold and the `DELETE`s it
counts are the marks the grid holds. It holds the draft's `UPDATE`s to the
same table and key, and its `INSERT`s to the same table.

A cell is editable only when DBDelve can name its row by primary key; when
it cannot, the grid stays read-only and says why, and never guesses at a
predicate.

The one exception is a table with no primary key: a row is named by all of its
values as read, a NULL as `IS NULL`, and a write runs only when exactly one row
matches. Only a plain read of that one table (`sql::table_read`) is edited so:
a self-join's columns each name the table, but not which row of it. On the six verified engines the re-read must return one row and the
write must count one, inside the apply's transaction
(`sql::draft_deletes_match` holds a `DELETE` to every column of the grid's
key, as a set). On ClickHouse, before each `UPDATE`, `DELETE` or `ALTER` write,
an exactly-one-row `COUNT(*)` with the identical predicate (`sql::row_guard`)
must return one. An update on a table lacking the block columns is spelled
`ALTER TABLE <t> UPDATE <assignments> WHERE <predicate> SETTINGS
mutations_sync = 2`, exactly, with the setting last. A column that cannot be
matched by equality (on SQLite, any declared type outside the INTEGER, TEXT
and REAL affinities, a blob rendering as `x'…'` text no blob equals; on
Postgres and DuckDB, any type not on the list
`docs/agents/engines.md` gives, each listed because a test matched it on that
engine) leaves the grid read-only and says why; it is never
left out of the `WHERE`, and no unique key, rowid or subset of the columns
names the row in its place.

## Agent statements

**`sql::agent_gate(engine, sql, mode)` is the one answer for a statement an
agent originates, and nothing else answers for one.** It returns `Run`,
`Approve(verdict, expiry)` or `Refuse(reason)`, reads no profile, no silenced
confirmation and no earlier approval, and decides about the text as written. The
`mode` is the profile's, and it is the ceiling: a `Run` or an `Approve` always
passes `gate` with that mode and nothing silenced. The ladder, in the order it
is asked (the header of `src/sql/agent.rs` is its source):

1. An unreadable statement is `Refuse(Unreadable)`, `Refuse(Delete)` where it is
   seen to remove data, or `Refuse(Disclosure)` where it is seen to disclose, in
   that order.
2. No statement is `Refuse(NoStatement)`, two or more `Refuse(MoreThanOne)`.
3. A Redis command the editor never sends is `Refuse(NotSent)`.
4. `DROP`, `TRUNCATE`, any `DELETE` and an `ALTER TABLE` that is not `ADD
   COLUMN` are refused by name. On MongoDB and Redis a command seen to remove
   data is `Refuse(Delete)`, a Redis expiry whose timeout is not a plain
   positive number among them.
5. A server-side script is `Refuse(Script)`, a Redis or MongoDB command
   returning credentials, password hashes, privileges or other clients' details
   `Refuse(Disclosure)`.
6. A statement that changes the session is `Refuse(Session)`.
7. A verdict needing Full is `Refuse(NeedsFull)`: a Full profile grants an agent
   nothing a Read-write one does not.
8. A statement the mode stops is `Refuse(NeedsMode)`.
9. A write the mode allows is `Approve`. A Redis command that schedules a
   removal is approved as an expiry: the `Approve` carries an `Expiry` whose
   text is what the approval surface states beside the statement. It says the
   key, or for the hash commands the named fields, is removed when the time
   passes, and at once for a time already past. A write that schedules nothing
   carries none.
10. A read that calls an egress function, or on DuckDB holds a path, is
    `Approve` with a Read-only verdict, in every mode.
11. A read that calls a function its engine's built-in list does not name is
    `Approve` with a Read-only verdict at Read-write and Full; at Read-only it
    `Run`s.
12. Anything else is a read and `Run`s.

Steps 1 to 6 do not depend on the mode, so a refusal's reason is the same on
every profile. A `Run` is therefore a lone `sql::rerunnable` read that changes no
session state, calls nothing on the engine's `denied_functions` or
`egress_functions` and, at Read-write and Full, calls only functions on its
engine's built-in list.

**An `Approve` waits for the human, and runs the statement the dialog was
drawn from.** The call is held until the person approves, denies or lets it
expire, and nothing stands in for that act. At Approve the gate runs again, on
the exact string the dialog was drawn from, against the profile's current mode:
a different verdict, a profile whose connection has changed since, a dialog
never drawn for this call, or a byte of difference between the statement and
the source the dialog was last drawn from cancels the call as `refused_by_gate`
and sends nothing. The check compares source strings: what the dialog draws is
the marked text `ui::visible` makes of that source, which is display only. An approved statement whose verdict is Read-only runs on the profile's
agent read lane, which the database holds to reads only where the engine has a
hold (`ConnectionConfig::agent_reads_are_held`: a file opened read-only, or a
session setting); on SQL Server, Snowflake, MongoDB and Redis nothing on the
server does, and the approval says so. A Read-write one runs on a connection
opened for that call and closed with it, so a session change the statement
makes dies with the connection. A write aimed at a DuckDB database is refused (`DUCKDB_WRITE_REFUSED`), because a write
from a second connection to the file is lost. `explain_query` still refuses a
statement that needs approval and never waits for one.

**The disclosure line is drawn per engine.** On Redis it is `CONFIG GET`,
`CLIENT LIST`, `ACL LIST` and `ACL GETUSER`: plaintext secrets, every client's
address and name, and every user's password hashes. On MongoDB it is `currentOp`
as the first key of a `runCommand` or `adminCommand`, written as a document or
as a string and under `explain` too, the `$currentOp` stage at any depth of a
statement DBDelve parses, and the `db.currentOp()` helper, together with
`connectionStatus` whose `showPrivileges` is anything but `false`. A
database-level `aggregate` carrying the stage is not a method DBDelve reads, so
it is `Refuse(Unreadable)` and never runs. Only the agent path reads this line.
The human path keeps its verdicts: `CONFIG GET` and `CLIENT LIST` are writes,
and the rest are reads. Redis `ACL LOG` and `SLOWLOG GET` (the client fields of
logged entries), MongoDB `getParameter` and the `$listSessions` and
`$listLocalSessions` stages stay reads on both paths on purpose.

**On Redis a removal and an expiry are told apart by the statement alone.**
`Refuse(Delete)` is a command that removes now: a name in `REMOVES`, `MIGRATE`,
`XGROUP DESTROY`, an `XADD` that trims, `EXPIRE` or `HEXPIRE` with a timeout
that is not a plain positive number, and `HSETEX` or `HGETEX` with such an `EX`
or `PX`. An expiry (`redis_cmd::expiry`) is a command that schedules a removal:
`EXPIRE`, `PEXPIRE`, `SETEX` and `PSETEX` with a positive timeout, `EXPIREAT`
and `PEXPIREAT` at any time, `SET` and `GETEX` with a positive `EX` or `PX` or
any `EXAT` or `PXAT`, and the `H` forms of the same for the fields of a hash.
The gate has no clock, so a time already past, which removes at once, is
approved with the same text as a time to come. An expiry is refused at Read-only
like any write. `RESTORE` with a `ttl` also schedules a removal; it is approved
as a plain write that names none, and that stays a residual. The option words
are matched anywhere after the name, so a key or value spelled like one is named
an expiry too, which is the safe direction. Only the agent path reads either:
the human path rates all of these as writes.

**Egress is its own list.** `SqlSpec::egress_functions` names the functions that
read a file, a URL, a program or another server, or run text that may
(`pg_read_file`, MySQL's `load_file`, DuckDB's `read_csv` and `read_xlsx`,
ClickHouse's `url`, `s3`, `file`, `remote`, `executable` and `eval`). An agent
that reads one chooses where rows come from and carries them off the machine, so
it waits for the human. It is not on `denied_functions` because that list raises
the human path's verdict: a user's own read of a file, and DBDelve's own CSV,
Excel, Parquet and JSON opens, which are these calls, would stop being reads.
Only the agent path reads the egress list, which is why it narrows what a mode
lets run and never widens it. Each list is pinned against its engine's own
catalog (`pg_catalog`, `duckdb_functions()`, `system.table_functions`) so a
function a later version adds fails a test by name; SQLite, Turso and Snowflake
list none, and SQL Server's list is from the documentation.

**Built-in lists.** `SqlSpec::builtin_functions` is a data file in
`src/db/functions/`, matched against a bare, unquoted, single-part call name.
Matching folds case, except on ClickHouse (`case_sensitive_functions`), where a
user's `todate` can sit beside the built-in `toDate`. A call to a name the list
does not hold asks at Read-write and Full, in any clause, in `FROM` and under
`EXPLAIN`, because it may be a user-defined function that writes and no engine
holds one back there. How each list was built:

- Postgres from `pg_proc` volatility, with the volatile names that change
  nothing added by hand.
- MySQL from its help tables, and MariaDB from `information_schema.sql_functions`,
  each minus the lock, sleep, benchmark, file and replication-wait functions.
- ClickHouse from `system.functions` minus its excluded categories, plus the
  local table functions.
- DuckDB from `duckdb_functions()` and SQLite from `pragma_function_list`, both
  generated from the bundled builds by an ignored writer test. Turso reads
  SQLite's list.
- SQL Server and Snowflake from the vendor's documentation, because no catalog
  lists their built-ins.

Every list leaves out its engine's deny and egress names. DuckDB's and SQLite's
are pinned by ordinary tests that compare the file to a fresh generation, and
Postgres's and ClickHouse's by `live_` tests that need the compose databases;
MySQL, MariaDB, SQL Server and Snowflake have no pin. A stale list costs one
approval, and a wrongly listed name is the breach.

At Read-only an unlisted call runs. Where the engine has a hold the server stops
what the call writes. Where it has none (SQLite, Turso, DuckDB, Snowflake, SQL
Server) that rests on the premise that a function there cannot write.

What the text cannot show is accepted:

- Postgres field notation (`SELECT t.f FROM t` calls `f(t)` with no
  parenthesis). Telling it from a column needs the catalog.
- A view, row-level policy, rule, user-defined cast or operator that calls a
  writing function.
- A user overload of a listed name with other argument types.
- A `search_path` the owner set ahead of `pg_catalog`.
- A listed name the connected server does not have, because the list came from a
  newer release or the name belongs to the grammar. A user function can take
  that name and run unasked.
- A ClickHouse executable UDF. It reaches outside the database, so the Read-only
  hold does not stop it, and at Read-only it runs.

**DuckDB reads a quoted string in a table position as a file or a URL**
(`FROM 'data.csv'`), so no function name marks the read. On an engine that sets
`SqlSpec::bare_path_reads`, a string literal or `"`-quoted word holding a `.`,
`/` or `\` asks first wherever it sits in the statement. That is a superset of
the table positions, which is the safe direction, and its accepted cost is that
any dotted literal asks (`WHERE v = '1.2'`); reading table positions off the
parse would be a second reading of DuckDB's grammar kept in step by hand. A
table the administrator set up that reads outside the database (a Postgres
foreign table, a ClickHouse dictionary) carries no signal in the statement and
is not covered here.

## Connection modes, per engine

The core of the mode check is in `AGENTS.md`, "Connection modes". The detail:

- **An unreadable statement on an engine with no server-side hold.**
  `Engine::holds_read_only` is false on SQLite, DuckDB, Snowflake and SQL
  Server, so a statement `sqlparser` cannot parse carries Read-write there, not
  Read-only, and the gate asks for that. A statement it parses keeps its own
  verdict. The opt-in to run unreadable statements unasked
  (`sql::gate_own_run`) lets only `Stop::RunOnce` through, so this upgrade
  still asks: the setting is about being asked, not about the mode.
  ClickHouse holds Read-only (`readonly=2`) but not for every statement, so what it cannot parse is read by its first keyword
  (`UnreadableFloor::FirstKeyword`): reads stay Read-only and run once on
  confirmation, writes need Read-write, DDL, server control and `BACKUP` and
  `RESTORE` need Full, and anything it does not know fails closed to
  Read-write.
- **Generic SQL reads as unreadable what a server it does not know could lex
  otherwise** (`lexes_apart`): a `/*` inside a block comment, which
  sqlparser's tokenizer nests and a server may not (Db2 does, the tree-sitter
  split does not), so `/* /* */ DELETE FROM t -- */` is a comment to one and a
  `DELETE` to the other; MySQL's `/*! … */` and MariaDB's `/*M! … */`, which
  they run; and a backslash in a literal or quoted name, which ends it a byte
  later where it escapes. An unreadable verdict asks; a wrong read verdict on
  Generic would run a write in Read-only with nothing on the server to stop it.
  On MySQL and MariaDB, sqlparser reads the inside of a `/*! … */` but keeps a
  `/*M! … */` a comment, so that is unreadable on both (`runs_mariadb_comment`):
  the dialect cannot tell the server that runs it from the one that ignores it.
- **What lifts or ignores the hold is never a read.** A `SET` of
  `default_transaction_read_only`, `transaction_read_only` or `tx_read_only`
  to anything but on, a `READ WRITE` transaction characteristic (`BEGIN`,
  `START TRANSACTION`, `SET [SESSION] TRANSACTION`, `SET SESSION
  CHARACTERISTICS`) and a `set_config` that may name one of those settings
  are writes on every engine (`set_lifts_hold`, `calls_verdict`): the hold
  is a session setting the user's own statement can lift. Advancing a
  sequence (`nextval`, `setval`, `NEXT VALUE FOR`, Db2's `NEXTVAL FOR`) is a
  write everywhere, since Postgres lets a temporary sequence advance in a
  read-only transaction. An unquoted `s.NEXTVAL` is one only on Generic and
  Snowflake, whose servers spell an advance that way (`advances_by_name`);
  elsewhere it is a column. A call that acts outside the
  transaction (`pg_terminate_backend`, `pg_reload_conf`, `dblink_exec`, the
  rest of `HOLD_IGNORING_CALLS`) needs Full.
- **On Generic, a function call is a read only if it is a known built-in.**
  A function writes whatever its body does, and with no hold behind
  Generic's Read-only, a call to anything outside Generic's built-in list
  (`src/db/functions/generic.txt`: a bare, unquoted name of an aggregate,
  window, string, number, date or cast built-in), in any clause or in `FROM`,
  needs Read-write (`calls_unlisted_function`). Built-in engines keep reading
  such a call as a read on the human path: their hold stops what it writes. An
  agent's such call asks at Read-write and Full (see Agent statements). The
  list matches names, not
  overloads, so a user's own overload of a listed name (Postgres's
  `public.abs(text)` whose body inserts) reads as a read; this is accepted,
  since SQL alone cannot resolve an overload and Read-only guards against the
  user's mistakes, not a schema built to trap it.
- **Db2's own spellings read on Generic** where sqlparser fails on them: a
  statement's trailing isolation clause (`WITH UR`, `CS`, `RS`, `RR`) is
  blanked and a statement opening with `VALUES` is read as `SELECT` over the
  same expressions (`db2_spelled`), for classifying only; what runs is the
  text as typed. Anything after the clause, `USE AND KEEP … LOCKS` or
  `SKIP LOCKED DATA`, leaves it unreadable.
- **A `--` comment a lone carriage return runs through** is unreadable on every
  dialect whose server may end the comment there and sqlparser does not
  (`line_comment_past_carriage_return`): DuckDB's tokenizer does, Snowflake's
  and SQL Server's are not known not to, and Generic's server is unknown.
  Postgres's dialect already stops where Postgres does, and MySQL, MariaDB,
  SQLite and Turso run on to the newline as sqlparser does.
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
- **Through a plugin the hold is the dialect's own session statement**
  (`read_only_statement`), sent through the plugin as an ordinary query on its
  one session, so `holds_read_only` means the same over a plugin as built in.
  A dialect whose hold is not a session statement (ClickHouse's per-request
  `readonly`) is never offered (`Engine::speaks_through_plugins`). A hold that
  fails to land loses the session for good, with the session held across the
  loss so nothing queued runs in between, and a session the plugin reports lost
  takes no further request, since a driver that reconnected quietly would run
  it unheld; a string turning on MySQL's `AUTO_RECONNECT` is refused on a
  dialect that holds. Generic holds nothing.
- **A connect re-checks the mode as it lands.** A mode picked while a connect
  is under way finds no connection to hold (`set_mode`), so a session landing
  held one way with the other mode on screen is opened again under the mode now
  picked (`begin_connect`), on every engine, since the gate runs a statement
  it cannot read in Read-only on the strength of that hold.
- **A mode change's hold is ordered and awaited.** `set_mode` sends each hold
  through the profile's `HoldLine`, which takes them in turn and skips one a
  newer hold overtook, since the connection mutex is not first come, first
  served. `Profile::held` is cleared as it goes and set only when the latest
  lands, or when a connect lands, which fails rather than open unheld; until
  then an unreadable statement in Read-only asks even with the opt-in on.

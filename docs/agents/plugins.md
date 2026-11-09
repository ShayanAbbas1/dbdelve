# Engine plugins and the ODBC bridge

A plugin is a separate executable DBDelve starts once per connection made
through it, speaking protocol 1 over its stdin and stdout. It is the only way a
driver DBDelve does not link reaches a server, and the ODBC bridge is the only
plugin that ships. Where the code is:

- `plugins/protocol`: frames, messages, limits and the Arrow encode, validate
  and decode both sides link, so neither can drift from the other's reading.
- `plugins/odbc`: the bridge. `plugins/echo`: a fake engine for the host's
  tests, never shipped. All three are workspace members outside
  `default-members`.
- `src/db/plugin.rs`: the manifest and discovery. `src/db/external.rs` (with
  `external/host.rs`, `external/rows.rs`): the host, one child process per
  `Connection`. `src/db/target.rs`: `Target`. `src/db/odbc_string.rs`: what the
  app reads in an ODBC connection string without a driver.

## What a plugin connection is

- **Its SQL is a built-in `EngineSpec`, the dialect the profile speaks.** The
  ODBC tile starts on Generic SQL and the form can pick any dialect
  `Engine::speaks_through_plugins` admits: SQL whose Read-only hold, where it
  has one, is a statement the plugin can run on its one session. ClickHouse is
  left out, since its hold rides on each request and nothing a plugin is sent
  carries it. A plugin defines no engine, no fields and no spelling.
- **A plugin writes rows only in a proven dialect, and nothing else.**
  `external::writes_rows` is the allowlist, Postgres and Generic SQL, each
  with a conformance subject passing `generated_writes_round_trip` through a
  real driver; adding a dialect means adding its subject first. Those get
  edits and inserts, never imports, Copy DDL or database switching; every
  other dialect is `Abilities::NONE`. Abilities are asked of `Target`; asking
  `Engine` would hand a plugin profile its dialect's writes.
- **The edit target is DBDelve's own reading** (`external/edit.rs`), since a
  driver need not say where a column came from, and only on a connection
  whose plugin offered `write`. One plain `SELECT` of a schema-qualified table
  (the session's schema is not known to DBDelve), resolved through the
  plugin's `structure`: a quoted name as written, an unquoted one folded as
  the server would, lower on Postgres and both ways on Generic SQL, refusing a
  name both folds find. A bare name a server may read as its own value
  instead of a column (`current_schema`, which sqlparser reads as an
  identifier and Postgres as the function, typed as the column; on Generic
  SQL also `ROWNUM`, Db2's `CURRENT SCHEMA`) refuses the lot until quoted
  (`edit::computed`). The key must be whole in the result, every key column
  `dbdelve.exact`, and read as the catalog types it; other columns' types are
  not compared, since drivers spell them differently in a result and in the
  catalog (psqlODBC: `unknown` for `json`). Where the driver names a base
  for any column (`dbdelve.base_*`), every column must name exactly the one
  DBDelve read, compared case and all (`edit::agrees`); a result with none
  anywhere, Db2's unmeasured, rests on the reading alone. A lookup that fails
  leaves the rows read-only and the query succeeded.
- **The grid's writes are one `write`.** `Connection::generated` sends a batch
  the gate admits as `write`, its statements cut from the gate's own tree
  (`sql::generated_writes`) with the `BEGIN`/`COMMIT` bracket dropped; anything
  else runs as `query`. A plugin target reviews query-tab edits in the dialog
  (`Target::reviews_query_edits`), since a copy in the buffer would run
  without the one-row rule.
- **Autocommit only.** The bridge leaves the driver in autocommit and protocol 1
  has no `begin` or `commit`; `write` turns autocommit off for its own
  statements alone and back on before it answers. A typed `BEGIN … COMMIT` is
  the user's text and reaches the server only as the driver passes it on
  (psqlODBC does, and its conformance subject keeps such a transaction across a
  capped read); a driver that commits each statement itself does not, so manual
  transactions are not supported through the bridge. They come with protocol
  messages for them.
- **What v1 leaves out:** `routines`, `sizes` and `databases` answer empty,
  Copy DDL is off, and only a statement's first result set crosses (the rest
  are counted, and the result says how many were not shown).

## The protocol

**Frames,** both directions: `u32 LE len` (bytes after the length field), `u8
kind` (0 control: one UTF-8 JSON object; 1 data: one Arrow IPC encapsulated
message), `u64 LE request`, then `len - 9` bytes of payload. The request id is
in the header only, never in the JSON, so the host's reader routes a frame
without parsing it. A length over `MAX_FRAME` is refused before anything is
allocated for it.

**Messages** (`plugins/protocol/src/message.rs`, `tag = "type"`). Host to
plugin: `hello`, `prepare`, `open`, `query { sql, eligible }`, `credit`,
`stop`, `cancel`, `catalog`, `structure`, `references`, `write { statements }`,
`close`. Plugin to host:
`hello`, `prepared`, `opened`, `end`, `error { kind, message, sqlstate,
position, lost }`, `catalog`, `structure`, `references`, `closed`. `credit`,
`stop` and `cancel` carry the id of the query they target and get no answer of
their own: that query's terminal frame is the answer. Unknown fields are
ignored; an unknown `type` is a violation on the host and `error { kind:
unsupported }` from a plugin.

- **Optional messages exist only once something sends them.** v1 defines two,
  `references` and `write`, each sent only when both the host's `hello` and
  the plugin's `opened` list it. A later one is added beside them with no
  protocol bump, because a host never sends what a plugin did not list.
- **`write` is the grid's edits, one transaction.** Statements DBDelve wrote,
  each meant to change exactly one row, run through the driver's own
  transaction control, never as `BEGIN` text, which a driver that commits each
  statement itself would run as a no-op. Committed only if every statement
  changed exactly one row, rolled back otherwise; answered as `query` is, by
  `end { rows: 0, rows_affected: total }` or by `error`, and a `cancel` rolls
  the lot back. The session is in autocommit again either way, and an `error {
  lost: true }` says it could not be rolled back or put back in autocommit.
- **The password travels only in `open`**, never in `prepare`, and in its own
  `Password` type whose `Debug` prints nothing, so no log of a request can carry
  it.

**Limits** are `plugins/protocol/src/limits.rs`. The ones with a reason:

- `MAX_FRAME` (64 MiB) bounds the largest cell a result can carry, text or
  binary, since bytes travel as Arrow Binary rather than hex. A larger value is
  an `error` naming its column, never a truncation. `ponytail:`
  chunked values lift it.
- **Credit.** A query starts with `WINDOW` (8 MiB) of credit. A data frame of
  payload `s` costs `credit_cost(s)`, the payload plus a flat `FRAME_COST`
  (1 KiB), because a frame costs the host a queue entry and a grant whatever it
  carries, so a stream of tiny frames must run out as a stream of large ones
  does. A frame goes when its cost fits in what is left, or alone when nothing
  is outstanding, so one row larger than the window still gets through. The
  host checks exactly that per frame on the reader thread, and grants the cost
  back only once the caller has handed that batch's kept rows on: a feed whose
  taker stops taking stops the credit, and backpressure reaches the plugin
  without the reader ever pausing. An empty data frame, or one for a request
  that is not a running query, is a violation.
- **The cell budget.** A record batch carries at most `MAX_BATCH_CELLS` cells
  (a batch with no columns counts one a row), enforced on both sides. Rendering
  a cell costs the host whether or not it took bytes, and a Null column takes
  none, so the frame's size alone cannot bound it.
- `BATCH_TARGET`, `BATCH_ROWS` and `BATCH_AGE` are a guideline for plugins,
  not enforced: they make first rows show quickly.

**Data.** A result set's first data frame is the IPC schema, each later one
exactly one record batch. No compression and no dictionaries, so a batch
decodes to the size its frame declares and nothing is kept between batches; no
nested types (`ponytail:` they render through `arrow-cast` once ADBC needs
them). The host refuses a body length the payload does not hold exactly, and
buffers that overlap, before decoding, since the decoder copies a misaligned
buffer as declared and overlapping spans would let one frame decode to many
times its size. A timestamp's zone is an offset (`+HH:MM`) only: a named zone
needs a time zone database the host does not ship and would fail only at its
first rendered value. Decoding runs under `catch_unwind`, because `arrow-ipc`
has panicked on malformed input, and a plugin's bad frame is a violation, never
a crash of DBDelve. Arrow arrays become `db::Row` in `external/rows.rs` and go
no further (hard rule 4).

Field metadata: `dbdelve.type` is the driver's name for the column's type, and
`dbdelve.exact` (`EXACT`), `"true"` on a column whose values travel as the
server's own text that reads back as the same value, says a row can be found by
them in a `WHERE`. A key column without it does not make rows editable.
`dbdelve.base_table` and `dbdelve.base_column` (`BASE_TABLE`, `BASE_COLUMN`,
with `dbdelve.base_schema` beside them where there is one) name the table
column a result column is read from, set together and only where the driver
names both: psqlODBC (`SQL_DESC_BASE_COLUMN_NAME`) repeats an expression's
label as its base column, `current_schema` included, and names no table.

## Opening

`hello` (protocol check: "The ODBC plugin speaks protocol 2; this DBDelve
speaks 1."), then `prepare` with the fields as stored, then the SSH tunnel if
the profile has one, then `open` with the string the bridge is to dial. The
tunnel opens only after `prepare` is answered, so a plugin that cannot start
fails without an ssh session first. A plugin that dies before `opened` is
reported with the tail of its stderr (`STDERR_RING`), since a driver manager
that would not load says why there and nowhere else ("libodbc.so.2: cannot
open shared object file").

`prepare` exists for the TLS answers the spec gives a driver. The bridge
answers `separate_name: false, name_in_verify_ca: true`: the driver verifies
whatever host the string names. No ODBC layout has an `sslmode` for those
answers to refuse; see `ssh-tunnels.md` for what that means through a tunnel.

## The row limit: stop or drain

The host sends `stop` once it has kept the limit's rows, for every capped
query. The plugin stops sending rows at once and then either ends the statement
early, only when the query was `eligible` (a lone `sql::stoppable` read under
a limit, on a connection whose `opened.stops_early` is true) and it establishes
then that ending it keeps the session and any open transaction, answering
`end { stopped: true }`; or fetches to the end without sending and answers with
the whole statement's row count, which the grid shows as capped from. Rows past
the cap never cross the pipe, so a million-row drain costs no Arrow encoding.

**The ODBC bridge always drains** (`stops_early: false`, and it never reads
`eligible`). ODBC has no portable way to ask whether a transaction is open:
`SQL_ATTR_AUTOCOMMIT` reports the driver's mode, not whether a typed `BEGIN`
opened one, and `SQLCancel` aborts what it lands in. Closing a cursor early is
usually harmless and not provably so across drivers, and the row limit's rule is
that a statement is cut short only when that is provable. Echo implements
`stops_early: true` with a fake transaction flag so the early-stop path is
tested.

## Cancel, deadlines and a dead plugin

- **Cancel goes by the run's own token.** A statement's request id sits on the
  `CancelToken` of the run that sent it until it returns, and `cancel` sends
  `cancel` for exactly those ids. Every statement queues on the one process, so
  this is what tells a statement from the ones beside it: one still waiting for
  the session under a cancelled token is dropped unsent, and the lane running
  beside it is untouched. A Cancel may abort the user's transaction, as on a
  built-in.
- **The cancel deadline.** A query that has not sent its terminal frame within
  `CANCEL_DEADLINE` (5 s) of `cancel` gets its plugin killed, which resets the
  connection: a Cancel that cannot land must still give the connection back.
  The deadline is met when the terminal frame arrives, not when its caller takes
  it, so a consumer that stopped taking rows never trips it.
- **Catalog requests have a deadline instead**, the statement timeout plus
  `CANCEL_DEADLINE`, or 300 s with no timeout (`catalog_deadline`). They belong
  to no run, so no Cancel reaches them, and a driver wedged in one would hold
  the session until disconnect.
- **A crash is a reported reset, never a replay.** On a violation, EOF or exit
  every pending request gets "The ODBC plugin stopped (exit status: 139). The
  connection was reset: any open transaction, temporary tables and session
  settings are gone.", plus "Whether the statement that was running committed
  is unknown." for a running query. Nothing resends anything and the host never
  reopens itself: `is_lost()` turns true and the workspace's own reconnect
  opens a fresh connection and re-applies the Read-only hold before anything
  runs, which a self-reopening host would skip. An `error { lost: true }`
  (SQLSTATE class `08`) loses the session the same way while the process lives
  on, and every later request is refused, since a driver that quietly
  reconnected would run the next statement without the hold.
- **Death is read from the exit, by a watcher thread, not from stdout
  closing.** A driver's helper process, or any child forked at the wrong moment,
  inherits the pipe's write end and keeps stdout open after the plugin is gone.
  `ponytail:` the watcher polls every 50 ms.
- **The reader never stops reading.** It never decodes Arrow and never holds a
  lock across a read, so credit can always come back and `cancel` can always be
  answered; callers and `cancel` hand frames to a writer thread and never block
  on a pipe the plugin stopped reading.
- **A plugin exits on stdin EOF**, which is how app exit or a DBDelve crash ends
  it. Dropping the last handle sends `close` and keeps stdin open until
  `closed` or `CLOSE_GRACE`, so the driver disconnects or rolls back before EOF,
  then kills a plugin still running. On Unix the child is spawned with the
  signal mask emptied and on Windows put in a kill-on-close Job Object, for the
  reasons in `ssh-tunnels.md`. The environment is inherited, which ODBC needs
  (`ODBCSYSINI`, `ODBCINI`, the vendors' own variables).

## Manifests and discovery

```toml
id = "odbc"          # [a-z0-9-]+, persisted as "plugin:odbc"
name = "ODBC"        # what errors and the form call it
version = "0.6.2"
protocol = 1
executable = "bin/dbdelve-odbc"   # relative, no ".."; ".exe" appended on Windows
```

- `deny_unknown_fields`, and a `protocol` other than this build's, keep a
  manifest from loading: a newer plugin then reads as needing a newer DBDelve
  instead of half-working. A bad manifest is written to stderr and skipped,
  never fatal.
- Directories, earlier first, the first to load an id winning, so a user
  directory cannot shadow a bundled plugin: `<exe dir>/plugins/` (the Linux
  tarball, AppImage, Windows zip, and `target/<profile>/plugins/` under
  `cargo run`); on macOS `<exe dir>/../Resources/plugins/`, whose
  `executable` resolves by file name to `Contents/Helpers/`, because code under
  `Resources` breaks a sealed bundle; then `<store::dbdelve_directory()>/plugins/`,
  which is variant-aware, so the dev and release apps share none. Where each
  artifact puts the bridge is in `shipping.md`.
- Discovery reads manifests only, once in `main`, and the list is handed to
  the store and the workspace (no global, so e2e tests inject one). No process
  starts until a plugin connection opens. Each `Plugin` is leaked to
  `&'static` so `Target` stays `Copy`.

## Running it

- `dev/plugins.sh` (`--release`, `--echo`) builds and stages the plugins under
  `target/<profile>/plugins/`, where `cargo run` discovers them.
- The host's tests build echo themselves on first use
  (`external::echo::plugin`, into the test binary's own target directory and
  profile), so `cargo test` needs nothing staged. Echo is driven by the
  statement text: its module doc lists the commands (`rows N`, `sleep MS`,
  `crash`, `flood`, `lost`, `begin`, …) and the environment switches.
- The live subjects (Db2 and Postgres through psqlODBC), their variables and
  the bridge's own pipe-level tests (`cargo test -p dbdelve-plugin-odbc --test
  bridge`) are in `dev-databases.md`.

## The ODBC bridge

Threads: the main thread reads stdin frames until EOF, so `credit`, `stop` and
`cancel` reach a running statement; one worker answers every other request in
order and is the only writer of stdout. Any panic aborts the process, which the
host reports as a reset: a panicked worker would otherwise leave its request
unanswered. fd 1 is pointed at stderr and the frames go to a copy of it, since a
driver printing to stdout would corrupt them (`ponytail:` not on Windows).

- **ODBC 3.0, not 3.80.** IBM's Db2 driver, told 3.80, answers every static
  `SQLGetInfo` string with `03.80`, and nothing here needs 3.80.
- **Escape scanning off** (`SQL_ATTR_NOSCAN`, read back, and the statement
  refused when the driver keeps its own value): a driver otherwise rewrites
  `{fn …}`, `{d '…'}` and `{call …}`, which is a rewrite of the user's statement
  (hard rule 1). A `?` is still a parameter marker to some drivers; their error
  is shown as is. The read-back trusts the driver's answer, and psqlODBC answers
  on while it keeps scanning: `SELECT {d '2020-01-01'}` returns a date there,
  seen live on 18.00.0005, where Db2 refuses the same escape. Postgres has no
  bare `{` in its grammar, so valid SQL is unchanged and only a statement that
  would have been a syntax error runs rewritten. Accepted as a driver limit;
  refusing a bare `{` for that driver is the upgrade.
- **psqlODBC alters some date and time values**, another driver limit:
  version 18 re-parses a date or timestamp for every C type
  (`copy_and_convert_field`, `bZone = FALSE`), so BC dates, `infinity`, years
  past 9999 and a `timestamptz`'s offset show altered. Cast to `::text` in the
  query, or use the Postgres engine.
- **Wide text everywhere** (`SQLExecDirectW`, `SQL_C_WCHAR`), so a client code
  page derived from a C or POSIX locale cannot mangle text. Binary columns are
  read as bytes and sent as Arrow Binary, which the host shows as `0x` and hex,
  as is a driver's own type code whose name is `BLOB` or `BINARY` (Db2's `BLOB`
  is code −98 and would otherwise arrive in its driver's character form, hex
  without the `0x`). Approximate numerics are read as binary floats and sent as
  their shortest round-trip digits, since a driver's text may round (Db2 shows
  a `DOUBLE` to 15 digits). Every other column is Utf8. Each is tagged with the
  driver's own type name.
- **IBM's XML loses what its driver adds:** Db2 keeps no declaration, but its
  driver serializes a value with a byte-order mark and an `encoding="UTF-16"`
  declaration, so both are taken off IBM's `SQL_XML` (−370) and only off it.
- **SQLLEN out-values are zeroed and normalised, for every driver.** IBM's
  macOS `libdb2.dylib` writes only the low 32 bits of every SQLLEN (an
  indicator, a row count, a numeric attribute), so a NULL reads as
  `0x…00000000FFFFFFFF`. Every SQLLEN out starts at zero and is read through
  `raw::sqllen`: upper half zero means the sign-extended `i32`. A correct 64-bit
  driver's −1 is unchanged, so no driver is sniffed. `ponytail:` a genuine
  count in [2³¹, 2³²) reads as unknown; no length can get there.
- **odbc-api owns handles and execution; `raw.rs` reads every value.**
  odbc-api's buffers, indicators and `row_count` trust a 64-bit SQLLEN, so no
  value, indicator or count is read through them, catalog results included.
  `raw::get_data` reads each value in chunks, driven by the return code
  (`01004`: more follows) and the bytes gathered, never by the indicator's
  total, which a 32-bit driver cannot report past 2³¹; binary versus text comes
  from the column's SQL type, an `SQLSMALLINT` no SQLLEN width garbles.
- **Columns are bound one row at a time, never as arrays** (`raw::Bindings`):
  a bound array's indicator stride would carry the bug where no wrapper could
  fix it. Only when the driver reports `SQL_GD_BOUND | SQL_GD_ANY_COLUMN`,
  since a value that outgrows its buffer (psqlODBC sizes an unbounded `text` at
  a guess) is read again whole with `SQLGetData`; floats and text of a type ODBC
  defines are bound, binary and a driver's own types stay unbound.
- **psqlODBC streams through a cursor.** Its default, `UseDeclareFetch=0`,
  reads a whole result into the bridge before the first row, which defeats a
  row limit that stops the stream. So when `SQL_DRIVER_NAME` says psqlODBC and
  the string sets no `UseDeclareFetch`, the bridge reconnects once per open
  with `UseDeclareFetch=1;Fetch=4096` (keeping a `Fetch` the string sets).
  Connection strings only: a bare DSN is left as configured.
- **`DRIVER=` is checked before the driver manager sees it.** unixODBC formats a
  failed load into a 2,048-byte buffer and aborts past it, which macOS's
  `dlopen` error, listing every path tried, overran. A blank `DRIVER=` or one
  over 512 bytes is refused, and on macOS the name is resolved through
  `odbcinst.ini` and loaded with the driver manager's own `dlopen` first, its
  error reported by the bridge.
- **Cancel** sets the request's flag, wakes a worker waiting for credit, and
  calls `SQLCancel` from the main thread on the handle the worker published.
  Only calls that may wait on the server are published, with the statement's
  diagnostics emptied first, because unixODBC frees them on entry to every call,
  `SQLCancel`'s included, without a lock. A cancel is told by the flag, not the
  SQLSTATE: Db2 reports its query timeout as the same `HY008`. `ponytail:` a
  cancel landing just before or after the published call is missed, and the
  host's deadline covers it; closing that needs the driver's async mode.
- **IBM's driver reconnects on its own** (automatic client reroute) when the
  link drops and no temporary table or global variable is in use, runs the
  statement on the new connection with no error, and replays only the `SET
  CURRENT` special registers. No string keyword turns that off, so on IBM's
  driver alone (by `SQL_DRIVER_NAME`: 2605 lies outside ODBC's driver range,
  and another driver may read it its own way) the bridge sets
  `SQL_ATTR_REPORT_SEAMLESSFAILOVER_WARNING`, the statement then warns
  `CLI0212W` (native −99999), and a line after it says what was reset, in the
  result's messages or an error's. The session is not marked lost, since the
  statement ran. A catalog request that comes first after the reconnect
  swallows the warning.
- **Credentials are appended braced only when needed** (`UID=…;PWD=…`, after a
  string the user wrote): Db2's CLI driver keeps braces as part of the value,
  so a value is braced, `}` doubled, only for a leading `{` or a space at
  either end. psqlODBC reads a plain `PWD` URL-encoded (`+` a space, `%41`
  `A`) and a braced one as written, so a password holding `+` or `%` is braced
  too when the `DRIVER=` is psqlODBC, by its library or a `PostgreSQL…` name.
  A `DSN=…` string names no driver before the connect, so through one such a
  password still fails; a bare DSN goes to `SQLConnect`, which psqlODBC reads
  as written. A `;` in the user or password is refused, because Db2 splits on it
  even inside braces, and a credential would become a key of the string. A user
  `a@b` and a blank password go as they are (hard rule 5). A string with no `=`
  is a DSN and goes to `SQLConnect` with the user and password beside it.
- Login timeout 30 s (below the host's `OPEN_DEADLINE`), the statement timeout
  on every statement handle, catalog calls included. Diagnostics are joined, at
  most 16 records, with the password blanked in every spelling the string could
  carry; `IM002` gets one sentence naming `DRIVER=/path/to/library`.
- **`write`** is offered in `opened` only when `SQL_TXN_CAPABLE` is other than
  `SQL_TC_NONE`. It sets `SQL_ATTR_AUTOCOMMIT` off, runs each statement on one
  handle through the query path's publishing, so `cancel` reaches it, and
  requires an `SQLRowCount` of exactly 1 (`SQL_NO_DATA` counts 0), no result
  set and no further result from `SQLMoreResults`; then `SQLEndTran` commits,
  or rolls back on any refusal, error or cancel, and autocommit goes back on.
  A transaction that would not roll back is left in manual commit and the
  session lost, since turning autocommit on would commit it. Refusals say
  which statement and why ("The 2nd of 3 statements changed 0 rows, not one,
  so DBDelve rolled back every change and nothing was saved."); a server's
  error keeps its text and SQLSTATE with that line after it.
- **`dbdelve.exact`** marks a column read whole as text whose SQL type is
  character (`CHAR`, `VARCHAR` and their wide forms), an exact number
  (`TINYINT` to `BIGINT`, `DECIMAL`, `NUMERIC`) or `GUID`, and whose type name
  is not `bool`/`boolean`: psqlODBC's default `BoolsAsChar` reports a `bool` as
  a varchar reading `1`. Not floats, which the bridge reformats; not dates,
  times or intervals, which drivers render their own way; not long text or
  binary.
- **Catalog:** `SQLTables`, `SQLColumns`, `SQLPrimaryKeys`, `SQLStatistics`,
  `SQLForeignKeys`. Names are trimmed of trailing blanks, since Db2 returns them
  as padded `CHAR` (`ponytail:` a delimited name really ending in a space shows
  trimmed). Pattern arguments escape `_` and `%` with the driver's search
  escape, and results are filtered to exact names as a second guard. Views are
  any type holding `VIEW`; `TABLE`, `MATERIALIZED QUERY TABLE` and `GLOBAL
  TEMPORARY TABLE` are tables; system tables, aliases, synonyms and nicknames
  are left out.

### The driver manager, per platform

macOS has no system driver manager, so unixODBC is compiled from its vendored
source into the bridge (`stack.md`); Linux links the system `libodbc.so.2`;
Windows `odbc32`. The app itself links no ODBC.

A driver is reached by `DRIVER=/abs/path/to/library` in the string, which needs
no file at all, or through the user's own `odbcinst.ini`/`odbc.ini`. On macOS
the static unixODBC reads `/etc` (or `$ODBCSYSINI`) and `~/.odbc.ini` (or
`$ODBCINI`), never Homebrew's prefix. Run on macOS against Db2 and psqlODBC,
with no `/etc/odbcinst.ini`, which macOS does not have:

- A driver named in the string, `DRIVER={Db2}` or `DRIVER=Db2`, fails with
  `01000`, "Can't open lib 'Db2'": unixODBC tries the name as a library path.
  That is not `IM002`, so the hint does not appear.
- The same string connects once `ODBCSYSINI` names a directory whose
  `odbcinst.ini` has that section.
- A DSN with an absolute `Driver=` connects with no `ODBCSYSINI`, as `DSN=name`
  or a bare `name`; the run read the file through `$ODBCINI`.
- A bare name no file defines gets `IM002` and the hint.

From unixODBC's source, not run: when the system `odbcinst.ini` cannot be
opened, the lookup fails without reading `~/.odbcinst.ini`. So the library path
is the route to give users. Variables set in a shell profile do not reach an
app launched from Finder, and the bridge inherits only the app's environment.

## Connection strings in the app

`odbc_string.rs` reads the string in the app rather than the bridge, because the
secret refusal must hold before anything is written to disk (saving starts no
process) and the tunnel rewrite happens where the tunnel is.

- **`storable()` refuses** a key holding any secret-shaped part (`PWD`,
  `PSWD`, `PASS`, `SECRET`, `TOKEN`, `ACCESSKEY`, `APIKEY`, `ACCOUNTKEY`,
  `SHAREDKEY`, `PRIVATEKEY`, `CREDENTIAL`), a secret-shaped `key=` inside a
  value braced or not (a driver reading braces differently, or libpq inside
  psqlODBC's `pqopt`, would take it as one), a control character, a string that
  does not parse cleanly, and a URL-shaped or non-DSN value, since each could
  hold a password stored in plain text. Wide on purpose: a refusal costs less
  than a password on disk. Only the form and profile restore call it; `open` does not, so an importer
  that builds an `ExternalConfig` itself must call it too.
- **`AUTO_RECONNECT` is refused** on a dialect that holds Read-only: MySQL's
  driver would open a new session behind a lost one without the hold.
- The tunnel's endpoint and rewrite are in `ssh-tunnels.md`.

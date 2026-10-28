# Session and tabs

The shape a change to the main pane has to fit (`session.rs`, with the
`Workspace` methods split across `src/workspace/`).

- **A project is a named list of profile ids** (`store::StoredProject`, the
  `[[projects]]` tables in `profiles.toml`). A connection is in at most one,
  and those in none form the No project group, so each appears in the
  palette's Connections scope exactly once (`Workspace::move_to_project` keeps
  it that way). Which project you are in is derived from the connection in
  front (`Workspace::current_group`), never stored: it is what the titlebar
  names and what All in the palette and next/previous connection keep to.
  Tabs, saved queries and history stay per profile, which is what keeps
  projects from mixing state. A project may hold no connections. The connection form's Project control
  (`Workspace::project_dropdown`) picks the project, new and edit alike: a new
  connection starts in the project its form was opened for
  (`ConnectionForm::project`), an edited one in its own, and "New project…"
  names one that saving makes, or joins if the name is taken
  (`projects::chosen_project`).
- **The titlebar's project and connection open the palette on Connections**
  (`Workspace::open_connections`, also `SwitchConnection`, no default chord);
  there is no switcher panel of its own. That scope is a tree
  (`palette::tree`, over the rows `palette::listed_connections` builds): the
  connections in no project first, under No project ("Connections" when
  there are no projects and another group stands beside them), then under
  Projects each project as a row of its own -- folder tile, name, connection
  count, chevron -- with its connections indented beneath it on a hairline
  guide, then Manage (New connection…, New project…) at the bottom whatever
  is typed. Connection rows carry their engine's logo (in their colour when
  they have one), a status dot read as the row is drawn, and the one in
  front is marked current; an empty project holds one "New connection in…"
  row. A profile on an engine this build lacks (`Workspace::unavailable`) is
  listed in its project greyed, its reason as the hint, with no status dot
  and never under All; `enter` goes straight to the Remove confirmation, and
  with no supported profile ⌘⇧P opens this scope to reach them
  (`palette::without_a_connection`). `enter` on a project's row folds or unfolds it
  (`Command::ToggleProject`, the one row the palette stays up for); what is
  folded is `Workspace::collapsed_projects`, by name, kept for the session,
  starting unfolded, and the project in front is unfolded whenever a palette
  opens. With something typed nothing is folded: a connection that matches
  brings its project's row above it as context (highlighted only where the
  project's own name matched), a project that matches brings every
  connection it holds, the project holding the best match comes first, and
  the list opens on the best match rather than on the context row above it.
  Rows are told apart by what they are (`palette::Group`, and a project's row
  by its command), never by the header's text, so a project called "Manage"
  is its own project. A connection also matches on its host (`Item::also`),
  scored apart so a match never runs from its name into it, but highlights
  only in its name. Under All the tree is not drawn: only the other
  connections in the project in front appear, as a flat group. Whatever is
  done to one connection or project is a follow-up list, never a panel of
  its own: `shift-enter` on a connection's row (`palette_row_actions`), its
  trailing actions button or a right click puts up `Mode::ConnectionActions`
  (switch, edit, duplicate, move to a project, leave its project, remove,
  and its project's new connection, rename and delete), the same on a
  project's row puts up `Mode::ProjectActions` (new connection in it,
  rename, delete), and the Commands scope offers the same for the connection
  in front plus Rename/Delete project over `Mode::Projects`. On a row with no
  actions `shift-enter` is `enter`. Removing a connection or deleting a
  project goes through a one-row `Mode::Confirm`, which is the only arming
  they get: it opens with nothing selected, and a click there only selects,
  so neither a held `enter` nor a double click goes through it. Nothing that
  removes or deletes, and no connection, is remembered under Recent
  (`palette::remembered`). New Project and renames take the name in the
  palette's own field (`Mode::NameProject`, checked by
  `projects::project_name_refusal` as it is typed, and offering no row for a
  rename to the name it has). File → New Project (`NewProject`, no default
  chord) opens that, or the welcome surface's name field while there is no
  connection.
- **No connection is a state, not a prompt.** With no profiles and no form
  open, the window is the welcome surface (`Workspace::render_welcome`): New
  project, New connection, each project with an Add connection of its own, and
  a Theme row whose button opens the theme palette, so a first launch can
  leave the opaque default before connecting anywhere.
  Importing from DBeaver or TablePlus starts there too, or from the File menu
  and the palette once connections exist, never from over the connection form
  (its root does not answer `ImportConnections`, so nothing typed into it is
  lost); before anything is added,
  `render_import_choice` asks which project the new connections join
  (`PendingImport`): "Imported from <client>" by default, made if it is new
  and joined if not, an existing project, or none. While it asks, the
  palette draws over it, so it lists nothing that could change the answer or
  take away the project it names (`palette::waits_for_import`): the
  Connections scope is empty and says why, and the rows for New Project,
  Rename and Delete project, moving a connection to or out of a project,
  New connection in a project, and Remove connection are left out, as are
  the lists they open. A palette opened before the question came up is
  checked again when a row is confirmed. The welcome surface's New Project
  is refused too.
  Nothing forces a connection on a first launch, the form can always be
  cancelled, and removing the last connection lands back there. Settings opens
  there too. `note` writes to `welcome_notice` while it is in front, and a
  refused project name gets a line of its own (`project_name_error`) so it
  never covers that notice.
- **Whatever closes over a surface hands focus back by name**
  (`Workspace::refocus_front`): `editor_needs_focus` for a tab,
  `welcome_needs_focus` for the welcome surface. Reading what holds focus
  instead does not work: that is checked against the last frame, where the
  field that just closed still sat inside the same `track_focus` floor, and a
  window left focused on a dead field has every binding dead.
- **A profile owns a `Session`**, and a session owns two lists of tabs:
  `queries: Vec<QueryTab>` and `objects: Vec<ObjectTab>`. `Tab` is
  `Query(u64) | Object(u64)` and `active: Tab` says which is in front.
- **A dropped session turns the profile `Failed`, not just the statement.**
  After a run fails, `Connection::is_lost` asks the driver whether the
  session survived (Postgres's closed flag, MySQL/MariaDB's `COM_PING`; the
  other engines redial on their own and answer `false`). A lost one sets
  `ProfileState::Failed`. A failed profile offers `ui::reconnect_button`
  (dispatching `RefreshConnection`) beside the status bar's message and under
  a query's error, where the failure is already being read. `ui::status_dot` draws a
  profile's state the same way in the status bar and on each connection row in
  the palette.
- **A profile's database is the one it opens, and Select Database moves it.**
  On an engine where `Engine::switches_database`, `Workspace::set_database`
  writes the picked name into the profile's config, persists it and
  reconnects, so the next launch opens there too; blank is the server's
  login default. It closes every object tab first, and their grid snapshots
  and `pending_objects` with them, because those are keyed by schema and name
  within the profile, not by database, and would name relations of the
  database left behind. Query tabs keep their SQL but drop their results,
  queued ones and snapshots included, because those rows belong to the old
  database and an edit staged on them would be applied to the new one. A
  switch with unapplied cell edits on any grid is refused rather than
  discarding them. On DuckDB (`Engine::switches_database_in_session`) the
  databases are the ones attached to the instance, which reopening the file
  would detach, so the same switch is a `USE` on the connection already open
  (`Workspace::use_database`): no reconnect, a new generation and a fresh
  catalog, and a `USE` that fails puts the profile back where it was. The
  database the file opens as is stored blank, so a renamed file still opens.
- **Where `Engine::picks_schema` (Postgres, Snowflake, DuckDB), the titlebar
  picks the schema unqualified names resolve in**, without a reconnect: tabs,
  results and staged edits stay, since every name DBDelve writes names its
  schema. `Workspace::set_schema` records it on the profile and on the live
  connection, which applies it to the sessions open and to every one it opens
  later. Postgres puts it in front of the session's own `search_path`, and
  refuses inside a transaction, since a `ROLLBACK` would undo it;
  Snowflake sends it with the user's statements only; DuckDB `USE`s
  `database.schema`, which a `ROLLBACK` does not undo. MySQL and MariaDB have
  no schema apart from the database, and SQL Server's default schema is the
  login's (`ALTER USER … WITH DEFAULT_SCHEMA`), with no per-session setting.
  A picked schema that is gone when a Postgres or Snowflake profile connects is
  kept, and names resolve past it or fail in the server's words; DuckDB's
  `USE` refuses one, so its connection opens where nothing is picked, or on the
  picked database without the schema, and the profile drops what was not
  there and says so (`Connection::unpicked`). A dropped database closes what
  the profile held of it on the next frame (`Profile::left_database`), as a
  switch does. An attached database always goes this way after a restart,
  since `ATTACH` lasts as long as the instance.
- **Both kinds are addressed by id, never by index.** A result comes back
  carrying the `Tab` it was issued for, and an id that no longer resolves drops
  the result rather than landing it somewhere. Indexing would put a slow query's
  rows into whatever tab had slid into that slot.
- **A `QueryTab` owns its own editor, grid, `QueryState`, saved-query name
  (`open_query`) and `last_query`.** Do not reintroduce a single shared editor
  for anything.
- **A selection holding more than one statement runs each of them in turn, one
  chip per result set, unless every one is a plain read, when they run at
  once.** `sql::queued_statements` splits it and `Queue` on
  the tab is the queue: `remaining` is what has not gone out, `done` holds a
  `Finished` per result kept, and `showing` says which of them the tab's one
  `query`/`results` pair is displaying. That is one per statement on four
  engines, where a submission holds one statement; on SQL Server the unit is
  the batch, so one submission can land several (see the SQL Server batch-results bullet in
  `docs/agents/engines.md`). Every statement has a chip from the moment Run is
  pressed: those not yet sent read "queued" and cannot be selected. Each
  statement goes out through
  `execute_sql` separately, so `sql::classify` and `sql::gate` answer for each
  one on its own and a statement the gate stops parks a `PendingRun` the rest
  wait behind. Nothing else changes: one statement -- selected, or the one
  under the cursor with no selection -- takes the old verbatim path, comments
  and spacing intact, so this adds a case rather than rewriting the one that
  was there. Two consequences are worth stating. The split costs the implicit
  transaction a single Postgres `simple_query` submission used to give a
  multi-statement selection, so a failure part way leaves the statements
  before it committed; an explicit `BEGIN`/`COMMIT` in the selection still
  works, since the session holds it across submissions. And a statement that
  fails raises a Stop/Continue decision (`Session::queue_failure`) rather
  than deciding for the user, while a Cancel ends the queue without asking,
  because a cancel was already the decision.
- **A queue of plain reads runs at once, each on a connection of its own**
  (`session::runs_at_once`, `Workspace::run_at_once`). Plain is
  `sql::plain_read`: every statement a query `classify` reads as a read, so no
  `SET`, `USE`, transaction control, declaration or `SELECT … INTO`, no call
  whose answer is the session's (`nextval`, advisory and named locks,
  `LAST_INSERT_ID`, `SCOPE_IDENTITY`, `@x := …`), and on MongoDB a read with
  no `$out` or `$merge`. And only while `Profile::in_turn` is `None`: what the
  user's statements left on the session (`sql::leaves`) -- a transaction
  still open (`in_transaction`, opened by `BEGIN`, ended by `COMMIT` or
  `ROLLBACK`, read off what was sent rather than asked of the server), or a
  `SET`, `USE`, temporary table or one of those session calls
  (`session_changed`, until the next connect)
  -- which a fresh connection would not see. A read, a `SHOW`, an `EXPLAIN`,
  a statement nothing can parse and an autocommitted write or DDL leave
  nothing. A queue of reads held in turn this way says why in the notice.
  Every entry is in `done` from the start, in `Idle`, and runs through
  `execute_unchecked` with its `session::Lane`: the profile (by id and
  generation) and run (its shared token) it was issued for, and where it
  starts in `Queue::sql`. It lands in its own entry, found by that key and
  never by position, because a SQL Server batch landing several sets puts
  results ahead of those still running, and only while the tab still runs
  that same run; a switch to another profile meanwhile changes nothing about
  where it lands. Each statement's blocking call runs on a thread of its own
  (`on_own_thread`), as a Cancel does, not on GPUI's background executor,
  which on Linux has a thread per core and would be filled by them.
  `Connection::query_alongside` opens the connection the way the
  profile's was (its tunnel, password and timeout; Postgres and MySQL share
  its side session; the Read-only hold is put on it too) and drops it when the
  statement ends; Snowflake and MongoDB run it on the profile's own, which
  already runs statements side by side, and so does any engine whose server
  refuses another connection, behind the profile's other statements. There
  Cancel stops it only once it holds that connection (`Fetch::held`); while
  it still waits its turn, Cancel drops it unsent and leaves the statement
  ahead of it alone. At most `STATEMENTS_AT_ONCE` (8) are
  out together; the rest start as those land. Each is the lone statement on
  its connection, so it streams and stops at the row limit as a single run
  does. A failure stays on its own chip and the rest carry on, with no
  Stop/Continue to raise. The tab's own slot is `Running` under a
  `CancelToken::alongside` that only names the run; each statement runs under
  one of its own. So Cancel stops the statement on screen and the rest carry
  on, while with a finished result on screen it stops every one still out, as
  closing the tab does, and drops those not yet sent. Nothing else runs on or
  is written over the tab (a refusal goes to the notice) until the last has
  landed and `land_lane` gives the slot the last statement's result, as a
  queue run in turn leaves it. The slot streams nothing meanwhile, so the
  status bar reads the statement on screen instead (`Queue::at_once`).
- **A run's server messages ride on `QueryState::Complete { messages }`**, so
  they follow the state wherever it goes: into a queue's `done`, through a
  lend and back, onto a restored tab. `QueryTab::messages` reads them
  statement by statement (the slot's live state for the slot, `done` for the
  rest), and the switcher offers a Messages chip once any statement said
  something, with or without a queue.
- **`showing_messages` is a tab flag beside `showing_plan`, not a value of
  `Queue::showing`.** Selecting Messages leaves the selected result to return
  to, and a no-queue run can still offer the chip. It resets when a run
  starts, not per statement, and is read through `messages_shown`, so a flag
  left on over a run that said nothing falls back to the result. While shown it
  stands in for the result, so `query_insert_target` yields nothing.
- **`write_grids` saves a column-less statement when it has messages**
  (`StoredGrid::messages`, `serde(default)` for older snapshots), so what the
  server said survives a restart; otherwise a statement with no columns writes
  no snapshot.
- **`Queue::awaiting` is load-bearing.** A finished queue stays on the tab so
  its results can still be switched between, and every completion on that tab
  reaches `advance_queue`. Without the flag an ordinary Run, a header sort or
  a grid edit on such a tab would land in `done` as another of the queue's
  statements. A restored queue has it false and `remaining` empty for the same
  reason: it is a strip to read, not a run to resume.
- **A view's sorting is its own (`session::Sorting`).** `Server` is the header
  click `AGENTS.md` hard rule 1 describes. `Client(keys)` reorders the rows already held
  (`ResultGrid::sort_in_memory`) and runs nothing, so a relation sorts only the
  page on screen. A view starts from the Default sorting setting and is
  switched from the status bar. Client keys are column expressions
  (`filter::sort_expression`) resolved against each result as it lands
  (`ResultGrid::with_client_sort`), so a page turn or a re-run comes back
  sorted the same way. They are never written into a relation's `sort` or a
  snapshot's `order_by`: those rebuild the statement, and an in-memory sort
  must not reach the server unasked. Rows are reordered in place rather than
  through a display permutation, so every row index in the grid names the row
  on screen; pending edits move with their rows.
- **`queries` can be empty.** Any buffer can be closed, the last one
  included, and a profile with no tab open shows an empty pane and relaunches
  that way. A new connection, and one written before buffers were tabs, opens
  on one empty buffer instead. `active` may
  then name a tab that is gone, which every lookup by id already answers with
  `None`, and focus falls back to the window so the bindings that open a tab
  still work.
- **Closing a tab never deletes a saved query.** The strip holds open tabs
  only; every query file a profile has is listed in the sidebar's Queries
  group and the palette, open or not (`Session::saved_queries`). Closing a
  saved query's tab writes its text to its file and keeps the file; only an
  explicit Delete (the sidebar row's menu, or "Delete saved query…" in the
  palette, both through one dialog) removes it. Deleting a query whose tab is
  open closes that tab, its text with it; a tab holding cell edits nobody
  applied asks first, as any close does (`PendingDiscard::deleting`), and the
  file stays until it is told. A file already removed outside the app counts
  as deleted, so its stale library row and open tab can still be removed.
  Rename… on a sidebar row turns the row into a field
  (`Session::query_rename`) that renames through `store::save_query_as`,
  the tab's own path, so an open tab follows the name. A rename is one native
  atomic no-overwrite move, never a write under the new name and a delete of
  the old one: on a case-insensitive filesystem `daily` and `Daily` are one
  file. A name taken in any case is refused. New saves also install their
  target without replacing an existing entry; a missed directory listing is
  not permission to overwrite it. Staging files are private to each writer.
- **A named buffer persists to its query file, an unnamed one to its own
  `.scratch-{id}.sql`.** Edits stay in memory until Save, a connection switch,
  opening another query, or normal quit/window close persists the buffers.
  Closing a saved query's tab also writes it before the tab is removed.
  Writes are synchronous at those boundaries; typing schedules no disk
  writes, timers or retries. There are no background buffer writes to order
  against rename or deletion, so neither needs a per-tab write gate.
  A crash or forced termination can lose edits since the last write.
  Giving an unnamed tab a query name removes that tab's old scratch only
  after the query write succeeds. A cleanup error stays visible without
  undoing the saved query, and a failed query write keeps the scratch.
  `store::read_scratch(id, 0)` migrates the single `.scratch.sql` an older
  build left behind, and `StoredProfile::open_query` is still read for the same
  reason and never written.
- **Grids are snapshotted to disk and restored on relaunch.** The first edit on
  a restored grid asks first, because its rows may be stale. A queue's results
  are snapshotted too, one key each (`store::queued_grid_key`), each carrying
  its own statement as the snapshot's `last_query` -- which is what a restored
  chip is labelled from, so there is no second metadata file. How many there
  are is `StoredQueryTab::queued_results`, without which a tab that ran five
  statements and later two would restore three results it no longer has:
  the later run overwrites the first two files and leaves the rest. The count
  is also what keeps the startup prune off them. `hydrate_tab` rebuilds the
  strip, which is why it takes a `&mut Window`: a grid per result cannot be
  built without one. A query tab's plan rides in its own snapshot
  (`StoredGrid::plan`) as the rows it was parsed from, never the parsed tree,
  and is parsed again on restore: a typed `EXPLAIN`'s are the snapshot's own
  rows, the Explain command's are kept beside the grid's, which still hold the
  last real run. Whether the pane was showing it comes back too.
- **`clip` reads only the window a cell shows, not the whole value.**
  `ResultGrid::shown` clips a cell as it is drawn, on the frame thread, for
  every visible cell of every frame. Nothing clipped is kept: a stored second
  copy of every cell (up to 300 characters each) was built for all of them
  before the first frame. A line break early in a mostly blank value stops the
  scan at `CLIP_SCAN_LIMIT` bytes, and a number longer than the limit is shown
  ungrouped, so neither walks a whole value per frame. `clip` used to scan each whole value for line
  breaks before keeping `CELL_DISPLAY_LIMIT` (300) characters, so a result of
  large documents (5,000 rows of 51 columns, 206 MB, values up to 1.85 M
  characters) cost 11 s in a debug build and 0.15 s in release. `clip` now
  looks for a line break only in that window: it returns
  `clip_to(value, 300)` unchanged if there is none, and otherwise a single
  streaming pass reproduces the old split/trim/drop-empty/join-with-space
  flattening and stops one character past the limit. The cost follows the
  number of cells drawn, not their size.


## Completion

`src/completion.rs` offers the schemas, relations and routines the loaded
catalog holds, the columns of the relations a statement actually names, and the
keywords that carry a statement's shape. It is a `CompletionProvider`
implementation and nothing else: the popup, its scroll and its keys all belong
to gpui-component.

**Columns are not in the catalog, and must not be put there.** Fetching every
column of every relation at connect is unbounded: on a large schema it is a
multi-million-row result the driver buffers whole, held for the life of the
connection and duplicated into the provider's snapshot, all paid before anyone
has asked a question. A relation's columns are fetched when a statement first
names it, through `Connection::structure`, which the Structure tab already
runs; so completion adds no SQL of its own to any engine. `Session::completion_columns`
is the cache, cleared whenever the catalog reloads, and a miss is marked
`Loading` before the request so a relation is asked about once rather than once
per keystroke. A name the catalog does not list is never fetched at all,
otherwise a typo puts a describe on the wire for as long as it is on screen.

Two things about that cache are load-bearing and easy to undo by accident.
**`load_structure` fills it too**, because opening a relation's Structure tab
makes exactly the call completion would make; dropping that line costs a
duplicate round trip per relation. And **a failed fetch is retried, but only
`FETCH_ATTEMPTS` times.** Never retrying lets one blip (or one statement
timeout) kill completion for a relation for the rest of the connection; always
retrying puts a describe on the wire per keystroke, each queued behind the last
on the connection mutex, which freezes the profile rather than degrading it.

Two rules it exists under. **It is a lexer, not a parser**: half-typed SQL is a
parse error by definition, and `SELECT * FROM ` is both the text a user most
wants completed and the text the grammar returns an `ERROR` node for. And **it
must offer nothing inside a string literal or a comment**, which a token scan
cannot do by itself and is the one place accepting a row rewrites data rather
than a query.

A Redis buffer offers command names only, in the first word of a line, from
`redis_cmd::names` (the classify tables, less the refused commands), tagged
read, write or destructive; a name whose subcommand decides that (`CONFIG`,
`CLIENT`) has no tag. Key names are not offered.

A MongoDB buffer (`Engine::syntax`) is read by `mql::completing` instead, a
scan of the same kind: collections and database methods after `db.`, the
collection methods `mql.rs` reads after `db.<collection>.`, cursor methods
after `find(…).` or `aggregate(…).`, and inside a document the `$` operators
with the collection's sampled field names, which arrive through the same
column cache.

The provider is a snapshot, replaced whole when the catalog reloads
(`Workspace::install_completions`), and installed on every buffer rather than
the visible one.

A raw filter bar completes too, through `FilterCompletions`: a `WHERE` names no
relation, so the tab's schema and relation are handed to the same provider as
its scope, whose columns lead the list. The name is never written into the text
the scan reads, where a `'`, a `--` or a space in it would read as a literal, a
comment or two names. On MongoDB the text is read inside `db.c.find(`, since a
document is only one inside a call, and the ranges are shifted back. A Redis
filter is a `MATCH` glob and completes nothing. `install_completions` reaches
every raw bar as well, from `Session::completions`, the provider it last built,
which is also where a bar turned raw later takes it from. A new bar starts raw:
one waiting on a column ignores whatever is typed into it.

## Storage and platforms

- **Storage** is decided in `store.rs`: Application Support on macOS,
  `XDG_DATA_HOME` (or `~/.local/share`) on Linux, `APPDATA` on Windows. Crash
  logs are state, not cache: `~/Library/Logs`, `XDG_STATE_HOME`, `LOCALAPPDATA`.
  A `profiles.toml` that will not load, whether it would not read or would not
  parse, is moved aside to `profiles.toml.broken` before anything can save
  over it; if it cannot be moved, nothing is saved for the rest of the session
  (`store_unreadable`). The keybinding read at launch uses `peek_profiles`,
  which moves nothing, so the move and its notice belong to the workspace's
  own load. A profile on an engine this build cannot read is kept whole as its
  TOML table (`store::UnavailableProfile`), written back in its place, listed
  greyed, and can only be removed; nothing filed under its id is deleted while
  it holds that id, and a file with none decodes and saves exactly as before.
- **Passwords** go through `keyring`, never into `profiles.toml`: Keychain on
  macOS, Secret Service on Linux, Credential Manager on Windows.
- **Keybindings** are spelled `secondary-`, which GPUI reads as Cmd on macOS and
  Ctrl elsewhere. Never `cmd-`: on Linux that is Super, which the window manager
  takes first. Every binding is rebindable from Settings (`keybindings.rs`), and
  a rebind takes effect on the next launch.

## The connection form

Pick the engine first, from the searchable picker the engine pill in the form's header opens; it decides which fields exist.
Then paste a URL into the form's URL row, which fills the fields as it lands (one
typed rather than pasted fills them on Enter), or fill the fields in.
`secondary-enter` connects, or saves an edit (`submit_connection`, scoped to
`ConnectionForm`): it is Run Query's chord, and the unscoped Run Query reaches
the form first, finds no handler there and lets it through
(`keybindings::INERT_IN` is what tells the conflict check so). **Test** opens
the connection the way **Connect** would (keychain password, Read-only hold)
and throws it away, so a profile can be checked before it is saved.

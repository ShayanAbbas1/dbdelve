# AGENTS.md

DBDelve is a native database client in Rust on GPUI for macOS, Linux and Windows,
speaking Postgres, MySQL, MariaDB, SQLite, DuckDB, Snowflake, SQL Server, MongoDB and
Redis.

The split everything below leans on is _whose SQL it is_. An editor buffer is
the user's and is never touched uninvited; a browsing surface (an object tab's
preview) runs SQL DBDelve generates, regenerated from visible controls and
inspectable, never spliced into anyone's buffer.

**Read this file before doing anything.** It is the source of truth for how
DBDelve is built and why. Each area's detail lives in `docs/agents/`; the
[Reference](#reference) list at the end says which file a task needs.

## Hard rules

Violating one of these is a bug regardless of the benefit. If a task seems to
require it, stop and raise it instead.

1. **Never rewrite SQL behind the user's back.** A user's own statement runs as
   typed: no silent `LIMIT` injection, no column projection, no reformatting
   on execute. A client that silently alters statements cannot be trusted with
   the statements that matter. The row limit bounds a result by keeping only
   that many rows as they arrive, never by changing the statement, and the UI
   says when a result was capped. A `LIMIT` is written only into
   DBDelve-generated preview queries.

   DBDelve _does_ write SQL when the user asks it to, and only then, always
   where the user can read it: on a view the server sorts, a header sort
   splices its `ORDER BY` into the buffer (one sorted in memory reorders its
   rows and writes nothing); grid edits are appended to the buffer (`sql::appended_statement`),
   or shown in a review dialog on an object tab, which has no buffer; Import
   Rows shows every statement before any runs; Explain prefixes a copy, never
   the buffer; Format Query rewrites on command only, at the token level
   (`sqlformat`, or `mql::format` on MongoDB), never an AST round-trip, which
   drops every comment the user wrote.

   It never writes `DROP` or `TRUNCATE` into a buffer or onto the wire. It
   writes `DELETE` only as the explicit deletion of one named row, by primary
   key, shown before it runs (`sql::delete_row`). The explorer's Copy DROP and
   Copy TRUNCATE put text on the clipboard and nothing else. It never writes
   into a statement it cannot parse whole: `sql::with_order_by` refuses rather
   than guessing at a clause boundary, because a corrupted statement is worse
   than an unsorted grid.

2. **Generated SQL passes a whitelist gate, and there is one per path.**
   `sql::is_generated_write_on` is the single gate every statement the grid
   writes passes first; `sql::is_generated_select` guards the filter bar, the
   one place user text is spliced into DBDelve's statement. Both dispatch on the
   engine, to SQL's own gate or to MongoDB's (`mql::`) and Redis's
   (`redis_cmd::`). Do not add a path that bypasses them.

   **A gate verifies the parse tree, never the generator's word.** A gate that
   trusts its caller is a comment; the check lives in the gate rather than the
   generator precisely so the two can disagree.

   A cell is editable only when DBDelve can name its row by primary key; when
   it cannot, the grid stays read-only and says why, and never guesses at a
   predicate. A table without a primary key can be inserted into and not
   edited, deliberately. **Multi-row deletion is not admitted.** When it is
   wanted, the path is one `DELETE` per row, each naming its own key, inside
   the `BEGIN`/`COMMIT` multi-row edits use, never one predicate covering
   several.

3. **No environment-specific behaviour.** No vendor binary names in error
   strings, no assumption that a loopback host means plaintext, no hardcoded
   ports or hostnames. DBDelve is a generic client.

4. **Driver types do not reach the UI layer.** The grid receives rendered
   strings and type tags, never a `postgres::Row`, a `mysql::Value`, a
   `rusqlite::ValueRef`, a `tiberius::ColumnData`, an OID, a storage class or
   an epoch count off Snowflake's wire. Engine dispatch is a closed enum inside
   `src/db/` and stops there: no trait, no plugin surface. A UI that knows which
   engine it is talking to grows an engine-shaped special case in every view,
   and those are the special cases nobody ever removes. An enum rather than a
   trait for the same reason in miniature: an arm per engine the compiler makes
   every match enumerate, instead of an open extension point.

   The one thing that crosses out is `db::Engine` (`src/db/mod.rs`), because
   DBDelve writes SQL and has to spell it the way the server will read it. It
   holds no connection. Views and workspace code _ask_ it, through its
   methods, and never match on it. Where an engine question is missing, add a
   method to `Engine` rather than a `match` at the caller. The SQL writers in
   `sql.rs` and `filter.rs`, `explorer.rs`'s generators, the code that builds
   a `ConnectionConfig`, and `import::already_have`'s MariaDB-is-MySQL check
   are the only places above `src/db/` that match on it.

5. **Blank passwords are valid.** Never warn about them. Usernames containing `@`
   must work. Both are required by cloud IAM auth and both are commonly broken.

6. **Errors describe what happened, not what to do about it.** "Connection
   refused: nothing is listening on `host:port`" and stop. No speculation about
   the user's machine, no process-list inspection.

7. **An `sslmode` is never quietly weakened.** A connection either gets what it
   asked for or fails saying which certificate check failed. This is a rule
   because the failure is silent by construction: the driver's default is
   `prefer`, and `prefer` with a connector that cannot do TLS hands back a
   plaintext socket without even sending an SSLRequest, a cleartext password
   under a UI reporting success. If a mode cannot be honoured, refuse it by
   name; `tls::SslMode::parse` does that for `allow`, which libpq defines in an
   order the driver cannot express.

## Connection modes

Every profile has a `sql::Mode`: Read-only, Read-write (what a profile written
before modes existed reads as) or Full. Every statement executed goes through
`sql::classify` (in `Workspace::execute_and_then`), which parses with
`sqlparser` into a `Verdict` (the lowest mode that may run it, plus any
`Destructive` kinds), and `sql::gate`, which decides whether to run it, ask for
an upgrade, or ask for confirmation. Which engines hold Read-only on the
server, and how `IF` conditions and `CycleMode` are read, are in
`docs/agents/gates.md`.

- **`Mode`'s variant order is load-bearing.** `Ord` derives from it and the
  whole check is `required <= allowed`.
- **`sqlparser` here, tree-sitter everywhere else.** tree-sitter is
  error-tolerant and finds statement boundaries in a half-typed buffer; the
  mode check needs a typed statement and would rather refuse than guess.
  tree-sitter-sequel recovers `GRANT SELECT ON t TO u` as a `select`, and a gate
  that reads a `GRANT` as a read is worse than none.
- **Anything it cannot read is `Destructive::Unreadable`** and runs once on
  confirmation, before any mode comparison: every typo lands there, and asking
  for Full to get a syntax error back would teach people to live in Full.
  The exception is Read-only on an engine where `Engine::holds_read_only` is
  false: nothing on the server would stop it writing, so the verdict carries
  Read-write and the gate asks for that instead.
- Unknown statements and `ALTER` operations other than `ADD COLUMN` need Full.
  Keep it that strict.
- **`IF`/`ELSEIF` conditions are classified, not treated as unreadable.** Every
  branch and every condition counts, since the server picks which runs
  (`condition_verdict`); a function call or anything else it does not
  recognise still needs Full.
- A new connection starts Read-only (`connection_form::starting_mode`); a
  duplicate keeps the mode of the one it copies. The connection form offers
  the mode only while creating: editing never changes one.
- `Workspace::set_mode` is the only place a mode changes; it also pushes the
  mode into every open grid, which caches it.
- `Connection::set_read_only` holds a Read-only session to reads on the server
  where the engine has such a setting. It is a backstop, not the boundary:
  `sql::gate` is. A write-less role is the only real privilege boundary.

## Stack

`Cargo.toml` is the dependency list, and its comments say why each one that
is not obvious is there.

**Pins are exact and the lockfile is committed. Do not bump without being asked.**
gpui is pre-1.0 and breaks on minor bumps. It is `gpui-pre` because the `gpui`
crate is a one-off snapshot nobody republishes, and gpui-component depends on
`gpui-pre` under the name `gpui`, so taking it keeps one copy of the framework
in the graph.

- **The three tree-sitter pins are correctness, not formatting.** The grammar
  decides where every statement boundary falls, what `sql.rs` will splice an
  `ORDER BY` into (and `mql.rs` a `.sort()`), and what the gates accept. A bump
  changes what DBDelve sends to the server. Treat them like the driver pins.
- **gpui and gpui-component are vendored** under `vendor/` through
  `[patch.crates-io]`: five crates, for the backdrop blur and for a renderer
  blend-factor fix that keeps glass from going grey. `Cargo.toml` says which
  patch each crate carries and when it can be dropped. Every change there is
  marked `dbdelve patch` and re-applied by hand on a bump, so anything that can
  live in DBDelve's own code goes there instead.
- **Feature flags are load-bearing.** Without `gpui_platform`'s `font-kit` the
  macOS backend renders no text at all, silently; without `x11` or `wayland`
  the Linux backend panics on the first frame. `default-features = false` is
  load-bearing on each crate that has it, mostly to keep `aws-lc-rs` and C
  builds out of the graph.
- **Build profiles are deliberate.** Dependencies are optimized even in dev,
  because GPUI lays out and shapes text on the CPU every frame; deleting that
  makes the grid crawl.
- **Do not put tokio on GPUI's executor.** GPUI's executor is `async-task` with
  no reactor. A tokio future on `cx.background_executor().spawn(...)` _panics_
  the moment it touches a socket or timer. Database work uses blocking calls
  spawned onto the background executor; a driver that is async (Postgres, SQL
  Server, MongoDB) owns a tokio runtime per connection, and DBDelve blocks on
  it from a background thread. Nothing that leaves `src/db/` is a future.

## Build, run and check

```sh
docker compose up -d
cargo run
```

Linux needs the system packages listed in `README.md` first. **Plain `cargo
run` is the release variant**: with `DBDELVE_VARIANT` unset it reads and
writes the installed app's profiles and keychain passwords. Run
`DBDELVE_VARIANT=dev cargo run` to share the dev bundle's instead. The
development databases accept:

```text
postgresql://dbdelve:dbdelve@127.0.0.1:55432/dbdelve_dev
mysql://dbdelve:dbdelve@127.0.0.1:53306/dbdelve_dev
mariadb://dbdelve:dbdelve@127.0.0.1:53307/dbdelve_dev
mssql://dbdelve:DBDelve_dev1@127.0.0.1:51433/dbdelve_dev
redis://dbdelve:dbdelve@127.0.0.1:56379/0
mongodb://dbdelve:dbdelve@127.0.0.1:57017/dbdelve_dev
```

What a change should pass locally:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                      # unit tests; no database needed
```

The `live_` tests are `#[ignore]`d and read their connection settings from the
environment. The `tests` job in `.github/workflows/ci.yml` is the recipe for
running them: the values for the dev databases, and the exact `cargo test`
runs, which split off the timing tests and the vendored renderer's tests.

## Platforms

Paths per platform and the rest of storage are in `docs/agents/session.md`.

- **Storage** is decided in `store.rs`. A `profiles.toml` that will not load is
  moved aside to `profiles.toml.broken` before anything can save over it. A
  profile on an engine this build cannot read is kept whole as its TOML table
  (`store::UnavailableProfile`), written back in its place, listed greyed, and
  can only be removed; a file with none decodes and saves exactly as before.
- **Passwords** go through `keyring`, never into `profiles.toml`.
- **Keybindings** are spelled `secondary-`, which GPUI reads as Cmd on macOS and
  Ctrl elsewhere. Never `cmd-`: on Linux that is Super, which the window manager
  takes first.
- **The dev and release apps share nothing on disk** when the dev one runs
  as `DBDELVE_VARIANT=dev`, which the bundle sets for itself.
  `store::variant_name` moves the support directory and the keychain service
  together, and a variant it rejects is an error, never a fall back to the
  release name.

## GPUI hazards

Hard-won and easy to rediscover. The ones only animation work reaches are in
`docs/agents/ui.md`.

- **A repeating `with_animation` element requests a redraw every display frame
  while mounted.** One spinner has been measured pinning a window at 120Hz and
  36% CPU. The remedy is a single shared throttled clock with per-view leases,
  reaping stale leases and parking when the list empties. **DBDelve does not have
  one**: anything else that moves needs raising first, and a second repeating
  animation means building that clock and moving both onto it. What moves
  today is listed in `docs/agents/ui.md`.
- **A window with nothing focused has no dispatch path, so every keybinding is
  dead.** A keystroke reaches a handler only along the focused element's path to
  the root. Unmount whatever had focus (close a modal, switch to a surface with
  no focusable element) and the app stops responding to the keyboard until
  something is clicked. Anything that takes focus away must hand it back;
  `Workspace::close_palette` is the worked example, and `Focus::Window` is the
  floor under it for surfaces that have nothing to type into.
- **A binding with no context predicate wins over a scoped one.**
  `Keymap::binding_enabled` scores an unscoped binding at `contexts.len()`, the
  maximum, while `Some("Foo")` scores at the depth of that node, and the deepest
  match takes the keystroke. So a library binding scoped to an inner element
  beats the container's, which is why the palette's arrows are bound against
  `Palette > Input`: a descendant predicate matches at the leaf, the only depth
  that takes them back from gpui-component's input. Ties are broken by
  registration order, and DBDelve's `cx.bind_keys` runs after
  `gpui_component::init`.

## Conventions

- **Comments explain why, never what.** Code should carry its own meaning. Add a
  comment when the code cannot convey a decision that is non-obvious from
  reading it.
- **No speculative abstraction.** No trait with one implementation, no factory
  for one product, no config for a value that never changes. Do not build ahead
  of what has been asked for.
- **Deletion over addition.** The shortest change that fully solves the problem
  wins, once the problem is actually understood.
- A `ponytail:` comment marks a deliberate simplification and names its ceiling
  and upgrade path. Leave one where a shortcut is a decision, not an oversight.
- Non-trivial logic leaves one runnable check behind: the smallest test that
  fails if the logic breaks. No fixture scaffolding.
- Match surrounding code's naming, density and idiom.
- Commit messages and PR titles are `type(scope): summary`; the types and
  scopes are listed in `CONTRIBUTING.md`.
- **This file holds what every task needs; `docs/agents/` holds the rest.** A
  rule or gotcha only one area's work reaches goes in that area's file. Record
  a decision and its reason, not its history, and not a list a grep would give.

## Reference

Read the file a task touches before changing that area:

- `docs/agents/gates.md`: **generated SQL**, the **row limit** and **connection
  modes**: what each gate admits, on every engine, how each engine stops a
  capped read, and how each holds Read-only.
- `docs/agents/engines.md`: **engine-specific** behaviour: how one engine spells
  SQL, filters, pages, brackets transactions, cancels, renders values or reads
  its catalog, and the decisions behind each.
- `docs/agents/session.md`: **projects, the palette, the connection form, tabs
  and running statements**: sessions, queues, sorting, persisted buffers and
  grid snapshots, storage paths, the welcome surface, and completion.
- `docs/agents/ui.md`: **building UI**: which gpui-component pieces to use, the
  titlebar and system appearance, the animation hazards, and everything that
  animates today.
- `docs/agents/stack.md`: **dependencies**: why each `default-features = false`,
  how the tokio-based drivers stay off GPUI's executor, and where the MongoDB
  and Redis code lives.
- `docs/agents/ssh-tunnels.md`: **SSH tunnels**: the system-`ssh` forward, its
  readiness check, watchdog and TLS naming.
- `docs/agents/dev-databases.md`: **dev databases** beyond the URLs above:
  SQLite and DuckDB seeds, Snowflake's mock and live tests, moving ports, and
  telling a half-seeded container from a good one.
- `docs/agents/shipping.md`: **bundling and releasing**: `dev/bundle.sh`, the
  dev and release variants, signing and the Keychain, and cutting a release.

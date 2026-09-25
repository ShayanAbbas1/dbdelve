# AGENTS.md

DBDelve is a native database client in Rust on GPUI for macOS, Linux and Windows,
speaking Postgres, MySQL, SQLite, Snowflake and SQL Server.

The split everything below leans on is _whose SQL it is_. An editor buffer is
the user's and is never touched uninvited; a browsing surface (an object tab's
preview) runs SQL DBDelve generates, regenerated from visible controls and
inspectable, never spliced into anyone's buffer.

**Read this file before doing anything.** It is the source of truth for how
DBDelve is built and why.

## Hard rules

Violating one of these is a bug regardless of the benefit. If a task seems to
require it, stop and raise it instead.

1. **Never rewrite SQL behind the user's back.** No silent `LIMIT` injection, no
   column projection, no reformatting on execute, and nothing at all on a
   statement the user did not ask DBDelve to change. A client that silently
   alters statements cannot be trusted with the statements that matter, which
   is why "silently" is the word that carries the rule. Row limits apply to
   DBDelve-generated preview queries only, and they are visible in the UI.

   DBDelve _does_ write SQL when the user asks it to, and only then, always
   where the user can read it:

   - A header click asking for a sort splices the `ORDER BY` into the
     statement in the buffer, where it can be read, edited and undone; the
     statement that runs is the statement on screen.
   - Grid edits on a query tab are appended to the buffer
     (`sql::appended_statement`) and run from there. On an object tab, which
     has no buffer, the statement is shown in a review dialog before it runs.
   - **Explain** puts the engine's `EXPLAIN` prefix on a copy of the
     statement, never into the buffer.
   - **Format Query** rewrites the buffer on command only, through
     `sqlformat`'s token-level reformatter. Never an AST round-trip: that
     regenerates the statement and drops every comment the user wrote.

   Limits on what DBDelve may write. It never writes `DROP` or `TRUNCATE`,
   whatever the user asked for. It writes `DELETE` only as the explicit
   deletion of one named row: by primary key, from a direct ask, with the
   statement shown before it runs (`sql::delete_row`). And it never writes into
   a statement it cannot parse whole: `sql::with_order_by` refuses rather than
   guessing at a clause boundary, because a corrupted statement is worse than
   an unsorted grid.

2. **Generated SQL passes a whitelist gate, and there is one per path.**
   `sql::is_generated_write` is the single gate every statement the grid writes
   passes first. It admits exactly three shapes:

   - a batch of `UPDATE`s, optionally bracketed by a `BEGIN`/`COMMIT` the gate
     can see closed;
   - one `INSERT`, naming the columns it fills;
   - one `DELETE` whose `WHERE` is a conjunction of equality predicates over
     distinct, unqualified columns against single-quoted literals: no `OR`, no
     other operator, no subquery, no function call, no CTE beside it, no
     `RETURNING`, no `LIMIT`, and nothing else in the submission.

   Because it is a whitelist, `DROP` and `TRUNCATE` are refused structurally,
   anywhere in the tree, CTEs included (`sql::forbidden`), and so is every
   `delete` outside that one shape (`sql::deletes_anything`, checked on the
   `INSERT` and `UPDATE` arms).

   `sql::is_generated_select` guards the filter bar, the one place user text is
   spliced into DBDelve's statement: exactly one query, nothing destructive
   under it, no delete at all. It admits no write and is not a way around the
   first gate. Do not add a path that bypasses either.

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

3. **No environment-specific behaviour.** No vendor binary names in error
   strings, no assumption that a loopback host means plaintext, no hardcoded
   ports or hostnames. DBDelve is a generic client.

4. **Driver types do not reach the UI layer.** The grid receives rendered
   strings and type tags, never a `postgres::Row`, a `mysql::Value`, a
   `rusqlite::ValueRef`, a `tiberius::ColumnData`, an OID, a storage class or
   an epoch count off Snowflake's wire. Engine dispatch is a closed enum inside `src/db/` and stops
   there: no trait, no plugin surface. A UI that knows which engine it is
   talking to grows an engine-shaped special case in every view, and those are
   the special cases nobody ever removes. An enum rather than a trait for the
   same reason in miniature: five arms the compiler makes every match
   enumerate, instead of an open extension point.

   The one thing that crosses out is `db::Engine`, because DBDelve writes SQL
   and has to spell it the way the server will read it. It holds no connection.
   Views and workspace code _ask_ it (`quote_identifier`, `quote_literal`,
   `qualified`, `transaction_start`, `explain_prefix`, `assigns_default`,
   `fields`, and `filter::Operator::on` for the filter dropdown) and never
   match on it. Where an engine question
   is missing, add a method to `Engine` rather than a `match` at the caller. The
   SQL writers in `sql.rs` and `filter.rs`, and the code that builds a
   `ConnectionConfig`, are the only places above `src/db/` that match on it.

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

---

## Connection modes

Every profile has a `sql::Mode`: Read-only, Read-write (what a profile written
before modes existed reads as) or Full. Every statement executed goes through
`sql::classify` (in `Workspace::execute_and_then`), which parses with
`sqlparser` into a `Verdict` (the lowest mode that may run it, plus any
`Destructive` kinds), and `sql::gate`, which decides whether to run it, ask for
an upgrade, or ask for confirmation.

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
  false (SQLite, Snowflake, SQL Server): nothing on the server would stop it writing, so
  the verdict carries Read-write and the gate asks for that instead.
- Unknown statements and `ALTER` operations other than `ADD COLUMN` need Full.
  Keep it that strict.
- `Workspace::set_mode` is the only place a mode changes; it also pushes the
  mode into every open grid, which caches it.
- `Connection::set_read_only` holds a Read-only session to reads on the server
  (Postgres and MySQL; SQLite has no such setting, Snowflake no session
  to hold it in, and SQL Server no session-level switch). It is a backstop, not the boundary: `sql::gate` is. A
  write-less role is the only real privilege boundary.

---

## Stack

Every direct dependency, because a list that omits some is a list nobody
trusts. `Cargo.toml` carries the full reasoning; this is the shape of it.

```toml
gpui = { package = "gpui-pre", version = "=0.3.5" }  # rolling republish of zed main
gpui_platform = { package = "gpui-pre-platform", version = "=0.3.5" }  # font-kit, x11, wayland
gpui-component = { version = "=0.6.4", features = ["tree-sitter-sql"] }

tree-sitter = "=0.26.13"        # statement boundaries; the library keeps its tree private
tree-sitter-sequel = "=0.3.11"  # the SQL grammar. A CORRECTNESS pin -- see below
sqlparser = "=0.63.0"           # sql::classify only. default-features = false
sqlformat = "=0.5.0"            # Format Query only. default-features = false

postgres = "0.19"          # blocking client, NOT tokio-postgres
mysql = "28"               # rust-mysql-simple, blocking. default-features = false
rusqlite = "0.40"          # bundled + column_metadata + column_decltype. dff = false

rustls = "0.23"            # TLS; the driver ships none. default-features = false
rustls-native-certs = "0.8"     # the platform trust store, for Postgres
rustls-pemfile = "2"            # a named root certificate, and Snowflake's key file
tokio-postgres-rustls = "0.14"
keyring = "4"              # passwords: Keychain, Secret Service, Credential Manager

ureq = "=3.4.2"            # Snowflake's SQL REST API; blocking. dff = false, rustls on ring
ring = "=0.17.14"          # its key-pair tokens. Already linked as the TLS provider
base64 = "=0.22.1"

tiberius = "=0.12.3"       # SQL Server (TDS). dff = false, tds73 + rustls. Async, see below
tokio = "=1.53.1"          # tiberius's runtime, one per connection. dff = false: rt, net, time
tokio-util = "=0.7.19"     # compat: tokio's socket as the futures I/O tiberius speaks
futures-util = "=0.3.34"   # try_next over tiberius's result stream. dff = false

lsp-types = "=0.97.0"      # the completion provider's vocabulary. No server is started
nucleo-matcher = "=0.3.1"  # fuzzy scoring; gpui-component ships no scorer
icondata_lu = "=0.1.0"     # Lucide icon data; gpui-component ships no icon files
icondata_core = "=0.1.0"
guic-gpui-assets = "=0.2.0"     # the bundled fonts

geozero = "=0.15.1"        # WKB to WKT, so PostGIS geometry renders as text
hex = "=0.4.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.9"               # profiles.toml
url = "2"

# [target.'cfg(target_os = "windows")'.build-dependencies]
winresource = "=0.1.31"    # embeds the .exe icon (build.rs)
```

**Pins are exact and the lockfile is committed. Do not bump without being asked.**
gpui is pre-1.0 and breaks on minor bumps. It is `gpui-pre` because the `gpui`
crate is a one-off snapshot nobody republishes, and gpui-component depends on
`gpui-pre` under the name `gpui`, so taking it keeps one copy of the framework
in the graph.

**The two tree-sitter pins are correctness, not formatting.** The grammar
decides where every statement boundary falls, which statements `sql.rs` will
splice an `ORDER BY` into, and what the gates accept. A bump changes what
DBDelve sends to the server. Treat them like the driver pins.

**`gpui_platform`'s features are load-bearing.** Without `font-kit` the macOS
backend swaps in a no-op text system and renders no text at all, silently.
Without `x11` or `wayland` the Linux backend panics on the first frame.

**`default-features = false` is load-bearing on each crate that has it.**

- `rustls`: its defaults select the `aws-lc-rs` provider; `ring` is what is
  already linked through gpui. Every crate has to agree on one or both get
  built, and `aws-lc-rs` builds C and assembly. TLS is `rustls` rather than
  `native-tls` because `rustls` and `rustls-native-certs` were already in the
  graph: an adapter, not a second TLS stack, and no OpenSSL anywhere.
- `mysql`: `rustls-tls-ring` rather than `rustls-tls` for the same `aws-lc-rs`
  reason, and `minimal-rust` takes flate2's pure-Rust backend over zlib: no C
  for something already solved in the graph.
- `rusqlite`: 0.40's defaults pull in a WASM backend. `bundled` compiles the
  amalgamation rather than linking whatever libsqlite3 the OS shipped;
  `column_metadata` is what makes in-grid editing reachable, since it is the
  only way to learn that a result column is `accounts.id` and not an expression.
- `sqlparser`: its defaults build assembly through `cc` (`psm`) to guard
  against pathological nesting nobody sends from a local buffer.
- `sqlformat`: its defaults colourise a token dump nothing prints.
- `tiberius`: its defaults are `native-tls` (OpenSSL on Linux) and `winauth`.
  Its `rustls` feature is **tokio-rustls 0.24 on rustls 0.21**, a second rustls
  major version beside the 0.23 everything else shares: pure Rust and still on
  `ring`, so no second crypto library, but a second copy of the TLS stack, and
  one `tls.rs` cannot configure, because tiberius builds its own rustls config.
  It drags `rustls-native-certs` 0.6, `rustls-pemfile` 1 and a second
  `security-framework` in with it. Accepted, because it is the only maintained
  TDS client and its only TLS that is not OpenSSL.

**Build profiles are deliberate.** `[profile.dev.package."*"] opt-level = 3`
builds dependencies optimized, because GPUI lays out and shapes text on the CPU
every frame; deleting it makes the grid crawl. `[profile.release]` sets
`lto = "thin"` and `codegen-units = 1`.

**`default-features = false` on `ureq` states what its defaults happen to be.**
`rustls` there is rustls *on ring* with `webpki-roots`, both already in the
graph through `mysql`. Named rather than inherited so a release that changes
its defaults cannot bring `aws-lc-rs` in. `gzip` is not optional: every result
partition after the first arrives compressed. Snowflake publishes no Rust
driver and the community ones are tokio futures, which is why this is an HTTP
client and not a driver.

**Do not put tokio on GPUI's executor.** GPUI's executor is `async-task` with no
reactor (Grand Central Dispatch on macOS). A tokio future on
`cx.background_executor().spawn(...)` _panics_ the moment it touches a socket or
timer. Database work uses blocking drivers, which own their runtimes
internally, spawned onto the background executor. `rusqlite` is blocking by
construction and has no runtime at all.

`tokio` is a direct dependency for one reason, and the same rule is why it is
safe: tiberius is async with no runtime of its own, so `mssql::Connection`
owns a tokio current-thread runtime per connection and every call into the
driver is a `runtime.block_on(...)` on the background thread the query was
already spawned onto. That is what the `postgres` crate does privately around
tokio-postgres, written out. The runtime lives behind the connection mutex and
nothing that leaves `src/db/mssql.rs` is a future.

`tokio-rustls` in the tree is not a breach of that rule, and the rule is why:
the TLS handshake is a future belonging to the connection, so it runs inside the
runtime the blocking client already owns, on the same thread as the connect it
is part of. The same holds for tiberius's tokio-rustls 0.24, inside the runtime
`mssql.rs` owns. Nothing tokio-shaped reaches GPUI's executor. Adding a tokio
future anywhere DBDelve spawns one still panics.

**Do not fork gpui or gpui-component.**

### Local build and run

```sh
docker compose up -d
cargo run
```

Linux needs the system packages listed in `README.md` first (the same list CI
installs). The app opens the connection form when no `PG*` environment is
configured. The repository-owned development databases accept:

```text
postgresql://dbdelve:dbdelve@127.0.0.1:55432/dbdelve_dev
mysql://dbdelve:dbdelve@127.0.0.1:53306/dbdelve_dev
mssql://dbdelve:DBDelve_dev1@127.0.0.1:51433/dbdelve_dev
```

The ports can be moved with `DBDELVE_POSTGRES_PORT`, `DBDELVE_MYSQL_PORT` and
`DBDELVE_MSSQL_PORT`. The SQL Server image is `linux/amd64` only, so on Apple
silicon it runs under emulation and takes a while to come up.
Pick the engine on the form's chip row first; it decides which fields exist.
Then paste a URL and choose **Use URL**, or fill the fields in. Connecting is
the connection test; there is deliberately no separate test button.

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

**The MySQL container reports itself healthy when its init script failed.**
`mysqladmin ping` does not care whether the seed applied, so a half-seeded
database looks exactly like a good one. Check a row count, not the status;
`live_the_development_database_is_fully_seeded` is that check.

### Checks and CI

What CI (`.github/workflows/ci.yml`) runs, and what a change should pass
locally:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                      # unit tests; no database needed
cargo test -- --include-ignored --skip snowflake::tests::live_ # plus the live_ tests
```

The `live_` tests are `#[ignore]`d and read `PGHOST`, `PGPORT`, `PGDATABASE`,
`PGUSER`, `PGPASSWORD`, `dbdelve_MYSQL_URL`, `dbdelve_MSSQL_URL` and
`dbdelve_SQLITE_PATH` (the
lowercase prefix is what they read); the `tests` job in `ci.yml` has the values
for the dev databases. It has no Snowflake account, so it skips the Snowflake
live tests but for the one that needs none; the mock tests stand in for them. CI lints on Linux, macOS and Windows, runs the live tests
on Linux, and runs the unit tests on Windows too, since the storage tests are
what exercise the `APPDATA` paths.

### Platforms

- **Storage** is decided in `store.rs`: Application Support on macOS,
  `XDG_DATA_HOME` (or `~/.local/share`) on Linux, `APPDATA` on Windows. Crash
  logs are state, not cache: `~/Library/Logs`, `XDG_STATE_HOME`, `LOCALAPPDATA`.
- **Passwords** go through `keyring`, never into `profiles.toml`: Keychain on
  macOS, Secret Service on Linux, Credential Manager on Windows.
- **Keybindings** are spelled `secondary-`, which GPUI reads as Cmd on macOS and
  Ctrl elsewhere. Never `cmd-`: on Linux that is Super, which the window manager
  takes first. Every binding is rebindable from Settings (`keybindings.rs`), and
  a rebind takes effect on the next launch.
- **Titlebar**: macOS and Windows get DBDelve's own strip over a transparent
  system titlebar (`ui::CLIENT_TITLEBAR`); Windows draws no buttons into it, so
  `ui::titlebar` draws them. Linux keeps the system bar.

### Bundling (macOS)

`dev/bundle.sh` builds `--release`, generates the icon, writes `Info.plist`
and signs. It is the only way to get a real app rather than a binary. It
builds **two variants from one script**, and neither of them installs:

- **Dev, the default.** `target/macos-dev/DBDelve Dev.app`, named
  `DBDelve Dev`, id `com.shayanabbas.dbdelve.dev`. Run from the build tree,
  never copied to `/Applications`, and launched with `open` so LaunchServices
  owns it. The script refuses to build while a dev instance is running
  (overwriting a running executable is what breaks it) but says nothing about
  the released app, which is meant to stay open alongside.
- **Release, under `DBDELVE_CHANNEL=release`.** `target/DBDelve.app`, named
  `DBDelve`, id `com.shayanabbas.dbdelve`, no `LSEnvironment`. Only the release
  workflow sets it, and the DMG's drag-to-Applications is the install.

**The two variants share nothing on disk.** `DBDELVE_VARIANT` moves both the
support directory and the keychain service together: unset or empty is
`dbdelve` (the release's, never to move), `dev` is `dbdelve-dev`. Both, always:
a build that suffixed only one would read the release's saved passwords or
write its `profiles.toml`. `store::variant_name` is the single place it is
decided. It validates the variant with the same `unsafe_component` every path
component goes through, and a variant it rejects is an error rather than a fall
back to the release name, because silently falling back is exactly how a dev
build corrupts the real one.

The dev variant carries `DBDELVE_VARIANT=dev` in an `LSEnvironment` dict in
its own `Info.plist` rather than exported by the script, because the isolation
has to survive a launch from the Dock, from Finder, or from a crash reporter's
"Quit & Reopen", none of which see the shell that built the app.

Four things in the script are load-bearing:

- **`CFBundleIdentifier` scopes the Keychain.** Every saved profile password
  belongs to `com.shayanabbas.dbdelve`. Changing it orphans all of them. The
  dev id differs precisely so the two cannot reach each other's prompts.
- **The signature is not optional on arm64.** An unsigned arm64 binary will not
  launch, and copying the binary into the bundle invalidates the signature
  rustc left. `DBDELVE_SIGN_ID` takes a real identity; `dev/identity.sh`'s
  self-signed one (used automatically once it exists) is what stops the
  Keychain re-prompting after every rebuild; ad-hoc is the fallback and runs,
  but prompts.
- **`codesign --identifier` is passed explicitly.** The Keychain pins its
  "Always Allow" to the signature's identifier, so the one value that must not
  drift between builds is stated rather than inferred.
- **The font licences ship inside the bundle**, because the fonts are compiled
  into the binary and the OFL asks the licence to travel with them.

### Releasing

A release is cut from the Actions tab: run `.github/workflows/release.yml` with
**publish** ticked. The version comes from `Cargo.toml`, so bump it first: a
version that is already tagged fails before anything builds, and a stable
version must run from main. It builds, in parallel:

- Linux x86_64 and aarch64 tarballs and AppImages (`dev/package-linux.sh`,
  `dev/package-appimage.sh`) on Ubuntu 22.04, never `ubuntu-latest`: glibc is
  forward-compatible only, so the oldest runner is the oldest system the
  release runs on;
- the macOS DMG: `dev/bundle.sh` with `DBDELVE_CHANNEL=release DBDELVE_SIGN_ID=-`,
  plus an `/Applications` symlink;
- the Windows zip (`dev/package-windows.ps1`), unsigned.

Only once every asset exists does it tag the commit `v$VERSION` and publish,
then it rewrites `Casks/dbdelve.rb` in the `ShayanAbbas1/homebrew-dbdelve` tap
through the `TAP_TOKEN` secret. A version with a hyphen (`0.2.0-rc.1`) is a
pre-release, may run from any branch and leaves the cask alone. Unticked, the
workflow builds everything and publishes nothing.

**There is no notarization and no Developer ID.** A release signs ad-hoc rather
than with `dev/identity.sh`'s certificate: that certificate is trusted only on
the machine that created it, and to Gatekeeper an issuer nobody trusts reads
worse than no issuer at all. The trade is that the ad-hoc hash moves with every
release, so an update costs one fresh Keychain prompt. Installing still means
clearing quarantine by hand:
`xattr -dr com.apple.quarantine /Applications/DBDelve.app`.

### Engine divergences

Decided, and not to be re-litigated:

- **Seven functions generate SQL, and every one quotes through `Engine`:**
  `explorer::preview_sql`, `sql::with_order_by`, `sql::update_row`,
  `sql::insert_row`, `sql::delete_row`, `filter::sort_expression` and
  `filter::filter_predicate`. `filter::sort_expression` is the one that gets
  forgotten, and forgetting it is silent: a double-quoted name is a _string
  literal_ in MySQL, so `ORDER BY "name"` sorts every row by the same constant
  with no error. `filter::filter_predicate` quotes a _value the user supplied_
  (a filter bar's value, typed or put there by following a foreign key), which
  is the other half of the same hazard. Every filter operator lives inside it,
  `filter::substring` and `filter::like_pattern` included;
  `filter::foreign_key_filter` yields a bar rather than a `WHERE`, and
  `filter::derived_filter` folds the bars through `filter_predicate` rather than
  quoting anything itself. `sql::paged` writes too, but only two integers, into
  a preview's limit (see the SQL Server paging entry below).
- **Three filter operators are written differently per engine, and two of those
  are decided by the grammar rather than by any server.**
  `sql::is_generated_select` refuses whatever `tree_sitter_sequel` cannot parse
  whole, and that pin does not move, so a predicate the grammar does not know
  is one DBDelve cannot run, however valid the server would find it. It has **no
  `ESCAPE` clause and no infix `REGEXP`**. So the pattern operators lean on the
  engine's default `LIKE` escape, which is the backslash on Postgres and MySQL,
  and `like_pattern` escapes `%`, `_` and the backslash itself with it.
  **SQLite, which has no default escape at all**, gets `instr`/`substr`
  substring arithmetic instead: case-sensitive where its own `LIKE` is not,
  which is the price of not silently widening a match on a value containing
  `%`. **SQL Server has no default escape either**, but a bracket makes any
  character literal there, so its pattern brackets `%`, `_` and `[` (`[%]`)
  and keeps `LIKE`. The regex match is Postgres `~`, MySQL `REGEXP_LIKE(col, pattern)`
  (8.0.4 and later, so not MariaDB), and **omitted from the dropdown on
  SQLite and SQL Server**, which ship no regex (SQL Server not before 2025).
- **MySQL, SQLite and SQL Server bracket a generated multi-row batch** in
  `BEGIN`/`COMMIT` (`sql::update_batch`), because each commits every statement
  on its own where a Postgres `simple_query` submission is one implicit
  transaction. The brackets go in the statement text, never around it
  invisibly, and `sql::is_generated_write` refuses a transaction it cannot see
  closed. `Engine::transaction_start` answers which engine needs one, with an
  explicit arm per engine: never a `_ =>` catch-all, which is how MySQL once
  went unbracketed while this file claimed it was atomic. `BEGIN` rather than
  MySQL's own `START TRANSACTION` because the gate has to read the brackets back
  and the grammar knows only the first; MySQL takes it as an alias outside a
  stored program. SQL Server's is `BEGIN TRANSACTION`, because a bare `BEGIN`
  opens a statement block in T-SQL; the grammar reads that spelling and
  `generated_statements` sees through its second word.
- **A batch that fails part way is rolled back, and the error says which state
  the data is in.** Without that the brackets produce a third state, neither
  applied nor discarded, and rendered as applied, because the refresh `SELECT`
  runs on the same long-lived connection and reads the uncommitted rows back.
  Only a transaction _this_ submission opened is rolled back; one the user began
  in an earlier run is theirs to finish. SQLite asks `is_autocommit` before and
  after; MySQL cannot, because the driver keeps the server's
  `SERVER_STATUS_IN_TRANS` flag private, so it reads the submitted text instead.
  **SQL Server's session runs with `SET XACT_ABORT ON`**, set at connect like
  a timeout, because without it a constraint violation ends only its own
  statement and the batch carries on to the `COMMIT` dbdelve wrote. It reads
  the submitted text like MySQL, then asks `@@TRANCOUNT`. T-SQL has no nested
  transactions, so that rollback takes a transaction the user opened earlier
  with it, and the error says the transaction was rolled back.
- **A statement timeout is one number per profile, applied at connect**, and
  each engine buys something different with it. Postgres's `statement_timeout`
  bounds any statement; MySQL's `max_execution_time` bounds read-only `SELECT`s
  only, so a runaway `UPDATE` or `ALTER` there is Cancel's problem alone, and a
  server older than 5.7.8 (or MariaDB, which spells it differently) fails the
  connect rather than the statement; SQLite has no such setting and gets a
  wall-clock timer firing `sqlite3_interrupt`, which counts waiting on a lock
  the same as scanning; SQL Server has none either and gets a timer around the
  run, which stops the statement the way Cancel does. It goes in at connect and never into the user's
  submission (hard rule 1, and on Postgres a `SET` inside their submission
  would be scoped to the implicit transaction around it). It therefore bounds
  DBDelve's own catalog and structure queries too, which is intended.
- **Cancel reaches the running statement and nothing queued behind it.** The
  handle it needs (Postgres's `CancelToken`, MySQL's connection id, SQLite's
  `InterruptHandle`, a second handle on SQL Server's socket) is captured in
  each engine's `open`, before the client goes behind the connection mutex,
  because the statement being cancelled is holding that mutex.
  `Connection::cancel` takes `&self` and locks nothing but its own flags.
- **SQL Server's Cancel closes the connection.** tiberius cannot send TDS's
  attention signal, and `KILL` needs `ALTER ANY CONNECTION`, which an ordinary
  login lacks, so Cancel shuts the socket down and the server abandons the
  batch, rolling back what it had open (`live_a_cancel_stops_the_statement_on_the_server_and_reconnects`
  proves the statement behind a `WAITFOR` never runs). The session does not
  survive, so the run reconnects and its error says what was lost.
- **Explain** is `EXPLAIN` / `EXPLAIN ANALYZE` on Postgres and MySQL and
  `EXPLAIN QUERY PLAN` on SQLite, which has no analyze form, so that mode is not
  offered there (`Engine::explain_prefix` returns `None`). SQL Server offers
  neither: its plans come from `SET SHOWPLAN_XML`, a session switch that must be
  a batch of its own, which is not a prefix on a copy of the statement. Server versions are
  not detected: an older server refuses the statement and says so.
- **`SET column = DEFAULT` is withheld on SQLite** (`Engine::assigns_default`),
  where `DEFAULT` is not an expression.
- **Read-only has no server-side backstop on SQLite, Snowflake or SQL Server.**
  Postgres gets `default_transaction_read_only`, MySQL `SET SESSION TRANSACTION
  READ ONLY`. So on the other three, Read-only refuses a statement `sql::classify` cannot
  read rather than offering to run it once (`Engine::holds_read_only`).
- **Snowflake has no session, because it is spoken to over its SQL REST API.**
  It publishes no Rust driver. Each submission is one HTTPS request, so nothing
  set in one run reaches the next, an open transaction included. The API does
  not even pretend: a `USE` comes back as "Command not supported by SQL API:
  USE", which `live_a_use_is_refused_rather_than_quietly_forgotten` pins. The database, warehouse, role, timeout and `MULTI_STATEMENT_COUNT`
  are fields of the request and never SQL — hard rule 1.
- **Snowflake has no connection mutex**, alone among the five: there is no
  socket to serialise, so a catalog load does not queue behind a slow query,
  and `snowflake::Connection::at_once` runs a structure load's four statements
  on four threads rather than one after another (1.3s against 3.6s, measured).
  The consequence is that more than one statement can be in flight -- every
  tab's, and the catalog's -- so a run goes out under a `db::CancelToken` the
  tab keeps in `QueryState::Running`, and `cancel` stops only the handles
  registered under that token. A cancel that lands before the submit has
  returned a handle is kept on the token and carried out when the handle
  arrives. The other engines take the token and ignore it:
  they stop whatever their one connection is running.
  Statements are always submitted `async=true`, because a synchronous submit
  withholds its handle for up to 45 seconds and the handle is what Cancel needs.
- **Snowflake signs in with a key pair and nothing else.** An RS256 token per
  request, signed with `ring` from the key file the profile points at by
  absolute path; nothing goes to the Keychain, so `ConnectionConfig::server`
  answers `None` for it and nothing prompts on its behalf. An encrypted key is refused by name (`ring` does not
  decrypt PKCS#8). Password, OAuth, browser SSO and access tokens are not
  implemented. There is no `sslmode` to honour or weaken: the API is HTTPS and
  always verified against `webpki-roots`.
- **An engine's own statements are submitted synchronously; the user's are
  not.** The asynchronous submit exists so Cancel has a handle from the first
  moment, and it costs a round trip -- 885ms against 315ms for three
  `SELECT 1`s. Nothing offers to cancel a catalog load, so
  `snowflake::Connection::internal_query` pays for neither.
- **The relations and the routines are two loads, everywhere.**
  `Connection::catalog` answers with relations alone and
  `Connection::routines` follows behind it, merged in by `Catalog::merge` when
  it lands. It is engine-agnostic by design, and Snowflake is why it exists:
  `INFORMATION_SCHEMA.FUNCTIONS` and `PROCEDURES` took four and eight seconds
  to report that a database had neither, with the tables in hand after two. A
  routines load that fails leaves the relations on screen and says so once.
  `Catalog::merge` appends a routines-only schema rather than re-sorting, so a
  `schema_index` taken before the merge still names the same schema;
  `Catalog::by_name` is the display order. Until `Routines::Loaded`, a stored
  routine tab stays pending rather than being resolved against a catalog that
  cannot have it yet.
- **Snowflake's catalog needs a running warehouse.** It is read through
  `INFORMATION_SCHEMA`, so connecting resumes a suspended warehouse and so does
  opening a Structure tab. That view has nothing naming the columns of a key,
  so keys come from `SHOW PRIMARY KEYS`, `SHOW UNIQUE KEYS` and `SHOW IMPORTED
  KEYS`, asked `IN SCHEMA` and narrowed to the relation because `IN TABLE` is
  an error for a view. The schema there is written from the database down: a
  `SHOW` does not resolve against the request's `database` field the way a
  query does, and refuses a bare schema with "Must specify the full search
  path". `SnowflakeConfig::stored_database` folds a bare name to upper case
  for it, as the server does. A profile is bound to one database, as on Postgres, and
  a foreign key into another database is listed and not followable.
- **A Snowflake result is never editable.** Its primary keys are declared and
  not enforced, so a `WHERE` over one may name several rows, and the API says
  nothing about which table a result column came from. `QueryResult::edit` is
  always `None`. Inserting a row from an object tab needs no key and works as
  it does elsewhere.
- **Snowflake's temporal values arrive as counts from the epoch** whatever
  output format is asked for, and `snowflake::render` turns them into text
  before they leave `src/db/`. `TIMESTAMP_LTZ` is shown in UTC with a `Z`,
  because the API carries no session time zone to show it in.
- **Snowflake filters use `CONTAINS`, `STARTSWITH` and `ENDSWITH`**, for
  SQLite's reason: no default `LIKE` escape. Its regex is `REGEXP_COUNT(col,
  pattern) > 0` and not `REGEXP_LIKE`, which there anchors the pattern to the
  whole value where Postgres `~` and MySQL's `REGEXP_LIKE` match anywhere.
  Explain is not offered: its plan is a fourth shape `explain.rs` does not read.
- **`CHECK` constraints are absent** from the Structure tab on MySQL and SQLite.
  SQLite keeps them only in the `CREATE TABLE` text; MySQL's
  `information_schema.CHECK_CONSTRAINTS` only exists from 8.0.16.
- **MySQL verifies certificates against `webpki-roots`**, not the platform
  trust store the Postgres path reads. It fails loudly, which rule 7 permits.
- **SQL Server identifiers are double-quoted, not bracketed.** The grammar
  every gate parses with has no `[name]` and its pin does not move; the login
  tiberius sends turns `QUOTED_IDENTIFIER` on, so `"name"` is an identifier.
  Literals are `N'…'`, because a bare one is converted to the database's code
  page and a character it lacks is stored as `?`; `sql::equality_columns`
  accepts the prefix as a single-quoted literal.
- **SQL Server previews page with `OFFSET … FETCH`.** T-SQL has no `LIMIT` and
  the grammar has no `FETCH`, so a preview is generated, sorted and gated as
  `LIMIT n OFFSET m` and re-spelled by `sql::paged` afterwards (`ORDER BY (SELECT
  NULL)` when unsorted, since `OFFSET` requires one). It replaces only the
  `limit` node the parse tree locates, with the two integers read out of it;
  what runs, and what the tab shows, is the T-SQL. A user's own `TOP` or
  `[bracketed]` statement cannot be sorted from a header, for the grammar's
  reason.
- **SQL Server edit targets come from a describe**, as Postgres's do:
  `sys.dm_exec_describe_first_result_set` in mode 2 (a view is its own source,
  and has no key), after the statement ran, only when it returned exactly one
  result set, and only when the described names match the result's. A column
  from another database is not local, since an `EditTarget` names no
  database. Writes report no row count: tiberius keeps the done tokens to
  itself.
- **A SQL Server profile is held to its database.** Every name dbdelve writes
  stops at the schema, so after a `USE` a generated `DELETE` would find the same
  name elsewhere. A run whose text says `USE` is followed by `DB_NAME()`; if it
  moved, the session is moved back and the run fails saying so.
- **SQL Server's TLS is tiberius's.** `disable` is `EncryptionLevel::Off`,
  which still encrypts the login packet; the promising rungs are `Required`
  (never `On`, which panics the driver when the server offers less); `prefer`
  and `disable` alone retry with `NotSupported` for a server that cannot
  encrypt. `verify-ca` checks the hostname too, since tiberius cannot switch
  that alone off: stricter, which rule 7 permits. A named root certificate must
  be one certificate.
- **SQL Server values are rendered in `mssql::render`.** TDS is binary, so the
  server's own formats are rebuilt there: dates from day counts, `datetime`'s
  1/300 s ticks, `datetimeoffset` from its UTC instant plus offset, decimals from
  the unscaled integer. `money` arrives from tiberius as an `f64`, exact below
  about 9 × 10¹¹. Driver panics (unimplemented tokens, an empty trust store) are
  caught and reported rather than poisoning the mutex.
- **Geometry is Postgres-only.** MySQL has a `GEOMETRY` type; rendering it is a
  separate decision nobody has asked for.

### Session and tabs

The shape a change to the main pane has to fit (`session.rs`, with the
`Workspace` methods split across `src/workspace/`).

- **A profile owns a `Session`**, and a session owns two lists of tabs:
  `queries: Vec<QueryTab>` and `objects: Vec<ObjectTab>`. `Tab` is
  `Query(u64) | Object(u64)` and `active: Tab` says which is in front.
- **Both kinds are addressed by id, never by index.** A result comes back
  carrying the `Tab` it was issued for, and an id that no longer resolves drops
  the result rather than landing it somewhere. Indexing would put a slow query's
  rows into whatever tab had slid into that slot.
- **A `QueryTab` owns its own editor, grid, `QueryState`, saved-query name
  (`open_query`) and `last_query`.** Do not reintroduce a single shared editor
  for anything.
- **`queries` is never empty.** A profile always has somewhere to write, so the
  last unsaved buffer has no closed state: `session::close_target` returns
  `None` for it, and deleting the saved query in the only tab empties and
  unnames that tab rather than closing it.
- **A named buffer persists to its query file, an unnamed one to its own
  `.scratch-{id}.sql`.** Every buffer is written on quit, not just the visible
  one. `store::read_scratch(id, 0)` migrates the single `.scratch.sql` an older
  build left behind, and `StoredProfile::open_query` is still read for the same
  reason and never written.
- **Grids are snapshotted to disk and restored on relaunch.** The first edit on
  a restored grid asks first, because its rows may be stale.

### Completion

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

The provider is a snapshot, replaced whole when the catalog reloads
(`Workspace::install_completions`), and installed on every buffer rather than
the visible one.

### What gpui-component provides

Use these rather than hand-rolling: `EditorState` (the SQL buffer: rope-backed,
IME, line numbers, tree-sitter highlighting), `DataTable` over a `TableDelegate`
(the grid, virtualized on both axes), `ListState` (the palette's search field,
virtualized scroll and click-to-confirm), `PopupMenu` and the `DropdownMenu`
trait (an anchored menu whose open state the library owns, which is the answer
to the "GPUI drops view state" hazard below rather than a dropdown of ours),
`Root` dialog layers, resizable panels, `Spinner`, and the completion popup.

**Completion is the library's.** `CompletionProvider` is the trait and the
editor's `lsp.completion_provider` is a public field. No language server is
involved: `lsp_types` is the vocabulary and nothing starts a process. The editor
draws the menu itself, so a provider is the whole integration, and `up`, `down`,
`enter` and `escape` are already routed to the menu when it is open. Do not
hand-roll a popup beside it.

The library binds `escape` scoped to the input; DBDelve's `show_editor` binding
is unscoped, and an unscoped binding wins at every depth (see "GPUI hazards").
`show_editor` must therefore `cx.propagate()` on the paths where DBDelve has
nothing stacked to close, or the completion popup cannot be dismissed.

It does **not** provide a fuzzy matcher or a command palette. Those are ours:
`src/palette.rs` and `src/completion.rs` both score with `nucleo-matcher`. A
palette row carries a `Command`; `Workspace::run_command` routes every one into
the method its button or keystroke already calls, so the palette is never a
second implementation of anything.

It also does **not** ship the icons its `IconName` names: those are Lucide file
paths with no files behind them. `src/icons.rs` is DBDelve's `AssetSource`: it
serves the same paths from `icondata_lu` in memory, so nothing is vendored and
the library's own widgets get their icons from it too. Add a row to `ICONS`
when something needs one; an unlisted path draws nothing.

Its `Button` is worth using for the mechanism (tooltip, focus ring, disabled
gate) and nothing else. **Never construct one directly**: go through
`ui::button`, `ui::icon_button` and `ui::button_label`, which measure the box
off the layout scale (`CONTROL_HEIGHT`) and put the label and icon in as
children carrying their own colour, so the library's hover tint does not reach
them. GPUI's `.hover()` panics if called twice, so there is no fixing it from
outside.

The library owns scroll math, text shaping and virtualization. **Every visible
pixel is still ours**: `TableDelegate::render_td` is pull-based, and the
highlighter emits neutral token kinds that we map to our own palette. Never
accept a library default appearance; map it to our theme tokens.

---

## GPUI hazards

Hard-won and easy to rediscover. Read before writing any animated element.

- **A repeating `with_animation` element requests a redraw every display frame
  while mounted.** One spinner has been measured pinning a window at 120Hz and
  36% CPU. The remedy is a single shared throttled clock with per-view leases,
  reaping stale leases and parking when the list empties. **DBDelve does not have
  one**; see "Animation" below before adding anything that moves.
- **`with_animation` replays from zero on remount.** Anything that must survive
  being unmounted mid-animation needs a wall-clock-driven tween evaluated fresh
  each render, not an element-id-keyed animation.
- **GPUI drops view state the same frame the view unmounts.** Exit animations
  need an explicit open → closing → closed lifecycle plus a reaping timer.
  Every dropdown, toast and modal hits this.
- **No scale transform on `div`.** SVG only at this revision. Approximate with
  fade plus translate.
- **`translateY` is a relative-position inset** applied after layout, so siblings
  do not shift.
- **`.hover()` snaps with no transition**, so hover states are instant
  everywhere. A colour fade would have to be hand-driven from a wall clock.
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

### Animation

There is no motion system of DBDelve's own: no transitions, no easing curves,
and hover states are instant. Two things move:

- **The query-in-flight `Spinner`** (gpui-component's, built in `views.rs`),
  shown while a query runs beside a live **Cancel** button. It is
  `Animation::new(speed).repeat()` internally, which makes it exactly the first
  hazard above, and **the throttled-clock remedy is not in place**: one spinner
  per running tab, mounted while `QueryState::Running` and dropped when the
  result lands. Tolerable because it is transient and few tabs run at once, but
  an unmeasured ceiling, not a solved problem. One case is not transient: a
  preview tab in `QueryState::Idle` shows the same spinner, so a preview that
  never runs spins forever.
- **The row panel's "copied" check** fades in once over 150ms. It does not
  repeat, so it is not the redraw hazard.

Anything else that moves needs raising first, and a second repeating animation
means building the shared throttled clock and moving both onto it.

---

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

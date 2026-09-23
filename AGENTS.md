# AGENTS.md

DBDelve is a native database client in Rust on GPUI for macOS and Linux,
speaking Postgres, MySQL, SQLite and Snowflake. A data browser and a SQL editor
as equals: open a table and browse it — page, sort, filter, edit — or write the
statement yourself.

This replaces the earlier "SQL editor that shows results, not a database
browser" framing, and supersedes the spec wherever the spec leans on it:
browsing surfaces are first-class, not an editor accessory. The split that
survives the change is between *whose SQL it is*. An editor buffer is the
user's and is never touched uninvited; a browsing surface (an object tab's
preview) runs SQL DBDelve generates, regenerated from visible controls and
inspectable, never spliced into anyone's buffer.

**Read this file before doing anything.** It is the source of truth for how
DBDelve is built and why.

The design documents behind it live in `docs/specs/` and are deliberately not
version-controlled — the reasoning and the rejected alternatives are working
notes, not something to publish. Read them if they are on your disk; the
`spec §` references in the source comments point into them. A clone will not
have them, and nothing in the repository should come to depend on them.

`HANDOFF.md` at the root points at the private operational notes — the current
handoff, the worklog and the release and feature audits — for the same reason
and under the same caveat.

---

## Hard rules

Violating one of these is a bug regardless of the benefit. If a task seems to
require it, stop and raise it instead.

1. **Never rewrite SQL behind the user's back.** No silent `LIMIT` injection, no
   column projection, no reformatting on execute, and nothing at all on a
   statement the user did not ask DBDelve to change. Row limits apply to
   DBDelve-generated preview queries only, and they are visible in the UI.

   DBDelve _does_ write SQL when the user asks it to, and only then. A header
   click asking for a sort is such an ask: the `ORDER BY` is spliced into the
   statement in the buffer, where the user can read it, edit it and undo it,
   and the statement that runs is the statement on screen. The same will hold
   for in-place row editing.

   Two limits on what DBDelve may write. It never writes `DROP` or `TRUNCATE`,
   whatever the user asked for; and `DELETE` only as the explicit deletion of
   named rows — by primary key, from a direct ask on a browsing surface, with
   the statement shown before it runs. (That deletion flow is `sql::delete_row`:
   one row per statement, its `WHERE` the row's whole primary key, with the gate
   reading that shape back out of the parse tree rather than trusting the
   generator.) And it never
   writes into a statement it cannot parse whole:
   `sql::with_order_by` refuses rather than guessing at a clause boundary,
   because a corrupted statement is worse than an unsorted grid.

   This replaces the earlier absolute rule, and supersedes invariant 1 of the
   spec's §5 on this point. The reasoning there — that a client which silently
   alters statements cannot be trusted with the statements that matter — is
   why "silently" is still the word that carries the rule.

2. **One gate stands between the grid and the server.** The grid can write an
   `UPDATE`, an `INSERT` of one row, and a `DELETE` of one row, and
   `sql::is_generated_write` is the single gate every generated statement passes
   first. All three admitted shapes are named here rather than left to be read
   into a rule about something else:

   - a batch of `UPDATE`s, optionally bracketed by a `BEGIN`/`COMMIT` the gate
     can see closed;
   - one `INSERT`, naming the columns it fills;
   - one `DELETE` whose `WHERE` is a conjunction of equality predicates over
     distinct, unqualified columns against single-quoted literals — no `OR`, no
     other operator, no subquery, no function call, no CTE beside it, no
     `RETURNING`, no `LIMIT`, and nothing else in the submission.

   It is a whitelist, so `DROP` and `TRUNCATE` are refused structurally rather
   than by name, anywhere in the tree, CTEs included — `sql::forbidden` — and so
   is every `delete` outside that one shape: `sql::deletes_anything` refuses one
   on the `INSERT` and `UPDATE` arms and in `sql::is_generated_select`, the
   filter bar's gate, which admits none at all. Do not add a second path that
   bypasses either.

   **The `DELETE`'s shape is verified from the parse tree, not trusted because
   `sql::delete_row` produced it.** A gate that trusts its caller is a comment,
   and the check lives in the gate rather than in the generator precisely so the
   two can disagree — the day they do is the day the gate earns its keep.

   `sql::delete_matches_key` answers the other half, the half the gate cannot:
   whether the columns the `WHERE` names are exactly the row's key, as a set.
   It is a readout and not the second gate this rule forbids — it admits
   nothing, has no say over what runs, and a caller runs both.

   **Multi-row deletion is not admitted**: one row per statement. The upgrade
   path, when it is wanted, is the `BEGIN`/`COMMIT` bracketing multi-row edits
   already use — one `DELETE` per row, each naming its own key, never one
   statement with a predicate covering several.

   A cell is editable only when DBDelve can name its row by primary key. An
   `INSERT` is the one write that needs no key — it has no existing row to name
   yet — so a table without a primary key can be inserted into and not edited.
   That asymmetry is deliberate and belongs in anything that documents either
   feature.

   When DBDelve cannot name a row, the grid stays read-only and says why; it
   never guesses at a predicate.
3. **No environment-specific behaviour.** No vendor binary names in error
   strings, no assumption that a loopback host means plaintext, no hardcoded
   ports or hostnames. DBDelve is a generic client.
4. **Driver types do not reach the UI layer.** The grid receives rendered
   strings and type tags, never a `postgres::Row`, a `mysql::Value`, a
   `rusqlite::ValueRef`, an OID, a storage class or an epoch count off
   Snowflake's wire. Engine dispatch is a closed enum inside `src/db/`
   and stops there: no trait, no plugin surface, and no code above `src/db/`
   that branches on which engine is connected.

   This replaces the earlier wording, which said the rendered-string rule was
   "the only concession to a future second engine — do not add a driver trait".
   That was written when the second engine was hypothetical. The reasoning is
   unchanged, and is why the rule survives at all: a UI that knows which engine
   it is talking to grows an engine-shaped special case in every view, and
   those are the special cases nobody ever removes. An enum rather than a trait
   for the same reason in miniature — four arms the compiler makes every match
   enumerate, instead of an open extension point.

   The one thing that legitimately crosses out is `db::Engine`, and only
   because DBDelve writes SQL: `explorer::preview_sql` and `sql::update_row` have
   to quote an identifier the way the server will read it. It answers three
   questions and holds no connection.
5. **Blank passwords are valid.** Never warn about them. Usernames containing `@`
   must work. Both are required by cloud IAM auth and both are commonly broken.
6. **Errors describe what happened, not what to do about it.** "Connection
   refused: nothing is listening on `host:port`" and stop. No speculation about
   the user's machine, no process-list inspection.
7. **An `sslmode` is never quietly weakened.** A connection either gets what it
   asked for or fails saying which certificate check failed. This is a rule
   because the failure is silent by construction: the driver's default is
   `prefer`, and `prefer` with a connector that cannot do TLS hands back a
   plaintext socket without even sending an SSLRequest — a cleartext password
   under a UI reporting success. That was the bug for as long as `sslmode` was
   dropped on the way in. If a mode cannot be honoured, refuse it by name;
   `tls::SslMode::parse` does that for `allow`, which libpq defines in an order
   the driver cannot express.

---

## Stack

Every direct dependency, because a list that omits some is a list nobody
trusts. `Cargo.toml` carries the full reasoning; this is the shape of it.

```toml
gpui = { package = "gpui-pre", version = "=0.3.5" }
gpui-component = { version = "=0.6.4", features = ["tree-sitter-sql"] }

tree-sitter = "=0.25.10"        # statement boundaries; the library keeps its tree private
tree-sitter-sequel = "=0.3.11"  # the SQL grammar. A CORRECTNESS pin -- see below

postgres = "0.19"          # blocking client, NOT tokio-postgres
mysql = "28"               # rust-mysql-simple, blocking. default-features = false
rusqlite = "0.40"          # bundled + column_metadata + column_decltype. dff = false

rustls = "0.23"            # TLS; the driver ships none. default-features = false
rustls-native-certs = "0.8"     # the Keychain, for Postgres verify-full
rustls-pemfile = "2"            # a named root certificate, and Snowflake's key file
tokio-postgres-rustls = "0.14"
security-framework = "3"        # the Keychain, for passwords

ureq = "=3.4.2"            # Snowflake's SQL REST API; blocking. dff = false, rustls on ring
ring = "=0.17.14"          # its key-pair tokens. Already linked as the TLS provider
base64 = "=0.22.1"

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
```

**The two tree-sitter pins are correctness, not formatting.** The grammar
decides where every statement boundary falls, which statements `sql.rs` will
splice an `ORDER BY` into, and what `is_generated_write` accepts as a closed
transaction. A bump changes what DBDelve sends to the server. Treat them like the
driver pins.

**Build profiles are deliberate.** `[profile.dev.package."*"] opt-level = 3`
builds dependencies optimized so a debug DBDelve is usable on real data; deleting
it makes the grid crawl. `[profile.release]` sets `lto = "thin"` and
`codegen-units = 1`.

**`default-features = false` on `rustls` is load-bearing.** Its defaults select
the `aws-lc-rs` provider; `ring` is what is already linked through gpui. Every
crate in the graph has to agree on one or both get built, and `aws-lc-rs` builds
C and assembly. `rustls`, `rustls-native-certs` and `rustls-pemfile` were all
already transitive dependencies, which is why TLS is `rustls` and not
`native-tls` — see the module header in `src/tls.rs` for the rest of that
reasoning.

**Pins are exact and the lockfile is committed. Do not bump without being asked.**
gpui is pre-1.0 and breaks on minor bumps; `main` has declared `0.2.2` for ten
months, which is a stalled version field rather than parity with the release.

**`default-features = false` on `mysql` is load-bearing too**, with
`minimal-rust` among its features: that takes flate2's pure-Rust backend over
zlib, the same "no C for something already solved in the graph" rule as the
provider choice above. `rustls-tls-ring` rather than `rustls-tls` for exactly
the `aws-lc-rs` reason.

**`default-features = false` on `rusqlite` is load-bearing too.** 0.40's
defaults pull in `ffi-sqlite-wasm-rs`, a WASM backend with no business in a
native build. `bundled` compiles the amalgamation rather than linking whatever
libsqlite3 macOS shipped; `column_metadata` is what makes in-grid editing
reachable, since it is the only way to learn that a result column is
`accounts.id` and not an expression.

**`default-features = false` on `ureq` states what its defaults happen to be.**
`rustls` there is rustls *on ring* with `webpki-roots`, both already in the
graph through `mysql`. Named rather than inherited so a release that changes
its defaults cannot bring `aws-lc-rs` in. `gzip` is not optional: every result
partition after the first arrives compressed. Snowflake publishes no Rust
driver and the community ones are tokio futures, which is why this is an HTTP
client and not a driver.

**Do not add tokio.** GPUI's executor is `async-task` over Grand Central
Dispatch. A tokio future on `cx.background_executor().spawn(...)` _panics_ the
moment it touches a socket or timer. Database work uses blocking drivers, which
own their runtimes internally, spawned onto the background executor. `rusqlite`
is blocking by construction and has no runtime at all.

`tokio-rustls` in the tree is not a breach of that rule, and the rule is why:
the TLS handshake is a future belonging to the connection, so it runs inside the
runtime the blocking client already owns, on the same thread as the connect it
is part of. Nothing tokio-shaped reaches GPUI's executor. Adding a tokio future
anywhere DBDelve spawns one still panics.

**Do not fork gpui.** Decided in the spec, §7.1.

### Local build and run

```sh
cargo build
docker compose up -d
cargo run
```

The app opens the connection form when no `PG*` environment is configured. The
repository-owned development databases accept:

```text
postgresql://dbdelve:dbdelve@127.0.0.1:55432/dbdelve_dev
mysql://dbdelve:dbdelve@127.0.0.1:53306/dbdelve_dev
```

Pick the engine on the form's chip row first — it decides which fields exist.
Then paste a URL and choose **Use URL**, or fill the fields in. Connecting is
the connection test; there is deliberately no separate test button.

Snowflake has no container. Its unit tests need nothing; its live tests are
`#[ignore]`d and read `DBDELVE_SNOWFLAKE_ACCOUNT`, `_USER`, `_PRIVATE_KEY` (an
absolute path) and `_DATABASE`, plus `_WAREHOUSE`, `_ROLE` and `_HOST` when set. The
catalog test creates and drops a `DBDELVE_TEST` schema.

SQLite has no server to connect to. Build the file once, then give the form its
absolute path:

```sh
sqlite3 dev/dbdelve_dev.db < dev/sqlite/001-dbdelve-demo.sql
```

**The MySQL container reports itself healthy when its init script failed.**
`mysqladmin ping` does not care whether the seed applied, so a half-seeded
database looks exactly like a good one. Check a row count, not the status —
`live_the_development_database_is_fully_seeded` is that check.

### Bundling

`dev/bundle.sh` builds `--release`, generates the icon, writes `Info.plist`
and signs. It is the only way to get a real app rather than a binary. It
builds **two variants from one script**, and neither of them installs:

- **Dev, the default.** `target/macos-dev/DBDelve Dev.app`, named
  `DBDelve Dev`, id `com.shayanabbas.dbdelve.dev`. Run from the build tree,
  never copied to `/Applications`, and launched with `open` so LaunchServices
  owns it. The script refuses to build while a dev instance is running —
  overwriting a running executable is what breaks it — but says nothing about
  the released app, which is meant to stay open alongside.
- **Release, under `DBDELVE_CHANNEL=release`.** `target/DBDelve.app`, named
  `DBDelve`, id `com.shayanabbas.dbdelve`, no `LSEnvironment`. Only
  `dev/release.sh` sets it, and the DMG's drag-to-Applications is the install.

**The two variants share nothing on disk.** `DBDELVE_VARIANT` moves both the
support directory and the Keychain service together — unset or empty is
`dbdelve` (the release's, unchanged and never to move), `dev` is
`dbdelve-dev`. Both, always: a build that suffixed only one would read the
release's saved passwords or write its `profiles.toml`. `store::variant_name`
is the single place it is decided, it validates the variant with the same
`unsafe_component` every path component goes through, and a variant it rejects
is an error rather than a fall back to the release name — silently falling
back is exactly how a dev build corrupts the real one. So a dev build has its
own profiles and its own Keychain items, and asks for its own passwords.

The dev variant carries `DBDELVE_VARIANT=dev` in an `LSEnvironment` dict in
its own `Info.plist` rather than exported by the script, because the isolation
has to survive a launch from the Dock, from Finder, or from a crash reporter's
"Quit & Reopen" — none of which see the shell that built the app.

Four things in the script are load-bearing:

- **`CFBundleIdentifier` scopes the Keychain.** Every saved profile password
  belongs to `com.shayanabbas.dbdelve`. Changing it orphans all of them. The
  dev id differs precisely so the two cannot reach each other's prompts.
- **The signature is not optional on arm64.** An unsigned arm64 binary will not
  launch, and copying the binary into the bundle invalidates the signature
  rustc left. `DBDELVE_SIGN_ID` takes a real identity; `dev/identity.sh`'s
  self-signed one is what stops the Keychain re-prompting after every rebuild;
  ad-hoc is the fallback and runs, but prompts.
- **`codesign --identifier` is passed explicitly.** The Keychain pins its
  "Always Allow" to the signature's identifier, so the one value that must not
  drift between builds is stated rather than inferred.
- **The font licences ship inside the bundle**, because the fonts are compiled
  into the binary and the OFL asks the licence to travel with them.

**There is still no notarization and no Developer ID**, but the app is no
longer stuck on this machine. `dev/release.sh` builds with `dev/bundle.sh`
(`DBDELVE_CHANNEL=release DBDELVE_SIGN_ID=-`), wraps `target/DBDelve.app` into `target/DBDelve-$VERSION.dmg` with an
`/Applications` symlink alongside it, publishes the DMG with
`gh release create`, and rewrites `Casks/dbdelve.rb` in the
`ShayanAbbas1/homebrew-dbdelve` tap (found at `../homebrew-dbdelve`, override with
`DBDELVE_TAP`) to point at it. A release still signs ad-hoc rather than with
`dev/identity.sh`'s certificate: that certificate is trusted only on this
machine, and to Gatekeeper an issuer nobody trusts reads worse than no issuer
at all. The trade is that the ad-hoc hash moves with every release, so an
update costs one fresh Keychain prompt. Installing still means clearing
quarantine by hand — `xattr -dr com.apple.quarantine /Applications/DBDelve.app`
— since nothing here is notarized.

### Engine divergences

Decided, recorded in the multi-engine spec, and not to be re-litigated:

- **`Engine` is the only engine-shaped thing above `src/db/`**, and only because
  DBDelve writes SQL. It answers three questions — quote an identifier, quote a
  literal, qualify a name — plus the inverse used to read a sort key back.
  There are **seven** call sites that generate SQL:
  `explorer::preview_sql`, `sql::with_order_by`, `sql::update_row`,
  `main::sort_expression`, `main::filter_predicate`, `sql::insert_row`, and
  `sql::delete_row`. `main::sort_expression` is
  the one that gets forgotten, and forgetting it is silent: a double-quoted name
  is a *string literal* in MySQL, so `ORDER BY "name"` sorts every row by the
  same constant with no error. `main::filter_predicate` quotes a *value the user
  supplied* rather than only an identifier — a filter bar's value, whether the
  user typed it or following a foreign key put it there — which is the other
  half of the same hazard. It was eight until the filter bars landed:
  `main::foreign_key_filter` now yields the bar's column and value rather than a
  `WHERE`, and `main::derived_filter` folds the bars through
  `filter_predicate` rather than quoting anything itself. Every filter operator
  lives inside that one function, `main::substring` and `main::like_pattern`
  included.
- **Three filter operators are written differently per engine, and two of those
  are decided by the grammar rather than by any server.**
  `sql::is_generated_select` refuses whatever `tree_sitter_sequel` cannot parse
  whole, and that pin does not move — so a predicate the grammar does not know
  is one DBDelve cannot run, however valid the server would find it. It has **no
  `ESCAPE` clause and no infix `REGEXP`**. So the pattern operators lean on the
  engine's default `LIKE` escape, which is the backslash on Postgres and MySQL,
  and `like_pattern` escapes `%`, `_` and the backslash itself with it;
  **SQLite, which has no default escape at all**, gets `instr`/`substr`
  substring arithmetic instead — case-sensitive where its own `LIKE` is not,
  which is the price of not silently widening a match on a value containing
  `%`. The regex match is Postgres `~`, MySQL `REGEXP_LIKE(col, pattern)`
  (8.0.4 and later, so not MariaDB), and **omitted from the dropdown on
  SQLite**, which ships no `REGEXP` at all.
- **MySQL and SQLite both bracket a generated multi-row batch** in
  `BEGIN`/`COMMIT`, because each commits every statement on its own where a
  Postgres `simple_query` submission is one implicit transaction. The brackets
  go in the statement text, never around it invisibly, and
  `sql::is_generated_write` refuses a transaction it cannot see closed.
  `Engine::transaction_start` answers which engine needs one, with an arm per
  engine; it was a `_ =>` catch-all at `main::update_batch` until 2026-09-08,
  which is how MySQL went unbracketed while this file claimed it was atomic.
  `BEGIN` rather than MySQL's own `START TRANSACTION` because the gate has to
  read the brackets back and the tree-sitter grammar knows only the first —
  MySQL takes it as an alias outside a stored program.
- **A batch that fails part way is rolled back, and the error says which state
  the data is in.** Without that the brackets produce a third state — neither
  applied nor discarded, and rendered as applied, because the refresh `SELECT`
  runs on the same long-lived connection and reads the uncommitted rows back.
  Only a transaction *this* submission opened is rolled back; one the user began
  in an earlier run is theirs to finish. SQLite asks `is_autocommit` before and
  after; MySQL cannot, because the driver keeps the server's
  `SERVER_STATUS_IN_TRANS` flag private, so it reads the submitted text instead.
- **A statement timeout is one number per profile, applied at connect**, and
  each engine buys something different with it. Postgres's `statement_timeout`
  bounds any statement; MySQL's `max_execution_time` bounds read-only `SELECT`s
  only, so a runaway `UPDATE` or `ALTER` there is Cancel's problem alone, and a
  server older than 5.7.8 (or MariaDB, which spells it differently) fails the
  connect rather than the statement; SQLite has no such setting and gets a
  wall-clock timer firing `sqlite3_interrupt`, which counts waiting on a lock
  the same as scanning. It goes in at connect and never into the user's
  submission — hard rule 1, and on Postgres a `SET` inside their submission
  would be scoped to the implicit transaction around it. It therefore bounds
  DBDelve's own catalog and structure queries too, which is intended.
- **Cancel reaches the running statement and nothing queued behind it.** The
  handle it needs — Postgres's `CancelToken`, MySQL's connection id, SQLite's
  `InterruptHandle` — is captured in each engine's `open`, before the client
  goes behind the connection mutex, because the statement being cancelled is
  holding that mutex. `Connection::cancel` takes `&self` and locks nothing.
- **Snowflake has no session, because it is spoken to over its SQL REST API.**
  It publishes no Rust driver. Each submission is one HTTPS request, so nothing
  set in one run reaches the next, an open transaction included. The API does
  not even pretend: a `USE` comes back as "Command not supported by SQL API:
  USE", which `live_a_use_is_refused_rather_than_quietly_forgotten` pins. The database, warehouse, role, timeout and `MULTI_STATEMENT_COUNT`
  are fields of the request and never SQL — hard rule 1.
- **Snowflake has no connection mutex**, alone among the four: there is no
  socket to serialise, so a catalog load does not queue behind a slow query,
  and `snowflake::Connection::at_once` runs a structure load's four statements
  on four threads rather than one after another (1.3s against 3.6s, measured).
  The consequence is that more than one statement can be in flight, so its
  `cancel` stops every handle the connection has running rather than one.
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
- **MySQL verifies certificates against `webpki-roots`**, not the Keychain the
  Postgres path reads. It fails loudly, which rule 7 permits.
- **Geometry is Postgres-only.** MySQL has a `GEOMETRY` type; rendering it is a
  separate decision nobody has asked for.

### Session and tabs

The shape a change to the main pane has to fit, and the one thing in `main.rs`
that is worth knowing before reading it.

- **A profile owns a `Session`**, and a session owns two lists of tabs:
  `queries: Vec<QueryTab>` and `objects: Vec<ObjectTab>`. `Tab` is
  `Query(u64) | Object(u64)` and `active: Tab` says which is in front.
- **Both kinds are addressed by id, never by index.** A result comes back
  carrying the `Tab` it was issued for, and an id that no longer resolves drops
  the result rather than landing it somewhere. Indexing would put a slow query's
  rows into whatever tab had slid into that slot.
- **A `QueryTab` owns its own editor, grid, `QueryState`, name and
  `last_query`.** There used to be exactly one editor per profile, which is why
  `cmd+t` on a dirty scratch buffer persisted it and then cleared it — there was
  nowhere else for a second buffer to be. Do not reintroduce a single shared
  editor for anything.
- **`queries` is never empty.** A profile always has somewhere to write, so the
  last unsaved buffer has no closed state: `close_target` returns `None` for it,
  and deleting the saved query in the only tab empties and unnames that tab
  rather than closing it.
- **A named buffer persists to its query file, an unnamed one to its own
  `.scratch-{id}.sql`.** Every buffer is written on quit, not just the visible
  one. `store::read_scratch(id, 0)` migrates the single `.scratch.sql` an older
  build left behind, and `StoredProfile::open_query` is still read for the same
  reason and never written.

### Completion

`src/completion.rs` offers the schemas, relations and routines the loaded
catalog holds, the columns of the relations a statement actually names, and the
keywords that carry a statement's shape. It is a `CompletionProvider`
implementation and nothing else: the popup, its scroll and its keys all belong
to gpui-component (see below).

**Columns are not in the catalog, and must not be put there.** The first
implementation fetched every column of every relation at connect, and that is
unbounded: on a large schema it is a multi-million-row result the driver
buffers whole before DBDelve sees a row, held for the life of the connection and
duplicated into the provider's snapshot — all of it paid before anyone has
asked a question. A relation's columns are fetched when a statement first names
it, through `Connection::structure`, which the Structure tab already runs; so
completion adds no SQL of its own to any engine, and a session holds only what
it wrote about. `Session::completion_columns` is the cache, cleared whenever
the catalog reloads, and a miss is marked `Loading` before the request so a
relation is asked about once rather than once per keystroke. A name the catalog
does not list is never fetched at all — otherwise a typo puts a describe on the
wire for as long as it is on screen.

Two things about that cache are load-bearing and easy to undo by accident.
**`load_structure` fills it too**, because opening a relation's Structure tab
makes exactly the call completion would make; dropping that line costs a
duplicate round trip per relation the user both opened and wrote about. And **a
failed fetch is retried, but only `FETCH_ATTEMPTS` times.** Neither extreme
works: never retrying lets one blip — or one statement timeout, which bounds
DBDelve's own catalog queries too — kill completion for a relation silently for
the rest of the connection, and always retrying puts a describe on the wire per
keystroke, each queued behind the last on the connection mutex, which freezes
the profile rather than degrading it.

Two rules it exists under. **It is a lexer, not a parser** — half-typed SQL is a
parse error by definition, and `SELECT * FROM ` is both the text a user most
wants completed and the text the grammar returns an `ERROR` node for, so
context comes from scanning tokens. And **it must offer nothing inside a string
literal or a comment**, which is the one thing a token scan cannot do by itself
and the one place accepting a row rewrites data rather than a query.

The provider is a snapshot, replaced whole when the catalog reloads
(`Workspace::install_completions`), and installed on every buffer rather than
the visible one.

### What gpui-component provides

Use these rather than hand-rolling: `InputState::new(window, cx).code_editor("sql")`
(rope-backed multi-line editor, IME, line numbers — the `InputMode` enum behind
it is `pub(crate)` in the library and cannot be named from here),
`src/highlighter/` (tree-sitter; SQL via
`tree_sitter_sequel`), `src/table/` (grid virtualized on both axes),
`src/dock/` (panels, tab bars), `Root` dialog layers (modal overlays),
`src/menu/` (`PopupMenu` and the `DropdownMenu` trait it implements for
`Button` -- an anchored menu whose open state the library owns, which is the
answer to the "GPUI drops view state the same frame the view unmounts" hazard
below rather than a dropdown of ours; the filter bar's column picker is the one
caller), and
`src/input/lsp/` plus `src/input/popovers/` (the completion provider trait and
the caret-anchored popup it drives).

**It does ship completion infrastructure, and this file said otherwise until
2026-09-09.** `input/lsp/completions.rs` defines `CompletionProvider`, two
required methods, and `InputState::lsp.completion_provider` is a public field.
No language server is involved -- `lsp_types` is the vocabulary and nothing
starts a process. `Render for InputState` draws the menu itself, so a provider
is the whole integration: nothing to render, nothing to anchor, and `up`,
`down`, `enter` and `escape` are already routed to the menu when it is open and
to the cursor when it is not.

Do not hand-roll a popup beside it. The caret's pixel position it would need --
`LastLayout::cursor_bounds`, `InputState::last_layout`,
`line_and_position_for_offset` -- is `pub(super)` with no accessor, so an
anchored overlay of our own is fork-only, and forking is out (spec §7.1).

One consequence worth knowing before writing an `escape` handler: the library
binds `escape` scoped to `Input`, DBDelve binds it unscoped, and an unscoped
binding ties at every depth and wins on registration order. `show_editor` must
therefore `cx.propagate()` on the paths where DBDelve has nothing stacked to
close, or the completion popup cannot be dismissed.

It does **not** provide a fuzzy matcher or a command palette. Those are ours —
`src/palette.rs` and `src/completion.rs` both score with `nucleo-matcher`, and
the palette presents through the
library's `ListState`, which owns the search field, the virtualized scroll and
the click-to-confirm. A palette row carries a `Command`; `Workspace::run_command`
routes every one of them into the method its button or keystroke already calls,
so the palette is never a second implementation of anything.

It also does **not** ship the icons its `IconName` names: those are Lucide file
paths with no files behind them. `src/icons.rs` is DBDelve's `AssetSource` — it
serves the same paths from `icondata_lu` in memory, so nothing is vendored into
the repository and the library's own widgets get their icons from it too. Add a
row to `ICONS` when something needs one; an unlisted path draws nothing.

Its `Button` is worth using for the mechanism and nothing else. **Never
construct one directly** — go through `button`, `icon_button` and `button_label`
in `main.rs`, which measure the box off `CONTROL_HEIGHT*` and put the label and
the icon in as children carrying their own colour. That last part is not style:
0.5.1 tints button content `red_400` on hover from a hardcoded colour, and a
child that sets a colour is the only thing that colour does not reach. GPUI's
`.hover()` panics if called twice, so there is no fixing it from outside.

The library owns scroll math, text shaping and virtualization. **Every visible
pixel is still ours** — `TableDelegate::render_td(row_ix, col_ix)` is pull-based,
and the highlighter emits neutral token kinds that we map to our own palette.
Never accept a library default appearance; map it to our theme tokens.

---

## GPUI hazards

Hard-won and easy to rediscover. Read before writing any animated element.

- **A repeating `with_animation` element requests a redraw every display frame
  while mounted.** One spinner has been measured pinning a window at 120Hz and
  36% CPU. The remedy is a single shared throttled clock with per-view leases,
  reaping stale leases and parking when the list empties. **DBDelve does not have
  one**, and the shipped query spinner is subject to this — see "Animation" at
  the end of this file before adding a second animated element.
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
- **`.hover()` snaps with no transition.** There is no fade: `motion.rs` was
  planned and never landed, and hover states are instant everywhere by
  consequence. A colour fade would have to be hand-driven from a wall clock.

Two more that are not about animation, and cost a round each to find:

- **A window with nothing focused has no dispatch path, so every keybinding is
  dead.** A keystroke reaches a handler only along the focused element's path to
  the root. Unmount whatever had focus — close a modal, switch to a surface with
  no focusable element — and the app stops responding to the keyboard entirely
  until something is clicked. Anything that takes focus away must hand it back;
  `Workspace::close_palette` is the worked example, and `Focus::Window` is the
  floor under it for surfaces that have nothing to type into.
- **A binding with no context predicate wins over a scoped one.**
  `Keymap::binding_enabled` scores an unscoped binding at `contexts.len()` —
  the maximum — while `Some("Foo")` scores at the depth of that node, and the
  deepest match takes the keystroke. So a library binding scoped to an inner
  element beats the container's, which is why the palette's arrows are bound
  against `Palette > Input`: a descendant predicate matches at the leaf, which
  is the only depth that takes them back from gpui-component's input. Ties are
  broken by registration order, and DBDelve's `cx.bind_keys` runs after
  `gpui_component::init`.

---

## Conventions

- **Comments explain why, never what.** Code should carry its own meaning. Add a
  comment when the code cannot convey a decision that is non-obvious from
  reading it.
- **No speculative abstraction.** No trait with one implementation, no factory
  for one product, no config for a value that never changes. The spec's Non-goals
  section is binding — do not build ahead of it.
- **Deletion over addition.** The shortest change that fully solves the problem
  wins, once the problem is actually understood.
- A `ponytail:` comment marks a deliberate simplification and names its ceiling
  and upgrade path. Leave one where a shortcut is a decision, not an oversight.
- Non-trivial logic leaves one runnable check behind — the smallest test that
  fails if the logic breaks. No fixture scaffolding.
- Match surrounding code's naming, density and idiom.

---

## Animation: one spinner, and it is the hazard

**This section said "no animation in v1" until 2026-09-09, and that had been
wrong since `40f3c11`.** Read it before adding anything else that moves.

There is still no motion system of DBDelve's own: no transitions, no easing
curves, no `with_animation` anywhere in `src/`, and hover states are instant.
Spec §7.4 is otherwise intact.

The exception is the query-in-flight indicator. `src/views.rs:256` builds
gpui-component's `Spinner`, shown while a query runs and beside a live **Cancel**
button — not the static text and disabled control this section used to promise.
`Spinner` is `Animation::new(speed).repeat()` internally, which makes it exactly
the first hazard in the list above: a repeating animation requests a redraw
every display frame while it is mounted.

**The throttled-clock requirement in that hazard is not met.** There is no
shared clock and no leases; there is one library spinner per running tab,
mounted while `QueryState::Running` and dropped when the result lands. That is
tolerable because it is transient and because at most a handful of tabs can be
running at once — but it is an unmeasured ceiling, not a solved problem, and the
120Hz/36% measurement behind that hazard was taken on a spinner just like this
one. One case is already not transient: a preview tab sitting in
`QueryState::Idle` shows the same spinner (`views.rs:276`), so a preview that
never runs spins forever.

Anything beyond this one indicator still needs raising first, and if a second
animated element ever ships, the shared throttled clock is what both should be
moved onto.

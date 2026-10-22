# Contributing to DBDelve

Thanks for looking at this. Here's how to get set up and what to check before sending a PR.

## Before a big change

Small fixes, obvious bugs, docs — just open a PR. For anything bigger (a new
database engine, a large feature, a change to how connections, editing or
completion work), open an issue first so we can agree on the approach before
you put the work in. `AGENTS.md` at the repository root has hard rules on
several of these (SQL rewriting, the write gate, engine dispatch); read it
before proposing a change that touches them.

## Dev setup

You need the Rust toolchain (stable; the crate is on edition 2024) and,
on Linux, the same build dependencies CI installs:

```sh
sudo apt install build-essential pkg-config cmake libfontconfig-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libdbus-1-dev
```

On Windows you need the MSVC toolchain and the Windows SDK (they compile the
bundled SQLite and GPUI's shaders). On macOS, Apple Silicon and macOS 12 or
later is what's built and tested; nothing extra is required beyond Xcode's
command line tools.

Build and run:

```sh
cargo build
docker compose up -d
cargo run
```

`docker compose up -d` starts the Postgres, MySQL, MariaDB, SQL Server, MongoDB and Redis dev databases (see
`compose.yaml`). It also starts two SSH bastions for testing tunnels;
`dev/ssh/setup.sh` generates a key and an `ssh_config` for them into
`dev/ssh/.generated/` (gitignored). `compose.yaml` pins the project name to
`dbdelve`, so `docker compose up` from a git worktree recreates the main
checkout's stack with the `./dev` mounts and the bastion key taken from that
worktree; after removing the worktree, run `docker compose up -d --wait` and
`dev/ssh/setup.sh` from the main checkout again. With nothing configured, DBDelve opens the
connection form; the repository-owned databases accept:

```text
postgresql://dbdelve:dbdelve@127.0.0.1:55432/dbdelve_dev
mysql://dbdelve:dbdelve@127.0.0.1:53306/dbdelve_dev
mariadb://dbdelve:dbdelve@127.0.0.1:53307/dbdelve_dev
mongodb://dbdelve:dbdelve@127.0.0.1:57017/dbdelve_dev
redis://dbdelve:dbdelve@127.0.0.1:56379/0
```

The MongoDB service is `mongo` in `compose.yaml`, on port 57017; its seed is
`dev/mongo/init/001-dbdelve-demo.js`, which `mongod` runs only on an empty data
volume. After editing it, `docker compose down -v` to apply it.

The Redis service is `redis` in `compose.yaml` (the `redis:8` image, which ships
JSON and Search), on port 56379, moved with `DBDELVE_REDIS_PORT`. Its ACL user
and seed are in `dev/redis/`; nothing is persisted, so every start seeds afresh
and the healthcheck passes only once the seed finished.

SQLite has no server to start — build the file once and point the form at its
absolute path:

```sh
sqlite3 dev/dbdelve_dev.db < dev/sqlite/001-dbdelve-demo.sql
```

## Running tests

Plain unit tests run with no setup:

```sh
cargo test
```

Most of the engine code is exercised by database-backed tests named
`live_*`, marked `#[ignore]` so a checkout with no databases running still
passes. To run those too, bring the containers up, seed SQLite, and point the
tests at all of them, exactly as CI does it (`.github/workflows/ci.yml`):

```sh
docker compose up -d --wait
sqlite3 dev/dbdelve_dev.db < dev/sqlite/001-dbdelve-demo.sql

export PGHOST=127.0.0.1
export PGPORT=55432
export PGDATABASE=dbdelve_dev
export PGUSER=dbdelve
export PGPASSWORD=dbdelve
export dbdelve_MYSQL_URL=mysql://dbdelve:dbdelve@127.0.0.1:53306/dbdelve_dev
export dbdelve_MARIADB_URL=mariadb://dbdelve:dbdelve@127.0.0.1:53307/dbdelve_dev
export dbdelve_MONGO_URL=mongodb://dbdelve:dbdelve@127.0.0.1:57017/dbdelve_dev
export dbdelve_REDIS_URL=redis://dbdelve:dbdelve@127.0.0.1:56379/0
export dbdelve_SQLITE_PATH=$(pwd)/dev/dbdelve_dev.db

cargo test -- --include-ignored
```

A MySQL or MariaDB container reporting healthy does not mean its seed applied —
`mysqladmin ping` doesn't check that. If the live tests fail oddly, check
`live_the_development_database_is_fully_seeded` first rather than assuming
your change broke something.

The MongoDB live tests are in `src/db/mongo.rs` and read `dbdelve_MONGO_URL`;
run them alone with:

```sh
cargo test db::mongo -- --include-ignored
```

The Redis live tests are in `src/db/redis.rs` and read `dbdelve_REDIS_URL`;
run them alone with `cargo test db::redis -- --include-ignored`.

The SSH tunnel's own tests (`live_ssh_*`, in `ssh.rs` and each engine module)
need the dev bastions `docker compose up` already started, plus the config
`dev/ssh/setup.sh` generates for them:

```sh
dev/ssh/setup.sh
export dbdelve_SSH_CONFIG=$(pwd)/dev/ssh/.generated/ssh_config
cargo test -- --include-ignored
```

`dbdelve_SSH_CONFIG` is what points the tunnel at that generated config (`ssh
-F`) instead of your real `~/.ssh/config`; without it, `live_ssh_*` tests
panic asking for it rather than silently skipping.

## Load testing

The seed keeps `events` narrow. To see how far DBDelve gets with a wide one,
`dev/load.sh` rebuilds `events` in the running Postgres: the same rows, key and
indexes, plus about a hundred columns of every type Postgres has, several JSON
columns (up to a couple of KB a row) and geometry.

```sh
dev/load.sh                  # a million rows
ROWS=100000 dev/load.sh      # fewer rows
```

Keep the default million if you'll run the live tests afterwards; they count
on it. Expect a few minutes and about 3 GB. `COMPOSE_PROJECT_NAME` points it at
another stack. CI never runs it. To get the default seed back:

```sh
docker compose down -v && docker compose up -d --wait
```

## Before opening a PR

CI runs these; run them locally first:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --include-ignored   # needs the databases up, see above
```

CI also builds on macOS and Windows, so keep the crate building on all three
platforms — `cargo build` on the platform you have is usually enough to
catch anything platform-specific.

## Code conventions

`AGENTS.md` is the source of truth for how DBDelve is built; read it rather
than relying on this summary. A few of its rules that come up often:

- **Engine dispatch stays inside `src/db/`.** It's a closed enum, not a
  trait; nothing above `src/db/` branches on which engine is connected, and
  the grid never sees a driver type (`postgres::Row`, `mysql::Value`,
  `rusqlite::ValueRef`) — only rendered strings and type tags.
- **DBDelve never rewrites SQL behind the user's back.** No silent `LIMIT`,
  no reformatting on execute, nothing on a statement the user didn't ask it
  to change. See "Hard rules" in `AGENTS.md` for the full list and the
  exceptions (sort clicks, the generated-write gate).
- **Comments explain why, not what.** Add one only where the code can't
  convey a non-obvious decision on its own.
- **No speculative abstraction.** No trait for one implementation, no config
  for a value that never changes; the shortest change that fully solves the
  problem wins.
- **Non-trivial logic leaves one runnable test behind** — the smallest one
  that fails if the logic breaks, no fixture scaffolding.

## Adding a new database engine

This is an "open an issue first" change. Roughly: a new module under
`src/db/` alongside `postgres.rs`, `mysql.rs` and `sqlite.rs`, wired into the
`Engine` enum in `src/db/mod.rs`. It needs the same test coverage the
existing engines have — unit tests plus `live_*` integration tests marked
`#[ignore]`, run against `docker compose` if the engine can run in a
container, or a documented external instance if it can't. Read "Engine
divergences" in `AGENTS.md` first: it lists where the three engines already
disagree (transaction bracketing, filter operators, timeouts, cancellation),
so a fourth engine's differences land in those same places rather than
scattered through the UI layer.

## Commit messages and PR titles

PRs are squash-merged, so the PR title becomes the commit on `main`. Write it,
and any commit you push, as `type(scope): summary`:

```text
feat(grid): hide or pin columns in the result grid
fix(editor): put the appended statement's terminator before a trailing comment
perf(grid): hold result rows packed, one buffer per row
feat(turso): add Turso support
chore(release): bump version to 0.3.8
```

**Type** — what kind of change it is:

| Type       | Use for                                                       |
| ---------- | ------------------------------------------------------------- |
| `feat`     | Something a user can now do or see that they couldn't before  |
| `fix`      | Behaviour that was wrong and now isn't                        |
| `perf`     | Same behaviour, measurably faster or lighter (say by how much) |
| `refactor` | Code moves or reshapes with no change in behaviour            |
| `test`     | Tests only                                                    |
| `docs`     | README, CONTRIBUTING, AGENTS.md, doc comments                 |
| `ci`       | `.github/workflows/`                                          |
| `chore`    | Everything else: dependency pins, version bumps, dev scripts  |

**Scope** — the part of the app it touches. Use one of these:

| Scope         | Covers                                                                  |
| ------------- | ----------------------------------------------------------------------- |
| `postgres`, `mysql`, `sqlite`, `mssql`, `mongo`, `redis`, `snowflake`, `turso` | One engine's module in `src/db/` (MySQL covers MariaDB) |
| `db`          | Engine dispatch and what every engine shares (`src/db/mod.rs`)          |
| `ssh`         | SSH tunnels                                                             |
| `connections` | Connection form, profiles, TLS                                          |
| `explorer`    | The schema/object tree                                                  |
| `editor`      | Query editor, SQL handling, completion                                  |
| `grid`        | Result grid, row panel, structure grid, filters, sorting                |
| `editing`     | Row edits and the write gate                                            |
| `explain`     | The plan pane                                                           |
| `tabs`        | Tabs, sessions, restore at startup                                      |
| `palette`     | Command palette, actions, keybindings                                   |
| `theme`       | Themes and theme import                                                 |
| `import`      | Importing connections from other tools                                  |
| `export`      | Exporting results                                                       |
| `update`      | The in-app updater                                                      |
| `settings`    | The settings panel                                                      |
| `release`     | Packaging and release scripts                                           |
| `dev`         | `compose.yaml`, seeds and scripts under `dev/`                          |

Change crosses two scopes? Name the one the user would notice. Truly
app-wide? Leave the scope off: `refactor: split ui.rs`. If a new area doesn't
fit any of these, add a row here in the same PR.

The summary is lowercase, imperative, no trailing full stop, and says what
changes for the user rather than which function moved — "keep the keyboard on
the grid after going to a column", not "update focus handler".

## PR guidelines

Keep PRs focused — one change, not a drive-by cleanup bundled in. Describe
what changed and why, and how you tested it (which commands you ran, and
whether that included the database-backed tests). Link the issue it was
discussed in, if there was one.

For any UI change, include screenshots. For a big feature or support for a
new engine, add a screen recording of it working in the app too. It's a
desktop UI; reviewers need to see the change, not just read the diff.

## License

By contributing, you agree your contribution is licensed under the GNU
General Public License v3.0, same as the rest of the project.

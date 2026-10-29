# Benchmarks

DBDelve 0.3.6 against DBeaver 25.3.1, measured on 2026-10-11. The numbers go
stale with every release; each run says which version it measured.

## Setup

- **Machine:** MacBook Pro, M1 Pro, 32 GB, macOS 26, on AC power, nothing
  running but Docker.
- **Database:** Postgres 15 from this repository's `compose.yaml`. The image is
  amd64, so on Apple silicon it runs under emulation, and at large sizes both
  apps spend part of their time waiting on it.
- **Table:** `events`, 1,000,000 rows × 107 columns, 3 GB on disk.
- **Query:** `SELECT * FROM events LIMIT n` for n = 100, 1,000, 100,000 and
  1,000,000, with the row limit turned off in both apps (DBDelve
  `preview_rows = 0`, DBeaver `resultset.maxrows = 2000000`).
- **DBeaver's heap:** raised to 16 GB with `-Xmx16g`. At its default 1 GB it
  runs out of memory at about 43,000 rows of this table. By default DBeaver
  fetches only 200 rows, so everyday use does not reach that limit.

## Method

Each run:

1. quits both apps and cold-launches one, with an isolated profile, so neither
   reads a saved setup;
2. hides every other app and sizes the window to the full screen;
3. runs `select 1` as a warm-up, then pastes the measured query and presses
   Cmd+Enter;
4. replaces the result with `select 1` and reads memory again 5 s later.

Time runs from the keypress to the last change in the results area, captured
on screen at 60 fps. A run counts as finished only after 3 s with no change and
the app below 10% CPU, because a JVM paused for garbage collection also paints
nothing. Memory is the process footprint, the figure Activity Monitor shows.
CPU is user plus system time from the keypress to the last repaint.

The apps take turns run by run, 5 runs per app per size, and every number below
is the median. A run during which any other window came to the front is thrown
out and redone.

## Results

Launch, median of 20 cold launches:

| | DBDelve | DBeaver |
|---|---|---|
| Launch to a connected window with the schema tree loaded | 1.13 s | 5.20 s |
| CPU spent launching | 0.48 s | 13.6 s |
| Idle memory | 133 MB | 606 MB |
| Idle CPU | 2.9% | 5.2% |

Queries:

| Rows | Time, DBDelve | Time, DBeaver | CPU, DBDelve | CPU, DBeaver | Memory, DBDelve | Memory, DBeaver | After release, DBDelve | After release, DBeaver |
|---|---|---|---|---|---|---|---|---|
| 100 | 0.16 s | 1.70 s | 0.05 s | 1.1 s | 186 MB | 578 MB | 174 MB | 579 MB |
| 1,000 | 0.21 s | 1.96 s | 0.08 s | 2.0 s | 191 MB | 604 MB | 179 MB | 604 MB |
| 100,000 | 5.55 s | 20.0 s | 2.1 s | 24.5 s | 659 MB | 3.0 GB | 175 MB | 3.0 GB |
| 1,000,000 | 55.6 s | 207.5 s | 20.5 s | 258 s | 4.9 GB | 17 GB | 344 MB | 17 GB |

Both apps show their first rows about 0.1 s after the keypress at every size.

The 1M-row memory figures are not a normal use case: the whole table is pulled
into memory on purpose, and the result alone is 4 GB of data. They show what
each app costs to hold that much, not what a day of browsing costs.

## Caveats

- One machine, one database, one table. Wider or narrower tables will move the
  numbers.
- The two apps keep their own default layouts, so their result panes are not
  the same size.
- DBeaver's launch CPU includes JIT warm-up.
- `footprint` reports values above 10 GB in whole gigabytes, hence DBeaver's
  flat 17 GB at 1M rows.
- The harness drives the apps through screen capture and synthetic input on
  one Mac and is not part of this repository.

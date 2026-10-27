# Dependencies

Where the Redis code lives: `src/redis_cmd.rs` splits a buffer into commands
by redis-cli's rules, refuses the session-breaking ones and classifies the
rest; `src/redis_cmd/browse.rs` writes the keys grid's preview, count, filter
and the read a key opens with, and holds the read gate;
`src/redis_cmd/edit.rs` writes the commands an edit becomes and holds the
write gate; `src/db/redis.rs` is the driver, its catalog and its rendering of
replies. The editor's command names (`redis_cmd::names`) come from the same
classify tables.

Where the MongoDB code lives: `src/mql.rs` parses and classifies mongosh
statements, formats them and splices a sort into one; `src/mql/browse.rs`
writes the object tab's preview, count and filter and holds the read gate;
`src/db/mongo.rs` is the driver, its catalog, its sampled structure and its
rendering of BSON.

**The three tree-sitter pins are correctness, not formatting.** The grammar
decides where every statement boundary falls, which statements `sql.rs` will
splice an `ORDER BY` into (and `mql.rs` a `.sort()`), and what the gates
accept. A bump changes what DBDelve sends to the server. Treat them like the driver pins.

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
- `tiberius`: `0.12.3` is its latest release, and its defaults are `native-tls`
  (OpenSSL on Linux) and `winauth`.
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
client and not a driver. ClickHouse uses the same `ureq`, with DBDelve's own
connector chain (TCP, the `Watched` transport that checks Cancel and an idle
bound between ticks, then TLS) in `src/db/clickhouse/http.rs`, and `memchr`
for finding a late exception's marker across reads.

**Do not put tokio on GPUI's executor.** GPUI's executor is `async-task` with no
reactor (Grand Central Dispatch on macOS). A tokio future on
`cx.background_executor().spawn(...)` _panics_ the moment it touches a socket or
timer. Database work uses blocking drivers, which own their runtimes
internally, spawned onto the background executor. `rusqlite` is blocking by
construction and has no runtime at all.

`tokio` is a direct dependency for three drivers, and the same rule is why it
is safe. tiberius is async with no runtime of its own, and tokio-postgres is
used directly rather than through the blocking `postgres` crate, whose
`simple_query` collects every row before returning one and so cannot stop
keeping rows at the row limit. Each `mssql::Connection` and
`postgres::Connection` owns a tokio current-thread runtime and every call into
the driver is a `runtime.block_on(...)` on the background thread the query was
already spawned onto, bar Postgres's cancel, which builds a throwaway runtime
on a thread of its own. The runtime lives behind the connection mutex and
nothing that leaves `src/db/mssql.rs` or `src/db/postgres.rs` is a future.
MongoDB's driver brings its own: `mongo.rs` builds a one-worker multi-thread
runtime per connection, the driver's tasks run on that worker, and DBDelve
blocks on it the same way.

`tokio-rustls` in the tree is not a breach of that rule, and the rule is why:
the TLS handshake is a future belonging to the connection, so it runs inside the
runtime the blocking client already owns, on the same thread as the connect it
is part of. The same holds for tiberius's tokio-rustls 0.24, inside the runtime
`mssql.rs` owns. Nothing tokio-shaped reaches GPUI's executor. Adding a tokio
future anywhere DBDelve spawns one still panics.


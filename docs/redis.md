# Redis

DBDelve speaks to Redis 6 or later, and to what speaks its protocol (Valkey,
KeyDB, Dragonfly). A server that refuses `HELLO 3`, as Redis before 6 does, is
spoken to in RESP2. This page is what you need to know before filling in the
form, and what behaves differently from the SQL engines.

## Connecting

Pick **Redis** in the form's Engine dropdown. Host, port (blank is 6379),
database, username, password and TLS. The username is an ACL user and the
username and password are both optional. The database is a number, 0 to the
server's `databases` setting. A URL works too:

```text
redis://user:password@db.example.com:6379/0
rediss://:password@db.example.com/2?sslmode=require
```

The development container:

```text
redis://dbdelve:dbdelve@127.0.0.1:56379/0
```

## TLS

`rediss://` means TLS, verified, unless `sslmode` says otherwise. The modes
mean what they mean on Postgres, except that `rediss://` with `sslmode=disable`
or `prefer` is refused by name rather than quietly weakened. A root certificate
named on the form (`sslrootcert` in a URL) replaces the system's trust store.

**Verify Full cannot be honoured through an SSH tunnel.** The driver checks the
certificate against the address it dials, which is the tunnel's loopback
address, so that combination is refused. Verify CA works through one.

## Commands

The editor takes redis-cli commands, one per line, split into words by
redis-cli's own rules, so a line pasted from redis-cli means here what it meant
there. A line starting with `#` is a comment. Run All sends the commands in
order on the one connection, so `MULTI` ... `EXEC` works, and shows the last
result. Command names complete as you type, tagged read, write or destructive.

Some commands are refused before anything is sent, because they take the
session DBDelve owns out from under it:

- `SELECT`: pick the database from the database list instead.
- `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE`, `UNSUBSCRIBE` and its siblings,
  `MONITOR`, `SYNC`, `PSYNC`: they turn the session into a stream with no end.
- `HELLO`, `RESET`, `QUIT`: they reset or end the session.
- `CLIENT REPLY`: it stops the server answering.

**Read-only holds by classification alone.** Redis has no read-only session to
ask the server for, so every command is looked up by name (and subcommand:
`CONFIG GET` reads, `CONFIG SET` is destructive) in a table. Reads need
nothing, writes need Read-write, `FLUSHDB`, `FLUSHALL`, `SHUTDOWN`, `DEBUG`,
`CONFIG SET` and the like need Full and ask first, and a command the table does
not know is refused in Read-only and asks once elsewhere. Scripts (`EVAL`,
`FCALL`) count as writes. A write-less ACL user is the only real boundary.

Results: a reply's shape becomes the grid's columns (a hash is `field` and
`value`, a sorted set with scores `member` and `score`, `INFO` is `section`,
`key` and `value`). A `SCAN`, `HSCAN`, `SSCAN` or `ZSCAN` follows its cursor
until it returns 0 or the row limit is reached, as `redis-cli --scan` does.
That is the one reshaping a command gets; nothing else is rewritten.

## The keys grid

Each database lists one relation, `keys`, with `key`, `type`, `ttl` and
`value` columns. Its preview is a real command, `SCAN 0 MATCH * COUNT 1000`,
shown in the tab. The value is the start of a string's or JSON document's own,
or a collection's size (`5 fields`).

- The filter bar narrows by `key` (written as a `MATCH` glob, with the value's
  glob characters escaped) and by `type`. Only one pattern and one type can be
  applied at once, and the raw filter is a `MATCH` glob as typed.
- There is no server-side sort, since `SCAN` is unordered. Sorting is in
  memory, over the page loaded.
- **Count is `DBSIZE`, and only without a filter.** Counting a pattern's keys
  means walking them, which the preview already did.
- Truncate is `FLUSHDB`, destructive, and Drop is not offered.
- Double-click a row, or use Open, to open the key.

## Opening a key

A key opens as a query tab of its own, whose statement is a read bounded by the
tab's row limit, never one read of the whole key:

| Type | Read |
| --- | --- |
| string | `GET` |
| hash, set, sorted set | `HSCAN`, `SSCAN`, `ZSCAN`, the cursor followed until the limit |
| list | `LRANGE key 0 limit-1` |
| stream | `XRANGE key - + COUNT limit` |
| JSON | `JSON.GET key $` |
| time series | `TS.RANGE key - + COUNT limit` |

## Editing

Edits are written as commands, shown in the review dialog before they run
(the keys grid and an opened key's grid have no buffer to append to) and passed
through the same gate as typed ones, so Read-only blocks them. Generated text
is parsed back and must match its canonical spelling exactly.

On the keys grid:

- `key` becomes `RENAMENX`, and a name already taken is an error rather than an
  overwrite.
- `ttl` becomes `EXPIRE`, or `PERSIST` when emptied.
- A string `value` becomes `SET key value KEEPTTL`.
- New row is `SET key value NX`, with `EX` when a ttl is given.
- Delete row is `UNLINK`.

On an opened key:

| Type | Edit | New row | Delete row |
| --- | --- | --- | --- |
| string | `SET ... KEEPTTL` | | `UNLINK` |
| hash | value: `HSET` | `HSETNX` | `HDEL` |
| list | value: `LSET` | `RPUSH` | `LSET` to a marker, then `LREM` |
| set | member: `SREM` + `SADD` in a `MULTI` | `SADD` | `SREM` |
| sorted set | score: `ZADD XX`; member: `ZREM` + `ZADD` in a `MULTI` | `ZADD NX` | `ZREM` |

A batch is not wrapped in a transaction: the commands apply in order, and a
failure says which one failed and how many ran before it.

## Things that behave differently here

- **Cancel kills the session.** DBDelve records the connection's `CLIENT ID`
  and sends `CLIENT KILL ID` from a second connection. The next command dials
  a fresh one, so a `MULTI` or `WATCH` open on the old session is gone. The
  statement timeout, and a broken socket, end the session the same way.
- **Databases are numbers.** Select Database moves the profile to another one.
- **There is no export as SQL and no Import Rows,** since a key names
  its own value rather than a row under a file's columns.

## Known limits

- **Cut values are read-only.** A string or document longer than the grid
  holds (4,000 characters) is shown cut, and a cut value cannot be edited,
  since writing it back would truncate the key.
- **JSON is read whole** (`JSON.GET key $`), not paged, and is read-only along
  with streams.
- **Binary keys and values are not editable.** A name or value that is not
  UTF-8 is shown in redis-cli's quoted spelling and has no edit.
- **`LSET` acts on the index as it is now.** A list changed by someone else
  between the read and the edit has the new value written to whatever sits at
  that position. Likewise, renaming a set member onto an existing one merges
  them, and renaming a sorted set member onto an existing one overwrites its
  score.
- A hash field and a list index are where a value is, so they are not editable.
- Numeric-looking values sort as text in the keys grid.
- Key names are not completed, only command names.
- DBeaver and TablePlus imports skip Redis connections: neither client's
  stored format for one is known.

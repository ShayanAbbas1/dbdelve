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
result. Command names complete as you type, tagged read, write or destructive,
except a name whose subcommand decides that (`CONFIG`, `CLIENT`), which has no
tag.

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
or a stream's size (`5 entries`).

A hash, list, set or sorted set is read beside its size in the same round
trip, at most its first 100 entries: `HSCAN key 0 COUNT 100` (one page, its
cursor not followed), `LRANGE key 0 99`, `SSCAN key 0 COUNT 100` or
`ZRANGE key 0 99 WITHSCORES`, so the read is bounded however big the key is.
When those entries are all of it and their compact JSON is at most 200
characters, about what a column shows readably, the value is that JSON: a hash
and a sorted set are an object (`{"name":"Ada","plan":"pro"}`,
`{"linus":72,"ada":100}` in score order), a list and a set an array. Otherwise
it is the size alone: `1000 fields`, `12 items`, `3 members`.

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
- A string `value` becomes `SET key value XX KEEPTTL GET`, which writes nothing
  to a key deleted since it was read (an error) or now holding another type
  (`WRONGTYPE`).
- A hash, list, set or sorted set shown as JSON is edited as JSON (see below).
- New row is `SET key value NX`, with `EX` when a ttl is given. Its form has
  no `type`: a new key is a string.
- Delete row is `UNLINK`.

A collection's JSON is diffed against the JSON shown, and each difference
becomes the command an opened key's grid writes (below): a hash takes an object
of string values (`HSET` the changed and new fields, `HDEL` the removed), a set
an array of strings with no duplicates (`SADD`, `SREM`), a sorted set an object
of member to number (`ZADD XX CH` a changed score, `ZADD NX` a new member,
`ZREM`), and a list an array of strings **of the same length** (`LSET` each
changed index). Anything else is refused: JSON that does not parse or is the
wrong shape, a value that is not a string, an empty collection (delete the row
instead), and JSON that changes nothing, which is no edit. A list edit that
adds or removes items is refused too: open the key to insert or remove them.

The write never lands over a change made since the read: `WATCH key`, then
`ECHO` of the JSON shown and a bounded read of the key, not the preview's own (`HSCAN`,
`SSCAN` or `ZSCAN key 0 COUNT 100`, `LRANGE key 0 100`), which must find
exactly that JSON's entries, with the cursor back at `0` and nothing more. A
key changed, grown, retyped or deleted is an error and nothing is written; a
change after that read voids the `MULTI` the writes run in. A collection with a
field, member or value that is not UTF-8, or an infinite score, is shown but is
not editable.

On an opened key:

| Type | Edit | New row | Delete row |
| --- | --- | --- | --- |
| string | `SET ... XX KEEPTTL GET` | | `UNLINK` |
| hash | value: `HSET` | `HSETNX` | `HDEL` |
| list | value: `LSET` | `RPUSH` | `LSET` to a marker, then `LREM` |
| set | member: `SADD` the new, then `SREM` the old | `SADD` | `SREM` |
| sorted set | score: `ZADD XX CH`; member: `ZADD NX` the new, then `ZREM` the old | `ZADD NX` | `ZREM` |

A list item is named by its index, which names whatever sits there now, so a
list write first checks the item: `WATCH key`, then `LPOS key value COUNT 0
MAXLEN n`, which lists where the value read sits among the first `n = index +
1` items and must end at the index, then the `LSET` (and `LREM`) in a
`MULTI`. A list changed since it was read, before or during the write, is an
error, and nothing is written.

Renaming a member onto one the set already holds is an error, and the old
member stays. So is a new score for a member that is gone, or one equal to the
score it has (`ZADD XX CH` answers 0 to both). Deleting what is
already gone is not: the end state is the one asked for.

A batch is not wrapped in a transaction: the commands apply in order, and a
failure says which one failed and how many ran before it.

## Things that behave differently here

- **Cancel kills the session.** DBDelve records the connection's `CLIENT ID`
  and sends `CLIENT KILL ID` from a second connection. The next command dials
  a fresh one, so a `MULTI` or `WATCH` open on the old session is gone. The
  statement timeout, and a broken socket, end the session the same way.
- **A `MULTI` left open holds the grids back.** A query tab's `MULTI` stays
  open across runs until its `EXEC` or `DISCARD`, and every command sent
  meanwhile is only queued, so the grids refuse to read or write until it is
  closed. A run that fails discards the `MULTI` it opened.
- **A `WATCH` left open holds list and collection edits back.** A list
  item's write and a collection's JSON edit carry a `WATCH` of their own,
  whose `EXEC` would clear the query tab's too, so they are refused until that
  tab runs `UNWATCH` or the `MULTI` and `EXEC` it guards.
- **Reading a key does not touch it.** Every session is `CLIENT NO-TOUCH ON`
  (Redis 7.2 and later), so neither browsing nor your own commands move a key's
  LRU or LFU, which eviction goes by; `TOUCH` still does.
- **Databases are numbers.** Select Database moves the profile to another one.
- **There is no export as SQL and no Import Rows,** since a key names
  its own value rather than a row under a file's columns.

## Known limits

- **Only a collection shown as JSON is editable from the keys grid,** and a
  list's only item by item: open the key to edit a larger one or to add or
  remove list items. Its edit's check is one bounded read, so a collection
  Redis cannot hand back in one page (rare under 200 characters) is refused
  rather than checked.
- **Cut values are read-only.** A string or document longer than the grid
  holds (4,000 bytes of a string, 4,000 characters of a document) is shown
  cut, and a cut value cannot be edited,
  since writing it back would truncate the key.
- **JSON is read whole** (`JSON.GET key $`), not paged, and is read-only along
  with streams.
- **Binary keys and values are not editable.** A name or value that is not
  UTF-8 is shown in redis-cli's quoted spelling and has no edit.
- A hash field and a list index are where a value is, so they are not editable.
- Numeric-looking values sort as text in the keys grid.
- Key names are not completed, only command names.
- DBeaver and TablePlus imports skip Redis connections: neither client's
  stored format for one is known.

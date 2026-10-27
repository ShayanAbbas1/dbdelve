//! Redis commands, read by DBDelve: the Redis counterpart of `sql.rs` and
//! `mql.rs`.
//!
//! A buffer holds one command per line, split into words by redis-cli's own
//! rules (`sdssplitargs`), so a line pasted from redis-cli means here what it
//! meant there. A line whose first character is `#` is a comment, DBDelve's
//! addition; redis-cli would send it.
//!
//! Classifying a command is a lookup by its name (and subcommand, where the
//! word after the name decides: `CONFIG GET` reads, `CONFIG SET` is
//! destructive). The server has no Read-only hold to fall back on, so this is
//! the whole boundary, and a name the table does not know is unreadable.

use std::ops::Range;

use crate::sql::{Destructive, Leaves, Mode, Verdict};

pub(crate) mod browse;
pub(crate) mod edit;

/// One command of a buffer, as DBDelve reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Command {
    /// The line it was read from, trimmed, as a byte range of the text.
    pub(crate) span: Range<usize>,
    /// Bytes rather than text: `"\xff"` is a byte no `str` can hold.
    pub(crate) words: Vec<Vec<u8>>,
}

impl Command {
    /// The command's name, upper-cased.
    pub(crate) fn name(&self) -> String {
        word(&self.words, 0)
    }

    /// The word after the name, upper-cased, or empty.
    pub(crate) fn subcommand(&self) -> String {
        word(&self.words, 1)
    }
}

fn word(words: &[Vec<u8>], at: usize) -> String {
    words
        .get(at)
        .map(|word| String::from_utf8_lossy(word).to_ascii_uppercase())
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub(crate) message: String,
    /// Byte offset in the text of the line that failed.
    pub(crate) at: usize,
}

/// Each command's line, trimmed: every line holding something other than
/// blanks or a comment.
pub(crate) fn statements(text: &str) -> Vec<Range<usize>> {
    // Only what `split` takes for a blank: `str::trim` would also take a key's
    // trailing U+00A0, which `split` keeps as part of the word.
    let blank = |character: char| u8::try_from(character).is_ok_and(is_space);
    let mut start = 0;
    let mut spans = Vec::new();
    for line in text.split_inclusive('\n') {
        let leading = line.len() - line.trim_start_matches(blank).len();
        let trimmed = line.trim_matches(blank);
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            spans.push(start + leading..start + leading + trimmed.len());
        }
        start += line.len();
    }
    spans
}

/// Every command in `text`, or the first line that cannot be read or may not
/// be sent. Nothing is sent from a text holding either.
pub(crate) fn parse(text: &str) -> Result<Vec<Command>, ParseError> {
    statements(text)
        .into_iter()
        .map(|span| {
            let words = split(&text[span.clone()]).map_err(|message| ParseError {
                message,
                at: span.start,
            })?;
            let command = Command { span, words };
            match refusal(&command) {
                Some(message) => Err(ParseError {
                    message,
                    at: command.span.start,
                }),
                None => Ok(command),
            }
        })
        .collect()
}

/// redis-cli's `sdssplitargs`: words split on whitespace, `"…"` taking
/// `\n \r \t \b \a \\ \" \xHH` escapes and `'…'` only `\'`, a quote opening
/// mid-word as readily as at its start. A closing quote must be followed by
/// whitespace or the end of the line, and an unclosed one is an error.
pub(crate) fn split(line: &str) -> Result<Vec<Vec<u8>>, String> {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Double,
        Single,
    }
    let bytes = line.as_bytes();
    let at = |index: usize| bytes.get(index).copied();
    let space = |byte: Option<u8>| byte.is_some_and(is_space);
    let unterminated = || "Unbalanced quotes in the command.".to_string();
    let mut words = Vec::new();
    let mut index = 0;
    loop {
        while space(at(index)) {
            index += 1;
        }
        if index >= bytes.len() {
            return Ok(words);
        }
        let mut quote = Quote::None;
        let mut current = Vec::new();
        loop {
            let byte = at(index);
            match quote {
                Quote::Double => match byte {
                    None => return Err(unterminated()),
                    Some(b'\\')
                        if at(index + 1) == Some(b'x')
                            && at(index + 2).is_some_and(|b| b.is_ascii_hexdigit())
                            && at(index + 3).is_some_and(|b| b.is_ascii_hexdigit()) =>
                    {
                        let hex = &line[index + 2..index + 4];
                        current.push(u8::from_str_radix(hex, 16).expect("two hex digits"));
                        index += 4;
                    }
                    Some(b'\\') if at(index + 1).is_some() => {
                        current.push(match bytes[index + 1] {
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            b'b' => 0x08,
                            b'a' => 0x07,
                            other => other,
                        });
                        index += 2;
                    }
                    Some(b'"') => {
                        if at(index + 1).is_some() && !space(at(index + 1)) {
                            return Err(unterminated());
                        }
                        index += 1;
                        break;
                    }
                    Some(other) => {
                        current.push(other);
                        index += 1;
                    }
                },
                Quote::Single => match byte {
                    None => return Err(unterminated()),
                    Some(b'\\') if at(index + 1) == Some(b'\'') => {
                        current.push(b'\'');
                        index += 2;
                    }
                    Some(b'\'') => {
                        if at(index + 1).is_some() && !space(at(index + 1)) {
                            return Err(unterminated());
                        }
                        index += 1;
                        break;
                    }
                    Some(other) => {
                        current.push(other);
                        index += 1;
                    }
                },
                Quote::None => match byte {
                    None => break,
                    Some(_) if space(byte) => break,
                    Some(b'"') => {
                        quote = Quote::Double;
                        index += 1;
                    }
                    Some(b'\'') => {
                        quote = Quote::Single;
                        index += 1;
                    }
                    Some(other) => {
                        current.push(other);
                        index += 1;
                    }
                },
            }
        }
        words.push(current);
    }
}

/// C's `isspace`, which is what ends a word in `sdssplitargs`: ASCII only.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `text` as one word [`split`] reads back as exactly `text`: bare where
/// nothing in it would be read otherwise, else double-quoted.
pub(crate) fn quoted(text: &str) -> String {
    let bare = !text.is_empty()
        && !text.bytes().any(|byte| {
            byte.is_ascii_whitespace() || byte.is_ascii_control() || b"\"'\\".contains(&byte)
        });
    match bare {
        true => text.to_string(),
        false => escaped(text.as_bytes(), false),
    }
}

/// A word as one word [`split`] reads back as exactly its bytes: [`quoted`]
/// where they are UTF-8, else [`quoted_bytes`].
pub(crate) fn spelled(word: &[u8]) -> String {
    match std::str::from_utf8(word) {
        Ok(text) => quoted(text),
        Err(_) => quoted_bytes(word),
    }
}

/// Bytes that are not UTF-8 as one double-quoted word, each byte outside
/// printable ASCII written `\xHH`.
pub(crate) fn quoted_bytes(bytes: &[u8]) -> String {
    escaped(bytes, true)
}

fn escaped(bytes: &[u8], ascii: bool) -> String {
    let mut quoted = Vec::with_capacity(bytes.len() + 2);
    quoted.push(b'"');
    for &byte in bytes {
        match byte {
            b'\\' | b'"' => quoted.extend([b'\\', byte]),
            b'\n' => quoted.extend(b"\\n"),
            b'\r' => quoted.extend(b"\\r"),
            b'\t' => quoted.extend(b"\\t"),
            0x07 => quoted.extend(b"\\a"),
            0x08 => quoted.extend(b"\\b"),
            byte if byte.is_ascii_control() || (ascii && !byte.is_ascii()) => {
                quoted.extend(format!("\\x{byte:02x}").bytes());
            }
            byte => quoted.push(byte),
        }
    }
    quoted.push(b'"');
    // Every byte escaped is ASCII, and the rest were UTF-8 coming in or are
    // escaped when they might not be.
    String::from_utf8(quoted).expect("escaped to UTF-8")
}

/// Why `command` is not sent, for the ones that would take the session
/// DBDelve owns out from under it.
fn refusal(command: &Command) -> Option<String> {
    let name = command.name();
    let why = match name.as_str() {
        "SELECT" => "the database is the profile's, picked from the database list",
        "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "UNSUBSCRIBE" | "PUNSUBSCRIBE"
        | "SUNSUBSCRIBE" | "MONITOR" | "SYNC" | "PSYNC" => {
            "it turns the session into a stream of messages with no end, which no result can hold"
        }
        "HELLO" | "RESET" | "QUIT" => "it resets or ends the session DBDelve holds open",
        "CLIENT" if command.subcommand() == "REPLY" => {
            return Some(
                "CLIENT REPLY is not sent from the editor: it stops the server answering, and \
                 DBDelve waits on every answer."
                    .into(),
            );
        }
        _ => return None,
    };
    Some(format!("{name} is not sent from the editor: {why}."))
}

/// How much a command may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    /// Runs a script whose body DBDelve cannot read, which may do anything a
    /// `Drop` does.
    Script,
    /// Throws data or the server's state away wholesale.
    Drop,
}

/// The lowest mode that may run `text`, and what makes it dangerous if
/// anything does. A command the table does not know, or a line that cannot be
/// split, is unreadable, and with no server-side hold behind Read-only it may
/// not run there even once. A refused command is sent nowhere, so it reads.
pub(crate) fn classify(text: &str) -> Verdict {
    let unreadable = Verdict {
        mode: Mode::ReadWrite,
        destructive: vec![Destructive::Unreadable],
    };
    let mut verdict = Verdict::READ;
    for span in statements(text) {
        let Ok(words) = split(&text[span.clone()]) else {
            return unreadable;
        };
        let command = Command { span, words };
        if refusal(&command).is_some() {
            continue;
        }
        verdict = verdict.max(match kind(&command) {
            Some(Kind::Read) => Verdict::READ,
            Some(Kind::Write) => Verdict::WRITE,
            Some(Kind::Script) => Verdict::FULL,
            Some(Kind::Drop) => Verdict::destroys(Destructive::Drop),
            None => return unreadable,
        });
    }
    verdict
}

/// What `text` leaves on its session that a fresh connection would not have:
/// a `MULTI` still open, or a session setting.
pub(crate) fn leaves(text: &str) -> Leaves {
    let mut leaves = Leaves::default();
    for command in parse(text).unwrap_or_default() {
        match command.name().as_str() {
            "MULTI" => leaves.transaction = Some(true),
            "EXEC" | "DISCARD" => leaves.transaction = Some(false),
            "WATCH" | "AUTH" | "READONLY" | "READWRITE" => leaves.settings = true,
            "CLIENT" if kind(&command) == Some(Kind::Write) => leaves.settings = true,
            _ => {}
        }
    }
    leaves
}

/// Every command name the editor offers, with how much it may do. A name that
/// only has subcommands (`CONFIG`) has no kind of its own, and a refused one
/// is left out.
pub(crate) fn names() -> impl Iterator<Item = (&'static str, Option<&'static str>)> {
    let tagged = |names: &'static [&'static str], tag| names.iter().map(move |n| (*n, Some(tag)));
    tagged(READS, "read")
        .chain(tagged(WRITES, "write"))
        .chain(tagged(SCRIPTS, "script"))
        .chain(tagged(DROPS, "destructive"))
        .chain(SUBCOMMANDED.iter().map(|name| (*name, None)))
        .filter(|(name, _)| {
            let command = Command {
                span: 0..0,
                words: vec![name.as_bytes().to_vec()],
            };
            refusal(&command).is_none()
        })
}

fn kind(command: &Command) -> Option<Kind> {
    let name = command.name();
    let sub = command.subcommand();
    use Kind::*;
    Some(match (name.as_str(), sub.as_str()) {
        ("CONFIG", "GET" | "HELP") => Read,
        ("CONFIG", "SET" | "REWRITE" | "RESETSTAT") => Drop,
        ("CLIENT", "PAUSE") => Drop,
        ("CLIENT", "LIST" | "INFO" | "ID" | "GETNAME" | "GETREDIR" | "TRACKINGINFO" | "HELP") => {
            Read
        }
        (
            "CLIENT",
            "KILL" | "UNPAUSE" | "SETNAME" | "SETINFO" | "NO-EVICT" | "NO-TOUCH" | "TRACKING"
            | "CACHING" | "UNBLOCK",
        ) => Write,
        ("SCRIPT", "EXISTS" | "HELP") => Read,
        ("SCRIPT", "LOAD" | "KILL") => Write,
        ("SCRIPT", "FLUSH") => Drop,
        ("FUNCTION", "LIST" | "DUMP" | "STATS" | "HELP") => Read,
        ("FUNCTION", "LOAD" | "KILL") => Write,
        ("FUNCTION", "FLUSH" | "DELETE" | "RESTORE") => Drop,
        ("MEMORY", "USAGE" | "STATS" | "DOCTOR" | "MALLOC-STATS" | "HELP") => Read,
        ("MEMORY", "PURGE") => Write,
        ("SLOWLOG", "GET" | "LEN" | "HELP") => Read,
        ("SLOWLOG", "RESET") => Write,
        ("LATENCY", "LATEST" | "HISTORY" | "DOCTOR" | "GRAPH" | "HISTOGRAM" | "HELP") => Read,
        ("LATENCY", "RESET") => Write,
        (
            "ACL",
            "WHOAMI" | "LIST" | "USERS" | "GETUSER" | "CAT" | "GENPASS" | "DRYRUN" | "HELP",
        ) => Read,
        ("ACL", "LOG") => match word(&command.words, 2).as_str() {
            "RESET" => Write,
            _ => Read,
        },
        ("ACL", "SAVE") => Write,
        ("ACL", "DELUSER" | "SETUSER" | "LOAD") => Drop,
        (
            "CLUSTER",
            "INFO"
            | "NODES"
            | "SLOTS"
            | "SHARDS"
            | "MYID"
            | "MYSHARDID"
            | "KEYSLOT"
            | "COUNTKEYSINSLOT"
            | "GETKEYSINSLOT"
            | "LINKS"
            | "REPLICAS"
            | "SLAVES"
            | "COUNT-FAILURE-REPORTS"
            | "HELP",
        ) => Read,
        (
            "CLUSTER",
            "RESET" | "FLUSHSLOTS" | "FAILOVER" | "DELSLOTS" | "DELSLOTSRANGE" | "SETSLOT",
        ) => Drop,
        (
            "CLUSTER",
            "ADDSLOTS" | "ADDSLOTSRANGE" | "BUMPEPOCH" | "FORGET" | "MEET" | "REPLICATE"
            | "SAVECONFIG" | "SET-CONFIG-EPOCH",
        ) => Write,
        ("MODULE", "LIST" | "HELP") => Read,
        ("MODULE", "LOAD" | "LOADEX" | "UNLOAD") => Drop,
        ("FT.CURSOR", "READ") => Read,
        ("FT.CURSOR", "DEL") => Write,
        ("JSON.DEBUG", "MEMORY" | "HELP") => Read,
        (name, _) if SUBCOMMANDED.contains(&name) => return None,
        (name, _) if READS.contains(&name) => Read,
        (name, _) if WRITES.contains(&name) => Write,
        (name, _) if SCRIPTS.contains(&name) => Script,
        (name, _) if DROPS.contains(&name) => Drop,
        _ => return None,
    })
}

/// Commands whose subcommand decides what they do, so `kind` has no answer
/// for an unknown one.
const SUBCOMMANDED: &[&str] = &[
    "CONFIG",
    "CLIENT",
    "SCRIPT",
    "FUNCTION",
    "MEMORY",
    "SLOWLOG",
    "LATENCY",
    "ACL",
    "CLUSTER",
    "MODULE",
    "FT.CURSOR",
    "JSON.DEBUG",
];

/// Commands that only read, whatever follows their name: those the server's
/// command table flags `readonly`, and the server and connection commands
/// that change nothing.
const READS: &[&str] = &[
    // Keys and strings.
    "GET",
    "MGET",
    "GETRANGE",
    "SUBSTR",
    "STRLEN",
    "LCS",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "KEYS",
    "SCAN",
    "RANDOMKEY",
    "DUMP",
    "TOUCH",
    "OBJECT",
    "SORT_RO",
    "DBSIZE",
    // Hashes.
    "HGET",
    "HMGET",
    "HGETALL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HEXISTS",
    "HSTRLEN",
    "HSCAN",
    "HRANDFIELD",
    "HTTL",
    "HPTTL",
    "HEXPIRETIME",
    "HPEXPIRETIME",
    // Lists.
    "LRANGE",
    "LINDEX",
    "LLEN",
    "LPOS",
    // Sets.
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SCARD",
    "SRANDMEMBER",
    "SSCAN",
    "SINTER",
    "SUNION",
    "SDIFF",
    "SINTERCARD",
    // Sorted sets.
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZREVRANGEBYLEX",
    "ZRANK",
    "ZREVRANK",
    "ZSCORE",
    "ZMSCORE",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZSCAN",
    "ZRANDMEMBER",
    "ZINTER",
    "ZUNION",
    "ZDIFF",
    "ZINTERCARD",
    // Streams.
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "XREAD",
    "XINFO",
    "XPENDING",
    // Bitmaps, HyperLogLog, geo.
    "GETBIT",
    "BITCOUNT",
    "BITPOS",
    "BITFIELD_RO",
    "PFCOUNT",
    "GEOPOS",
    "GEODIST",
    "GEOHASH",
    "GEORADIUS_RO",
    "GEORADIUSBYMEMBER_RO",
    "GEOSEARCH",
    // Scripts that the server holds to reads.
    "EVAL_RO",
    "EVALSHA_RO",
    "FCALL_RO",
    // Server and connection.
    "PING",
    "ECHO",
    "TIME",
    "INFO",
    "LASTSAVE",
    "ROLE",
    "LOLWUT",
    "COMMAND",
    "PUBSUB",
    "WAIT",
    "WAITAOF",
    // Modules: JSON, Search, time series, probabilistic, vector sets.
    "JSON.GET",
    "JSON.MGET",
    "JSON.TYPE",
    "JSON.STRLEN",
    "JSON.OBJKEYS",
    "JSON.OBJLEN",
    "JSON.ARRLEN",
    "JSON.ARRINDEX",
    "JSON.RESP",
    "FT.SEARCH",
    "FT.AGGREGATE",
    "FT.INFO",
    "FT._LIST",
    "FT.EXPLAIN",
    "FT.EXPLAINCLI",
    "FT.PROFILE",
    "FT.SPELLCHECK",
    "FT.SYNDUMP",
    "FT.TAGVALS",
    "FT.DICTDUMP",
    "FT.SUGGET",
    "FT.SUGLEN",
    "TS.GET",
    "TS.MGET",
    "TS.RANGE",
    "TS.REVRANGE",
    "TS.MRANGE",
    "TS.MREVRANGE",
    "TS.INFO",
    "TS.QUERYINDEX",
    "BF.EXISTS",
    "BF.MEXISTS",
    "BF.INFO",
    "BF.CARD",
    "BF.SCANDUMP",
    "CF.EXISTS",
    "CF.MEXISTS",
    "CF.COUNT",
    "CF.INFO",
    "CF.SCANDUMP",
    "CMS.QUERY",
    "CMS.INFO",
    "TOPK.QUERY",
    "TOPK.COUNT",
    "TOPK.LIST",
    "TOPK.INFO",
    "TDIGEST.CDF",
    "TDIGEST.QUANTILE",
    "TDIGEST.MIN",
    "TDIGEST.MAX",
    "TDIGEST.RANK",
    "TDIGEST.REVRANK",
    "TDIGEST.BYRANK",
    "TDIGEST.BYREVRANK",
    "TDIGEST.TRIMMED_MEAN",
    "TDIGEST.INFO",
    "VSIM",
    "VCARD",
    "VDIM",
    "VEMB",
    "VGETATTR",
    "VINFO",
    "VLINKS",
    "VRANDMEMBER",
    "VISMEMBER",
];

/// Commands that change data or the server's state, short of throwing it
/// away. Scripts count, since nothing says what one does.
const WRITES: &[&str] = &[
    // Keys and strings.
    "SET",
    "SETNX",
    "SETEX",
    "PSETEX",
    "MSET",
    "MSETNX",
    "APPEND",
    "INCR",
    "INCRBY",
    "INCRBYFLOAT",
    "DECR",
    "DECRBY",
    "GETSET",
    "GETDEL",
    "GETEX",
    "SETRANGE",
    "DEL",
    "UNLINK",
    "EXPIRE",
    "PEXPIRE",
    "EXPIREAT",
    "PEXPIREAT",
    "PERSIST",
    "RENAME",
    "RENAMENX",
    "MOVE",
    "COPY",
    "RESTORE",
    "MIGRATE",
    "SORT",
    "DELEX",
    // Hashes.
    "HSET",
    "HSETNX",
    "HMSET",
    "HDEL",
    "HINCRBY",
    "HINCRBYFLOAT",
    "HEXPIRE",
    "HPEXPIRE",
    "HEXPIREAT",
    "HPEXPIREAT",
    "HPERSIST",
    "HGETDEL",
    "HGETEX",
    "HSETEX",
    // Lists.
    "LPUSH",
    "RPUSH",
    "LPUSHX",
    "RPUSHX",
    "LPOP",
    "RPOP",
    "LSET",
    "LREM",
    "LTRIM",
    "LINSERT",
    "LMOVE",
    "BLMOVE",
    "RPOPLPUSH",
    "BRPOPLPUSH",
    "BLPOP",
    "BRPOP",
    "LMPOP",
    "BLMPOP",
    // Sets.
    "SADD",
    "SREM",
    "SPOP",
    "SMOVE",
    "SINTERSTORE",
    "SUNIONSTORE",
    "SDIFFSTORE",
    // Sorted sets.
    "ZADD",
    "ZREM",
    "ZINCRBY",
    "ZPOPMIN",
    "ZPOPMAX",
    "BZPOPMIN",
    "BZPOPMAX",
    "ZMPOP",
    "BZMPOP",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
    "ZREMRANGEBYLEX",
    "ZRANGESTORE",
    "ZINTERSTORE",
    "ZUNIONSTORE",
    "ZDIFFSTORE",
    // Streams.
    "XADD",
    "XDEL",
    "XTRIM",
    "XGROUP",
    "XREADGROUP",
    "XACK",
    "XCLAIM",
    "XAUTOCLAIM",
    "XSETID",
    "XACKDEL",
    "XDELEX",
    // Bitmaps, HyperLogLog, geo.
    "SETBIT",
    "BITOP",
    "BITFIELD",
    "PFADD",
    "PFMERGE",
    "PFDEBUG",
    "PFSELFTEST",
    "GEOADD",
    "GEORADIUS",
    "GEORADIUSBYMEMBER",
    "GEOSEARCHSTORE",
    // Transactions, messages.
    "MULTI",
    "EXEC",
    "DISCARD",
    "WATCH",
    "UNWATCH",
    "PUBLISH",
    "SPUBLISH",
    // Server and connection.
    "SAVE",
    "BGSAVE",
    "BGREWRITEAOF",
    "AUTH",
    "READONLY",
    "READWRITE",
    // Modules.
    "JSON.SET",
    "JSON.DEL",
    "JSON.FORGET",
    "JSON.MSET",
    "JSON.MERGE",
    "JSON.NUMINCRBY",
    "JSON.NUMMULTBY",
    "JSON.STRAPPEND",
    "JSON.ARRAPPEND",
    "JSON.ARRINSERT",
    "JSON.ARRPOP",
    "JSON.ARRTRIM",
    "JSON.TOGGLE",
    "JSON.CLEAR",
    "FT.CREATE",
    "FT.ALTER",
    "FT.ALIASADD",
    "FT.ALIASDEL",
    "FT.ALIASUPDATE",
    "FT.SYNUPDATE",
    "FT.DICTADD",
    "FT.DICTDEL",
    "FT.SUGADD",
    "FT.SUGDEL",
    "TS.CREATE",
    "TS.ALTER",
    "TS.ADD",
    "TS.MADD",
    "TS.INCRBY",
    "TS.DECRBY",
    "TS.DEL",
    "TS.CREATERULE",
    "TS.DELETERULE",
    "BF.ADD",
    "BF.MADD",
    "BF.INSERT",
    "BF.RESERVE",
    "BF.LOADCHUNK",
    "CF.ADD",
    "CF.ADDNX",
    "CF.INSERT",
    "CF.INSERTNX",
    "CF.DEL",
    "CF.RESERVE",
    "CF.LOADCHUNK",
    "CMS.INITBYDIM",
    "CMS.INITBYPROB",
    "CMS.INCRBY",
    "CMS.MERGE",
    "TOPK.RESERVE",
    "TOPK.ADD",
    "TOPK.INCRBY",
    "TDIGEST.CREATE",
    "TDIGEST.ADD",
    "TDIGEST.RESET",
    "TDIGEST.MERGE",
    "VADD",
    "VREM",
    "VSETATTR",
];

/// Commands that run a script, whose `redis.call`s are DBDelve's to guess at:
/// `EVAL "return redis.call('flushall')" 0` is a `FLUSHALL`. Their `_RO`
/// forms, which the server holds to reads, are in `READS`.
const SCRIPTS: &[&str] = &["EVAL", "EVALSHA", "FCALL"];

/// Commands that empty a database, hand the server to another, or stop it.
const DROPS: &[&str] = &[
    "FLUSHDB",
    "FLUSHALL",
    "SWAPDB",
    "SHUTDOWN",
    "DEBUG",
    "REPLICAOF",
    "SLAVEOF",
    "FAILOVER",
    "FT.DROPINDEX",
    "FT.DROP",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        split(line)
            .unwrap()
            .into_iter()
            .map(|word| String::from_utf8(word).unwrap())
            .collect()
    }

    #[test]
    fn words_split_the_way_redis_cli_splits_them() {
        assert_eq!(words("  SET  key   value "), ["SET", "key", "value"]);
        assert_eq!(words(r#"SET "a key" 'b c'"#), ["SET", "a key", "b c"]);
    }

    #[test]
    fn double_quotes_take_redis_cli_escapes() {
        assert_eq!(
            split(r#"SET k "a\nb\r\t\"\\\x41\xff\b\a\q""#).unwrap()[2],
            b"a\nb\r\t\"\\A\xff\x08\x07q"
        );
        // Not two hex digits, so `\x` is just an `x`.
        assert_eq!(words(r#"ECHO "\xZZ""#), ["ECHO", "xZZ"]);
    }

    #[test]
    fn single_quotes_take_only_an_escaped_quote() {
        assert_eq!(words(r#"ECHO 'it\'s \n'"#), ["ECHO", r"it's \n"]);
    }

    #[test]
    fn a_quote_opens_mid_word_and_an_empty_one_is_a_word() {
        assert_eq!(words(r#"ECHO "" ''"#), ["ECHO", "", ""]);
        assert_eq!(words(r#"ECHO a"b c""#), ["ECHO", "ab c"]);
    }

    #[test]
    fn an_unclosed_or_crowded_quote_is_an_error() {
        assert!(split(r#"SET k "open"#).is_err());
        assert!(split("SET k 'open").is_err());
        assert!(split(r#"SET k "closed"tail"#).is_err());
        assert!(split("SET k 'closed'tail").is_err());
        assert!(split(r#"SET k "trailing\"#).is_err());
    }

    #[test]
    fn a_quoted_word_reads_back_as_itself() {
        for text in [
            "user:1",
            "",
            "with space",
            "quo\"te",
            "it's",
            "back\\slash",
            "tab\tnew\nline",
            "é ü",
            "\u{a0}no-break\u{a0}",
            "ideographic\u{3000}",
        ] {
            let line = format!("GET {}", quoted(text));
            assert_eq!(parse(&line).unwrap()[0].words[1], text.as_bytes(), "{line}");
        }
        assert_eq!(quoted("user:1"), "user:1");
        assert_eq!(quoted("a b"), "\"a b\"");
        let binary = b"\x00\xff\xfe\"raw";
        let line = format!("GET {}", quoted_bytes(binary));
        assert_eq!(split(&line).unwrap()[1], binary);
        assert_eq!(quoted_bytes(binary), r#""\x00\xff\xfe\"raw""#);
    }

    #[test]
    fn each_line_is_a_statement_and_comments_and_blanks_are_not() {
        let text = "# keys\nGET a\n\n   SET b 1  \n  # SET c 2\nDEL a";
        let spans: Vec<&str> = statements(text)
            .into_iter()
            .map(|span| &text[span])
            .collect();
        assert_eq!(spans, ["GET a", "SET b 1", "DEL a"]);
    }

    #[test]
    fn commands_that_would_take_the_session_are_refused_before_anything_is_sent() {
        for line in [
            "SELECT 1",
            "select 1",
            "SUBSCRIBE news",
            "PSUBSCRIBE n*",
            "SSUBSCRIBE s",
            "UNSUBSCRIBE",
            "PUNSUBSCRIBE n*",
            "SUNSUBSCRIBE s",
            "MONITOR",
            "SYNC",
            "PSYNC ? -1",
            "HELLO 2",
            "RESET",
            "QUIT",
            "client reply off",
        ] {
            let error = parse(&format!("GET a\n{line}")).unwrap_err();
            assert_eq!(error.at, 6, "{line}");
            assert!(!error.message.is_empty());
        }
        assert!(parse("CLIENT LIST").is_ok());
    }

    #[test]
    fn reads_writes_and_drops_are_told_apart_by_name_and_subcommand() {
        use Destructive::*;
        let read = Verdict::READ;
        let write = Verdict::WRITE;
        let drop = Verdict::destroys(Drop);
        let full = Verdict::FULL;
        for (text, verdict) in [
            ("GET a", &read),
            ("hgetall h", &read),
            ("SCAN 0 MATCH user:* COUNT 100", &read),
            ("CONFIG GET maxmemory", &read),
            ("config set maxmemory 1", &drop),
            ("CONFIG REWRITE", &drop),
            ("CLIENT LIST", &read),
            ("CLIENT KILL ID 4", &write),
            ("SCRIPT EXISTS abc", &read),
            ("SCRIPT FLUSH", &drop),
            ("FUNCTION LIST", &read),
            ("FUNCTION DELETE lib", &drop),
            ("MEMORY USAGE k", &read),
            ("ACL WHOAMI", &read),
            ("ACL LOG", &read),
            ("ACL LOG RESET", &write),
            ("ACL SETUSER bob on", &drop),
            ("XINFO STREAM s", &read),
            ("OBJECT ENCODING k", &read),
            ("JSON.GET doc $", &read),
            ("JSON.SET doc $ 1", &write),
            ("FT.DROPINDEX idx", &drop),
            ("EVAL \"return 1\" 0", &full),
            ("eval \"return redis.call('flushall')\" 0", &full),
            ("EVALSHA abc 0", &full),
            ("FCALL f 0", &full),
            ("EVAL_RO \"return 1\" 0", &read),
            ("EVALSHA_RO abc 0", &read),
            ("FCALL_RO f 0", &read),
            ("SCRIPT LOAD \"return 1\"", &write),
            ("GET a\nEVALSHA abc 0", &full),
            ("EVALSHA abc 0\nFLUSHDB", &drop),
            ("SET a 1", &write),
            ("DEL a", &write),
            ("MULTI", &write),
            ("FLUSHDB", &drop),
            ("FLUSHALL ASYNC", &drop),
            ("SWAPDB 0 1", &drop),
            ("SHUTDOWN NOSAVE", &drop),
            ("DEBUG SLEEP 1", &drop),
            ("MODULE LOAD /m.so", &drop),
            ("CLIENT PAUSE 1000", &drop),
            ("CLUSTER FAILOVER FORCE", &drop),
            ("CLUSTER DELSLOTS 1", &drop),
            ("CLUSTER SETSLOT 1 STABLE", &drop),
            ("GET a\nSET b 1", &write),
            ("GET a\nFLUSHDB\nSET b 1", &drop),
            ("# nothing at all", &read),
            ("SELECT 1", &read),
        ] {
            assert_eq!(&classify(text), verdict, "{text}");
        }
    }

    #[test]
    fn the_offered_names_are_the_tables_less_what_is_refused() {
        let offered: Vec<_> = names().collect();
        assert!(offered.contains(&("GET", Some("read"))));
        assert!(offered.contains(&("FLUSHALL", Some("destructive"))));
        assert!(offered.contains(&("CONFIG", None)));
        assert!(
            offered
                .iter()
                .all(|(name, _)| !matches!(*name, "SUBSCRIBE" | "HELLO"))
        );
    }

    #[test]
    fn anything_unknown_fails_closed() {
        let unreadable = Verdict {
            mode: Mode::ReadWrite,
            destructive: vec![Destructive::Unreadable],
        };
        for text in [
            "FROBNICATE a",
            "CONFIG FROBNICATE",
            "CLIENT",
            "GET a\nNOPE",
            "SET k \"open",
        ] {
            assert_eq!(classify(text), unreadable, "{text}");
        }
    }

    #[test]
    fn a_multi_holds_its_session_until_exec_or_discard() {
        assert_eq!(leaves("MULTI").transaction, Some(true));
        assert_eq!(leaves("EXEC").transaction, Some(false));
        assert_eq!(leaves("DISCARD").transaction, Some(false));
        assert!(leaves("CLIENT SETNAME me").settings);
        assert_eq!(leaves("GET a"), Leaves::default());
    }
}

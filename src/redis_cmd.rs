//! Redis commands, read by DBDelve: the Redis counterpart of `sql.rs` and
//! `mql.rs`.
//!
//! A buffer holds one command per line, split into words by redis-cli's own
//! rules (`sdssplitargs`), so a line pasted from redis-cli means here what it
//! meant there. A line whose first character is `#` is a comment, DBDelve's
//! addition; redis-cli would send it.
//!
//! Classifying a command is a lookup by its name (and subcommand, where the
//! word after the name decides: `CONFIG GET` writes nothing and is
//! rated a write for what it returns, `requirepass` and `masterauth`, while
//! `CONFIG SET` is destructive). The server has no Read-only hold to fall back
//! on, so this is the whole boundary, and a name the table does not know is
//! unreadable.

use std::ops::Range;

use crate::sql::{Destructive, Expiry, Leaves, Mode, Verdict};

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
            // By name, not by verdict: `CLIENT LIST` is a write for what it
            // discloses, and says nothing about the session it is run on.
            "CLIENT"
                if matches!(
                    command.subcommand().as_str(),
                    "KILL"
                        | "UNPAUSE"
                        | "SETNAME"
                        | "SETINFO"
                        | "NO-EVICT"
                        | "NO-TOUCH"
                        | "TRACKING"
                        | "CACHING"
                        | "UNBLOCK"
                ) =>
            {
                leaves.settings = true
            }
            _ => {}
        }
    }
    leaves
}

/// Every command of `text` that splits into words, in order. A line that does
/// not split is left out: the callers below answer for what they can read, and
/// [`classify`] already calls the text unreadable.
fn commands(text: &str) -> impl Iterator<Item = Command> + '_ {
    statements(text).into_iter().filter_map(|span| {
        let words = split(&text[span.clone()]).ok()?;
        Some(Command { span, words })
    })
}

/// Whether any command of `text` takes data out of the database: a name in
/// [`REMOVES`], `MIGRATE`, `XGROUP DESTROY` or `DELCONSUMER`, an `XADD` that
/// trims (`MAXLEN` or `MINID`), an expiry whose timeout is not a positive
/// integer written plainly (`EXPIRE k 0` deletes the key at once, as does
/// `EX 0` on `HSETEX` or `HGETEX`). The agent path refuses all of them,
/// whatever the mode (GATE-07); the human path never asks. A command that
/// schedules a removal (a positive timeout, an absolute time) is not one: it
/// is an expiry, which [`expiry`] names and the agent path approves.
pub(crate) fn removes(text: &str) -> bool {
    commands(text).any(|command| removes_data(&command))
}

/// What the first command of `text` that schedules a removal schedules it
/// for: the key (`EXPIRE`, `SETEX` or `SET` with a positive timeout, an
/// `*EXPIREAT` or an `EXAT` or `PXAT` at any time), or the named fields of a
/// hash (the `H` forms of the same). The agent path approves such a command and
/// names the effect; the human path never asks. A timeout that is not a
/// positive integer written plainly schedules nothing here: the server rejects
/// it on `SET`, `GETEX` and `SETEX`, and `EXPIRE` and the hash commands remove
/// on it, which [`removes`] answers.
///
/// ponytail: the option words are matched anywhere after the name, so a key,
/// field or value spelled like one is named an expiry too. That is the safe
/// direction: the human sees one sentence too many, never one too few. The
/// ceiling is that this reads words and not each command's option grammar; the
/// upgrade path is reading the grammar.
pub(crate) fn expiry(text: &str) -> Option<Expiry> {
    commands(text).find_map(|command| match command.name().as_str() {
        "EXPIRE" | "PEXPIRE" | "SETEX" | "PSETEX" => {
            keeps_the_key(&word(&command.words, 2)).then_some(Expiry::Key)
        }
        "EXPIREAT" | "PEXPIREAT" => Some(Expiry::Key),
        "HEXPIRE" | "HPEXPIRE" => keeps_the_key(&word(&command.words, 2)).then_some(Expiry::Field),
        "HEXPIREAT" | "HPEXPIREAT" => Some(Expiry::Field),
        "SET" | "GETEX" => times_out(&command).then_some(Expiry::Key),
        "HSETEX" | "HGETEX" => times_out(&command).then_some(Expiry::Field),
        _ => None,
    })
}

/// Whether `command` carries an absolute time, or a relative one the server
/// takes as positive.
fn times_out(command: &Command) -> bool {
    has_option(command, &["EXAT", "PXAT"])
        || command.words.iter().enumerate().skip(1).any(|(at, word)| {
            (word.eq_ignore_ascii_case(b"EX") || word.eq_ignore_ascii_case(b"PX"))
                && keeps_the_key(&self::word(&command.words, at + 1))
        })
}

fn removes_data(command: &Command) -> bool {
    let name = command.name();
    match name.as_str() {
        "MIGRATE" => true,
        "XGROUP" => matches!(command.subcommand().as_str(), "DESTROY" | "DELCONSUMER"),
        "EXPIRE" | "PEXPIRE" | "HEXPIRE" | "HPEXPIRE" => !keeps_the_key(&word(&command.words, 2)),
        // An `XADD` that trims is an `XTRIM`, and `LIMIT` only ever follows
        // `MAXLEN` or `MINID`.
        "XADD" => has_option(command, &["MAXLEN", "MINID"]),
        // Unlike `SET`, these take a zero `EX` or `PX` and delete the field
        // at once (checked on Redis 8.10.3).
        "HSETEX" | "HGETEX" => command.words.iter().enumerate().skip(1).any(|(at, word)| {
            (word.eq_ignore_ascii_case(b"EX") || word.eq_ignore_ascii_case(b"PX"))
                && !keeps_the_key(&self::word(&command.words, at + 1))
        }),
        name => REMOVES.contains(&name),
    }
}

/// The fail-safe reading of a timeout the server may take as non-positive or
/// reject: only a run of digits not starting with `0` is known to keep the
/// key.
fn keeps_the_key(timeout: &str) -> bool {
    !timeout.is_empty()
        && timeout.bytes().all(|byte| byte.is_ascii_digit())
        && !timeout.starts_with('0')
}

/// Whether any word after the command's name is one of `options`, compared
/// without regard to case.
fn has_option(command: &Command, options: &[&str]) -> bool {
    command.words.iter().skip(1).any(|word| {
        options
            .iter()
            .any(|option| word.eq_ignore_ascii_case(option.as_bytes()))
    })
}

/// Whether any command of `text` runs a script the server holds, or loads one:
/// `EVAL`, `EVALSHA`, `FCALL`, their `_RO` forms (the server holds those to
/// reads, but what the script does is still code DBDelve cannot read),
/// `SCRIPT LOAD` and `FUNCTION LOAD`.
pub(crate) fn runs_script(text: &str) -> bool {
    commands(text).any(|command| {
        let name = command.name();
        match name.as_str() {
            "EVAL_RO" | "EVALSHA_RO" | "FCALL_RO" => true,
            "SCRIPT" | "FUNCTION" => command.subcommand() == "LOAD",
            name => SCRIPTS.contains(&name),
        }
    })
}

/// Whether any command of `text` is `CONFIG GET` or `CLIENT LIST`, which can
/// answer with `requirepass`, `masterauth` or another client's address and name
/// (D-27), or `ACL LIST` or `ACL GETUSER`, which answer with every user's
/// password hashes. Matched by name and subcommand and nothing wider; [`kind`]
/// records why the line is drawn there.
pub(crate) fn discloses(text: &str) -> bool {
    commands(text).any(|command| {
        matches!(
            (command.name().as_str(), command.subcommand().as_str()),
            ("CONFIG", "GET") | ("CLIENT", "LIST") | ("ACL", "LIST" | "GETUSER")
        )
    })
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
        ("CONFIG", "HELP") => Read,
        // These two answer with what a Read-only profile should not be handed
        // unasked: `requirepass` and `masterauth`, and every connected
        // client's address and name. This is where the human path's line is
        // drawn, so they are rated writes and Read-only asks. Their siblings
        // stay reads -- `CONFIG HELP`, and on `CLIENT` `INFO`, `ID`, `GETNAME`,
        // `GETREDIR`, `TRACKINGINFO` and `HELP` -- and so do `ACL LIST` and
        // `ACL GETUSER`, because the human is reading their own server. The
        // agent path refuses all four through [`discloses`]. The client fields
        // in `ACL LOG` and `SLOWLOG GET` stay reads on both paths.
        ("CONFIG", "GET") => Write,
        ("CONFIG", "SET" | "REWRITE" | "RESETSTAT") => Drop,
        ("CLIENT", "PAUSE") => Drop,
        ("CLIENT", "INFO" | "ID" | "GETNAME" | "GETREDIR" | "TRACKINGINFO" | "HELP") => Read,
        (
            "CLIENT",
            "LIST" | "KILL" | "UNPAUSE" | "SETNAME" | "SETINFO" | "NO-EVICT" | "NO-TOUCH"
            | "TRACKING" | "CACHING" | "UNBLOCK",
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

/// Commands that take data out of the database, one word each: what the agent
/// path's GATE-07 refuses on Redis whatever the mode. The human path never
/// reads this list. Names that decide by an argument or a subcommand
/// (`MIGRATE`, `XGROUP DESTROY`, an `XADD` that trims, the `EXPIRE` family,
/// and a non-positive `EX` or `PX` on `HSETEX` and `HGETEX`) are in
/// [`removes`].
///
/// ponytail: these overwrite or move data without a removal flag and stay
/// approvable (D-23): `RENAME`, `COPY ... REPLACE`, `RESTORE ... REPLACE` and
/// every `*STORE` command overwrite their destination, and `LMOVE`, `BLMOVE`,
/// `RPOPLPUSH`, `BRPOPLPUSH`, `SMOVE` and `MOVE` hand what leaves one key to
/// another. The ceiling is that a pure function has no clock and does not know
/// whether a destination exists, so the human reads the exact command before
/// approving it. An expiry is told from a removal by the statement alone: a
/// pure function has no clock, so an absolute time already past, which removes
/// at once, is approved as an expiry, and the approval's text tells the human
/// so ([`expiry`]). The upgrade path is a removal flag on those words, once the
/// approval surface can show the destination's state.
const REMOVES: &[&str] = &[
    "DEL",
    "UNLINK",
    "GETDEL",
    "DELEX",
    "HDEL",
    "HGETDEL",
    "LPOP",
    "RPOP",
    "BLPOP",
    "BRPOP",
    "LMPOP",
    "BLMPOP",
    "LREM",
    "LTRIM",
    "SREM",
    "SPOP",
    "ZREM",
    "ZPOPMIN",
    "ZPOPMAX",
    "BZPOPMIN",
    "BZPOPMAX",
    "ZMPOP",
    "BZMPOP",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
    "ZREMRANGEBYLEX",
    "XDEL",
    "XTRIM",
    "XACKDEL",
    "XDELEX",
    "JSON.DEL",
    "JSON.FORGET",
    "JSON.CLEAR",
    "JSON.ARRPOP",
    "JSON.ARRTRIM",
    "TS.DEL",
    "TS.DELETERULE",
    "CF.DEL",
    "FT.ALIASDEL",
    "FT.DICTDEL",
    "FT.SUGDEL",
    "VREM",
    "TDIGEST.RESET",
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
            ("CONFIG GET maxmemory", &write),
            ("CONFIG HELP", &read),
            ("config set maxmemory 1", &drop),
            ("CONFIG REWRITE", &drop),
            ("CLIENT LIST", &write),
            ("CLIENT ID", &read),
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
    fn a_disclosing_command_has_the_same_verdict_first_or_last_in_a_buffer() {
        for (first, second) in [
            ("CONFIG GET requirepass", "GET a"),
            ("CLIENT LIST", "GET a"),
        ] {
            let forward = classify(&format!("{first}\n{second}"));
            assert_eq!(forward, Verdict::WRITE, "{first} then {second}");
            assert_eq!(
                classify(&format!("{second}\n{first}")),
                forward,
                "{second} then {first}"
            );
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

    #[test]
    fn the_two_commands_that_return_secrets_are_split_from_their_siblings_by_name() {
        let write = Verdict::WRITE;
        for text in ["config get requirepass", "CLIENT LIST TYPE normal"] {
            assert_eq!(classify(text), write, "{text}");
        }
        for text in [
            "CONFIG HELP",
            "CLIENT ID",
            "CLIENT INFO",
            "CLIENT GETNAME",
            "CLIENT GETREDIR",
            "CLIENT TRACKINGINFO",
            "CLIENT HELP",
        ] {
            assert_eq!(classify(text), Verdict::READ, "{text}");
        }
    }

    #[test]
    fn listing_clients_leaves_no_session_setting_but_naming_one_does() {
        assert_eq!(leaves("CLIENT LIST"), Leaves::default());
        assert!(leaves("CLIENT SETNAME me").settings);
        assert!(leaves("CLIENT NO-TOUCH ON").settings);
    }

    #[test]
    fn every_removal_and_script_name_is_one_the_tables_know() {
        for name in REMOVES {
            assert!(WRITES.contains(name), "{name} is not a write");
        }
        for name in ["EVAL", "EVALSHA", "FCALL"] {
            assert!(SCRIPTS.contains(&name), "{name}");
            assert!(runs_script(&format!("{name} x 0")), "{name}");
        }
        for name in ["EVAL_RO", "EVALSHA_RO", "FCALL_RO"] {
            assert!(READS.contains(&name), "{name}");
            assert!(runs_script(&format!("{name} x 0")), "{name}");
        }
        assert!(runs_script("SCRIPT LOAD \"return 1\""));
        assert!(runs_script("function load code"));
        assert!(!runs_script("SCRIPT EXISTS abc"));
        assert!(!runs_script("GET a"));
        for text in [
            "CONFIG GET x",
            "config get *",
            "CLIENT LIST",
            "GET a\nCLIENT LIST",
            "ACL LIST",
            "acl getuser u",
            "ACL GETUSER u",
        ] {
            assert!(discloses(text), "{text}");
        }
        for text in [
            "CONFIG HELP",
            "CLIENT INFO",
            "ACL WHOAMI",
            "ACL USERS",
            "ACL LOG",
            "SLOWLOG GET",
        ] {
            assert!(!discloses(text), "{text}");
        }
    }

    #[test]
    fn an_expiry_removes_only_with_a_timeout_that_is_not_positive() {
        assert!(!removes("EXPIRE k 10"));
        assert!(!removes("hexpire k 10 FIELDS 1 f"));
        for text in [
            "EXPIRE k 0",
            "EXPIRE k -5",
            "EXPIRE k 010",
            "EXPIRE k +5",
            "EXPIRE k abc",
            "EXPIRE k",
            "PEXPIRE k 0",
            "HEXPIRE k 0 FIELDS 1 f",
            "HPEXPIRE k -1 FIELDS 1 f",
        ] {
            assert!(removes(text), "{text}");
        }
        // The `*EXPIREAT` commands cannot be told a time in the past from one
        // in the future without a clock, so they are expiries the approval
        // names, not removals.
        assert!(!removes("EXPIREAT k 1"));
        assert!(!removes("PEXPIREAT k 1"));
    }

    #[test]
    fn a_stream_add_removes_only_when_it_trims() {
        for text in [
            "XADD s * f v",
            "XADD s NOMKSTREAM * f v",
            // The grid's guard read: `MAXLEN` decides only on `XADD`.
            "LPOS l v COUNT 0 MAXLEN 1",
        ] {
            assert!(!removes(text), "{text}");
        }
        for text in [
            "XADD s MAXLEN 0 * f v",
            "XADD s MAXLEN ~ 1000 * f v",
            "XADD s MINID = 9999999999999-0 * f v",
            "xadd s minid 1 * f v",
            "XADD s NOMKSTREAM MAXLEN ~ 10 LIMIT 5 * f v",
        ] {
            assert!(removes(text), "{text}");
        }
    }

    #[test]
    fn a_set_or_getex_with_a_timeout_or_a_fixed_time_is_an_expiry_of_the_key() {
        for text in [
            "SET k v EXAT 1",
            "SET k v PXAT 1",
            "set k v exat 4102444800",
            "SET k v NX PXAT 1 GET",
            "SET k v EX 10",
            "SET k v PX 10",
            "GETEX k EX 10",
            "GETEX k EXAT 1",
            "GETEX k PXAT 1",
            // A value spelled like the option is read as the option, which
            // fails safe.
            "SET k EXAT",
        ] {
            assert_eq!(expiry(text), Some(Expiry::Key), "{text}");
            assert!(!removes(text), "{text}");
        }
    }

    #[test]
    fn a_command_that_schedules_no_removal_is_no_expiry() {
        for text in [
            "SET k v",
            "SET k v KEEPTTL",
            "GETEX k PERSIST",
            "SET k v EX 0",
            "GETEX k PX 0",
            "SET k v EX abc",
            "GET k",
            "PERSIST k",
            "EXPIRE k 0",
            "EXPIRE k -1",
            "HEXPIRE k 0 FIELDS 1 f",
            "HSETEX h EX 0 FIELDS 1 f v",
            "HGETEX h PERSIST FIELDS 1 f",
            "HSET h f v",
            "HGETEX h FIELDS 1 f",
            // The server rejects these, so they schedule nothing and remove
            // nothing.
            "SETEX k 0 v",
            "PSETEX k -1 v",
            "SETEX k 010 v",
        ] {
            assert_eq!(expiry(text), None, "{text}");
        }
    }

    #[test]
    fn a_hash_command_with_a_timeout_or_a_fixed_time_is_an_expiry_of_its_fields() {
        for text in [
            "HEXPIRE h 10 FIELDS 1 f",
            "HPEXPIRE h 10 FIELDS 1 f",
            "hexpire h 10 fields 1 f",
            "HEXPIREAT h 1 FIELDS 1 f",
            "HPEXPIREAT h 1 FIELDS 1 f",
            "HSETEX h EX 10 FIELDS 1 f v",
            "HSETEX h FXX EXAT 1 FIELDS 1 f v",
            "HSETEX h PXAT 1 FIELDS 1 f v",
            "HGETEX h PX 1 FIELDS 1 f",
            "HGETEX h EXAT 1 FIELDS 1 f",
            "hgetex h pxat 1 fields 1 f",
        ] {
            assert_eq!(expiry(text), Some(Expiry::Field), "{text}");
            assert!(!removes(text), "{text}");
        }
    }

    #[test]
    fn an_expire_or_setex_with_a_positive_timeout_or_any_fixed_time_is_an_expiry_of_the_key() {
        for text in [
            "EXPIRE k 10",
            "PEXPIRE k 10",
            "EXPIRE k 10 NX",
            "EXPIREAT k 1",
            "PEXPIREAT k 1",
            "expireat k 4102444800",
            // No time at all is over-named, which fails safe.
            "EXPIREAT k",
            "SETEX k 10 v",
            "PSETEX k 10 v",
            "setex k 10 v",
        ] {
            assert_eq!(expiry(text), Some(Expiry::Key), "{text}");
            assert!(!removes(text), "{text}");
        }
    }

    #[test]
    fn a_hash_field_expiry_that_is_not_positive_removes_the_field() {
        // Redis 8.10.3 deletes the field at once for a zero `EX` or `PX` on
        // `HSETEX` and `HGETEX`, where `SET` refuses the same timeout.
        for text in [
            "HSETEX h EX 0 FIELDS 1 f v",
            "HSETEX h PX 0 FIELDS 1 f v",
            "HGETEX h EX 0 FIELDS 1 f",
            "hgetex h px 00 fields 1 f",
            "HSETEX h FNX EX -1 FIELDS 1 f v",
            "HGETEX h EX abc FIELDS 1 f",
            "HGETEX h EX",
        ] {
            assert!(removes(text), "{text}");
        }
        for text in [
            "HSETEX h EX 10 FIELDS 1 f v",
            "HSETEX h PXAT 1 FIELDS 1 f v",
            "HGETEX h PX 1 FIELDS 1 f",
            "HGETEX h EXAT 1 FIELDS 1 f",
            "HGETEX h PERSIST FIELDS 1 f",
            "SET k v EX 0",
            "GETEX k PX 0",
        ] {
            assert!(!removes(text), "{text}");
        }
    }

    #[test]
    fn a_removal_is_found_wherever_the_line_sits_and_however_it_is_cased() {
        assert!(removes("GET a\ndel a"));
        assert!(removes("# note\nXGROUP destroy s g"));
        assert!(removes("MIGRATE h 6379 k 0 1000"));
        assert!(!removes("SET k v"));
        assert!(!removes("RENAME a b"));
        assert!(!removes("SET k \"open"));
    }
}

//! What the grid writes back to Redis: the commands an edit, a new row and a
//! deleted row become, and the gate they pass.
//!
//! A grid that can be written to names what it shows in its `EditTarget`:
//! the keys grid has the database as its schema and [`KEYS`] as its table, and
//! a key opened as a grid of its own has its type as its schema ([`HASH`],
//! [`LIST`], [`SET`], [`ZSET`], [`STRING`]) and its name, spelled as one word,
//! as its table. Each of its cells is tagged with how its text stands for the
//! bytes behind it, and only text that is the whole of them is written back.
//!
//! Nothing here clobbers what it did not read: a new key is `SET … NX`, a new
//! field `HSETNX`, a new member `SADD` or `ZADD NX`, a rename `RENAMENX`, and
//! the connection turns the reply that says nothing happened into an error.

use std::hash::{BuildHasher, RandomState};
use std::time::{SystemTime, UNIX_EPOCH};

use super::browse::KEYS;
use super::{Command, parse, quoted, spelled, split};
use crate::result_grid::{NewValue, PendingRow};

/// A cell's tag: text that is the whole of its value.
pub(crate) const STRING: &str = "string";
/// Bytes that are not UTF-8, shown as redis-cli's quoted spelling of them.
pub(crate) const BYTES: &str = "bytes";
pub(crate) const INTEGER: &str = "integer";
pub(crate) const DOUBLE: &str = "double";
/// The start of a string or a document too long for the keys grid to hold.
pub(crate) const TRUNCATED: &str = "truncated";
/// A whole JSON document, as the keys grid shows one.
pub(crate) const JSON: &str = "json";
/// What the keys grid shows for a key it does not hold the value of: a
/// collection's size, or nothing for a type it has no read for.
pub(crate) const SUMMARY: &str = "summary";

const TAGS: [&str; 7] = [STRING, BYTES, INTEGER, DOUBLE, TRUNCATED, JSON, SUMMARY];

/// The schema of an opened key's grid: the key's type.
pub(crate) const HASH: &str = "hash";
pub(crate) const LIST: &str = "list";
pub(crate) const SET: &str = "set";
pub(crate) const ZSET: &str = "zset";

/// What every deleted list item is set to before the `LREM` that takes it out.
const SENTINEL: &str = "dbdelve:deleted:";

/// The tag `name` is, for a snapshot read back from disk.
pub(crate) fn cell_type(name: &str) -> Option<&'static str> {
    TAGS.into_iter().find(|tag| *tag == name)
}

/// Whether a cell tagged `tag` holds what an edit would write back.
pub(crate) fn writes_back(tag: &str) -> bool {
    matches!(tag, STRING | INTEGER | DOUBLE)
}

/// Whether a key column of a grid can be edited: a key's name, a member, a
/// score and a string's value are what an edit changes. A hash field and a
/// list index are where a value is, and are not.
pub(crate) fn edits_key_column(column: &str) -> bool {
    matches!(column, "key" | "member" | "score" | "value")
}

/// The columns of each grid an edit or a new row can fill.
fn columns(schema: &str) -> &'static [&'static str] {
    match schema {
        HASH => &["field", "value"],
        LIST => &["index", "value"],
        SET => &["member"],
        ZSET => &["member", "score"],
        STRING => &["value"],
        _ => &["key", "type", "ttl", "value"],
    }
}

/// Each pending row as the commands that write it, in the order the rows
/// were edited. A set member or a zset member renamed is a `MULTI` of its
/// removal and its addition, so neither lands without the other.
pub(crate) fn update_batch(rows: &[PendingRow]) -> Result<String, String> {
    if rows.is_empty() {
        return Err("There are no edits to apply.".into());
    }
    let mut lines = Vec::new();
    for row in rows {
        for (column, _) in &row.sets {
            if !columns(&row.schema).contains(&column.as_str()) {
                return Err(format!("{column} is not something an edit can change."));
            }
            if !type_of(&row.types, column).is_some_and(writes_back) {
                return Err(format!(
                    "{column} does not hold its whole value as text, so it is not written back."
                ));
            }
        }
        let set = |column: &str| {
            row.sets
                .iter()
                .find(|(name, _)| name == column)
                .map(|(_, value)| value)
        };
        let typed = |column: &str| typed_value(column, set(column));
        let held = |column: &str| fetched(&row.keys, &row.types, column);
        match row.schema.as_str() {
            HASH => lines.push(format!(
                "HSET {} {} {}",
                key_word(&row.table)?,
                held("field")?,
                typed("value")?
            )),
            LIST => lines.push(format!(
                "LSET {} {} {}",
                key_word(&row.table)?,
                held("index")?,
                typed("value")?
            )),
            SET => {
                let key = key_word(&row.table)?;
                lines.extend([
                    "MULTI".to_string(),
                    format!("SREM {key} {}", held("member")?),
                    format!("SADD {key} {}", typed("member")?),
                    "EXEC".to_string(),
                ]);
            }
            ZSET => {
                let key = key_word(&row.table)?;
                let score = match set("score") {
                    Some(value) => score(value)?,
                    None => held("score")?,
                };
                match set("member") {
                    None => lines.push(format!("ZADD {key} XX {score} {}", held("member")?)),
                    Some(_) => lines.extend([
                        "MULTI".to_string(),
                        format!("ZREM {key} {}", held("member")?),
                        format!("ZADD {key} {score} {}", typed("member")?),
                        "EXEC".to_string(),
                    ]),
                }
            }
            STRING => lines.push(format!(
                "SET {} {} KEEPTTL",
                key_word(&row.table)?,
                typed("value")?
            )),
            _ => {
                keyspace(&row.table)?;
                let key = held("key")?;
                if set("value").is_some() {
                    lines.push(format!("SET {key} {} KEEPTTL", typed("value")?));
                }
                match set("ttl") {
                    None => {}
                    Some(NewValue::Null) => lines.push(format!("PERSIST {key}")),
                    Some(NewValue::Value(text)) if text.is_empty() => {
                        lines.push(format!("PERSIST {key}"));
                    }
                    Some(NewValue::Value(text)) => {
                        lines.push(format!("EXPIRE {key} {}", ttl(text)?))
                    }
                    Some(NewValue::Default) => {
                        return Err("Redis has no default for ttl to take.".into());
                    }
                }
                // Last, so the row's other edits still name the key.
                if set("key").is_some() {
                    lines.push(format!("RENAMENX {key} {}", typed("key")?));
                }
            }
        }
    }
    Ok(lines.join("\n"))
}

/// The command a new row is: a key that does not exist yet, or a field,
/// item or member of the opened key, never written over one already there.
/// A field left empty is left out, and so is a list item's index, which is
/// wherever `RPUSH` puts it.
pub(crate) fn insert_row(
    schema: &str,
    table: &str,
    columns: &[(&str, Option<&str>)],
) -> Result<String, String> {
    if schema == STRING {
        return Err("A string holds one value: edit it rather than adding a row.".into());
    }
    // What a field left empty means where it is not text: nothing.
    let optional = |column: &str| matches!(column, "type" | "ttl" | "index" | "score");
    let filled = |column: &str| -> Option<Option<&str>> {
        columns
            .iter()
            .find(|(name, value)| *name == column && !(optional(name) && *value == Some("")))
            .map(|(_, value)| *value)
    };
    for &(column, value) in columns {
        if optional(column) && value == Some("") {
            continue;
        }
        if !self::columns(schema).contains(&column) {
            return Err(format!("{column} is not something a new row can set."));
        }
        if (schema, column) == (LIST, "index") {
            return Err("A new item goes at the end of the list: leave its index empty.".into());
        }
    }
    let required = |column: &str| -> Result<String, String> {
        match filled(column) {
            Some(Some(text)) => Ok(quoted(text)),
            Some(None) => Err(format!(
                "{column} cannot be NULL: Redis has no NULL to store."
            )),
            None => Err(format!("A new row needs a {column}.")),
        }
    };
    Ok(match schema {
        HASH => format!(
            "HSETNX {} {} {}",
            key_word(table)?,
            required("field")?,
            required("value")?
        ),
        LIST => format!("RPUSH {} {}", key_word(table)?, required("value")?),
        SET => format!("SADD {} {}", key_word(table)?, required("member")?),
        ZSET => {
            let score = match filled("score") {
                Some(Some(text)) => score(&NewValue::Value(text.to_string().into()))?,
                Some(None) => return Err("score cannot be NULL.".into()),
                None => return Err("A new row needs a score.".into()),
            };
            format!(
                "ZADD {} NX {score} {}",
                key_word(table)?,
                required("member")?
            )
        }
        _ => {
            keyspace(table)?;
            match filled("type") {
                None | Some(Some("string")) => {}
                Some(_) => return Err("A new row is a string key: leave its type empty.".into()),
            }
            let set = format!("SET {} {} NX", required("key")?, required("value")?);
            match filled("ttl") {
                None | Some(None) => set,
                Some(Some(text)) => format!("{set} EX {}", ttl(text)?),
            }
        }
    })
}

/// The command a deleted row is: the key itself on the keys grid and on a
/// string's, else the field, item or member it names. A list item is named
/// by its index, which `LREM` cannot take, so it is set to a word nothing
/// else holds and that word removed, in one `MULTI`.
pub(crate) fn delete_row(
    schema: &str,
    table: &str,
    keys: &[(&str, &str)],
    types: &[(String, String)],
) -> Result<String, String> {
    let keys: Vec<(String, String)> = keys
        .iter()
        .map(|&(column, value)| (column.to_string(), value.to_string()))
        .collect();
    let held = |column: &str| fetched(&keys, types, column);
    Ok(match schema {
        HASH => format!("HDEL {} {}", key_word(table)?, held("field")?),
        LIST => {
            let key = key_word(table)?;
            let sentinel = sentinel();
            [
                "MULTI".to_string(),
                format!("LSET {key} {} {sentinel}", held("index")?),
                format!("LREM {key} 1 {sentinel}"),
                "EXEC".to_string(),
            ]
            .join("\n")
        }
        SET => format!("SREM {} {}", key_word(table)?, held("member")?),
        ZSET => format!("ZREM {} {}", key_word(table)?, held("member")?),
        STRING => format!("UNLINK {}", key_word(table)?),
        _ => {
            keyspace(table)?;
            format!("UNLINK {}", held("key")?)
        }
    })
}

/// One edit, new row or deleted row, as the gate reads it back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Edit,
    Insert,
    /// What the deleted row was named by.
    Delete(&'static [&'static str]),
    /// The key deleted whole, named by its name on the keys grid and by its
    /// one value on a string's.
    Unlink,
}

/// Whether `text` is a write the grid could have generated, read back out of
/// the parse rather than trusted because this module wrote it:
/// `sql::is_generated_write` for Redis.
///
/// Exactly one of three shapes, each command spelled as this module spells
/// it, one to a line, with no comment or blank line between:
///
/// - one or more edits: `SET k v KEEPTTL`, `EXPIRE k n` with `n` above zero
///   (zero would delete the key), `PERSIST k`, `RENAMENX k new`, `HSET k f v`,
///   `LSET k i v`, `ZADD k XX score m`, and the `MULTI` of a `SREM` and an
///   `SADD`, or of a `ZREM` and a `ZADD`, on one key;
/// - one new row: `SET k v NX` (with `EX n`), `HSETNX k f v`, `RPUSH k v`,
///   `SADD k m` or `ZADD k NX score m`;
/// - one deleted row: `UNLINK k`, `HDEL k f`, `SREM k m`, `ZREM k m`, or the
///   `MULTI` of an `LSET` to a sentinel and the `LREM` of that sentinel.
pub(crate) fn is_generated_write(text: &str) -> bool {
    match units(text).as_deref() {
        Some([Unit::Insert | Unit::Delete(_) | Unit::Unlink]) => true,
        Some(units) => !units.is_empty() && units.iter().all(|unit| *unit == Unit::Edit),
        None => false,
    }
}

/// `sql::delete_matches_key` for Redis: whether `text` is one deleted row,
/// named by exactly the columns `keys` holds, as a set. A readout, not a gate.
pub(crate) fn delete_matches_key(text: &str, keys: &[&str]) -> bool {
    let named: &[&str] = match units(text).as_deref() {
        Some(&[Unit::Delete(named)]) => named,
        Some([Unit::Unlink]) => match keys {
            ["value"] => &["value"],
            _ => &["key"],
        },
        _ => return false,
    };
    let mut keys = keys.to_vec();
    keys.sort_unstable();
    keys.dedup();
    let mut named = named.to_vec();
    named.sort_unstable();
    keys == named
}

fn units(text: &str) -> Option<Vec<Unit>> {
    let commands = parse(text).ok()?;
    let spelled: Vec<String> = commands.iter().map(spelling).collect();
    if spelled.join("\n") != text {
        return None;
    }
    let words: Vec<Vec<&[u8]>> = commands
        .iter()
        .map(|command| command.words.iter().map(Vec::as_slice).collect())
        .collect();
    let mut units = Vec::new();
    let mut rest = words.as_slice();
    while let [first, after @ ..] = rest {
        if first.as_slice() == [b"MULTI".as_slice()] {
            let [body, second, end, after @ ..] = after else {
                return None;
            };
            if end.as_slice() != [b"EXEC".as_slice()] {
                return None;
            }
            units.push(transaction(body, second)?);
            rest = after;
        } else {
            units.push(single(first)?);
            rest = after;
        }
    }
    Some(units)
}

fn single(words: &[&[u8]]) -> Option<Unit> {
    use Unit::*;
    Some(match words {
        [b"SET", _, _, b"KEEPTTL"]
        | [b"PERSIST", _]
        | [b"RENAMENX", _, _]
        | [b"HSET", _, _, _]
        | [b"ZADD", _, b"XX", _, _] => Edit,
        [b"EXPIRE", _, seconds] if positive(seconds) => Edit,
        [b"LSET", _, index, _] if whole(index) => Edit,
        [b"SET", _, _, b"NX"]
        | [b"HSETNX", _, _, _]
        | [b"RPUSH", _, _]
        | [b"SADD", _, _]
        | [b"ZADD", _, b"NX", _, _] => Insert,
        [b"SET", _, _, b"NX", b"EX", seconds] if positive(seconds) => Insert,
        [b"UNLINK", _] => Unlink,
        [b"HDEL", _, _] => Delete(&["field"]),
        [b"SREM", _, _] => Delete(&["member"]),
        [b"ZREM", _, _] => Delete(&["member", "score"]),
        _ => return None,
    })
}

fn transaction(first: &[&[u8]], second: &[&[u8]]) -> Option<Unit> {
    match (first, second) {
        ([b"SREM", key, _], [b"SADD", same, _]) if key == same => Some(Unit::Edit),
        ([b"ZREM", key, _], [b"ZADD", same, _, _]) if key == same => Some(Unit::Edit),
        ([b"LSET", key, index, word], [b"LREM", same, b"1", removed])
            if key == same && word == removed && whole(index) && sentinel_word(word) =>
        {
            Some(Unit::Delete(&["index"]))
        }
        _ => None,
    }
}

/// A command as this module writes it: each word bare where it can be and
/// quoted where it cannot, one space apart.
fn spelling(command: &Command) -> String {
    command
        .words
        .iter()
        .map(|word| spelled(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn positive(word: &[u8]) -> bool {
    std::str::from_utf8(word).is_ok_and(|digits| {
        digits.bytes().all(|byte| byte.is_ascii_digit())
            && digits.parse::<i64>().is_ok_and(|n| n > 0)
    })
}

fn whole(word: &[u8]) -> bool {
    std::str::from_utf8(word)
        .is_ok_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

fn sentinel_word(word: &[u8]) -> bool {
    word.strip_prefix(SENTINEL.as_bytes())
        .is_some_and(|hex| hex.len() == 32 && hex.iter().all(u8::is_ascii_hexdigit))
}

fn sentinel() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{SENTINEL}{:016x}{:016x}",
        RandomState::new().hash_one(nanos),
        RandomState::new().hash_one(nanos)
    )
}

/// The opened key's name, which its grid holds already spelled as one word.
fn key_word(table: &str) -> Result<String, String> {
    match split(table).as_deref() {
        Ok([_]) => Ok(table.to_string()),
        _ => Err(format!("{table} does not name one key.")),
    }
}

fn keyspace(table: &str) -> Result<(), String> {
    match table == KEYS {
        true => Ok(()),
        false => Err(format!("{table} is not a grid DBDelve writes to.")),
    }
}

/// A cell as the server sent it, as the word that names the same bytes.
fn fetched(
    keys: &[(String, String)],
    types: &[(String, String)],
    column: &str,
) -> Result<String, String> {
    let text = keys
        .iter()
        .find(|(name, _)| name == column)
        .map(|(_, text)| text.as_str())
        .ok_or_else(|| format!("This row's {column} is not known."))?;
    match type_of(types, column) {
        Some(BYTES) if matches!(split(text).as_deref(), Ok([_])) => Ok(text.to_string()),
        Some(STRING | INTEGER | DOUBLE) => Ok(quoted(text)),
        _ => Err(format!(
            "DBDelve cannot tell which bytes this row's {column} is, so it will not name it."
        )),
    }
}

/// What a cell was edited to, as one word.
fn typed_value(column: &str, value: Option<&NewValue>) -> Result<String, String> {
    match value {
        Some(NewValue::Value(text)) => Ok(quoted(text)),
        Some(NewValue::Null) => Err(format!(
            "{column} cannot be NULL: Redis has no NULL to store."
        )),
        Some(NewValue::Default) => Err(format!("Redis has no default for {column} to take.")),
        None => Err(format!("{column} was not edited.")),
    }
}

fn score(value: &NewValue) -> Result<String, String> {
    match value {
        NewValue::Value(text) if text.parse::<f64>().is_ok_and(|score| !score.is_nan()) => {
            Ok(quoted(text))
        }
        NewValue::Value(text) => Err(format!("{text} is not a score: a score is a number.")),
        NewValue::Null | NewValue::Default => Err("score cannot be NULL.".into()),
    }
}

fn ttl(text: &str) -> Result<String, String> {
    match positive(text.as_bytes()) {
        true => Ok(text.to_string()),
        false => Err(format!(
            "{text} is not a TTL: it is a number of seconds above zero, and zero or less would \
             delete the key."
        )),
    }
}

fn type_of<'a>(types: &'a [(String, String)], column: &str) -> Option<&'a str> {
    types
        .iter()
        .find(|(name, _)| name == column)
        .map(|(_, tag)| tag.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{Mode, Stop, gate};

    fn row(
        schema: &str,
        table: &str,
        sets: &[(&str, NewValue)],
        keys: &[(&str, &str)],
        types: &[(&str, &str)],
    ) -> PendingRow {
        PendingRow {
            schema: schema.into(),
            table: table.into(),
            sets: sets
                .iter()
                .map(|(column, value)| (column.to_string(), value.clone()))
                .collect(),
            keys: keys
                .iter()
                .map(|(column, value)| (column.to_string(), value.to_string()))
                .collect(),
            types: types
                .iter()
                .map(|(column, tag)| (column.to_string(), tag.to_string()))
                .collect(),
        }
    }

    fn value(text: &str) -> NewValue {
        NewValue::Value(text.to_string().into())
    }

    const KEYS_TYPES: [(&str, &str); 3] = [("key", STRING), ("ttl", INTEGER), ("value", STRING)];

    fn words(text: &str) -> Vec<Vec<Vec<u8>>> {
        parse(text)
            .unwrap()
            .into_iter()
            .map(|command| command.words)
            .collect()
    }

    /// Every generated write passes the gate, and Read-only refuses it.
    fn admitted(text: &str) -> &str {
        assert!(is_generated_write(text), "{text}");
        let verdict = crate::sql::classify(crate::db::Engine::Redis, text);
        assert_eq!(
            gate(&verdict, Mode::ReadOnly, &[]),
            Some(Stop::Upgrade(Mode::ReadWrite)),
            "{text}"
        );
        assert_eq!(gate(&verdict, Mode::ReadWrite, &[]), None, "{text}");
        text
    }

    #[test]
    fn a_keys_grid_edit_keeps_the_ttl_and_renames_last_without_clobbering() {
        let edited = row(
            "db0",
            KEYS,
            &[
                ("key", value("user:2 \"new\"")),
                ("value", value("a\nb é")),
                ("ttl", value("60")),
            ],
            &[("key", "user:1")],
            &KEYS_TYPES,
        );
        let batch = update_batch(&[edited]).unwrap();
        assert_eq!(
            admitted(&batch),
            "SET user:1 \"a\\nb é\" KEEPTTL\nEXPIRE user:1 60\nRENAMENX user:1 \"user:2 \\\"new\\\"\""
        );
        assert_eq!(words(&batch)[0][2], "a\nb é".as_bytes());
        assert_eq!(words(&batch)[2][2], b"user:2 \"new\"");

        for cleared in [NewValue::Null, value("")] {
            let persisted = row(
                "db0",
                KEYS,
                &[("ttl", cleared)],
                &[("key", "k")],
                &KEYS_TYPES,
            );
            assert_eq!(admitted(&update_batch(&[persisted]).unwrap()), "PERSIST k");
        }
    }

    #[test]
    fn a_binary_key_is_named_by_its_bytes() {
        let name = crate::redis_cmd::quoted_bytes(b"\x00\xff k");
        let edited = row(
            "db0",
            KEYS,
            &[("ttl", value("5"))],
            &[("key", &name)],
            &[("key", BYTES), ("ttl", INTEGER)],
        );
        let batch = update_batch(&[edited]).unwrap();
        assert_eq!(words(admitted(&batch))[0][1], b"\x00\xff k");
        // The same text read as UTF-8 is the text, quoted.
        let text = row(
            "db0",
            KEYS,
            &[("ttl", value("5"))],
            &[("key", &name)],
            &KEYS_TYPES,
        );
        let batch = update_batch(&[text]).unwrap();
        assert_eq!(words(&batch)[0][1], name.as_bytes());
    }

    #[test]
    fn what_is_not_the_whole_value_is_not_written_back() {
        for tag in [TRUNCATED, BYTES, SUMMARY, JSON] {
            let edited = row(
                "db0",
                KEYS,
                &[("value", value("x"))],
                &[("key", "k")],
                &[("key", STRING), ("value", tag)],
            );
            assert!(update_batch(&[edited]).is_err(), "{tag}");
        }
        let untagged = row("db0", KEYS, &[("value", value("x"))], &[("key", "k")], &[]);
        assert!(update_batch(&[untagged]).is_err());
        assert!(!writes_back(TRUNCATED) && writes_back(STRING));
    }

    #[test]
    fn a_ttl_is_seconds_above_zero_and_a_value_is_never_null() {
        for ttl in ["0", "-1", "1.5", "soon", " 5"] {
            let edited = row(
                "db0",
                KEYS,
                &[("ttl", value(ttl))],
                &[("key", "k")],
                &KEYS_TYPES,
            );
            assert!(update_batch(&[edited]).is_err(), "{ttl}");
        }
        let nulled = row(
            "db0",
            KEYS,
            &[("value", NewValue::Null)],
            &[("key", "k")],
            &KEYS_TYPES,
        );
        assert!(update_batch(&[nulled]).is_err());
        let defaulted = row(
            "db0",
            KEYS,
            &[("value", NewValue::Default)],
            &[("key", "k")],
            &KEYS_TYPES,
        );
        assert!(update_batch(&[defaulted]).is_err());
    }

    #[test]
    fn an_opened_keys_edits_write_its_own_type() {
        let rows = [
            row(
                HASH,
                "user:1",
                &[("value", value("Ada L."))],
                &[("field", "name")],
                &[("field", STRING), ("value", STRING)],
            ),
            row(
                LIST,
                "\"a list\"",
                &[("value", value("v"))],
                &[("index", "2")],
                &[("index", INTEGER), ("value", STRING)],
            ),
            row(
                SET,
                "tags",
                &[("member", value("in memory"))],
                &[("member", "fast")],
                &[("member", STRING)],
            ),
            row(
                ZSET,
                "board",
                &[("score", value("99.5"))],
                &[("member", "ada"), ("score", "100")],
                &[("member", STRING), ("score", DOUBLE)],
            ),
            row(
                ZSET,
                "board",
                &[("member", value("grace"))],
                &[("member", "ada"), ("score", "100")],
                &[("member", STRING), ("score", DOUBLE)],
            ),
            row(
                STRING,
                "greeting",
                &[("value", value("hi"))],
                &[("value", "hello")],
                &[("value", STRING)],
            ),
        ];
        assert_eq!(
            admitted(&update_batch(&rows).unwrap()),
            [
                "HSET user:1 name \"Ada L.\"",
                "LSET \"a list\" 2 v",
                "MULTI",
                "SREM tags fast",
                "SADD tags \"in memory\"",
                "EXEC",
                "ZADD board XX 99.5 ada",
                "MULTI",
                "ZREM board ada",
                "ZADD board 100 grace",
                "EXEC",
                "SET greeting hi KEEPTTL",
            ]
            .join("\n")
        );
        let field = row(
            HASH,
            "h",
            &[("field", value("renamed"))],
            &[("field", "f")],
            &[("field", STRING)],
        );
        assert!(update_batch(&[field]).is_err());
        let nan = row(
            ZSET,
            "z",
            &[("score", value("NaN"))],
            &[("member", "m"), ("score", "1")],
            &[("member", STRING), ("score", DOUBLE)],
        );
        assert!(update_batch(&[nan]).is_err());
        assert!(
            edits_key_column("member") && !edits_key_column("field") && !edits_key_column("index")
        );
    }

    #[test]
    fn a_new_row_never_writes_over_what_is_there() {
        let insert = |schema: &str, table: &str, columns: &[(&str, Option<&str>)]| {
            insert_row(schema, table, columns)
        };
        for (text, expected) in [
            (
                insert(
                    "db0",
                    KEYS,
                    &[
                        ("key", Some("a b")),
                        ("value", Some("")),
                        ("type", Some("")),
                    ],
                ),
                "SET \"a b\" \"\" NX",
            ),
            (
                insert(
                    "db0",
                    KEYS,
                    &[
                        ("key", Some("k")),
                        ("value", Some("v")),
                        ("ttl", Some("30")),
                    ],
                ),
                "SET k v NX EX 30",
            ),
            (
                insert(HASH, "h", &[("field", Some("f")), ("value", Some("v"))]),
                "HSETNX h f v",
            ),
            (
                insert(LIST, "l", &[("index", Some("")), ("value", Some("v"))]),
                "RPUSH l v",
            ),
            (insert(SET, "s", &[("member", Some("m"))]), "SADD s m"),
            (
                insert(ZSET, "z", &[("member", Some("m")), ("score", Some("-inf"))]),
                "ZADD z NX -inf m",
            ),
        ] {
            assert_eq!(admitted(&text.unwrap()), expected);
        }
        for refused in [
            insert("db0", KEYS, &[("key", Some("k"))]),
            insert("db0", KEYS, &[("key", Some("k")), ("value", None)]),
            insert(
                "db0",
                KEYS,
                &[
                    ("key", Some("k")),
                    ("value", Some("v")),
                    ("type", Some("hash")),
                ],
            ),
            insert(
                "db0",
                KEYS,
                &[("key", Some("k")), ("value", Some("v")), ("ttl", Some("0"))],
            ),
            insert("db0", "other", &[("key", Some("k")), ("value", Some("v"))]),
            insert(LIST, "l", &[("index", Some("0")), ("value", Some("v"))]),
            insert(STRING, "s", &[("value", Some("v"))]),
            insert(ZSET, "z", &[("member", Some("m"))]),
            insert(
                HASH,
                "two words",
                &[("field", Some("f")), ("value", Some("v"))],
            ),
        ] {
            assert!(refused.is_err(), "{refused:?}");
        }
    }

    #[test]
    fn a_deleted_row_is_named_by_its_key_columns() {
        let delete = |schema: &str, table: &str, keys: &[(&str, &str)], types: &[(&str, &str)]| {
            let types: Vec<(String, String)> = types
                .iter()
                .map(|(column, tag)| (column.to_string(), tag.to_string()))
                .collect();
            delete_row(schema, table, keys, &types).unwrap()
        };
        for (text, keys, expected) in [
            (
                delete("db0", KEYS, &[("key", "k")], &[("key", STRING)]),
                &["key"][..],
                "UNLINK k",
            ),
            (
                delete(HASH, "h", &[("field", "f g")], &[("field", STRING)]),
                &["field"][..],
                "HDEL h \"f g\"",
            ),
            (
                delete(SET, "s", &[("member", "m")], &[("member", STRING)]),
                &["member"][..],
                "SREM s m",
            ),
            (
                delete(
                    ZSET,
                    "z",
                    &[("member", "m"), ("score", "1")],
                    &[("member", STRING), ("score", DOUBLE)],
                ),
                &["score", "member"][..],
                "ZREM z m",
            ),
            (
                delete(STRING, "s", &[("value", "v")], &[("value", STRING)]),
                &["value"][..],
                "UNLINK s",
            ),
        ] {
            assert_eq!(admitted(&text), expected);
            assert!(delete_matches_key(&text, keys), "{text}");
            assert!(!delete_matches_key(&text, &["other"]), "{text}");
        }

        let list = delete(LIST, "l", &[("index", "3")], &[("index", INTEGER)]);
        let commands = words(admitted(&list));
        assert_eq!(commands.len(), 4);
        assert_eq!(
            commands[1][..3],
            [b"LSET".to_vec(), b"l".to_vec(), b"3".to_vec()]
        );
        assert_eq!(commands[1][3], commands[2][3]);
        assert!(delete_matches_key(&list, &["index"]));
        // A second delete writes a sentinel of its own.
        assert_ne!(
            list,
            delete(LIST, "l", &[("index", "3")], &[("index", INTEGER)])
        );
    }

    #[test]
    fn the_gate_admits_nothing_but_the_shapes_it_names() {
        let sentinel = sentinel();
        for text in [
            "",
            "FLUSHDB",
            "DEL k",
            "SET k v",
            "set k v KEEPTTL",
            "SET k v KEEPTTL\nFLUSHDB",
            "SET k v KEEPTTL\n",
            "# note\nSET k v KEEPTTL",
            "SET \"k\" v KEEPTTL",
            "EXPIRE k 0",
            "EXPIRE k -1",
            "EXPIRE k +5",
            "UNLINK a b",
            "UNLINK a\nUNLINK b",
            "SET k v KEEPTTL\nUNLINK k",
            "SET k v NX\nSET j v NX",
            "SET k v NX EX 0",
            "HSET h f v g w",
            "LSET l x v",
            "MULTI\nSREM s a\nSADD t b\nEXEC",
            "MULTI\nSREM s a\nSADD s b",
            "MULTI\nSREM s a\nFLUSHDB\nEXEC",
            "MULTI\nLSET l 1 x\nLREM l 1 x\nEXEC",
            &format!("MULTI\nLSET l 1 {sentinel}\nLREM l 0 {sentinel}\nEXEC"),
            &format!("MULTI\nLSET l 1 {sentinel}\nLREM m 1 {sentinel}\nEXEC"),
            &format!("MULTI\nLSET l 1 {sentinel}\nLREM l 1 {sentinel}\nEXEC\nUNLINK l"),
        ] {
            assert!(!is_generated_write(text), "{text}");
        }
        assert!(is_generated_write(&format!(
            "MULTI\nLSET l 1 {sentinel}\nLREM l 1 {sentinel}\nEXEC"
        )));
        assert!(is_generated_write(
            "SET a 1 KEEPTTL\nPERSIST b\nZADD z XX 1 m"
        ));
    }

    #[test]
    fn every_tag_reads_back_from_a_snapshot() {
        for tag in TAGS {
            assert_eq!(cell_type(tag), Some(tag));
        }
        assert_eq!(cell_type("vectorset"), None);
    }
}

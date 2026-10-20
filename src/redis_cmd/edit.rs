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
//! field `HSETNX`, a new member `SADD` or `ZADD NX`, a rename `RENAMENX`, a
//! string's new value `SET … XX … GET` (so a key deleted or retyped since is
//! not written), a list item's write checked against the item read
//! ([`checked`]), a collection's JSON edit checked against the JSON shown
//! ([`collection_edit`]), and the connection turns the reply that says
//! nothing happened into an error.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, RandomState};
use std::time::{SystemTime, UNIX_EPOCH};

use super::browse::{KEYS, PREVIEWED};
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
/// collection's size, one whose bytes are not all text, or nothing for a
/// type it has no read for.
pub(crate) const SUMMARY: &str = "summary";
/// A small collection's whole JSON on the keys grid, by its type, which says
/// how an edit of it is written ([`collection_edit`]).
pub(crate) const HASH_JSON: &str = "hash json";
pub(crate) const LIST_JSON: &str = "list json";
pub(crate) const SET_JSON: &str = "set json";
pub(crate) const ZSET_JSON: &str = "zset json";

const TAGS: [&str; 11] = [
    STRING, BYTES, INTEGER, DOUBLE, TRUNCATED, JSON, SUMMARY, HASH_JSON, LIST_JSON, SET_JSON,
    ZSET_JSON,
];

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
    matches!(tag, STRING | INTEGER | DOUBLE) || shown_whole(tag).is_some()
}

/// The type of the collection a cell tagged `tag` holds the whole JSON of.
fn shown_whole(tag: &str) -> Option<&'static str> {
    Some(match tag {
        HASH_JSON => HASH,
        LIST_JSON => LIST,
        SET_JSON => SET,
        ZSET_JSON => ZSET,
        _ => return None,
    })
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
/// were edited. A set member or a zset member renamed is the new one added,
/// never over one already there, and only then the old one removed.
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
            LIST => {
                let key = key_word(&row.table)?;
                let index = held("index")?;
                lines.extend(checked(&key, &index, &held("value")?)?);
                lines.push(format!("LSET {key} {index} {}", typed("value")?));
                lines.push("EXEC".into());
            }
            SET => {
                let key = key_word(&row.table)?;
                lines.extend([
                    format!("SADD {key} {}", typed("member")?),
                    format!("SREM {key} {}", held("member")?),
                ]);
            }
            ZSET => {
                let key = key_word(&row.table)?;
                let score = match set("score") {
                    Some(value) => score(value)?,
                    None => held("score")?,
                };
                match set("member") {
                    None => lines.push(format!("ZADD {key} XX CH {score} {}", held("member")?)),
                    Some(_) => lines.extend([
                        format!("ZADD {key} NX {score} {}", typed("member")?),
                        format!("ZREM {key} {}", held("member")?),
                    ]),
                }
            }
            STRING => lines.push(format!(
                "SET {} {} XX KEEPTTL GET",
                key_word(&row.table)?,
                typed("value")?
            )),
            _ => {
                keyspace(&row.table)?;
                let key = held("key")?;
                match (
                    set("value"),
                    type_of(&row.types, "value").and_then(shown_whole),
                ) {
                    (None, _) => {}
                    (Some(value), Some(kind)) => {
                        let shown = row
                            .held
                            .iter()
                            .find(|(column, _)| column == "value")
                            .map(|(_, text)| text.as_str())
                            .ok_or("This row's value is not known.")?;
                        lines.extend(collection_edit(&key, kind, shown, value)?);
                    }
                    (Some(_), None) => {
                        lines.push(format!("SET {key} {} XX KEEPTTL GET", typed("value")?));
                    }
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
    if lines.is_empty() {
        return Err("The edits change nothing.".into());
    }
    Ok(lines.join("\n"))
}

/// A JSON object read only for its names, which `serde_json::Value` would
/// collapse to the last of any name given twice. Holds the first such name.
struct Names(Option<String>);

impl<'de> serde::Deserialize<'de> for Names {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Names;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Names, A::Error> {
                let mut seen = BTreeSet::new();
                let mut twice = None;
                while let Some(name) = map.next_key::<String>()? {
                    map.next_value::<serde::de::IgnoredAny>()?;
                    if !seen.insert(name.clone()) {
                        twice.get_or_insert(name);
                    }
                }
                Ok(Names(twice))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

/// A small collection as the keys grid shows it whole, read from its JSON or
/// from the server's reply, to compare and to diff.
#[derive(Debug, PartialEq)]
pub(crate) enum Collection {
    Hash(BTreeMap<String, String>),
    List(Vec<String>),
    Set(BTreeSet<String>),
    Zset(BTreeMap<String, f64>),
}

impl Collection {
    /// `text` read as the JSON of a collection of `kind`, refused where it is
    /// not that shape: Redis holds text, and a score is a number.
    pub(crate) fn parse(kind: &str, text: &str) -> Result<Self, String> {
        use serde_json::Value as Json;
        let json: Json =
            serde_json::from_str(text).map_err(|error| format!("This is not JSON: {error}."))?;
        if matches!(kind, HASH | ZSET)
            && json.is_object()
            && let Ok(Names(Some(name))) = serde_json::from_str(text)
        {
            return Err(format!(
                "A name can be given only once, and {name} is twice."
            ));
        }
        let string = |value: &Json, what: &str| match value {
            Json::String(text) => Ok(text.clone()),
            other => Err(format!(
                "{what} must be a JSON string, and {other} is not one."
            )),
        };
        let parsed = match (kind, json) {
            (HASH, Json::Object(fields)) => Self::Hash(
                fields
                    .iter()
                    .map(|(field, value)| Ok((field.clone(), string(value, field)?)))
                    .collect::<Result<_, String>>()?,
            ),
            (ZSET, Json::Object(members)) => Self::Zset(
                members
                    .iter()
                    .map(|(member, score)| match score.as_f64() {
                        Some(score) => Ok((member.clone(), score)),
                        None => Err(format!("{member}'s score must be a number, not {score}.")),
                    })
                    .collect::<Result<_, String>>()?,
            ),
            (LIST, Json::Array(items)) => Self::List(
                items
                    .iter()
                    .map(|item| string(item, "Each item"))
                    .collect::<Result<_, _>>()?,
            ),
            (SET, Json::Array(members)) => {
                let mut set = BTreeSet::new();
                for member in &members {
                    let member = string(member, "Each member")?;
                    if !set.insert(member.clone()) {
                        return Err(format!(
                            "A set holds each member once, and {member} is twice."
                        ));
                    }
                }
                Self::Set(set)
            }
            (HASH, _) => return Err("A hash is a JSON object of field to value.".into()),
            (ZSET, _) => return Err("A sorted set is a JSON object of member to score.".into()),
            (LIST, _) => return Err("A list is a JSON array of its items.".into()),
            (SET, _) => return Err("A set is a JSON array of its members.".into()),
            _ => return Err(format!("A {kind} is not edited as JSON.")),
        };
        let empty = match &parsed {
            Self::Hash(map) => map.is_empty(),
            Self::List(items) => items.is_empty(),
            Self::Set(set) => set.is_empty(),
            Self::Zset(map) => map.is_empty(),
        };
        match empty {
            true => {
                Err("Redis holds no empty collection: delete the row to delete the key.".into())
            }
            false => Ok(parsed),
        }
    }
}

/// A score as redis-cli spells it (`3`, not `3.0`), which is also JSON.
pub(crate) fn spelled_score(score: f64) -> String {
    let text =
        serde_json::Number::from_f64(score).map_or_else(|| score.to_string(), |n| n.to_string());
    text.strip_suffix(".0").unwrap_or(&text).to_string()
}

/// A collection's JSON edited on the keys grid, as the opened key's grid
/// would write each difference from the JSON `shown`, nothing where there is
/// none. Like [`checked`], under a `WATCH` and in a `MULTI`, after a read
/// the connection compares with `shown` (the `ECHO`), bounded as the
/// preview's is, so a change made since the read is never written over.
fn collection_edit(
    key: &str,
    kind: &str,
    shown: &str,
    value: &NewValue,
) -> Result<Vec<String>, String> {
    let NewValue::Value(edited) = value else {
        return Err("value cannot be NULL: Redis has no NULL to store.".into());
    };
    let held = Collection::parse(kind, shown)?;
    let writes = changes(key, &held, &Collection::parse(kind, edited)?)?;
    if writes.is_empty() {
        return Ok(writes);
    }
    let read = match kind {
        LIST => format!("LRANGE {key} 0 {PREVIEWED}"),
        HASH => format!("HSCAN {key} 0 COUNT {PREVIEWED}"),
        SET => format!("SSCAN {key} 0 COUNT {PREVIEWED}"),
        _ => format!("ZSCAN {key} 0 COUNT {PREVIEWED}"),
    };
    let mut lines = vec![
        format!("WATCH {key}"),
        format!("ECHO {}", quoted(shown)),
        read,
        "MULTI".to_string(),
    ];
    lines.extend(writes);
    lines.push("EXEC".into());
    Ok(lines)
}

/// The commands that take `key` from `held` to `edited`.
fn changes(key: &str, held: &Collection, edited: &Collection) -> Result<Vec<String>, String> {
    use Collection::*;
    let mut lines = Vec::new();
    match (held, edited) {
        (Hash(held), Hash(edited)) => {
            for (field, value) in edited {
                if held.get(field) != Some(value) {
                    lines.push(format!("HSET {key} {} {}", quoted(field), quoted(value)));
                }
            }
            for field in held.keys().filter(|field| !edited.contains_key(*field)) {
                lines.push(format!("HDEL {key} {}", quoted(field)));
            }
        }
        (Set(held), Set(edited)) => {
            for member in edited.difference(held) {
                lines.push(format!("SADD {key} {}", quoted(member)));
            }
            for member in held.difference(edited) {
                lines.push(format!("SREM {key} {}", quoted(member)));
            }
        }
        (Zset(held), Zset(edited)) => {
            for (member, &score) in edited {
                let flag = match held.get(member) {
                    Some(&old) if old == score => continue,
                    Some(_) => "XX CH",
                    None => "NX",
                };
                lines.push(format!(
                    "ZADD {key} {flag} {} {}",
                    spelled_score(score),
                    quoted(member)
                ));
            }
            for member in held.keys().filter(|member| !edited.contains_key(*member)) {
                lines.push(format!("ZREM {key} {}", quoted(member)));
            }
        }
        (List(held), List(edited)) if held.len() != edited.len() => {
            return Err(format!(
                "The list has {} items here and {} were read: open the key to add or remove items.",
                edited.len(),
                held.len()
            ));
        }
        (List(held), List(edited)) => {
            for (index, (was, item)) in held.iter().zip(edited).enumerate() {
                if was != item {
                    lines.push(format!("LSET {key} {index} {}", quoted(item)));
                }
            }
        }
        _ => return Err("The edit is not the collection that was read.".into()),
    }
    Ok(lines)
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
/// else holds and that word removed, in one `MULTI` that [`checked`] opens.
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
            let index = held("index")?;
            let sentinel = sentinel();
            let mut lines = checked(&key, &index, &held("value")?)?.to_vec();
            lines.extend([
                format!("LSET {key} {index} {sentinel}"),
                format!("LREM {key} 1 {sentinel}"),
                "EXEC".to_string(),
            ]);
            lines.join("\n")
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

/// The start of a write to the list item at `index`, which names whatever
/// sits there now: the key watched, so a change before the `EXEC` voids it,
/// and the server asked where the item read is among the first `index + 1`,
/// which the connection takes for an error unless `index` is the last.
fn checked(key: &str, index: &str, value: &str) -> Result<[String; 3], String> {
    let length = index
        .parse::<u64>()
        .ok()
        .and_then(|index| index.checked_add(1))
        .ok_or_else(|| format!("{index} is not a list index."))?;
    Ok([
        format!("WATCH {key}"),
        format!("LPOS {key} {value} COUNT 0 MAXLEN {length}"),
        "MULTI".to_string(),
    ])
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
/// - one or more edits: `SET k v XX KEEPTTL GET`, `EXPIRE k n` with `n`
///   above zero (zero would delete the key), `PERSIST k`, `RENAMENX k new`,
///   `HSET k f v`, `ZADD k XX CH score m`, on one key an `SADD` then an
///   `SREM`, or a `ZADD k NX score m` then a `ZREM`, an `LSET k i v`
///   in the `MULTI` its [`checked`] opens, and a collection's writes, each of
///   its type, in the `MULTI` a [`collection_edit`] opens;
/// - one new row: `SET k v NX` (with `EX n`), `HSETNX k f v`, `RPUSH k v`,
///   `SADD k m` or `ZADD k NX score m`;
/// - one deleted row: `UNLINK k`, `HDEL k f`, `SREM k m`, `ZREM k m`, or an
///   `LSET` to a sentinel and the `LREM` of that sentinel in the `MULTI`
///   [`checked`] opens.
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
    let lines: Vec<&[&[u8]]> = words.iter().map(Vec::as_slice).collect();
    let mut units = Vec::new();
    let mut rest = lines.as_slice();
    while !rest.is_empty() {
        let (unit, after) = unit(rest)?;
        units.push(unit);
        rest = after;
    }
    Some(units)
}

type Lines<'a> = [&'a [&'a [u8]]];

/// The unit `lines` starts with, and the lines after it.
fn unit<'a, 'b>(lines: &'b Lines<'a>) -> Option<(Unit, &'b Lines<'a>)> {
    Some(match lines {
        [[b"SADD", key, _], [b"SREM", same, _], after @ ..]
        | [[b"ZADD", key, b"NX", _, _], [b"ZREM", same, _], after @ ..]
            if key == same =>
        {
            (Unit::Edit, after)
        }
        [
            [b"WATCH", key],
            [b"LPOS", read, _, b"COUNT", b"0", b"MAXLEN", length],
            [b"MULTI"],
            [b"LSET", set, index, _],
            [b"EXEC"],
            after @ ..,
        ] if [read, set].iter().all(|same| same == &key) && last(index, length) => {
            (Unit::Edit, after)
        }
        [
            [b"WATCH", key],
            [b"LPOS", read, _, b"COUNT", b"0", b"MAXLEN", length],
            [b"MULTI"],
            [b"LSET", set, index, word],
            [b"LREM", removed, b"1", sentinel],
            [b"EXEC"],
            after @ ..,
        ] if [read, set, removed].iter().all(|same| same == &key)
            && last(index, length)
            && word == sentinel
            && sentinel_word(word) =>
        {
            (Unit::Delete(&["index", "value"]), after)
        }
        [[b"WATCH", key], [b"ECHO", _], read, [b"MULTI"], rest @ ..] => {
            let kind = collection_read(read, key)?;
            let writes = rest
                .iter()
                .take_while(|words| collection_write(kind, words, key))
                .count();
            match &rest[writes..] {
                [[b"EXEC"], after @ ..] if writes > 0 => (Unit::Edit, after),
                _ => return None,
            }
        }
        [first, after @ ..] => (single(first)?, after),
        [] => return None,
    })
}

/// The type a [`collection_edit`]'s check reads `key` as.
fn collection_read(words: &[&[u8]], key: &[u8]) -> Option<&'static str> {
    let previewed = PREVIEWED.to_string();
    let kind = match words {
        [b"LRANGE", read, b"0", last] if *last == previewed.as_bytes() => (LIST, read),
        [name, read, b"0", b"COUNT", count] if *count == previewed.as_bytes() => (
            match *name {
                b"HSCAN" => HASH,
                b"SSCAN" => SET,
                b"ZSCAN" => ZSET,
                _ => return None,
            },
            read,
        ),
        _ => return None,
    };
    (*kind.1 == key).then_some(kind.0)
}

/// Whether `words` is one of the writes a [`collection_edit`] of `kind` makes.
fn collection_write(kind: &str, words: &[&[u8]], key: &[u8]) -> bool {
    match (kind, words) {
        (HASH, [b"HSET", written, _, _] | [b"HDEL", written, _])
        | (SET, [b"SADD", written, _] | [b"SREM", written, _])
        | (
            ZSET,
            [b"ZADD", written, b"XX", b"CH", _, _]
            | [b"ZADD", written, b"NX", _, _]
            | [b"ZREM", written, _],
        ) => *written == key,
        (LIST, [b"LSET", written, index, _]) => *written == key && whole(index),
        _ => false,
    }
}

fn single(words: &[&[u8]]) -> Option<Unit> {
    use Unit::*;
    Some(match words {
        [b"SET", _, _, b"XX", b"KEEPTTL", b"GET"]
        | [b"PERSIST", _]
        | [b"RENAMENX", _, _]
        | [b"HSET", _, _, _]
        | [b"ZADD", _, b"XX", b"CH", _, _] => Edit,
        [b"EXPIRE", _, seconds] if positive(seconds) => Edit,
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

/// Whether `index` is the last of the first `length` items.
fn last(index: &[u8], length: &[u8]) -> bool {
    let number = |word: &[u8]| {
        std::str::from_utf8(word)
            .ok()
            .filter(|_| whole(word))
            .and_then(|digits| digits.parse::<u64>().ok())
    };
    number(index).is_some_and(|index| number(length).and_then(|n| n.checked_sub(1)) == Some(index))
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

    #[test]
    fn a_hash_or_sorted_set_json_naming_one_entry_twice_is_refused() {
        for (kind, text) in [
            (HASH, r#"{"a":"1","b":"2","a":"3"}"#),
            (ZSET, r#"{"a":1,"b":2,"a":3}"#),
        ] {
            let error = Collection::parse(kind, text).unwrap_err();
            assert!(error.contains("a is twice"), "{kind}: {error}");
        }
        assert!(Collection::parse(HASH, r#"{"a":"1","b":"2"}"#).is_ok());
    }

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
            held: Vec::new(),
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
            "SET user:1 \"a\\nb é\" XX KEEPTTL GET\nEXPIRE user:1 60\nRENAMENX user:1 \"user:2 \\\"new\\\"\""
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
                &[("index", "2"), ("value", "u")],
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
                "WATCH \"a list\"",
                "LPOS \"a list\" u COUNT 0 MAXLEN 3",
                "MULTI",
                "LSET \"a list\" 2 v",
                "EXEC",
                "SADD tags \"in memory\"",
                "SREM tags fast",
                "ZADD board XX CH 99.5 ada",
                "ZADD board NX 100 grace",
                "ZREM board ada",
                "SET greeting hi XX KEEPTTL GET",
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

        let item = [("index", "3"), ("value", "a b")];
        let tags = [("index", INTEGER), ("value", STRING)];
        let list = delete(LIST, "l", &item, &tags);
        let commands = words(admitted(&list));
        let lines: Vec<&str> = list.lines().collect();
        assert_eq!(
            lines[..3],
            ["WATCH l", "LPOS l \"a b\" COUNT 0 MAXLEN 4", "MULTI"]
        );
        assert_eq!(lines[5], "EXEC");
        assert_eq!(
            commands[3][..3],
            [b"LSET".to_vec(), b"l".to_vec(), b"3".to_vec()]
        );
        assert_eq!(commands[3][3], commands[4][3]);
        assert!(delete_matches_key(&list, &["value", "index"]));
        // A second delete writes a sentinel of its own.
        assert_ne!(list, delete(LIST, "l", &item, &tags));
    }

    #[test]
    fn the_gate_admits_nothing_but_the_shapes_it_names() {
        let sentinel = sentinel();
        let checked = "WATCH l\nLPOS l a COUNT 0 MAXLEN 2\nMULTI\n";
        for text in [
            "",
            "FLUSHDB",
            "DEL k",
            "SET k v",
            "SET k v KEEPTTL",
            "SET k v KEEPTTL XX GET",
            "set k v XX KEEPTTL GET",
            "SET k v XX KEEPTTL GET\nFLUSHDB",
            "SET k v XX KEEPTTL GET\n",
            "# note\nSET k v XX KEEPTTL GET",
            "SET \"k\" v XX KEEPTTL GET",
            "EXPIRE k 0",
            "EXPIRE k -1",
            "EXPIRE k +5",
            "UNLINK a b",
            "UNLINK a\nUNLINK b",
            "SET k v XX KEEPTTL GET\nUNLINK k",
            "SET k v NX\nSET j v NX",
            "SET k v NX EX 0",
            "HSET h f v g w",
            "LSET l x v",
            "LSET l 1 v",
            "MULTI\nLSET l 1 v\nEXEC",
            "WATCH l\nLPOS l a COUNT 0 MAXLEN 1\nMULTI\nLSET l 1 v\nEXEC",
            "WATCH l\nLPOS m a COUNT 0 MAXLEN 2\nMULTI\nLSET l 1 v\nEXEC",
            "WATCH l\nLPOS l a COUNT 0 MAXLEN 2\nMULTI\nLSET m 1 v\nEXEC",
            "WATCH l\nLPOS l a COUNT 1 MAXLEN 2\nMULTI\nLSET l 1 v\nEXEC",
            "WATCH l\nLPOS l a COUNT 0 MAXLEN 2\nMULTI\nLSET l 1 v",
            "WATCH l\nLPOS l a COUNT 0 MAXLEN 2\nLSET l 1 v",
            "LPOS l a COUNT 0 MAXLEN 2\nMULTI\nLSET l 1 v\nEXEC",
            "MULTI\nSREM s a\nSADD s b\nEXEC",
            "SADD s b\nSREM t a",
            "SREM s a\nSADD s b",
            "ZADD z 1 b\nZREM z a",
            "ZADD z NX 1 b\nZREM y a",
            "ZADD z XX 1 m",
            "SADD s b\nFLUSHDB",
            &format!("MULTI\nLSET l 1 {sentinel}\nLREM l 1 {sentinel}\nEXEC"),
            &format!("{checked}LSET l 1 x\nLREM l 1 x\nEXEC"),
            &format!("{checked}LSET l 1 {sentinel}\nLREM l 0 {sentinel}\nEXEC"),
            &format!("{checked}LSET l 1 {sentinel}\nLREM m 1 {sentinel}\nEXEC"),
            &format!("{checked}LSET l 1 {sentinel}\nLREM l 1 {sentinel}\nEXEC\nUNLINK l"),
        ] {
            assert!(!is_generated_write(text), "{text}");
        }
        assert!(is_generated_write(&format!(
            "{checked}LSET l 1 {sentinel}\nLREM l 1 {sentinel}\nEXEC"
        )));
        assert!(is_generated_write(&format!(
            "{checked}LSET l 1 v\nEXEC\nHSET h f v"
        )));
        assert!(is_generated_write(
            "SET a 1 XX KEEPTTL GET\nPERSIST b\nZADD z XX CH 1 m"
        ));
    }

    /// A keys grid row whose `value` shows `shown`, tagged `tag`, edited to `edited`.
    fn collection_row(tag: &str, shown: &str, edited: &str) -> PendingRow {
        PendingRow {
            held: vec![("value".into(), shown.into())],
            ..row(
                "db0",
                KEYS,
                &[("value", value(edited))],
                &[("key", "k")],
                &[("key", STRING), ("ttl", INTEGER), ("value", tag)],
            )
        }
    }

    fn collection_batch(tag: &str, shown: &str, edited: &str) -> Result<String, String> {
        update_batch(&[collection_row(tag, shown, edited)])
    }

    #[test]
    fn a_collections_json_edit_writes_each_difference_under_a_check() {
        let check =
            |read: &str, shown: &str| format!("WATCH k\nECHO {}\n{read}\nMULTI", quoted(shown));
        let hash = r#"{"a":"1","b":"2","c":"3"}"#;
        assert_eq!(
            admitted(
                &collection_batch(HASH_JSON, hash, r#"{"a":"1","b":"two","d":"4 4"}"#).unwrap()
            ),
            format!(
                "{}\nHSET k b two\nHSET k d \"4 4\"\nHDEL k c\nEXEC",
                check("HSCAN k 0 COUNT 100", hash)
            )
        );
        let set = r#"["x","y"]"#;
        assert_eq!(
            admitted(&collection_batch(SET_JSON, set, r#"["y","z"]"#).unwrap()),
            format!(
                "{}\nSADD k z\nSREM k x\nEXEC",
                check("SSCAN k 0 COUNT 100", set)
            )
        );
        let zset = r#"{"a":1,"b":2.5,"c":3}"#;
        assert_eq!(
            admitted(&collection_batch(ZSET_JSON, zset, r#"{"a":1.0,"b":7,"d":-0.5}"#).unwrap()),
            format!(
                "{}\nZADD k XX CH 7 b\nZADD k NX -0.5 d\nZREM k c\nEXEC",
                check("ZSCAN k 0 COUNT 100", zset)
            )
        );
        let list = r#"["a","b","c"]"#;
        assert_eq!(
            admitted(&collection_batch(LIST_JSON, list, r#"["a","B","c"]"#).unwrap()),
            format!("{}\nLSET k 1 B\nEXEC", check("LRANGE k 0 100", list))
        );
        // A ttl edit on the same row follows the collection's.
        let mut both = collection_row(LIST_JSON, list, r#"["A","b","c"]"#);
        both.sets.push(("ttl".into(), value("9")));
        let batch = update_batch(&[both]).unwrap();
        assert!(admitted(&batch).ends_with("EXEC\nEXPIRE k 9"), "{batch}");
    }

    #[test]
    fn a_collections_json_edit_refuses_what_is_not_its_shape_or_no_change() {
        for (tag, shown, edited, said) in [
            (HASH_JSON, r#"{"a":"1"}"#, "{", "not JSON"),
            (HASH_JSON, r#"{"a":"1"}"#, r#"["a"]"#, "JSON object"),
            (HASH_JSON, r#"{"a":"1"}"#, r#"{"a":1}"#, "JSON string"),
            (HASH_JSON, r#"{"a":"1"}"#, "{}", "empty"),
            (SET_JSON, r#"["a"]"#, r#"{"a":"1"}"#, "JSON array"),
            (SET_JSON, r#"["a"]"#, r#"["a","b","a"]"#, "once"),
            (SET_JSON, r#"["a"]"#, r#"["a",null]"#, "JSON string"),
            (ZSET_JSON, r#"{"a":1}"#, r#"{"a":"1"}"#, "number"),
            (ZSET_JSON, r#"{"a":1}"#, r#"[1]"#, "JSON object"),
            (LIST_JSON, r#"["a","b"]"#, r#"["a"]"#, "open the key"),
            (
                LIST_JSON,
                r#"["a","b"]"#,
                r#"["a","b","c"]"#,
                "open the key",
            ),
            (LIST_JSON, r#"["a"]"#, r#"[true]"#, "JSON string"),
            (LIST_JSON, r#"["a"]"#, r#"["a"]"#, "change nothing"),
            (ZSET_JSON, r#"{"a":1}"#, r#"{"a":1.0}"#, "change nothing"),
            (
                HASH_JSON,
                r#"{"a":"1","b":"2"}"#,
                r#" {"b":"2", "a":"1"} "#,
                "change nothing",
            ),
        ] {
            let error = collection_batch(tag, shown, edited).unwrap_err();
            assert!(error.contains(said), "{edited}: {error}");
        }
        // NULL, and a cell whose JSON could not be written back, are refused.
        let mut nulled = collection_row(HASH_JSON, r#"{"a":"1"}"#, "");
        nulled.sets = vec![("value".into(), NewValue::Null)];
        assert!(update_batch(&[nulled]).is_err());
        assert!(collection_batch(SUMMARY, r#"["\"\\xff\""]"#, r#"["a"]"#).is_err());
        assert!(!writes_back(SUMMARY) && writes_back(SET_JSON));
    }

    #[test]
    fn the_gate_admits_a_collection_edit_only_whole_and_of_its_own_type() {
        let head = "WATCH k\nECHO x\nHSCAN k 0 COUNT 100\nMULTI\n";
        assert!(is_generated_write(&format!(
            "{head}HSET k f v\nHDEL k g\nEXEC"
        )));
        for text in [
            format!("{head}EXEC"),
            format!("{head}HSET k f v"),
            format!("{head}SADD k m\nEXEC"),
            format!("{head}HSET j f v\nEXEC"),
            format!("{head}HSET k f v\nFLUSHDB\nEXEC"),
            format!("{head}HSET k f v\nEXEC\nFLUSHDB"),
            "WATCH k\nECHO x\nHSCAN k 0 COUNT 1000\nMULTI\nHSET k f v\nEXEC".into(),
            "WATCH k\nECHO x\nHSCAN j 0 COUNT 100\nMULTI\nHSET k f v\nEXEC".into(),
            "WATCH k\nHSCAN k 0 COUNT 100\nMULTI\nHSET k f v\nEXEC".into(),
            "WATCH k\nECHO x\nLRANGE k 0 -1\nMULTI\nLSET k 0 v\nEXEC".into(),
            "WATCH k\nECHO x\nLRANGE k 0 100\nMULTI\nLSET k -1 v\nEXEC".into(),
            "WATCH k\nECHO x\nZSCAN k 0 COUNT 100\nMULTI\nZADD k 1 m\nEXEC".into(),
            "WATCH k\nECHO x\nSSCAN k 0 COUNT 100\nMULTI\nHDEL k f\nEXEC".into(),
        ] {
            assert!(!is_generated_write(&text), "{text}");
        }
    }

    #[test]
    fn every_tag_reads_back_from_a_snapshot() {
        for tag in TAGS {
            assert_eq!(cell_type(tag), Some(tag));
        }
        assert_eq!(cell_type("vectorset"), None);
    }
}

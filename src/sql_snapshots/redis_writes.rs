use super::{check, outcome, set, statement, statement_with};
use crate::{
    db::Engine,
    redis_cmd::{
        browse::KEYS,
        edit::{
            BYTES, DOUBLE, HASH, HASH_JSON, INTEGER, LIST, LIST_JSON, SET, SET_JSON, STRING, ZSET,
            ZSET_JSON,
        },
    },
    result_grid::{NewValue, PendingRow},
    sql,
};

const REDIS: Engine = Engine::Redis;

/// Duplicated from the private `SENTINEL` in `redis_cmd/edit.rs`; if its
/// spelling changes, the "exactly twice" assertion below fails loudly.
const SENTINEL_PREFIX: &str = "dbdelve:deleted:";

fn pending(
    schema: &str,
    table: &str,
    sets: &[(&str, NewValue)],
    keys: &[(&str, &str)],
    types: &[(&str, &str)],
    held: &[(&str, &str)],
) -> PendingRow {
    let owned = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    };
    PendingRow {
        schema: schema.to_owned(),
        table: table.to_owned(),
        sets: sets
            .iter()
            .map(|(column, value)| ((*column).to_owned(), value.clone()))
            .collect(),
        keys: owned(keys),
        types: owned(types),
        held: owned(held),
    }
}

/// Redacts only a complete sentinel: the prefix, exactly 32 lowercase hex
/// digits, then a token boundary. Any other spelling stays visible, so a
/// change in the generator shows up in the snapshot.
fn redact(record: &str) -> String {
    let mut out = String::with_capacity(record.len());
    let mut rest = record;
    while let Some(at) = rest.find(SENTINEL_PREFIX) {
        let (before, after) = rest.split_at(at + SENTINEL_PREFIX.len());
        out.push_str(before);
        let digits = after
            .bytes()
            .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            .count();
        let boundary = after[digits..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let complete = after.as_bytes().get(..32).is_some_and(|run| {
            run.iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        }) && digits == 32
            && boundary;
        if complete {
            out.push_str("<32 hex>");
            rest = &after[32..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn redact_touches_only_a_complete_lowercase_sentinel() {
    let hex = "0123456789abcdef0123456789abcdef";
    assert_eq!(
        redact(&format!(
            "LSET l 3 {SENTINEL_PREFIX}{hex}\nLREM l 1 {SENTINEL_PREFIX}{hex}"
        )),
        format!("LSET l 3 {SENTINEL_PREFIX}<32 hex>\nLREM l 1 {SENTINEL_PREFIX}<32 hex>")
    );
    for untouched in [
        format!("{SENTINEL_PREFIX}{}", hex.to_uppercase()),
        format!("{SENTINEL_PREFIX}{}", &hex[..31]),
        format!("{SENTINEL_PREFIX}{hex}0"),
        format!("{SENTINEL_PREFIX}{hex}g"),
    ] {
        assert_eq!(redact(&untouched), untouched);
    }
}

fn types(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

const KEYS_TYPES: [(&str, &str); 3] = [("key", STRING), ("ttl", INTEGER), ("value", STRING)];
const HASH_TYPES: [(&str, &str); 2] = [("field", STRING), ("value", STRING)];
const LIST_TYPES: [(&str, &str); 2] = [("index", INTEGER), ("value", STRING)];
const SET_TYPES: [(&str, &str); 1] = [("member", STRING)];
const ZSET_TYPES: [(&str, &str); 2] = [("member", STRING), ("score", DOUBLE)];
const STRING_TYPES: [(&str, &str); 1] = [("value", STRING)];

fn update(out: &mut String, case: &str, rows: &[PendingRow], ok: bool) {
    outcome(out, REDIS, case, sql::update_batch(REDIS, rows), ok);
}

#[test]
fn redis_family_updates() {
    let mut out = String::new();
    let hash = |sets: &[(&str, NewValue)], keys: &[(&str, &str)]| {
        [pending(HASH, "h", sets, keys, &HASH_TYPES, &[])]
    };
    let f = [("field", "f")];
    update(
        &mut out,
        "hash value",
        &hash(&[("value", set("v"))], &f),
        true,
    );
    update(
        &mut out,
        "hash awkward value",
        &hash(&[("value", set("a b \"q\"\nü 😀"))], &f),
        true,
    );
    update(
        &mut out,
        "hash empty value",
        &hash(&[("value", set(""))], &f),
        true,
    );
    update(
        &mut out,
        "hash null",
        &hash(&[("value", NewValue::Null)], &f),
        false,
    );
    update(
        &mut out,
        "hash default",
        &hash(&[("value", NewValue::Default)], &f),
        false,
    );
    update(
        &mut out,
        "hash field edit",
        &hash(&[("field", set("g"))], &f),
        false,
    );
    update(
        &mut out,
        "hash no field key",
        &hash(&[("value", set("v"))], &[]),
        false,
    );

    let list = |sets: &[(&str, NewValue)], keys: &[(&str, &str)]| {
        [pending(LIST, "l", sets, keys, &LIST_TYPES, &[])]
    };
    let v = [("value", set("z"))];
    for (case, index, ok) in [
        ("list index 3", "3", true),
        ("list index 0", "0", true),
        ("list large index", "4294967296", true),
        ("list index x", "x", false),
        ("list index -1", "-1", false),
        ("list index overflow", "18446744073709551615", false),
    ] {
        update(
            &mut out,
            case,
            &list(&v, &[("index", index), ("value", "a b")]),
            ok,
        );
    }
    update(
        &mut out,
        "list no value key",
        &list(&v, &[("index", "3")]),
        false,
    );

    let member = |sets: &[(&str, NewValue)], keys: &[(&str, &str)]| {
        [pending(SET, "s", sets, keys, &SET_TYPES, &[])]
    };
    let old = [("member", "old")];
    update(
        &mut out,
        "set member",
        &member(&[("member", set("new"))], &old),
        true,
    );
    update(
        &mut out,
        "set spaced member",
        &member(&[("member", set("a b"))], &old),
        true,
    );
    update(
        &mut out,
        "set null member",
        &member(&[("member", set("NULL"))], &old),
        true,
    );
    update(
        &mut out,
        "set real null member",
        &member(&[("member", NewValue::Null)], &old),
        false,
    );

    let zset = |sets: &[(&str, NewValue)], keys: &[(&str, &str)]| {
        [pending(ZSET, "z", sets, keys, &ZSET_TYPES, &[])]
    };
    let mk = [("member", "m"), ("score", "1")];
    update(
        &mut out,
        "zset rescore",
        &zset(&[("score", set("2.5"))], &mk),
        true,
    );
    for (score, ok) in [
        ("-inf", true),
        ("inf", true),
        ("1e3", true),
        ("0", true),
        ("nan", false),
        ("abc", false),
        ("", false),
    ] {
        update(
            &mut out,
            &format!("zset score {score:?}"),
            &zset(&[("score", set(score))], &mk),
            ok,
        );
    }
    update(
        &mut out,
        "zset rename",
        &zset(&[("member", set("n"))], &mk),
        true,
    );
    update(
        &mut out,
        "zset rename and rescore",
        &zset(&[("member", set("n")), ("score", set("3"))], &mk),
        true,
    );
    update(
        &mut out,
        "zset rename without score key",
        &zset(&[("member", set("n"))], &[("member", "m")]),
        false,
    );

    let string = |table: &str| {
        [pending(
            STRING,
            table,
            &[("value", set("v"))],
            &[("value", "hello")],
            &STRING_TYPES,
            &[],
        )]
    };
    update(&mut out, "string value", &string("s"), true);
    update(
        &mut out,
        "string unspelled table",
        &string("two words"),
        false,
    );
    update(
        &mut out,
        "string spelled table",
        &string("\"two words\""),
        true,
    );

    let grid = |sets: &[(&str, NewValue)], keys: &[(&str, &str)]| {
        [pending("db0", KEYS, sets, keys, &KEYS_TYPES, &[])]
    };
    let k = [("key", "k")];
    update(
        &mut out,
        "keys value",
        &grid(&[("value", set("v"))], &k),
        true,
    );
    update(
        &mut out,
        "keys stream value",
        &[pending(
            "db0",
            KEYS,
            &[("value", set("v"))],
            &k,
            &[("key", STRING), ("ttl", INTEGER), ("value", "stream")],
            &[],
        )],
        false,
    );
    update(
        &mut out,
        "keys type column",
        &grid(&[("type", set("hash"))], &k),
        false,
    );
    for (case, value, ok) in [
        ("ttl 60", set("60"), true),
        ("ttl null", NewValue::Null, true),
        ("ttl empty", set(""), true),
        ("ttl 0", set("0"), false),
        ("ttl -1", set("-1"), false),
        ("ttl abc", set("abc"), false),
        ("ttl default", NewValue::Default, false),
    ] {
        update(
            &mut out,
            &format!("keys {case}"),
            &grid(&[("ttl", value)], &k),
            ok,
        );
    }
    update(
        &mut out,
        "keys rename",
        &grid(&[("key", set("k2"))], &k),
        true,
    );
    update(
        &mut out,
        "keys everything",
        &grid(
            &[("key", set("k2")), ("value", set("v")), ("ttl", set("60"))],
            &k,
        ),
        true,
    );
    update(
        &mut out,
        "keys other table",
        &[pending(
            "db0",
            "other",
            &[("value", set("v"))],
            &k,
            &KEYS_TYPES,
            &[],
        )],
        false,
    );
    let mixed = vec![
        pending(HASH, "h", &[("value", set("v"))], &f, &HASH_TYPES, &[]),
        pending(SET, "s", &[("member", set("new"))], &old, &SET_TYPES, &[]),
    ];
    update(&mut out, "mixed families", &mixed, true);
    update(&mut out, "empty batch", &[], false);
    update(&mut out, "no changes", &grid(&[], &k), false);
    check("redis_updates", &out);
}

fn collection(tag: &str, shown: Option<&str>, edited: NewValue) -> [PendingRow; 1] {
    let held: Vec<(&str, &str)> = shown.map(|text| ("value", text)).into_iter().collect();
    [pending(
        "db0",
        KEYS,
        &[("value", edited)],
        &[("key", "k")],
        &[("key", STRING), ("ttl", INTEGER), ("value", tag)],
        &held,
    )]
}

#[test]
fn redis_collection_edits() {
    let mut out = String::new();
    let mut edit = |case: &str, tag: &str, shown: &str, edited: &str, ok: bool| {
        update(
            &mut out,
            case,
            &collection(tag, Some(shown), set(edited)),
            ok,
        );
    };
    let hash = r#"{"a":"1","b":"2","c":"3"}"#;
    edit(
        "hash add",
        HASH_JSON,
        hash,
        r#"{"a":"1","b":"2","c":"3","d":"4"}"#,
        true,
    );
    edit(
        "hash change",
        HASH_JSON,
        hash,
        r#"{"a":"1","b":"two","c":"3"}"#,
        true,
    );
    edit("hash remove", HASH_JSON, hash, r#"{"a":"1","b":"2"}"#, true);
    edit(
        "hash all three",
        HASH_JSON,
        hash,
        r#"{"a":"1","b":"two","d":"4 4"}"#,
        true,
    );
    let list = r#"["a","b","c"]"#;
    edit(
        "list append",
        LIST_JSON,
        list,
        r#"["a","b","c","d"]"#,
        false,
    );
    edit("list change", LIST_JSON, list, r#"["a","B","c"]"#, true);
    edit("list remove", LIST_JSON, list, r#"["a","c"]"#, false);
    edit("list reorder", LIST_JSON, list, r#"["c","b","a"]"#, true);
    let members = r#"["x","y"]"#;
    edit("set add", SET_JSON, members, r#"["x","y","z"]"#, true);
    edit("set remove", SET_JSON, members, r#"["x"]"#, true);
    let zset = r#"{"a":1,"b":2.5,"c":3}"#;
    edit(
        "zset add",
        ZSET_JSON,
        zset,
        r#"{"a":1,"b":2.5,"c":3,"d":-0.5}"#,
        true,
    );
    edit(
        "zset rescore",
        ZSET_JSON,
        zset,
        r#"{"a":1,"b":7,"c":3}"#,
        true,
    );
    edit("zset remove", ZSET_JSON, zset, r#"{"a":1,"b":2.5}"#, true);
    edit("hash no change", HASH_JSON, hash, hash, false);
    edit("hash not json", HASH_JSON, hash, "{", false);
    edit("hash wrong shape", HASH_JSON, hash, r#"["a"]"#, false);
    edit("list wrong shape", LIST_JSON, list, r#"{"a":"1"}"#, false);
    edit("hash empty", HASH_JSON, hash, "{}", false);
    edit("list empty", LIST_JSON, list, "[]", false);
    edit(
        "hash duplicate field",
        HASH_JSON,
        hash,
        r#"{"a":"1","a":"2"}"#,
        false,
    );
    edit(
        "zset duplicate member",
        ZSET_JSON,
        zset,
        r#"{"a":1,"a":2}"#,
        false,
    );
    edit(
        "set duplicate member",
        SET_JSON,
        members,
        r#"["x","y","x"]"#,
        false,
    );
    edit(
        "hash numeric value",
        HASH_JSON,
        hash,
        r#"{"a":1,"b":"2","c":"3"}"#,
        false,
    );
    edit(
        "zset string score",
        ZSET_JSON,
        zset,
        r#"{"a":"1","b":2.5,"c":3}"#,
        false,
    );
    update(
        &mut out,
        "null value",
        &collection(HASH_JSON, Some(hash), NewValue::Null),
        false,
    );
    update(
        &mut out,
        "held missing",
        &collection(HASH_JSON, None, set(r#"{"a":"1"}"#)),
        false,
    );
    check("redis_collection_edits", &out);
}

fn delete_record(
    out: &mut String,
    case: &str,
    schema: &str,
    table: &str,
    keys: &[(&str, &str)],
    tags: &[(&str, &str)],
    expect_some: bool,
) {
    let text = sql::delete_row(REDIS, schema, table, keys, &types(tags));
    assert_eq!(text.is_some(), expect_some, "{case}");
    let Some(text) = text else {
        statement(out, REDIS, case, None);
        return;
    };
    let names: Vec<&str> = keys.iter().map(|(name, _)| *name).collect();
    let matches = |list: &[&str]| sql::delete_matches_key_on(REDIS, &text, list);
    let extra = format!(
        " matches_key=exact:{} other:{}",
        matches(&names),
        matches(&["other"])
    );
    let mut record = String::new();
    statement_with(&mut record, REDIS, case, Some(&text), &extra);
    if schema == LIST {
        assert_eq!(text.matches(SENTINEL_PREFIX).count(), 2, "{text}");
        let words: Vec<&str> = text
            .split(|c: char| c.is_whitespace())
            .filter(|word| word.starts_with(SENTINEL_PREFIX))
            .collect();
        assert_eq!(words.len(), 2, "{text}");
        assert_eq!(words[0], words[1], "{text}");
        let again = sql::delete_row(REDIS, schema, table, keys, &types(tags)).unwrap();
        assert_ne!(text, again, "the sentinel should differ between calls");
        let redacted = redact(&record);
        assert!(
            !redacted
                .split(SENTINEL_PREFIX)
                .skip(1)
                .any(|tail| tail.len() >= 32
                    && tail.as_bytes()[..32].iter().all(u8::is_ascii_hexdigit)),
            "{redacted}"
        );
        out.push_str(&redacted);
    } else {
        out.push_str(&record);
    }
}

#[test]
fn redis_family_deletes() {
    let mut out = String::new();
    let mut run = |case: &str,
                   schema: &str,
                   table: &str,
                   keys: &[(&str, &str)],
                   tags: &[(&str, &str)],
                   some: bool| {
        delete_record(&mut out, case, schema, table, keys, tags, some);
    };
    run("hash", HASH, "h", &[("field", "f g")], &HASH_TYPES, true);
    run("hash no field", HASH, "h", &[], &HASH_TYPES, false);
    run(
        "hash unspelled table",
        HASH,
        "two words",
        &[("field", "f")],
        &HASH_TYPES,
        false,
    );
    let item = [("index", "3"), ("value", "a b")];
    run("list", LIST, "l", &item, &LIST_TYPES, true);
    run(
        "list index 0",
        LIST,
        "l",
        &[("index", "0"), ("value", "v")],
        &LIST_TYPES,
        true,
    );
    run(
        "list large index",
        LIST,
        "l",
        &[("index", "4294967296"), ("value", "v")],
        &LIST_TYPES,
        true,
    );
    run(
        "list index x",
        LIST,
        "l",
        &[("index", "x"), ("value", "v")],
        &LIST_TYPES,
        false,
    );
    run(
        "list no value",
        LIST,
        "l",
        &[("index", "3")],
        &LIST_TYPES,
        false,
    );
    run("set", SET, "s", &[("member", "m")], &SET_TYPES, true);
    run("set no member", SET, "s", &[], &SET_TYPES, false);
    run(
        "zset",
        ZSET,
        "z",
        &[("member", "m"), ("score", "1")],
        &ZSET_TYPES,
        true,
    );
    run(
        "zset no member",
        ZSET,
        "z",
        &[("score", "1")],
        &ZSET_TYPES,
        false,
    );
    run(
        "string",
        STRING,
        "s",
        &[("value", "v")],
        &STRING_TYPES,
        true,
    );
    run(
        "string unspelled table",
        STRING,
        "two words",
        &[("value", "v")],
        &STRING_TYPES,
        false,
    );
    run(
        "keys",
        "db0",
        KEYS,
        &[("key", "k")],
        &[("key", STRING)],
        true,
    );
    let binary = crate::redis_cmd::quoted_bytes(b"\x00\xff k");
    run(
        "keys binary",
        "db0",
        KEYS,
        &[("key", binary.as_str())],
        &[("key", BYTES)],
        true,
    );
    run(
        "keys other table",
        "db0",
        "other",
        &[("key", "k")],
        &[("key", STRING)],
        false,
    );
    run("keys no key", "db0", KEYS, &[], &[("key", STRING)], false);
    check("redis_deletes", &out);
}

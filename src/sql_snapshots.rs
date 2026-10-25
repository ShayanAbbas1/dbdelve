//! The statements and fragments DBDelve generates today, recorded per engine as
//! `insta` snapshots, so the engine-spec refactor can be held to reproducing
//! them character for character. It sits outside `sql.rs`, `filter.rs`,
//! `explorer.rs` and `db/mod.rs` so that refactor never edits the tests that
//! judge it.

mod filters;
mod previews;
mod sorts;
mod spellings;

use crate::{
    db::Engine,
    filter::{self, Operator},
    redis_cmd,
    sql::{self, Mode},
};

pub(crate) const RELATIONS: [(&str, &str, &str); 7] = [
    ("plain", "public", "accounts"),
    ("quotes", "my \"odd\" schema", "it's `tick` \"dq\""),
    ("unicode", "données", "Ünïcødé 表"),
    ("reserved", "select", "order"),
    ("mixed case", "Sales", "OrderLines"),
    ("brackets and dots", "[dbo]", "a.b"),
    ("backslash", "back\\slash", "trail\\"),
];

pub(crate) const NAMES: [&str; 12] = [
    "id",
    "Name",
    "select",
    "we\"ird",
    "back`tick",
    "with space",
    "ümlaut",
    "it's",
    "[x]",
    "a.b",
    "",
    "_id",
];

pub(crate) const VALUES: [(&str, &str); 17] = [
    ("plain", "ok"),
    ("single quote", "it's"),
    ("backslash", r"back\slash"),
    ("backslash before quote", r"\'"),
    ("odd backslashes then quote", r"a\\\'b"),
    ("unicode", "李小龍 ünï 😀"),
    ("empty", ""),
    ("newline and tab", "a\nb\tc"),
    ("control characters", "a\u{0}b\u{7}"),
    ("like metacharacters", "100%_[x]"),
    ("glob metacharacters", r"*?[a]\"),
    ("number", "42"),
    ("negative float", "-3.5e10"),
    ("boolean", "true"),
    ("date", "2024-01-02"),
    ("timestamp", "2024-01-02 03:04:05.123"),
    ("hex", "0xDEADbeef"),
];

pub(crate) fn long_text() -> String {
    "0123456789".repeat(50)
}

pub(crate) const TYPES: [Option<&str>; 10] = [
    None,
    Some("varchar(20)"),
    Some("nvarchar(20)"),
    Some("int"),
    Some("bit"),
    Some("datetime"),
    Some("date"),
    Some("varbinary(16)"),
    Some("uniqueidentifier"),
    Some("bytea"),
];

pub(crate) const PAGE_KEYS: [&[&str]; 5] = [
    &[],
    &["id"],
    &["id", "Region"],
    &["we\"ird", "it's", "back`tick", "ümlaut", "[x]", "a.b"],
    &[""],
];

pub(crate) fn home(engine: Engine) -> (&'static str, &'static str) {
    match engine {
        Engine::MongoDb => ("dbdelve_dev", "accounts"),
        Engine::Redis => ("db0", redis_cmd::browse::KEYS),
        _ => ("public", "accounts"),
    }
}

pub(crate) fn sample_filter(engine: Engine) -> String {
    let field = if engine == Engine::Redis {
        "key"
    } else {
        "Name"
    };
    filter::filter_predicate(engine, field, None, Operator::Equals, "it's").unwrap_or_default()
}

/// Backslash first, so the encoding stays injective: a real NUL records as
/// `\u{0}` and the five characters `\u{0}` as `\\u{0}`.
fn visible(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn verdicts(engine: Engine, text: &str) -> String {
    let verdict = sql::classify(engine, text);
    let [ro, rw, full] = Mode::ALL.map(|mode| sql::gate(&verdict, mode, &[]));
    format!(
        "write_on={} select={} classify={:?} {:?} gate ro={ro:?} rw={rw:?} full={full:?}",
        sql::is_generated_write_on(engine, text),
        sql::is_generated_select(engine, text),
        verdict.mode,
        verdict.destructive,
    )
}

pub(crate) fn statement(out: &mut String, engine: Engine, case: &str, text: Option<&str>) {
    statement_with(out, engine, case, text, "");
}

pub(crate) fn statement_with(
    out: &mut String,
    engine: Engine,
    case: &str,
    text: Option<&str>,
    extra: &str,
) {
    out.push_str(&format!("\n[{}] {case}\n", engine.as_str()));
    match text {
        None => out.push_str("  (none)\n"),
        Some(text) => {
            for piece in text.split('\n') {
                out.push_str(&format!("  |{}|\n", visible(piece)));
            }
            out.push_str(&format!("  = {}{extra}\n", verdicts(engine, text)));
        }
    }
}

pub(crate) fn fragment(out: &mut String, engine: Engine, case: &str, text: Option<&str>) {
    let shown = match text {
        Some(text) => format!("|{}|", visible(text)),
        None => "(none)".to_owned(),
    };
    out.push_str(&format!("[{}] {case} => {shown}\n", engine.as_str()));
}

pub(crate) fn refusal(out: &mut String, engine: Engine, case: &str, message: &str) {
    out.push_str(&format!(
        "[{}] {case} => error: |{}|\n",
        engine.as_str(),
        visible(message)
    ));
}

pub(crate) fn check(name: &str, out: &str) {
    insta::with_settings!({
        snapshot_path => "sql_snapshots/snapshots",
        prepend_module_to_snapshot => false,
        omit_expression => true,
    }, {
        insta::assert_snapshot!(name, out);
    });
}

#[test]
fn a_record_keeps_line_ends_and_spells_control_characters() {
    let mut out = String::new();
    fragment(&mut out, Engine::Postgres, "prefix", Some("EXPLAIN "));
    fragment(&mut out, Engine::Postgres, "controls", Some("a\r\u{0}b"));
    fragment(&mut out, Engine::Redis, "missing", None);
    assert_eq!(
        out,
        "[postgres] prefix => |EXPLAIN |\n[postgres] controls => |a\\u{d}\\u{0}b|\n[redis] missing => (none)\n"
    );
    let mut out = String::new();
    statement(
        &mut out,
        Engine::Postgres,
        "two lines",
        Some("BEGIN;\nCOMMIT;"),
    );
    assert!(
        out.starts_with("\n[postgres] two lines\n  |BEGIN;|\n  |COMMIT;|\n  = write_on="),
        "{out}"
    );
}

#[test]
fn distinct_texts_record_distinctly() {
    let texts = [
        "a\u{0}b", "a\\u{0}b", "a\nb", "a\\nb", "a\tb", "a\\tb", "a\\b", "a\\\\b", "a\r\nb",
        "a\nb\r", "a\n", "a\n\n", "a", "a ", "a  ", " a", "", "\n",
    ];
    let records: Vec<String> = texts
        .iter()
        .map(|text| {
            let mut out = String::new();
            statement(&mut out, Engine::Postgres, "t", Some(text));
            out.split("\n  = ").next().unwrap().to_owned()
        })
        .collect();
    for (i, a) in records.iter().enumerate() {
        for (j, b) in records.iter().enumerate().skip(i + 1) {
            assert_ne!(a, b, "{:?} and {:?} record alike", texts[i], texts[j]);
        }
    }
    let mut out = String::new();
    fragment(
        &mut out,
        Engine::Postgres,
        "escapes",
        Some("a\\b\u{0}\\u{0}"),
    );
    assert_eq!(out, "[postgres] escapes => |a\\\\b\\u{0}\\\\u{0}|\n");
}

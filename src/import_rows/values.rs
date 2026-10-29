//! What a cell means under the rule its column was given: NULL, text, a date
//! rewritten to ISO 8601, a number, or a boolean. Pure functions over a
//! [`Cell`]; nothing here touches a database.

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use super::{Cell, EmptyField, Json, Options};
use crate::db::Engine;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Order {
    Dmy,
    Mdy,
    Ymd,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum DateFormat {
    Iso,
    Calendar {
        order: Order,
        separator: char,
        time: bool,
    },
    UnixSeconds,
    UnixMillis,
}

const fn calendar(order: Order, separator: char, time: bool) -> DateFormat {
    DateFormat::Calendar {
        order,
        separator,
        time,
    }
}

pub(crate) const DATE_FORMATS: [DateFormat; 11] = [
    DateFormat::Iso,
    calendar(Order::Dmy, '/', false),
    calendar(Order::Mdy, '/', false),
    calendar(Order::Dmy, '.', false),
    calendar(Order::Ymd, '/', false),
    calendar(Order::Dmy, '/', true),
    calendar(Order::Mdy, '/', true),
    calendar(Order::Dmy, '.', true),
    calendar(Order::Ymd, '/', true),
    DateFormat::UnixSeconds,
    DateFormat::UnixMillis,
];

impl DateFormat {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Iso => "Auto (ISO 8601)",
            Self::UnixSeconds => "Unix seconds (as UTC)",
            Self::UnixMillis => "Unix milliseconds (as UTC)",
            Self::Calendar {
                order,
                separator,
                time,
            } => match (order, separator, time) {
                (Order::Dmy, '/', false) => "DD/MM/YYYY",
                (Order::Mdy, '/', false) => "MM/DD/YYYY",
                (Order::Dmy, '.', false) => "DD.MM.YYYY",
                (Order::Ymd, '/', false) => "YYYY/MM/DD",
                (Order::Dmy, '/', true) => "DD/MM/YYYY HH:mm[:ss]",
                (Order::Mdy, '/', true) => "MM/DD/YYYY HH:mm[:ss]",
                (Order::Dmy, '.', true) => "DD.MM.YYYY HH:mm[:ss]",
                (Order::Ymd, '/', true) => "YYYY/MM/DD HH:mm[:ss]",
                _ => unreachable!("only the DATE_FORMATS presets are built"),
            },
        }
    }

    fn noun(self) -> String {
        match self {
            Self::Iso => "an ISO 8601 date".into(),
            Self::UnixSeconds => "a Unix time in seconds".into(),
            Self::UnixMillis => "a Unix time in milliseconds".into(),
            Self::Calendar { .. } => format!("a {} date", self.label()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Decimal {
    Dot,
    Comma,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Spellings {
    pub(crate) truthy: Vec<String>,
    pub(crate) falsy: Vec<String>,
}

impl Default for Spellings {
    fn default() -> Self {
        Self::parse("true/false, yes/no, y/n, 1/0, t/f").expect("the default pairs are well formed")
    }
}

impl Spellings {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut spellings = Self {
            truthy: Vec::new(),
            falsy: Vec::new(),
        };
        for pair in text.split(',') {
            match pair.split_once('/').map(|(t, f)| (t.trim(), f.trim())) {
                Some((t, f)) if !t.is_empty() && !f.is_empty() => {
                    spellings.truthy.push(t.into());
                    spellings.falsy.push(f.into());
                }
                _ => return Err("Each pair is written true/false, separated by commas.".into()),
            }
        }
        Ok(spellings)
    }

    pub(crate) fn text(&self) -> String {
        self.truthy
            .iter()
            .zip(&self.falsy)
            .map(|(t, f)| format!("{t}/{f}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Rule {
    AsIs,
    Date(DateFormat),
    Number(Decimal),
    Boolean,
    Exact,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Null,
    Text(String),
    Number(String),
    Bool(bool),
    Date { text: String, millis: i64 },
    Json(Json),
}

impl Value {
    pub(crate) fn shown(&self) -> Option<String> {
        match self {
            Self::Null => None,
            Self::Bool(b) => Some(b.to_string()),
            Self::Json(json) => Some(json.to_string()),
            Self::Text(text) | Self::Number(text) | Self::Date { text, .. } => Some(text.clone()),
        }
    }
}

/// `Ok(None)` is a cell the file does not have (a short row, a missing key).
/// `column` is the SQL type the value is spelled for, `None` for a document,
/// which keeps JSON's own types and a date's instant.
pub(crate) fn value(
    cell: &Cell,
    rule: &Rule,
    options: &Options,
    column: Option<&str>,
    engine: Engine,
) -> Result<Option<Value>, String> {
    match cell {
        Cell::Missing => Ok(None),
        Cell::Json(json) => json_value(json, rule, options, column, engine).map(Some),
        Cell::Text { text, quoted } => {
            text_value(text, *quoted, rule, options, column, engine).map(Some)
        }
    }
}

fn text_value(
    text: &str,
    quoted: bool,
    rule: &Rule,
    options: &Options,
    column: Option<&str>,
    engine: Engine,
) -> Result<Value, String> {
    let exact = *rule == Rule::Exact;
    let mut text = text;
    if !quoted {
        if options.trim && !exact {
            text = text.trim();
        }
        // Matched trimmed even with Trim off, so a stray space around the word
        // is not kept as the text "  NULL ".
        if !exact && !options.null_word.is_empty() && text.trim() == options.null_word {
            return Ok(Value::Null);
        }
        if text.is_empty() {
            return Ok(match options.empty {
                EmptyField::Null => Value::Null,
                EmptyField::EmptyString => Value::Text(String::new()),
            });
        }
    }
    typed(text, rule, options, column, engine)
}

/// A text column, where a boolean is the word, not the engine's `1`.
fn textual(data_type: &str) -> bool {
    let t = data_type.to_lowercase();
    ["char", "text", "clob", "string", "json"]
        .iter()
        .any(|word| t.contains(word))
}

/// A column that keeps an instant rather than a wall clock, so a value's
/// offset means something to it. MySQL's bare `timestamp` converts from the
/// session's zone, so it reads an offset too; Postgres spells its own
/// `timestamp` out as `… without time zone`, and Snowflake's are `TIMESTAMP_NTZ` and the like.
fn zone_aware(data_type: &str) -> bool {
    let t = data_type.to_lowercase();
    if t.contains("without time zone") {
        return false;
    }
    [
        "with time zone",
        "offset",
        "timestamptz",
        "timetz",
        "timestamp_tz",
        "timestamp_ltz",
    ]
    .iter()
    .any(|word| t.contains(word))
        || t == "timestamp"
        || t.starts_with("timestamp(")
}

fn json_value(
    json: &Json,
    rule: &Rule,
    options: &Options,
    column: Option<&str>,
    engine: Engine,
) -> Result<Value, String> {
    Ok(match (json, rule) {
        (Json::Null, _) => Value::Null,
        (Json::String(s), _) => return typed(s, rule, options, column, engine),
        (Json::Bool(b), Rule::AsIs | Rule::Exact) if column.is_some_and(textual) => {
            Value::Text(b.to_string())
        }
        (Json::Bool(b), Rule::AsIs | Rule::Exact | Rule::Boolean) => Value::Bool(*b),
        // JSON spells every number with a dot, whatever separator the rule was given.
        (Json::Number(n), Rule::AsIs | Rule::Exact | Rule::Number(_)) => Value::Number(n.clone()),
        (Json::Object(_) | Json::Array(_), Rule::AsIs | Rule::Exact) => Value::Json(json.clone()),
        (other, _) => return typed(&other.to_string(), rule, options, column, engine),
    })
}

fn typed(
    text: &str,
    rule: &Rule,
    options: &Options,
    column: Option<&str>,
    engine: Engine,
) -> Result<Value, String> {
    match rule {
        Rule::AsIs | Rule::Exact => Ok(Value::Text(text.into())),
        Rule::Date(format) => date(text, *format, column, engine),
        Rule::Number(decimal) => number(text, *decimal),
        Rule::Boolean => boolean(text, &options.booleans),
    }
}

fn shown(text: &str) -> String {
    if text.is_empty() {
        "an empty string".into()
    } else {
        format!("`{text}`")
    }
}

fn number(text: &str, decimal: Decimal) -> Result<Value, String> {
    let not = || format!("{} is not a number", shown(text));
    let (point, group) = match decimal {
        Decimal::Dot => ('.', ','),
        Decimal::Comma => (',', '.'),
    };
    let trimmed = text.trim();
    let (sign, unsigned) = match trimmed.strip_prefix(['+', '-']) {
        Some(rest) => (&trimmed[..1], rest),
        None => ("", trimmed),
    };
    let (mantissa, exponent) =
        unsigned.split_at(unsigned.find(['e', 'E']).unwrap_or(unsigned.len()));
    let (whole, fraction) = match mantissa.split_once(point) {
        Some((whole, fraction)) => (whole, format!(".{fraction}")),
        None => (mantissa, String::new()),
    };
    // A separator groups thousands or it is not one: `1,5` under a dot decimal is a typo, not 15.
    let mut groups = whole.split(group);
    let lead = groups.next().unwrap_or_default();
    if whole.contains(group) && !((1..=3).contains(&lead.len()) && groups.all(|g| g.len() == 3)) {
        return Err(not());
    }
    let cleaned = format!("{sign}{}{fraction}{exponent}", whole.replace(group, ""));
    // The character check keeps `inf` and `nan` out; the finite check keeps `1e999`, which overflows to inf.
    let plain = cleaned
        .chars()
        .all(|c| c.is_ascii_digit() || "+-.eE".contains(c));
    if plain && cleaned.parse::<f64>().is_ok_and(f64::is_finite) {
        Ok(Value::Number(cleaned))
    } else {
        Err(not())
    }
}

fn boolean(text: &str, spellings: &Spellings) -> Result<Value, String> {
    let word = text.trim().to_lowercase();
    let is = |list: &[String]| list.iter().any(|s| s.to_lowercase() == word);
    if is(&spellings.truthy) {
        Ok(Value::Bool(true))
    } else if is(&spellings.falsy) {
        Ok(Value::Bool(false))
    } else {
        Err(format!(
            "{} is not one of {}",
            shown(text),
            spellings.text()
        ))
    }
}

fn date(
    text: &str,
    format: DateFormat,
    column: Option<&str>,
    engine: Engine,
) -> Result<Value, String> {
    let trimmed = text.trim();
    let not = || format!("{} is not {}", shown(text), format.noun());
    // ponytail: MariaDB reads a `timestamp` literal in the session's zone and
    // refuses an offset, so it gets UTC's wall clock, exact on a UTC server
    // only. Reading `@@time_zone` on the import's session is the upgrade.
    let zone_aware = column.map(|column| zone_aware(column) && engine.reads_literal_offsets());
    // A Unix time is an instant: a column that keeps one is told it is UTC,
    // and one that keeps a wall clock gets UTC's.
    let unix = |at: OffsetDateTime, fraction| match zone_aware {
        Some(true) => format!("{}+00:00", stamp(at, true, fraction).replacen(' ', "T", 1)),
        _ => stamp(at, true, fraction),
    };
    let parsed = match format {
        DateFormat::Iso => {
            let (at, offset, _) = crate::mql::iso_parts(trimmed).ok_or_else(not)?;
            let (millis, _) = crate::mql::iso_millis(trimmed).ok_or_else(not)?;
            match (offset, zone_aware) {
                (Some(offset), Some(false)) if !offset.is_utc() => {
                    return Err(format!(
                        "{} has a UTC offset, which a `{}` column has no place for",
                        shown(text),
                        column.unwrap_or_default()
                    ));
                }
                // Sent bare, so a server that would read `Z` as "convert to my zone" cannot.
                (Some(_), Some(false)) => Some((wall_clock(at), millis)),
                _ => Some((trimmed.to_owned(), millis)),
            }
        }
        DateFormat::Calendar {
            order,
            separator,
            time,
        } => calendar_date(trimmed, order, separator, time),
        DateFormat::UnixSeconds => trimmed
            .parse::<i64>()
            .ok()
            .and_then(|n| OffsetDateTime::from_unix_timestamp(n).ok())
            .map(|at| (unix(at, false), at.unix_timestamp() * 1000)),
        DateFormat::UnixMillis => trimmed
            .parse::<i64>()
            .ok()
            .and_then(|n| i128::from(n).checked_mul(1_000_000))
            .and_then(|nanos| OffsetDateTime::from_unix_timestamp_nanos(nanos).ok())
            .map(|at| {
                let millis = at.unix_timestamp() * 1000 + i64::from(at.millisecond());
                (unix(at, true), millis)
            }),
    };
    parsed
        .map(|(text, millis)| Value::Date { text, millis })
        .ok_or_else(not)
}

fn wall_clock(at: PrimitiveDateTime) -> String {
    let mut out = stamp(at.assume_utc(), true, false);
    if at.nanosecond() > 0 {
        out.push_str(format!(".{:09}", at.nanosecond()).trim_end_matches('0'));
    }
    out
}

fn stamp(at: OffsetDateTime, with_time: bool, fraction: bool) -> String {
    let mut out = format!(
        "{:04}-{:02}-{:02}",
        at.year(),
        u8::from(at.month()),
        at.day()
    );
    if with_time {
        out.push_str(&format!(
            " {:02}:{:02}:{:02}",
            at.hour(),
            at.minute(),
            at.second()
        ));
    }
    if fraction {
        out.push_str(&format!(".{:03}", at.millisecond()));
    }
    out
}

/// Digit groups split on `separator`, each within its allowed width.
fn digit_groups(text: &str, separator: char, widths: &[usize]) -> Option<Vec<u32>> {
    let parts: Vec<&str> = text.split(separator).collect();
    if parts.len() != widths.len() {
        return None;
    }
    parts
        .iter()
        .zip(widths)
        .map(|(part, &width)| {
            let fits = part.len() == width || (width == 2 && part.len() == 1);
            if fits && part.bytes().all(|b| b.is_ascii_digit()) {
                part.parse().ok()
            } else {
                None
            }
        })
        .collect()
}

fn calendar_date(
    text: &str,
    order: Order,
    separator: char,
    with_time: bool,
) -> Option<(String, i64)> {
    let (day_part, time_part) = match text.split_once(' ') {
        Some((d, t)) => (d, Some(t.trim_start())),
        None => (text, None),
    };
    if time_part.is_some() != with_time {
        return None;
    }
    let (year, month, day) = match order {
        Order::Dmy => {
            let n = digit_groups(day_part, separator, &[2, 2, 4])?;
            (n[2], n[1], n[0])
        }
        Order::Mdy => {
            let n = digit_groups(day_part, separator, &[2, 2, 4])?;
            (n[2], n[0], n[1])
        }
        Order::Ymd => {
            let n = digit_groups(day_part, separator, &[4, 2, 2])?;
            (n[0], n[1], n[2])
        }
    };
    let date = Date::from_calendar_date(
        i32::try_from(year).ok()?,
        Month::try_from(u8::try_from(month).ok()?).ok()?,
        u8::try_from(day).ok()?,
    )
    .ok()?;
    let clock = match time_part {
        None => Time::MIDNIGHT,
        Some(t) => {
            let n = digit_groups(t, ':', &[2, 2, 2]).or_else(|| {
                let mut n = digit_groups(t, ':', &[2, 2])?;
                n.push(0);
                Some(n)
            })?;
            Time::from_hms(
                u8::try_from(n[0]).ok()?,
                u8::try_from(n[1]).ok()?,
                u8::try_from(n[2]).ok()?,
            )
            .ok()?
        }
    };
    let at = date.with_time(clock).assume_utc();
    Some((stamp(at, with_time, false), at.unix_timestamp() * 1000))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::db::{Column, Row};
    use crate::export::render_delimited;
    use crate::import_rows::read;

    fn text(t: &str, quoted: bool) -> Cell {
        Cell::Text {
            text: t.into(),
            quoted,
        }
    }
    fn opts() -> Options {
        Options::detected(Path::new("f.csv"), b"")
    }
    fn v(cell: Cell, rule: Rule, o: &Options) -> Result<Option<Value>, String> {
        value(&cell, &rule, o, None, Engine::Postgres)
    }

    #[test]
    fn as_is_follows_the_null_rules() {
        let o = opts();
        assert_eq!(v(text("", false), Rule::AsIs, &o), Ok(Some(Value::Null)));
        assert_eq!(
            v(text("", true), Rule::AsIs, &o),
            Ok(Some(Value::Text("".into())))
        );
        let empty = Options {
            empty: EmptyField::EmptyString,
            ..opts()
        };
        assert_eq!(
            v(text("", false), Rule::AsIs, &empty),
            Ok(Some(Value::Text("".into())))
        );
        let word = Options {
            null_word: "\\N".into(),
            ..opts()
        };
        assert_eq!(
            v(text("\\N", false), Rule::AsIs, &word),
            Ok(Some(Value::Null))
        );
        assert_eq!(
            v(text("\\N", true), Rule::AsIs, &word),
            Ok(Some(Value::Text("\\N".into())))
        );
        let trimmed = Options {
            null_word: "NULL".into(),
            trim: true,
            ..opts()
        };
        assert_eq!(
            v(text("  NULL ", false), Rule::AsIs, &trimmed),
            Ok(Some(Value::Null))
        );
        assert_eq!(
            v(text("null", false), Rule::AsIs, &trimmed),
            Ok(Some(Value::Text("null".into())))
        );
        let untrimmed = Options {
            null_word: "NULL".into(),
            ..opts()
        };
        assert_eq!(
            v(text("  NULL ", false), Rule::AsIs, &untrimmed),
            Ok(Some(Value::Null))
        );
    }

    #[test]
    fn the_export_round_trips_with_no_option_touched() {
        let columns = [
            Column {
                name: "id".into(),
                data_type: None,
            },
            Column {
                name: "note".into(),
                data_type: None,
            },
        ];
        let rows = [
            Row::new([Some("1"), None]).unwrap(),
            Row::new([Some("2"), Some("")]).unwrap(),
            Row::new([Some("3"), Some("a,\"b\"\nc")]).unwrap(),
        ];
        let file = render_delimited(',', &columns, &rows);
        let o = Options::detected(Path::new("f.csv"), file.as_bytes());
        let table = read(file.as_bytes(), &o).unwrap();
        let got: Vec<Vec<Value>> = table
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| {
                        value(cell, &Rule::AsIs, &o, Some("text"), Engine::Postgres)
                            .unwrap()
                            .unwrap()
                    })
                    .collect()
            })
            .collect();
        let t = |s: &str| Value::Text(s.into());
        assert_eq!(
            got,
            vec![
                vec![t("1"), Value::Null],
                vec![t("2"), t("")],
                vec![t("3"), t("a,\"b\"\nc")],
            ]
        );
    }

    #[test]
    fn text_kept_exactly_ignores_trim_and_the_null_word() {
        let o = Options {
            null_word: "NULL".into(),
            trim: true,
            ..opts()
        };
        assert_eq!(
            v(text(" NULL ", false), Rule::Exact, &o),
            Ok(Some(Value::Text(" NULL ".into())))
        );
        assert_eq!(v(text("", false), Rule::Exact, &o), Ok(Some(Value::Null)));
    }

    #[test]
    fn every_date_preset_rewrites_to_iso_8601() {
        let cal = |order, separator, time| DateFormat::Calendar {
            order,
            separator,
            time,
        };
        for (format, input, expected) in [
            (DateFormat::Iso, "2024-02-29", "2024-02-29"),
            (
                DateFormat::Iso,
                "2024-02-29T13:05:09Z",
                "2024-02-29T13:05:09Z",
            ),
            (cal(Order::Dmy, '/', false), "29/02/2024", "2024-02-29"),
            (cal(Order::Dmy, '/', false), "1/2/2024", "2024-02-01"),
            (cal(Order::Mdy, '/', false), "02/29/2024", "2024-02-29"),
            (cal(Order::Dmy, '.', false), "29.02.2024", "2024-02-29"),
            (cal(Order::Ymd, '/', false), "2024/02/29", "2024-02-29"),
            (
                cal(Order::Dmy, '/', true),
                "29/02/2024 13:05",
                "2024-02-29 13:05:00",
            ),
            (
                cal(Order::Dmy, '/', true),
                "29/02/2024 13:05:09",
                "2024-02-29 13:05:09",
            ),
            (DateFormat::UnixSeconds, "1709211909", "2024-02-29 13:05:09"),
            (
                DateFormat::UnixMillis,
                "1709211909123",
                "2024-02-29 13:05:09.123",
            ),
        ] {
            let Ok(Some(Value::Date { text: got, .. })) =
                v(text(input, false), Rule::Date(format), &opts())
            else {
                panic!("{input}")
            };
            assert_eq!(got, expected, "{input}");
        }
        let Ok(Some(Value::Date { millis, .. })) = v(
            text("2024-02-29", false),
            Rule::Date(DateFormat::Iso),
            &opts(),
        ) else {
            panic!()
        };
        assert_eq!(millis, 1_709_164_800_000);
    }

    #[test]
    fn a_date_keeps_its_instant_in_the_column_it_is_spelled_for() {
        let at = |input: &str, format, column| {
            value(
                &text(input, false),
                &Rule::Date(format),
                &opts(),
                column,
                Engine::Postgres,
            )
            .map(|v| match v {
                Some(Value::Date { text, .. }) => text,
                other => panic!("{other:?}"),
            })
        };
        let (iso, secs, millis) = (
            DateFormat::Iso,
            DateFormat::UnixSeconds,
            DateFormat::UnixMillis,
        );
        let zoned = [
            "timestamp with time zone",
            "datetimeoffset(7)",
            "timestamp",
            "timestamp(6)",
            "TIMESTAMP_TZ",
        ];
        for column in zoned {
            assert_eq!(
                at("1709211909", secs, Some(column)),
                Ok("2024-02-29T13:05:09+00:00".into())
            );
            assert_eq!(
                at("2024-02-29T13:05:09+02:00", iso, Some(column)),
                Ok("2024-02-29T13:05:09+02:00".into())
            );
        }
        assert_eq!(
            at("1709211909123", millis, Some("timestamptz")),
            Ok("2024-02-29T13:05:09.123+00:00".into())
        );
        let unzoned = [
            "timestamp without time zone",
            "timestamp(3) without time zone",
            "datetime2(7)",
            "datetime",
            "TIMESTAMP_NTZ",
            "date",
        ];
        for column in unzoned {
            assert_eq!(
                at("1709211909", secs, Some(column)),
                Ok("2024-02-29 13:05:09".into())
            );
            assert_eq!(
                at("2024-02-29T13:05:09+02:00", iso, Some(column)),
                Err(format!(
                    "`2024-02-29T13:05:09+02:00` has a UTC offset, which a `{column}` column has no place for"
                ))
            );
            assert_eq!(
                at("2024-02-29T13:05:09.25Z", iso, Some(column)),
                Ok("2024-02-29 13:05:09.25".into())
            );
            assert_eq!(
                at("2024-02-29T13:05:09+00:00", iso, Some(column)),
                Ok("2024-02-29 13:05:09".into())
            );
            assert_eq!(
                at("2024-02-29T13:05", iso, Some(column)),
                Ok("2024-02-29T13:05".into())
            );
        }
        assert_eq!(
            at("2024-02-29T13:05:09+02:00", iso, None),
            Ok("2024-02-29T13:05:09+02:00".into())
        );
    }

    #[test]
    fn mariadb_takes_a_timestamp_as_utc_wall_clock_since_it_refuses_an_offset() {
        let at = |input: &str, format| {
            value(
                &text(input, false),
                &Rule::Date(format),
                &opts(),
                Some("timestamp"),
                Engine::MariaDb,
            )
            .map(|v| match v {
                Some(Value::Date { text, .. }) => text,
                other => panic!("{other:?}"),
            })
        };
        assert_eq!(
            at("1709211909", DateFormat::UnixSeconds),
            Ok("2024-02-29 13:05:09".into())
        );
        assert_eq!(
            at("2024-02-29T13:05:09Z", DateFormat::Iso),
            Ok("2024-02-29 13:05:09".into())
        );
        assert!(at("2024-02-29T13:05:09+02:00", DateFormat::Iso).is_err());
    }

    #[test]
    fn a_json_boolean_lands_in_a_text_column_as_its_word() {
        let o = opts();
        let yes = Cell::Json(Json::Bool(true));
        for column in ["text", "varchar(20)", "nvarchar(max)", "TEXT", "json"] {
            for rule in [Rule::AsIs, Rule::Exact] {
                assert_eq!(
                    value(&yes, &rule, &o, Some(column), Engine::Postgres),
                    Ok(Some(Value::Text("true".into())))
                );
            }
        }
        for column in [Some("boolean"), Some("tinyint(1)"), Some("bit"), None] {
            assert_eq!(
                value(&yes, &Rule::AsIs, &o, column, Engine::Postgres),
                Ok(Some(Value::Bool(true)))
            );
        }
        assert_eq!(
            value(&yes, &Rule::Boolean, &o, Some("text"), Engine::Postgres),
            Ok(Some(Value::Bool(true)))
        );
    }

    #[test]
    fn a_date_that_does_not_parse_is_a_problem_never_null() {
        let cal = |time| DateFormat::Calendar {
            order: Order::Dmy,
            separator: '/',
            time,
        };
        assert_eq!(
            v(text("31/02/2024", false), Rule::Date(cal(false)), &opts()),
            Err("`31/02/2024` is not a DD/MM/YYYY date".into())
        );
        assert_eq!(
            v(text("29/02/2024", false), Rule::Date(cal(true)), &opts()),
            Err("`29/02/2024` is not a DD/MM/YYYY HH:mm[:ss] date".into())
        );
        assert_eq!(
            v(
                text("12a", false),
                Rule::Date(DateFormat::UnixSeconds),
                &opts()
            ),
            Err("`12a` is not a Unix time in seconds".into())
        );
        assert_eq!(
            v(text("", true), Rule::Date(cal(false)), &opts()),
            Err("an empty string is not a DD/MM/YYYY date".into())
        );
    }

    #[test]
    fn numbers_drop_the_thousands_separator_and_use_a_dot() {
        for (decimal, input, expected) in [
            (Decimal::Comma, "1.234,56", "1234.56"),
            (Decimal::Dot, "1,234.56", "1234.56"),
            (Decimal::Dot, "-0.5", "-0.5"),
            (Decimal::Dot, "1e3", "1e3"),
            (Decimal::Dot, " 42 ", "42"),
            (Decimal::Comma, "1,5", "1.5"),
        ] {
            assert_eq!(
                v(text(input, false), Rule::Number(decimal), &opts()),
                Ok(Some(Value::Number(expected.into()))),
                "{input}"
            );
        }
        assert_eq!(
            v(text("1.2.3", false), Rule::Number(Decimal::Dot), &opts()),
            Err("`1.2.3` is not a number".into())
        );
        for (decimal, input) in [
            (Decimal::Dot, "1,5"),
            (Decimal::Dot, "1,2,3"),
            (Decimal::Dot, ",123"),
            (Decimal::Dot, "1234,567"),
            (Decimal::Dot, "1.5,0"),
            (Decimal::Comma, "1.5"),
            (Decimal::Comma, "1.23,4"),
            (Decimal::Comma, "1,2,3"),
        ] {
            assert_eq!(
                v(text(input, false), Rule::Number(decimal), &opts()),
                Err(format!("`{input}` is not a number")),
                "{input}"
            );
        }
        for (decimal, input, expected) in [
            (Decimal::Dot, "1,234,567", "1234567"),
            (Decimal::Dot, "-12,345.50", "-12345.50"),
            (Decimal::Comma, "1.234.567,89", "1234567.89"),
            (Decimal::Comma, "12345", "12345"),
        ] {
            assert_eq!(
                v(text(input, false), Rule::Number(decimal), &opts()),
                Ok(Some(Value::Number(expected.into()))),
                "{input}"
            );
        }
        assert!(v(text("inf", false), Rule::Number(Decimal::Dot), &opts()).is_err());
        for huge in ["1e999", "-1e999"] {
            assert!(v(text(huge, false), Rule::Number(Decimal::Dot), &opts()).is_err());
        }
    }

    #[test]
    fn booleans_read_the_default_spellings_case_insensitively() {
        for t in ["TRUE", "yes", "Y", "1", "t"] {
            assert_eq!(
                v(text(t, false), Rule::Boolean, &opts()),
                Ok(Some(Value::Bool(true)))
            );
        }
        for t in ["False", "NO", "n", "0", "F"] {
            assert_eq!(
                v(text(t, false), Rule::Boolean, &opts()),
                Ok(Some(Value::Bool(false)))
            );
        }
        assert_eq!(
            v(text("maybe", false), Rule::Boolean, &opts()),
            Err("`maybe` is not one of true/false, yes/no, y/n, 1/0, t/f".into())
        );
        assert_eq!(
            Spellings::parse("on/off, sí/no").unwrap(),
            Spellings {
                truthy: vec!["on".into(), "sí".into()],
                falsy: vec!["off".into(), "no".into()]
            }
        );
        assert_eq!(
            Spellings::parse("on"),
            Err("Each pair is written true/false, separated by commas.".into())
        );
        assert_eq!(
            Spellings::default().text(),
            "true/false, yes/no, y/n, 1/0, t/f"
        );
    }

    #[test]
    fn json_values_keep_their_types() {
        let o = opts();
        let json = |text: &str| Cell::Json(serde_json::from_str(text).unwrap());
        assert_eq!(v(json("null"), Rule::AsIs, &o), Ok(Some(Value::Null)));
        assert_eq!(
            v(
                json("\"NULL\""),
                Rule::AsIs,
                &Options {
                    null_word: "NULL".into(),
                    ..opts()
                }
            ),
            Ok(Some(Value::Text("NULL".into())))
        );
        for rule in [Rule::AsIs, Rule::Exact, Rule::Number(Decimal::Comma)] {
            for n in [
                "1.50",
                "123456789012345678901234567890",
                "0.12345678901234567890123",
            ] {
                assert_eq!(
                    v(json(n), rule.clone(), &o),
                    Ok(Some(Value::Number(n.into())))
                );
            }
        }
        assert_eq!(v(json("true"), Rule::AsIs, &o), Ok(Some(Value::Bool(true))));
        assert_eq!(
            v(json(r#"{"a": 1.50}"#), Rule::AsIs, &o),
            Ok(Some(Value::Json(Json::Object(vec![(
                "a".into(),
                Json::Number("1.50".into())
            )]))))
        );
        assert_eq!(v(Cell::Missing, Rule::AsIs, &o), Ok(None));
    }
}

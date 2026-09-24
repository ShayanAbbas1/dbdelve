//! The filter bars and sort keys, and the SQL fragments they add up to.
//!
//! A bar is a control, never an expression: one predicate over one column, so
//! the stack can be built, stored and re-derived without parsing anything back.
//! `Raw` is the single exception, and is not an operator at all.
//!
//! Everything here was a free function or a plain data type at the crate root.
//! They moved out whole; nothing changed but their visibility.

use gpui::{App, AppContext, Context, Entity, Window};
use gpui_component::input::{InputEvent, InputState};
use serde::Deserialize;

use crate::{Workspace, db, db::Engine, explorer::preview_sql, sql, sql::SortKey, store};

/// What a filter bar compares its column against.
///
/// The whole list is equality-shaped in the sense that matters here: every arm
/// is one predicate over one column, so the bar stays a control and never
/// becomes an expression. `Raw` is the exception and is not an operator at all
/// — see [`FilterBar::raw`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Deserialize)]
pub(crate) enum Operator {
    #[default]
    Equals,
    NotEquals,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
    IsNull,
    IsNotNull,
    IsEmpty,
    IsNotEmpty,
    InList,
    NotInList,
    Between,
    Regex,
}

impl Operator {
    /// Every operator in the order the dropdown offers them, which is the order
    /// they were asked for: the common comparisons, then the absences, then the
    /// set and range shapes.
    pub(crate) const ALL: [Self; 18] = [
        Self::Equals,
        Self::NotEquals,
        Self::Contains,
        Self::NotContains,
        Self::StartsWith,
        Self::EndsWith,
        Self::Greater,
        Self::GreaterOrEqual,
        Self::Less,
        Self::LessOrEqual,
        Self::IsNull,
        Self::IsNotNull,
        Self::IsEmpty,
        Self::IsNotEmpty,
        Self::InList,
        Self::NotInList,
        Self::Between,
        Self::Regex,
    ];

    /// What the bar's own button shows: the SQL shape rather than the English,
    /// because the bar is read beside the statement it writes.
    pub(crate) fn symbol(self) -> &'static str {
        match self {
            Self::Equals => "=",
            Self::NotEquals => "!=",
            Self::Contains => "LIKE %..%",
            Self::NotContains => "NOT LIKE %..%",
            Self::StartsWith => "LIKE ..%",
            Self::EndsWith => "LIKE %..",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::IsNull => "is NULL",
            Self::IsNotNull => "is not NULL",
            Self::IsEmpty => "is empty",
            Self::IsNotEmpty => "is not empty",
            Self::InList => "IN (..)",
            Self::NotInList => "NOT IN (..)",
            Self::Between => "BETWEEN",
            Self::Regex => "~",
        }
    }

    /// What the dropdown row reads: the symbol and the English for it, so the
    /// list can be scanned by either.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Equals => "= equals",
            Self::NotEquals => "!= not equals",
            Self::Contains => "LIKE %..% contains",
            Self::NotContains => "NOT LIKE %..% not contains",
            Self::StartsWith => "LIKE ..% starts with",
            Self::EndsWith => "LIKE %.. ends with",
            Self::Greater => "> greater than",
            Self::GreaterOrEqual => ">= greater or equal",
            Self::Less => "< less than",
            Self::LessOrEqual => "<= less or equal",
            Self::InList => "IN (..) in list",
            Self::NotInList => "NOT IN (..) not in list",
            Self::Between => "BETWEEN between",
            Self::Regex => "~ matches regex",
            // The four that are their own English already.
            other => other.symbol(),
        }
    }

    /// Whether the bar shows a value input at all. An absence needs no value,
    /// and a box that cannot change what runs is a box to read past.
    pub(crate) fn takes_value(self) -> bool {
        !matches!(
            self,
            Self::IsNull | Self::IsNotNull | Self::IsEmpty | Self::IsNotEmpty
        )
    }

    /// What the value input asks for, where the shape is not a plain value.
    pub(crate) fn placeholder(self) -> &'static str {
        match self {
            Self::Between => "min..max",
            Self::InList | Self::NotInList => "a, b, c",
            _ => "Value…",
        }
    }

    /// Whether this engine can express the operator at all. Only the regex
    /// match cannot: SQLite ships no `REGEXP` implementation, so the operator is
    /// a syntax error until an application registers the function (spec §7).
    pub(crate) fn on(self, engine: Engine) -> bool {
        self != Self::Regex || engine != Engine::Sqlite
    }

    /// How the operator is written to disk. A name rather than an index, so
    /// inserting an arm cannot silently rewrite everyone's saved bars, and one
    /// this build does not know reads back as the default it always had.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Equals => "equals",
            Self::NotEquals => "not-equals",
            Self::Contains => "contains",
            Self::NotContains => "not-contains",
            Self::StartsWith => "starts-with",
            Self::EndsWith => "ends-with",
            Self::Greater => "greater",
            Self::GreaterOrEqual => "greater-or-equal",
            Self::Less => "less",
            Self::LessOrEqual => "less-or-equal",
            Self::IsNull => "is-null",
            Self::IsNotNull => "is-not-null",
            Self::IsEmpty => "is-empty",
            Self::IsNotEmpty => "is-not-empty",
            Self::InList => "in-list",
            Self::NotInList => "not-in-list",
            Self::Between => "between",
            Self::Regex => "regex",
        }
    }

    pub(crate) fn from_slug(slug: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|operator| operator.slug() == slug)
            .unwrap_or_default()
    }
}

/// How a bar joins to the bar above it. The first bar in a stack has nothing to
/// join to and its own value is never read.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Deserialize)]
pub(crate) enum Conjunction {
    #[default]
    And,
    Or,
}

impl Conjunction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::And => "AND",
            Self::Or => "OR",
        }
    }

    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::And => Self::Or,
            Self::Or => Self::And,
        }
    }

    /// `AND` for anything this build cannot read, which is what every bar
    /// written before the joiner existed was.
    pub(crate) fn from_str(value: &str) -> Self {
        match value {
            "OR" => Self::Or,
            _ => Self::And,
        }
    }
}

/// One filter bar as the UI holds it, with no window in sight: the value type
/// every predicate is derived from and the shape that goes to disk.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct FilterBar {
    /// `None` until the dropdown has been used, which is a bar that narrows
    /// nothing. Unread on a raw bar.
    pub(crate) column: Option<String>,
    pub(crate) operator: Operator,
    pub(crate) conjunction: Conjunction,
    /// Whether the value is SQL of the user's own, conjoined verbatim rather
    /// than built from a column and an operator. `sql::is_generated_select` is
    /// what stands behind it, and is why nothing here inspects the text.
    pub(crate) raw: bool,
    pub(crate) value: String,
}

/// One filter bar on screen: [`FilterBar`] with its value in an input.
pub(crate) struct FilterRow {
    pub(crate) column: Option<String>,
    pub(crate) operator: Operator,
    pub(crate) conjunction: Conjunction,
    pub(crate) raw: bool,
    pub(crate) value: Entity<InputState>,
}

/// A filter bar, wired to the tab it belongs to. Enter is the apply: a filter
/// that ran on every keystroke would put a half-typed predicate on the wire.
pub(crate) fn filter_row(
    id: u64,
    bar: FilterBar,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> FilterRow {
    let placeholder = value_placeholder(bar.raw, bar.operator);
    let input = cx.new(|cx| {
        let mut state = InputState::new(window, cx).placeholder(placeholder);
        state.set_value(bar.value, window, cx);
        state
    });
    cx.subscribe(&input, move |workspace, _, event: &InputEvent, cx| {
        if matches!(event, InputEvent::PressEnter { .. }) {
            workspace.apply_filter(id, cx);
        }
    })
    .detach();
    FilterRow {
        column: bar.column,
        operator: bar.operator,
        conjunction: bar.conjunction,
        raw: bar.raw,
        value: input,
    }
}

pub(crate) fn value_placeholder(raw: bool, operator: Operator) -> &'static str {
    match raw {
        true => "SQL…",
        false => operator.placeholder(),
    }
}

/// Every bar as it stands, unfinished ones included.
pub(crate) fn filter_bars(filters: &[FilterRow], cx: &App) -> Vec<FilterBar> {
    filters
        .iter()
        .map(|row| FilterBar {
            column: row.column.clone(),
            operator: row.operator,
            conjunction: row.conjunction,
            raw: row.raw,
            value: row.value.read(cx).value().to_string(),
        })
        .collect()
}

/// The bars that narrow anything, which is the same question as whether a bar
/// has a predicate: an unfinished one reaches neither the statement nor the
/// file.
pub(crate) fn applied_filters(engine: Engine, bars: &[FilterBar]) -> Vec<FilterBar> {
    bars.iter()
        .filter(|bar| bar_predicate(engine, bar).is_some())
        .cloned()
        .collect()
}

/// The `WHERE` the bars add up to, without the keyword and empty for no bars.
///
/// Folded left to right with each side parenthesised, so the stack means what
/// it looks like: `a OR b` then `AND c` reads `(a OR b) AND c` and not the
/// `a OR (b AND c)` that SQL's own precedence would give it. A lone bar is
/// unwrapped, which is also what keeps a tab filtered before joiners existed
/// on the grid-snapshot key it already had.
pub(crate) fn derived_filter(engine: Engine, bars: &[FilterBar]) -> String {
    let mut folded: Option<String> = None;
    for bar in bars {
        let Some(predicate) = bar_predicate(engine, bar) else {
            continue;
        };
        folded = Some(match folded {
            None => predicate,
            Some(left) => format!("({left}) {} ({predicate})", bar.conjunction.as_str()),
        });
    }
    folded.unwrap_or_default()
}

/// The predicate one bar contributes, or `None` for a bar that narrows nothing:
/// no column picked, no value where the operator needs one, an empty list, half
/// a range, or an operator this engine does not have.
pub(crate) fn bar_predicate(engine: Engine, bar: &FilterBar) -> Option<String> {
    let value = bar.value.trim();
    if bar.raw {
        // Verbatim, and checked as a whole statement by `is_generated_select`
        // rather than inspected here: a filter dbdelve does not understand is
        // exactly what the gate is for (spec §2.3).
        return (!value.is_empty()).then(|| value.to_string());
    }
    let column = bar.column.as_deref()?;
    if !bar.operator.on(engine) || (bar.operator.takes_value() && value.is_empty()) {
        return None;
    }
    filter_predicate(engine, column, bar.operator, value)
}

/// One filter against the tab's, trimmed. `false` when nothing moved, so
/// retyping the same expression does not re-run the statement.
///
/// A change puts the page back to the first, for the reason a sort or a limit
/// change does: page three of a different question is not a page the user
/// asked for.
pub(crate) fn changed_filter(filter: &mut String, offset: &mut usize, typed: &str) -> bool {
    let typed = typed.trim();
    if filter == typed {
        return false;
    }
    *filter = typed.to_string();
    *offset = 0;
    true
}

/// One column against one value under one operator, quoted the way the server
/// will read it. `None` where the value does not add up to a predicate.
///
/// A SQL-generating call site (`AGENTS.md`, engine divergences), and the only
/// place a filter bar becomes SQL -- every operator composes here, through
/// `quote_identifier` and `quote_literal`, so there is one place a quote can be
/// got wrong rather than one per operator.
pub(crate) fn filter_predicate(
    engine: Engine,
    column: &str,
    operator: Operator,
    value: &str,
) -> Option<String> {
    let name = engine.quote_identifier(column);
    let literal = |value: &str| engine.quote_literal(value);
    let comparison = |symbol: &str| Some(format!("{name} {symbol} {}", literal(value)));
    // `<>` rather than `!=` on both of the negations, because it is the
    // spelling all three engines agree on; the dropdown says `!=` because that
    // is the one people read.
    match operator {
        Operator::Equals => comparison("="),
        Operator::NotEquals => comparison("<>"),
        Operator::Greater => comparison(">"),
        Operator::GreaterOrEqual => comparison(">="),
        Operator::Less => comparison("<"),
        Operator::LessOrEqual => comparison("<="),
        Operator::IsNull => Some(format!("{name} IS NULL")),
        Operator::IsNotNull => Some(format!("{name} IS NOT NULL")),
        Operator::IsEmpty => Some(format!("{name} = {}", literal(""))),
        Operator::IsNotEmpty => Some(format!("{name} <> {}", literal(""))),
        Operator::Contains | Operator::NotContains | Operator::StartsWith | Operator::EndsWith => {
            Some(substring(engine, &name, operator, value))
        }
        Operator::InList | Operator::NotInList => {
            let items: Vec<_> = value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(&literal)
                .collect();
            // An empty list is `IN ()`, which does not parse anywhere.
            if items.is_empty() {
                return None;
            }
            let keyword = match operator {
                Operator::NotInList => "NOT IN",
                _ => "IN",
            };
            Some(format!("{name} {keyword} ({})", items.join(", ")))
        }
        Operator::Between => {
            // One input rather than two, split on the ellipsis the placeholder
            // shows. Half a range is not a range.
            let (low, high) = value.split_once("..")?;
            let (low, high) = (low.trim(), high.trim());
            if low.is_empty() || high.is_empty() {
                return None;
            }
            Some(format!(
                "{name} BETWEEN {} AND {}",
                literal(low),
                literal(high)
            ))
        }
        // Postgres spells it as an operator; MySQL's own `REGEXP` infix is not
        // in the grammar the gate parses with, so the function form it has had
        // since 8.0.4 goes out instead. SQLite has no regex at all, which
        // `Operator::on` is what refuses (spec §7).
        Operator::Regex => match engine {
            Engine::Postgres => comparison("~"),
            Engine::MySql => Some(format!("REGEXP_LIKE({name}, {})", literal(value))),
            Engine::Sqlite => None,
            // Not `REGEXP_LIKE`: Snowflake's anchors the pattern to the whole
            // value, where the other two match anywhere in it. Counting matches
            // asks the question the dropdown's entry has always meant.
            Engine::Snowflake => Some(format!("REGEXP_COUNT({name}, {}) > 0", literal(value))),
        },
    }
}

/// Whether one column holds a value, at the start, at the end or anywhere.
///
/// `LIKE` where the engine has an escape character for the wildcards the user's
/// own value may hold, and exact substring arithmetic where it does not. Two
/// engine divergences meet here (`AGENTS.md`, and spec §7):
///
/// The escape is the engine's **default** rather than an `ESCAPE` clause naming
/// it, because `tree_sitter_sequel` does not parse one and `is_generated_select`
/// would refuse every `LIKE` this writes. Postgres and MySQL both default to
/// the backslash, which is what [`like_pattern`] escapes with.
///
/// **SQLite has no default escape character at all**, so a pattern is the wrong
/// tool there: a value holding `%` or `_` would silently widen the match, which
/// is the one failure this exists to prevent. `instr` and `substr` ask the same
/// question with no pattern to escape. They are case-sensitive where SQLite's
/// `LIKE` is not, which is the cost of the trade and is smaller than answering
/// a question nobody asked.
pub(crate) fn substring(engine: Engine, name: &str, operator: Operator, value: &str) -> String {
    let literal = engine.quote_literal(value);
    if engine == Engine::Sqlite {
        return match operator {
            Operator::NotContains => format!("instr({name}, {literal}) = 0"),
            Operator::StartsWith => format!("instr({name}, {literal}) = 1"),
            // A suffix longer than the value leaves the whole of it, which
            // cannot equal a longer literal -- so no length guard is needed.
            Operator::EndsWith => format!(
                "substr({name}, -{}) = {literal}",
                value.chars().count().max(1)
            ),
            _ => format!("instr({name}, {literal}) > 0"),
        };
    }
    // No default escape here either, but there are exact functions for the
    // job, so there is no arithmetic to do. Case-sensitive, like SQLite's arm.
    if engine == Engine::Snowflake {
        let function = match operator {
            Operator::StartsWith => "STARTSWITH",
            Operator::EndsWith => "ENDSWITH",
            _ => "CONTAINS",
        };
        let negation = match operator {
            Operator::NotContains => "NOT ",
            _ => "",
        };
        return format!("{negation}{function}({name}, {literal})");
    }
    let escaped = like_pattern(value);
    let pattern = match operator {
        Operator::StartsWith => format!("{escaped}%"),
        Operator::EndsWith => format!("%{escaped}"),
        _ => format!("%{escaped}%"),
    };
    format!(
        "{name} {}LIKE {}",
        match operator {
            Operator::NotContains => "NOT ",
            _ => "",
        },
        engine.quote_literal(&pattern)
    )
}

/// The user's text as the literal part of a `LIKE` pattern: the wildcards it
/// holds are escaped with the backslash both pattern engines take as their
/// default escape, so a value containing `%` matches a percent sign rather than
/// silently widening the match to anything.
pub(crate) fn like_pattern(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// The filter bar following a foreign key writes: the referenced column against
/// the value the cell held (spec §6.2).
///
/// A bar rather than an expression, because the bars are the editable state --
/// so the tab a key opens arrives with a control the user can change, and no
/// `WHERE` anywhere has to be parsed back into one. `derived_filter` does the
/// quoting, which is why this is no longer a SQL-generating call site.
///
/// `None` for a NULL, which references nothing. `<column> = NULL` is a filter
/// that parses, runs, matches no row, and looks like a bug in the data rather
/// than in the gesture.
pub(crate) fn foreign_key_filter(key: &db::ForeignKey, value: Option<&str>) -> Option<FilterBar> {
    Some(FilterBar {
        column: Some(key.referenced_column.clone()),
        value: value?.to_string(),
        ..FilterBar::default()
    })
}

/// A bar on its way to disk. The operator and the joiner travel as names and
/// the raw flag as itself, because a `WHERE` is never read back into controls.
pub(crate) fn stored_filter(bar: &FilterBar) -> store::StoredFilter {
    store::StoredFilter {
        column: bar.column.clone().unwrap_or_default(),
        value: bar.value.clone(),
        operator: bar.operator.slug().to_string(),
        conjunction: bar.conjunction.as_str().to_string(),
        raw: bar.raw,
    }
}

/// What a stored tab's `WHERE` is on the engine it is being reopened on.
///
/// Derived from the bars rather than trusted as written: the two agree for a
/// profile that has not changed engine, and where it has, an operator the new
/// engine cannot express drops out rather than being sent to a server that
/// cannot parse it (spec §7). A tab stored before the bars existed has none,
/// and keeps the expression it came with.
pub(crate) fn restored_filter(
    engine: Engine,
    stored: &store::StoredObject,
) -> (String, Vec<FilterBar>) {
    let bars = stored_bars(stored);
    let filter = match bars.is_empty() {
        true => stored.filter.clone(),
        false => derived_filter(engine, &bars),
    };
    (filter, bars)
}

/// The bars a stored tab comes back with. `bars` is what this build writes;
/// `filters` is the column-and-value pair an older one wrote, which restores as
/// the equality joined by `AND` that it was.
pub(crate) fn stored_bars(stored: &store::StoredObject) -> Vec<FilterBar> {
    if stored.bars.is_empty() {
        return stored
            .filters
            .iter()
            .map(|(column, value)| FilterBar {
                column: Some(column.clone()),
                value: value.clone(),
                ..FilterBar::default()
            })
            .collect();
    }
    stored
        .bars
        .iter()
        .map(|bar| FilterBar {
            column: Some(bar.column.clone()).filter(|column| !column.is_empty()),
            operator: Operator::from_slug(&bar.operator),
            conjunction: Conjunction::from_str(&bar.conjunction),
            raw: bar.raw,
            value: bar.value.clone(),
        })
        .collect()
}

/// dbdelve's statement for a relation's tab, carrying the filter and the sort the
/// controls asked for. Regenerated rather than edited, so the row limit and the
/// quoting stay in one place.
pub(crate) fn relation_sql(
    engine: Engine,
    schema: &str,
    relation: &str,
    filter: &str,
    sort: &[SortKey],
    limit: usize,
    offset: usize,
) -> String {
    let preview = preview_sql(engine, schema, relation, filter, limit, offset);
    sql::with_order_by(&preview, sort).unwrap_or(preview)
}

/// How a column is named in an `ORDER BY`.
///
/// By name, so the statement reads as something a person would have written --
/// except where a name cannot identify one column, and then by position, which
/// always can. Duplicate names come back from any join written with `*`.
pub(crate) fn sort_expression(
    engine: Engine,
    columns: &[db::Column],
    column: usize,
) -> Option<String> {
    let name = &columns.get(column)?.name;
    let unique = columns.iter().filter(|other| &other.name == name).count() == 1;

    Some(match unique && !name.is_empty() {
        true => engine.quote_identifier(name),
        false => (column + 1).to_string(),
    })
}

/// One header click against a sort: append, turn around, or drop out.
pub(crate) fn cycle(keys: &mut Vec<SortKey>, expression: &str) {
    match keys.iter().position(|key| key.expression == expression) {
        None => keys.push(SortKey::new(expression, true)),
        Some(index) if keys[index].ascending => keys[index].ascending = false,
        Some(index) => {
            keys.remove(index);
        }
    }
}

/// Which of a result's columns the statement ordered by, for the headers to
/// show. A key naming something other than a column of the result -- an
/// expression, or a column that is not in the select list -- lights nothing up,
/// because there is no header for it.
pub(crate) fn sort_columns(
    engine: Engine,
    keys: &[SortKey],
    columns: &[db::Column],
) -> Vec<(usize, bool)> {
    keys.iter()
        .filter_map(|key| {
            let expression = key.expression.trim();
            let named = engine.unquote_identifier(expression);
            let column = columns
                .iter()
                .position(|column| column.name == named)
                .or_else(|| {
                    expression
                        .parse::<usize>()
                        .ok()
                        .filter(|position| (1..=columns.len()).contains(position))
                        .map(|position| position - 1)
                })?;
            Some((column, key.ascending))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, db::RelationKind, explorer, explorer::PREVIEW_ROW_LIMIT, sql};

    fn columns(names: &[&str]) -> Vec<db::Column> {
        names
            .iter()
            .map(|name| db::Column {
                name: (*name).to_string(),
                data_type: None,
            })
            .collect()
    }

    /// A bar as the UI holds it. Most tests care about one operator and one
    /// value, and nothing else about the bar.
    fn bar(column: Option<&str>, operator: Operator, value: &str) -> FilterBar {
        FilterBar {
            column: column.map(str::to_string),
            operator,
            value: value.to_string(),
            ..FilterBar::default()
        }
    }

    /// The predicate one operator writes over one column.
    fn predicate(engine: Engine, operator: Operator, value: &str) -> Option<String> {
        bar_predicate(engine, &bar(Some("state"), operator, value))
    }

    #[test]
    fn a_bar_writes_a_quoted_predicate_into_the_filter() {
        // The identifier and the literal are both the user's text, and both are
        // quoted the way the engine will read them -- a double-quoted name is a
        // string literal on MySQL, which is the silent failure this exists to
        // avoid.
        assert_eq!(
            predicate(Engine::Postgres, Operator::Equals, "ok").as_deref(),
            Some(r#""state" = 'ok'"#)
        );
        assert_eq!(
            predicate(Engine::MySql, Operator::Equals, "ok").as_deref(),
            Some("`state` = 'ok'")
        );
        assert_eq!(
            predicate(Engine::Sqlite, Operator::Equals, "ok").as_deref(),
            Some(r#""state" = 'ok'"#)
        );
        // An apostrophe in the value and a quote in the column name are the two
        // ways a typed value becomes SQL of its own.
        assert_eq!(
            filter_predicate(Engine::Postgres, r#"od"d"#, Operator::Equals, "it's").as_deref(),
            Some(r#""od""d" = 'it''s'"#)
        );
        // And a backslash is the third, on the one engine that reads it as an
        // escape.
        assert_eq!(
            filter_predicate(Engine::MySql, "path", Operator::Equals, r"a\b").as_deref(),
            Some(r"`path` = 'a\\b'")
        );
    }

    #[test]
    fn every_operator_writes_the_shape_its_symbol_promises() {
        let sql = |operator, value| predicate(Engine::Postgres, operator, value).expect("applied");
        assert_eq!(sql(Operator::Equals, "1"), r#""state" = '1'"#);
        // `<>` on both negations rather than `!=`: it is the spelling all three
        // engines agree on, and the dropdown says `!=` because that is the one
        // people read.
        assert_eq!(sql(Operator::NotEquals, "1"), r#""state" <> '1'"#);
        assert_eq!(sql(Operator::Greater, "1"), r#""state" > '1'"#);
        assert_eq!(sql(Operator::GreaterOrEqual, "1"), r#""state" >= '1'"#);
        assert_eq!(sql(Operator::Less, "1"), r#""state" < '1'"#);
        assert_eq!(sql(Operator::LessOrEqual, "1"), r#""state" <= '1'"#);
        assert_eq!(sql(Operator::IsNull, ""), r#""state" IS NULL"#);
        assert_eq!(sql(Operator::IsNotNull, ""), r#""state" IS NOT NULL"#);
        assert_eq!(sql(Operator::IsEmpty, ""), r#""state" = ''"#);
        assert_eq!(sql(Operator::IsNotEmpty, ""), r#""state" <> ''"#);
        assert_eq!(sql(Operator::Contains, "ok"), r#""state" LIKE '%ok%'"#);
        assert_eq!(
            sql(Operator::NotContains, "ok"),
            r#""state" NOT LIKE '%ok%'"#
        );
        assert_eq!(sql(Operator::StartsWith, "ok"), r#""state" LIKE 'ok%'"#);
        assert_eq!(sql(Operator::EndsWith, "ok"), r#""state" LIKE '%ok'"#);
        assert_eq!(sql(Operator::InList, "a, b"), r#""state" IN ('a', 'b')"#);
        assert_eq!(
            sql(Operator::NotInList, "a, b"),
            r#""state" NOT IN ('a', 'b')"#
        );
        assert_eq!(
            sql(Operator::Between, "1..9"),
            r#""state" BETWEEN '1' AND '9'"#
        );
        assert_eq!(sql(Operator::Regex, "^a"), r#""state" ~ '^a'"#);
    }

    #[test]
    fn a_like_value_cannot_smuggle_a_wildcard_of_its_own() {
        // `50%` is a value, not "anything starting with 50" -- and without the
        // escaping the widening is silent, which is the whole hazard. The
        // backslash is what Postgres and MySQL take as the escape with no
        // `ESCAPE` clause naming it, which is the clause the gate's grammar
        // cannot read.
        assert_eq!(
            predicate(Engine::Postgres, Operator::Contains, "50%").as_deref(),
            Some(r#""state" LIKE '%50\%%'"#)
        );
        assert_eq!(
            predicate(Engine::Postgres, Operator::StartsWith, "a_b").as_deref(),
            Some(r#""state" LIKE 'a\_b%'"#)
        );
        // The escape character itself, or the escaping would be escapable.
        assert_eq!(
            predicate(Engine::Postgres, Operator::Contains, r"a\b").as_deref(),
            Some(r#""state" LIKE '%a\\b%'"#)
        );
        // MySQL reads a backslash inside a literal as an escape of its own, so
        // every one of them is doubled on the way into the string and the
        // pattern still sees the single one the escaping put there.
        assert_eq!(
            predicate(Engine::MySql, Operator::Contains, "50%").as_deref(),
            Some(r"`state` LIKE '%50\\%%'")
        );
    }

    #[test]
    fn sqlite_asks_for_a_substring_rather_than_a_pattern_it_cannot_escape() {
        // SQLite has no default escape character, so `%` in a value would be a
        // wildcard whatever the pattern did. `instr` has no pattern to escape.
        assert_eq!(
            predicate(Engine::Sqlite, Operator::Contains, "50%").as_deref(),
            Some(r#"instr("state", '50%') > 0"#)
        );
        assert_eq!(
            predicate(Engine::Sqlite, Operator::NotContains, "ok").as_deref(),
            Some(r#"instr("state", 'ok') = 0"#)
        );
        assert_eq!(
            predicate(Engine::Sqlite, Operator::StartsWith, "ok").as_deref(),
            Some(r#"instr("state", 'ok') = 1"#)
        );
        // Counted in characters rather than bytes, or a multi-byte suffix asks
        // for more of the string than it is.
        assert_eq!(
            predicate(Engine::Sqlite, Operator::EndsWith, "ök").as_deref(),
            Some(r#"substr("state", -2) = 'ök'"#)
        );
    }

    #[test]
    fn a_list_is_split_on_commas_and_quoted_item_by_item() {
        assert_eq!(
            predicate(Engine::Postgres, Operator::InList, " a , b ,, c ").as_deref(),
            Some(r#""state" IN ('a', 'b', 'c')"#)
        );
        // Each item is a literal of its own, so a comma cannot be typed into
        // one to end it early.
        assert_eq!(
            predicate(Engine::Postgres, Operator::InList, "it's").as_deref(),
            Some(r#""state" IN ('it''s')"#)
        );
        // `IN ()` does not parse anywhere, so an empty list is no predicate.
        assert_eq!(predicate(Engine::Postgres, Operator::InList, " , "), None);
    }

    #[test]
    fn a_range_needs_both_of_its_halves() {
        assert_eq!(
            predicate(Engine::Postgres, Operator::Between, " 1 .. 9 ").as_deref(),
            Some(r#""state" BETWEEN '1' AND '9'"#)
        );
        assert_eq!(predicate(Engine::Postgres, Operator::Between, "1"), None);
        assert_eq!(predicate(Engine::Postgres, Operator::Between, "1.."), None);
        assert_eq!(predicate(Engine::Postgres, Operator::Between, "..9"), None);
    }

    #[test]
    fn an_absence_is_applied_with_no_value_at_all() {
        // The four that take no value are the four that would otherwise be
        // unreachable: their bar shows no input to type into.
        for operator in [
            Operator::IsNull,
            Operator::IsNotNull,
            Operator::IsEmpty,
            Operator::IsNotEmpty,
        ] {
            assert!(!operator.takes_value(), "{} takes a value", operator.slug());
            assert!(
                predicate(Engine::Postgres, operator, "").is_some(),
                "{} was not applied",
                operator.slug()
            );
        }
        // And every other operator needs one.
        for operator in Operator::ALL.into_iter().filter(|o| o.takes_value()) {
            assert!(
                predicate(Engine::Postgres, operator, "").is_none(),
                "{} was applied with no value",
                operator.slug()
            );
        }
    }

    #[test]
    fn sqlite_is_neither_offered_nor_sent_a_regex_it_cannot_run() {
        // SQLite ships no `REGEXP`, so the operator is a syntax error there
        // rather than a query that returns nothing.
        assert!(Operator::Regex.on(Engine::Postgres));
        assert!(Operator::Regex.on(Engine::MySql));
        assert!(!Operator::Regex.on(Engine::Sqlite));
        // The infix `REGEXP` MySQL documents is not in the grammar the gate
        // parses with, so the function form goes out instead.
        assert_eq!(
            predicate(Engine::MySql, Operator::Regex, "^a").as_deref(),
            Some("REGEXP_LIKE(`state`, '^a')")
        );
        // A bar restored onto a SQLite profile from a session that was on
        // another engine narrows nothing rather than breaking the statement.
        assert_eq!(predicate(Engine::Sqlite, Operator::Regex, "^a"), None);
    }

    #[test]
    fn snowflake_asks_with_exact_functions_rather_than_a_pattern() {
        // No default `LIKE` escape, as on SQLite, but functions that ask the
        // question directly -- so a `%` in the value is a percent sign.
        assert_eq!(
            predicate(Engine::Snowflake, Operator::Contains, "50%").as_deref(),
            Some(r#"CONTAINS("state", '50%')"#)
        );
        assert_eq!(
            predicate(Engine::Snowflake, Operator::NotContains, "ok").as_deref(),
            Some(r#"NOT CONTAINS("state", 'ok')"#)
        );
        assert_eq!(
            predicate(Engine::Snowflake, Operator::StartsWith, "ok").as_deref(),
            Some(r#"STARTSWITH("state", 'ok')"#)
        );
        assert_eq!(
            predicate(Engine::Snowflake, Operator::EndsWith, "ok").as_deref(),
            Some(r#"ENDSWITH("state", 'ok')"#)
        );
    }

    #[test]
    fn a_snowflake_regex_matches_anywhere_like_the_others() {
        // `REGEXP_LIKE` there anchors to the whole value, which would make the
        // same dropdown entry mean something narrower on one engine.
        assert!(Operator::Regex.on(Engine::Snowflake));
        assert_eq!(
            predicate(Engine::Snowflake, Operator::Regex, r"^a\d").as_deref(),
            Some(r#"REGEXP_COUNT("state", '^a\\d') > 0"#)
        );
    }

    #[test]
    fn a_raw_bar_is_the_users_own_sql_verbatim() {
        let raw = |value: &str| {
            bar_predicate(
                Engine::Postgres,
                &FilterBar {
                    raw: true,
                    value: value.to_string(),
                    ..FilterBar::default()
                },
            )
        };
        // Nothing here inspects it: `is_generated_select` is what stands behind
        // a raw bar, and it reads the whole statement rather than the fragment.
        assert_eq!(
            raw("id > 5 OR name IS NULL").as_deref(),
            Some("id > 5 OR name IS NULL")
        );
        assert_eq!(raw("   "), None);
    }

    #[test]
    fn probe_escape() {
        for filter in [
            r#""state" LIKE '%ok%'"#,
            r#""state" LIKE '%ok%' ESCAPE '\'"#,
            r#""state" LIKE '%ok%' escape '!'"#,
            r#"instr("state", 'a') > 0"#,
            r#"instr("state", 'a') = 0"#,
            r#"instr("state", 'a') = 1"#,
            r#"substr("state", -2) = 'ok'"#,
            r#"instr("state", 'it''s') > 0"#,
            r#"REGEXP_LIKE(`state`, '^a')"#,
            r#"NOT instr("state", 'a') > 0"#,
            r#"strpos("state", 'a') > 0"#,
            r#"REGEXP_LIKE("state", '^a')"#,
            r#""state" regexp '^a'"#,
            r#"("state" = '1') OR ("state" = '2')"#,
            r#""state" LIKE CONCAT('%', 'a', '%')"#,
            r#"lower("state") LIKE '%a%'"#,
            r#""state" NOT LIKE '%ok%'"#,
            r#""state" ~ '^a'"#,
            r#""state" REGEXP '^a'"#,
            r#""state" BETWEEN '1' AND '9'"#,
            r#""state" IN ('a', 'b')"#,
            r#""state" NOT IN ('a', 'b')"#,
            r#""state" IS NOT NULL"#,
            r#""state" <> ''"#,
        ] {
            let sql = explorer::preview_sql(Engine::Postgres, "public", "accounts", filter, 100, 0);
            println!("{} {filter}", sql::is_generated_select(&sql));
        }
    }

    #[test]
    fn every_operator_writes_a_statement_the_gate_admits() {
        // The other half of the operator list: a predicate that does not parse
        // is refused before it runs, which would make an operator unusable
        // rather than unsafe. Checked per engine, because the quoting differs.
        for engine in Engine::ALL {
            for operator in Operator::ALL.into_iter().filter(|o| o.on(engine)) {
                let value = match operator {
                    Operator::Between => "1..9",
                    Operator::InList | Operator::NotInList => "a, b",
                    _ => "ok",
                };
                let filter = predicate(engine, operator, value).expect("applied");
                let sql = explorer::preview_sql(engine, "public", "accounts", &filter, 100, 0);
                assert!(
                    sql::is_generated_select(&sql),
                    "{} was refused: {sql}",
                    operator.slug()
                );
            }
        }
    }

    fn account_key() -> db::ForeignKey {
        db::ForeignKey {
            column: "account_id".to_string(),
            referenced_schema: "public".to_string(),
            referenced_table: "accounts".to_string(),
            referenced_column: "id".to_string(),
        }
    }

    /// What following a key ends up asking for: the bar it writes, composed the
    /// way the tab composes its bars.
    fn followed(engine: Engine, key: &db::ForeignKey, value: Option<&str>) -> Option<String> {
        Some(derived_filter(engine, &[foreign_key_filter(key, value)?]))
    }

    #[test]
    fn a_followed_key_filters_on_the_column_it_references() {
        // Per engine, because the identifier quote differs.
        let key = account_key();
        assert_eq!(
            followed(Engine::Postgres, &key, Some("42")).as_deref(),
            Some(r#""id" = '42'"#)
        );
        assert_eq!(
            followed(Engine::MySql, &key, Some("42")).as_deref(),
            Some("`id` = '42'")
        );
        assert_eq!(
            followed(Engine::Sqlite, &key, Some("42")).as_deref(),
            Some(r#""id" = '42'"#)
        );
    }

    #[test]
    fn a_followed_key_quotes_the_value_it_carries() {
        // A quote in the referenced column name and an apostrophe in the value
        // are the two ways a cell's contents become SQL of its own.
        let key = db::ForeignKey {
            referenced_column: r#"od"d"#.to_string(),
            ..account_key()
        };
        assert_eq!(
            followed(Engine::Postgres, &key, Some("it's")).as_deref(),
            Some(r#""od""d" = 'it''s'"#)
        );
        // And a backslash is the third, on the one engine that reads it as an
        // escape: `Engine::MySql::quote_literal` writes it back as two.
        assert_eq!(
            followed(Engine::MySql, &account_key(), Some(r"a\b")).as_deref(),
            Some(r"`id` = 'a\\b'")
        );
    }

    #[test]
    fn a_null_has_no_key_to_follow() {
        let key = account_key();
        assert_eq!(foreign_key_filter(&key, None), None);
    }

    #[test]
    fn the_filter_a_followed_key_writes_passes_the_generated_select_gate() {
        // §6.2: the filter dbdelve writes for itself goes out through the same
        // check as one the user typed, and this is that check run on it.
        let key = account_key();
        for engine in [Engine::Postgres, Engine::MySql, Engine::Sqlite] {
            let filter = followed(engine, &key, Some("it's 42")).expect("a value");
            let sql = explorer::preview_sql(
                engine,
                &key.referenced_schema,
                &key.referenced_table,
                &filter,
                100,
                0,
            );
            assert!(sql::is_generated_select(&sql), "{sql} was refused");
        }
    }

    #[test]
    fn a_value_that_looks_like_a_statement_is_still_one_literal() {
        // A cell holding `1'; DROP TABLE accounts --` must still produce one
        // readable SELECT, with the payload inert inside a literal.
        let key = account_key();
        let filter =
            followed(Engine::Postgres, &key, Some("1'; DROP TABLE accounts --")).expect("a value");
        assert_eq!(filter, r#""id" = '1''; DROP TABLE accounts --'"#);

        let sql = explorer::preview_sql(Engine::Postgres, "public", "accounts", &filter, 100, 0);
        assert!(sql::is_generated_select(&sql), "{sql} was refused");
        // The statement runs on past the payload: neither the `;` ended it nor
        // the `--` commented out its tail, because both sit inside the literal.
        assert!(sql.ends_with(" LIMIT 100"), "{sql} was cut short");
    }

    /// The bars as the UI holds them, straight through to the `WHERE`.
    fn filter_of(engine: Engine, bars: &[FilterBar]) -> String {
        derived_filter(engine, bars)
    }

    /// An equality bar joined to the one above it.
    fn joined(column: &str, value: &str, conjunction: Conjunction) -> FilterBar {
        FilterBar {
            conjunction,
            ..bar(Some(column), Operator::Equals, value)
        }
    }

    #[test]
    fn no_bars_is_no_filter_at_all() {
        // Not `WHERE` with nothing after it: an empty filter is what makes
        // `preview_sql` emit the statement it always has.
        assert_eq!(filter_of(Engine::Postgres, &[]), "");
    }

    #[test]
    fn a_bar_narrows_nothing_until_it_is_filled_in() {
        // Added and not yet used, either half at a time. A half-filled bar that
        // reached the statement would re-query on every keystroke of a column
        // name nobody has picked.
        assert_eq!(
            filter_of(Engine::Postgres, &[bar(None, Operator::Equals, "")]),
            ""
        );
        assert_eq!(
            filter_of(Engine::Postgres, &[bar(None, Operator::Equals, "ok")]),
            ""
        );
        assert_eq!(
            filter_of(
                Engine::Postgres,
                &[bar(Some("state"), Operator::Equals, "")]
            ),
            ""
        );
        // Not even with an operator that needs no value: there is still no
        // column for it to be about.
        assert_eq!(
            filter_of(Engine::Postgres, &[bar(None, Operator::IsNull, "")]),
            ""
        );
    }

    #[test]
    fn the_bars_are_conjoined_in_the_order_they_are_stacked() {
        assert_eq!(
            filter_of(
                Engine::Postgres,
                &[
                    joined("state", "ok", Conjunction::And),
                    joined("tier", "2", Conjunction::And)
                ]
            ),
            r#"("state" = 'ok') AND ("tier" = '2')"#
        );
        // An unfinished bar between two finished ones drops out rather than
        // breaking the conjunction.
        assert_eq!(
            filter_of(
                Engine::Postgres,
                &[
                    joined("state", "ok", Conjunction::And),
                    bar(None, Operator::Equals, ""),
                    joined("tier", "2", Conjunction::And)
                ]
            ),
            r#"("state" = 'ok') AND ("tier" = '2')"#
        );
        // A lone bar is unwrapped, which is what keeps a tab filtered before
        // joiners existed on the grid key it already had.
        assert_eq!(
            filter_of(Engine::Postgres, &[joined("state", "ok", Conjunction::And)]),
            r#""state" = 'ok'"#
        );
    }

    #[test]
    fn a_mixed_stack_folds_left_to_right_rather_than_by_sql_precedence() {
        // `a OR b AND c` is `a OR (b AND c)` to every engine, and the stack
        // reads top to bottom -- so each fold is parenthesised and the result
        // is `(a OR b) AND c`. The one semantic choice in the stack.
        assert_eq!(
            filter_of(
                Engine::Postgres,
                &[
                    joined("a", "1", Conjunction::And),
                    joined("b", "2", Conjunction::Or),
                    joined("c", "3", Conjunction::And),
                ]
            ),
            r#"(("a" = '1') OR ("b" = '2')) AND ("c" = '3')"#
        );
        // The first bar's own joiner is never read: it has nothing above it.
        assert_eq!(
            filter_of(
                Engine::Postgres,
                &[
                    joined("a", "1", Conjunction::Or),
                    joined("b", "2", Conjunction::Or),
                ]
            ),
            r#"("a" = '1') OR ("b" = '2')"#
        );
    }

    #[test]
    fn a_mixed_stack_is_still_a_statement_the_gate_admits() {
        let filter = filter_of(
            Engine::Postgres,
            &[
                joined("a", "1", Conjunction::And),
                FilterBar {
                    raw: true,
                    conjunction: Conjunction::Or,
                    value: "id > 5".to_string(),
                    ..FilterBar::default()
                },
                joined("c", "3", Conjunction::And),
            ],
        );
        assert_eq!(filter, r#"(("a" = '1') OR (id > 5)) AND ("c" = '3')"#);
        let sql = explorer::preview_sql(Engine::Postgres, "public", "accounts", &filter, 100, 0);
        assert!(sql::is_generated_select(&sql), "{sql} was refused");
    }

    #[test]
    fn a_raw_bar_that_breaks_the_statement_is_refused_rather_than_run() {
        // The gate is the whole safety story for a raw bar (spec §2.3): the
        // tab keeps its rows and says so instead of sending this.
        let filter = filter_of(
            Engine::Postgres,
            &[FilterBar {
                raw: true,
                value: "1 = 1; DROP TABLE accounts".to_string(),
                ..FilterBar::default()
            }],
        );
        let sql = explorer::preview_sql(Engine::Postgres, "public", "accounts", &filter, 100, 0);
        assert!(!sql::is_generated_select(&sql), "{sql} was admitted");
    }

    #[test]
    fn every_bar_is_quoted_the_way_its_engine_reads_it() {
        // The same hazard `filter_predicate` carries, now that a stack of them
        // is what a preview runs: a double-quoted name is a string literal on
        // MySQL, so this fails silently rather than loudly when it is wrong.
        let bars = [
            joined("state", "it's", Conjunction::And),
            joined(r#"od"d"#, r"a\b", Conjunction::And),
        ];
        assert_eq!(
            filter_of(Engine::Postgres, &bars),
            r#"("state" = 'it''s') AND ("od""d" = 'a\b')"#
        );
        // The backslash doubles on the one engine that reads it as an escape,
        // and the double quote is an ordinary character inside backticks.
        assert_eq!(
            filter_of(Engine::MySql, &bars),
            r#"(`state` = 'it''s') AND (`od"d` = 'a\\b')"#
        );
        assert_eq!(
            filter_of(Engine::Sqlite, &bars),
            r#"("state" = 'it''s') AND ("od""d" = 'a\b')"#
        );
    }

    #[test]
    fn the_bars_a_stored_tab_comes_back_with_are_the_bars_it_had() {
        let stored = store::StoredObject {
            schema: "public".into(),
            name: "accounts".into(),
            routine: false,
            kind: RelationKind::default(),
            filter: String::new(),
            filters: Vec::new(),
            active: true,
            bars: vec![
                store::StoredFilter {
                    column: "state".into(),
                    value: "ok".into(),
                    operator: "contains".into(),
                    conjunction: "OR".into(),
                    raw: false,
                },
                store::StoredFilter {
                    value: "id > 5".into(),
                    raw: true,
                    ..store::StoredFilter::default()
                },
            ],
        };
        assert_eq!(
            stored_bars(&stored),
            vec![
                FilterBar {
                    column: Some("state".into()),
                    operator: Operator::Contains,
                    conjunction: Conjunction::Or,
                    raw: false,
                    value: "ok".into(),
                },
                FilterBar {
                    column: None,
                    operator: Operator::Equals,
                    conjunction: Conjunction::And,
                    raw: true,
                    value: "id > 5".into(),
                },
            ]
        );
    }

    #[test]
    fn a_tab_restored_onto_another_engine_drops_what_that_engine_cannot_run() {
        // A profile that changed engine, or a file carried between machines:
        // the expression is re-derived from the bars rather than trusted as
        // written, so a regex does not reach a SQLite that has none.
        let stored = store::StoredObject {
            schema: "public".into(),
            name: "accounts".into(),
            routine: false,
            kind: RelationKind::default(),
            filter: r#"("state" ~ '^a') AND ("tier" = '2')"#.into(),
            filters: Vec::new(),
            active: true,
            bars: vec![
                store::StoredFilter {
                    column: "state".into(),
                    value: "^a".into(),
                    operator: "regex".into(),
                    conjunction: "AND".into(),
                    raw: false,
                },
                store::StoredFilter {
                    column: "tier".into(),
                    value: "2".into(),
                    operator: "equals".into(),
                    conjunction: "AND".into(),
                    raw: false,
                },
            ],
        };
        // Unchanged where the engine can still express it, so the tab keeps the
        // grid snapshot its key was written under.
        assert_eq!(
            restored_filter(Engine::Postgres, &stored).0,
            r#"("state" ~ '^a') AND ("tier" = '2')"#
        );
        assert_eq!(
            restored_filter(Engine::Sqlite, &stored).0,
            r#""tier" = '2'"#
        );
    }

    #[test]
    fn a_bar_written_before_operators_comes_back_as_the_equality_it_was() {
        // The one field an older profile has, read once on the way in. An
        // operator or joiner this build cannot read falls the same way.
        let stored = store::StoredObject {
            schema: "public".into(),
            name: "accounts".into(),
            routine: false,
            kind: RelationKind::default(),
            filter: r#""state" = 'ok'"#.into(),
            filters: vec![("state".into(), "ok".into())],
            active: true,
            bars: Vec::new(),
        };
        assert_eq!(
            stored_bars(&stored),
            vec![bar(Some("state"), Operator::Equals, "ok")]
        );
        // And its expression is the one it was written with, because there are
        // no bars to derive a replacement from.
        assert_eq!(
            restored_filter(Engine::Postgres, &stored).0,
            r#""state" = 'ok'"#
        );
        assert_eq!(Operator::from_slug("no-such-operator"), Operator::Equals);
        assert_eq!(Conjunction::from_str("XOR"), Conjunction::And);
        // And the slug survives the round trip for every operator there is.
        for operator in Operator::ALL {
            assert_eq!(Operator::from_slug(operator.slug()), operator);
        }
    }

    #[test]
    fn a_new_filter_puts_the_preview_back_on_its_first_page() {
        // Page three of a different question is not a page anyone asked for.
        let mut filter = String::new();
        let mut offset = 2_000;

        assert!(changed_filter(
            &mut filter,
            &mut offset,
            r#""state" = 'ok'"#
        ));
        assert_eq!(filter, r#""state" = 'ok'"#);
        assert_eq!(offset, 0);

        // The same filter again is not a change, so nothing re-runs and the
        // page the user is on survives.
        offset = 2_000;
        assert!(!changed_filter(
            &mut filter,
            &mut offset,
            r#"  "state" = 'ok'  "#
        ));
        assert_eq!(offset, 2_000);

        // Clearing is a change like any other, and lands on the first page too.
        assert!(changed_filter(&mut filter, &mut offset, ""));
        assert_eq!(filter, "");
        assert_eq!(offset, 0);
        // And clearing what is already clear is not a change.
        assert!(!changed_filter(&mut filter, &mut offset, "   "));
    }

    #[test]
    fn clicking_a_column_appends_then_turns_around_then_drops_out() {
        let mut keys = Vec::new();

        cycle(&mut keys, r#""a""#);
        assert_eq!(keys, vec![SortKey::new(r#""a""#, true)]);
        // A second column joins the first rather than replacing it: that is
        // what makes a compound sort reachable by clicking.
        cycle(&mut keys, r#""b""#);
        assert_eq!(
            keys,
            vec![SortKey::new(r#""a""#, true), SortKey::new(r#""b""#, true)]
        );
        cycle(&mut keys, r#""a""#);
        assert_eq!(keys[0], SortKey::new(r#""a""#, false));
        cycle(&mut keys, r#""a""#);
        assert_eq!(keys, vec![SortKey::new(r#""b""#, true)]);
    }

    #[test]
    fn a_column_is_named_in_the_order_by_unless_a_name_cannot_identify_it() {
        let unique = columns(&["id", "name"]);
        assert_eq!(
            sort_expression(Engine::Postgres, &unique, 1),
            Some(r#""name""#.into())
        );

        // `SELECT *` across a join returns the same name twice, and ordering by
        // it would be ambiguous -- so the position, which never is.
        let duplicated = columns(&["id", "id"]);
        assert_eq!(
            sort_expression(Engine::Postgres, &duplicated, 1),
            Some("2".into())
        );
        assert_eq!(sort_expression(Engine::Postgres, &unique, 7), None);

        // MySQL reads a double-quoted name as a *string literal*, so ordering
        // by one is ordering by a constant: every row compares equal, the
        // server raises nothing, and the grid comes back in the same order it
        // went out. This is the assertion that catches that.
        assert_eq!(
            sort_expression(Engine::MySql, &unique, 1),
            Some("`name`".into())
        );
        assert_eq!(
            sort_expression(Engine::Sqlite, &unique, 1),
            Some(r#""name""#.into())
        );

        // A quote in a column name would otherwise end the identifier early.
        let odd = columns(&["we\"ird"]);
        assert_eq!(
            sort_expression(Engine::Postgres, &odd, 0),
            Some("\"we\"\"ird\"".into())
        );
    }

    #[test]
    fn a_sort_key_finds_its_way_back_to_the_header_it_came_from() {
        // The round trip every engine has to survive: the expression written
        // into the statement is the one read back out to light the header up,
        // and the quoting in between is the engine's own.
        let result = columns(&["id", "name"]);
        for engine in Engine::ALL {
            let expression = sort_expression(engine, &result, 1).expect("a unique name");
            assert_eq!(
                sort_columns(engine, &[SortKey::new(expression, false)], &result),
                vec![(1, false)],
                "{engine:?}"
            );
        }
    }

    #[test]
    fn the_headers_read_the_sort_back_off_the_statement() {
        let result = columns(&["id", "name"]);
        let keys = vec![
            SortKey::new(r#""name""#, false),
            SortKey::new("1", true),
            // Neither of these has a header to light up.
            SortKey::new("lower(name)", true),
            SortKey::new("9", true),
        ];

        assert_eq!(
            sort_columns(Engine::Postgres, &keys, &result),
            vec![(1, false), (0, true)]
        );
    }

    #[test]
    fn a_preview_asks_for_the_rows_its_tab_was_set_to() {
        assert_eq!(
            relation_sql(Engine::Postgres, "public", "accounts", "", &[], 100, 0),
            r#"SELECT * FROM "public"."accounts" LIMIT 100"#
        );
        // A raised limit still keeps the sort ahead of it, or the rows would be
        // ordered after being cut.
        assert_eq!(
            relation_sql(
                Engine::Postgres,
                "public",
                "accounts",
                "",
                &[SortKey::new(r#""id""#, true)],
                100_000,
                0
            ),
            r#"SELECT * FROM "public"."accounts" ORDER BY "id" ASC LIMIT 100000"#
        );
    }

    #[test]
    fn a_paged_preview_stays_sortable() {
        // The load-bearing pair: the sort must land ahead of a `LIMIT` that
        // carries an `OFFSET`, and the statement must still parse afterwards,
        // or the headers would go dark on every page but the first
        // (`execute_and_then` reads the sort back off the statement it ran).
        let paged = relation_sql(
            Engine::Postgres,
            "public",
            "accounts",
            "",
            &[SortKey::new(r#""id""#, true)],
            100,
            200,
        );

        assert_eq!(
            paged,
            r#"SELECT * FROM "public"."accounts" ORDER BY "id" ASC LIMIT 100 OFFSET 200"#
        );
        assert_eq!(
            sql::order_by(&paged),
            Some(vec![SortKey::new(r#""id""#, true)])
        );
    }

    #[test]
    fn a_filtered_preview_still_sorts_and_still_pages() {
        // All four controls at once, in the only order that parses: the filter
        // ahead of the sort, the sort ahead of the limit, the offset inside it.
        let filtered = relation_sql(
            Engine::Postgres,
            "public",
            "accounts",
            r#""state" = 'ok'"#,
            &[SortKey::new(r#""id""#, true)],
            100,
            200,
        );

        assert_eq!(
            filtered,
            r#"SELECT * FROM "public"."accounts" WHERE "state" = 'ok' ORDER BY "id" ASC LIMIT 100 OFFSET 200"#
        );
        // And the sort is still readable back off the statement, which is what
        // lights the headers up on a page that is not the first.
        assert_eq!(
            sql::order_by(&filtered),
            Some(vec![SortKey::new(r#""id""#, true)])
        );
    }

    #[test]
    fn a_relations_statement_carries_its_sort_before_the_limit() {
        let sorted = relation_sql(
            Engine::Postgres,
            "public",
            "accounts",
            "",
            &[SortKey::new(r#""id""#, false)],
            PREVIEW_ROW_LIMIT,
            0,
        );

        assert_eq!(
            sorted,
            r#"SELECT * FROM "public"."accounts" ORDER BY "id" DESC LIMIT 1000"#
        );
        // And the sort dbdelve wrote is the sort its headers show.
        assert_eq!(
            sort_columns(
                Engine::Postgres,
                &sql::order_by(&sorted).unwrap(),
                &columns(&["id", "email"])
            ),
            vec![(0, false)]
        );
    }

    #[test]
    fn the_gate_accepts_the_statement_a_filtered_preview_writes() {
        // Keeps the generator and the gate from drifting apart, per engine:
        // the identifier quote differs, and a MySQL preview the gate cannot
        // read would refuse every filtered browse on MySQL.
        for engine in [Engine::Postgres, Engine::MySql, Engine::Sqlite] {
            let predicate = format!(
                "{} = {}",
                engine.quote_identifier("state"),
                engine.quote_literal("ok")
            );
            let statement = relation_sql(
                engine,
                "public",
                "accounts",
                &predicate,
                &[SortKey::new(engine.quote_identifier("id"), true)],
                100,
                200,
            );
            assert!(
                sql::is_generated_select(&statement),
                "{statement} was refused"
            );
        }
    }
}

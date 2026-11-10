use super::{Builtin, agg, func, val, win};

#[rustfmt::skip]
pub(crate) const BUILTINS: &[Builtin] = &[
    func("CURRENT_WAREHOUSE", "", "VARCHAR", "Name of the warehouse the session is using."),
    func("DATEADD", "date_or_time_part, value, date_or_time_expr", "type of date_or_time_expr", "date_or_time_expr with value units of date_or_time_part added."),
    func("IFF", "condition, expr1, expr2", "any", "expr1 if condition is true, otherwise expr2."),
    func("PARSE_JSON", "expr", "VARIANT", "expr parsed as JSON."),
    func("TO_DATE", "expr [, format]", "DATE", "expr converted to a date."),
    agg("ARRAY_AGG", "[DISTINCT] expr", "ARRAY", "The values collected into an array."),
    agg("COUNT", "* | [DISTINCT] expr [, ...]", "NUMBER", "Number of rows, or of non-NULL values."),
    agg("LISTAGG", "[DISTINCT] expr [, delimiter]", "VARCHAR", "The non-NULL values concatenated, separated by delimiter."),
    win("LAG", "expr [, offset [, default]]", "type of expr", "expr at the row offset rows before the current one."),
    win("ROW_NUMBER", "", "NUMBER", "Number of the current row within its partition, from 1."),
    val("CURRENT_DATE", "DATE", "The current date."),
    val("CURRENT_TIMESTAMP", "TIMESTAMP_LTZ", "The current date and time."),
    val("FALSE", "BOOLEAN", "Boolean false."),
    val("NULL", "any", "The missing value."),
    val("TRUE", "BOOLEAN", "Boolean true."),
];

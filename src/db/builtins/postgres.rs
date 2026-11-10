use super::{Builtin, agg, func, val, win};

#[rustfmt::skip]
pub(crate) const BUILTINS: &[Builtin] = &[
    func("coalesce", "value [, ...]", "any", "The first argument that is not NULL."),
    func("date_trunc", "field text, source timestamp [, time_zone text]", "timestamp", "Truncates source to the precision field names."),
    func("length", "s text", "integer", "Number of characters in s."),
    func("lower", "s text", "text", "s in lower case."),
    func("now", "", "timestamp with time zone", "Start time of the current transaction."),
    func("to_char", "value, format text", "text", "value formatted as text by a format pattern."),
    agg("array_agg", "value", "anyarray", "The input values collected into an array."),
    agg("count", "* | value", "bigint", "Number of rows, or of non-NULL values."),
    agg("string_agg", "value text, delimiter text", "text", "The non-NULL values concatenated, separated by delimiter."),
    win("lag", "value [, offset integer [, default]]", "any", "value at the row offset rows before the current one."),
    win("row_number", "", "bigint", "Number of the current row within its partition, from 1."),
    val("CURRENT_DATE", "date", "The current date."),
    val("CURRENT_TIMESTAMP", "timestamp with time zone", "Start time of the current transaction."),
    val("CURRENT_USER", "name", "Name of the role the session is acting as."),
    val("FALSE", "boolean", "Boolean false."),
    val("NULL", "any", "The missing value."),
    val("TRUE", "boolean", "Boolean true."),
];

use super::{Builtin, agg, func, val, win};

#[rustfmt::skip]
pub(crate) const BUILTINS: &[Builtin] = &[
    func("coalesce", "expr [, ...]", "any", "The first argument that is not NULL."),
    func("date_trunc", "part, timestamp", "timestamp", "Truncates timestamp to the precision part names."),
    func("length", "string", "bigint", "Number of characters in string."),
    func("now", "", "timestamp with time zone", "Start time of the current transaction."),
    func("regexp_matches", "string, regex [, options]", "boolean", "Whether string contains a match for regex."),
    func("strftime", "timestamp, format", "varchar", "timestamp formatted by the % specifiers in format."),
    agg("count", "* | arg", "bigint", "Number of rows, or of non-NULL values of arg."),
    agg("list", "arg", "list", "The values collected into a list."),
    agg("string_agg", "arg, sep", "varchar", "The non-NULL values concatenated, separated by sep."),
    win("lag", "expr [, offset [, default]]", "any", "expr at the row offset rows before the current one."),
    win("row_number", "", "bigint", "Number of the current row within its partition, from 1."),
    val("CURRENT_DATE", "date", "The current date, as of the start of the transaction."),
    val("CURRENT_TIMESTAMP", "timestamp with time zone", "Start time of the current transaction."),
    val("FALSE", "boolean", "Boolean false."),
    val("NULL", "any", "The missing value."),
    val("TRUE", "boolean", "Boolean true."),
];

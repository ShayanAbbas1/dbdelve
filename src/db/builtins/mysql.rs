use super::{Builtin, agg, func, val, win};

#[rustfmt::skip]
pub(crate) const BUILTINS: &[Builtin] = &[
    func("CONCAT", "str [, ...]", "string", "Its arguments joined, or NULL if any is NULL."),
    func("DATE_FORMAT", "date, format", "string", "date formatted by the % specifiers in format."),
    func("IFNULL", "expr1, expr2", "any", "expr1 if it is not NULL, otherwise expr2."),
    func("NOW", "[fsp]", "DATETIME", "Date and time the statement began."),
    func("UUID", "", "string", "A new version 1 UUID."),
    agg("COUNT", "* | [DISTINCT] expr", "BIGINT", "Number of rows, or of non-NULL values of expr."),
    agg("GROUP_CONCAT", "[DISTINCT] expr [ORDER BY ...] [SEPARATOR str]", "string", "The group's non-NULL values concatenated into one string."),
    win("LAG", "expr [, N [, default]]", "any", "expr at the row N rows before the current one."),
    win("ROW_NUMBER", "", "BIGINT", "Number of the current row within its partition, from 1."),
    val("CURRENT_DATE", "DATE", "The current date."),
    val("CURRENT_TIMESTAMP", "DATETIME", "Date and time the statement began."),
    val("CURRENT_USER", "string", "The account the server authenticated the session as."),
    val("FALSE", "INT", "Alias for 0."),
    val("NULL", "any", "The missing value."),
    val("TRUE", "INT", "Alias for 1."),
];

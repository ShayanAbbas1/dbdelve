use super::{Builtin, agg, func, val, win};

#[rustfmt::skip]
pub(crate) const BUILTINS: &[Builtin] = &[
    func("coalesce", "x, y [, ...]", "any", "The first argument that is not NULL."),
    func("date", "time-value [, modifier, ...]", "text", "A date as YYYY-MM-DD."),
    func("ifnull", "x, y", "any", "x if it is not NULL, otherwise y."),
    func("length", "x", "integer", "Characters in a string, or bytes in a blob."),
    func("random", "", "integer", "A pseudo-random 64-bit signed integer."),
    func("substr", "x, y [, z]", "text", "z characters of x, starting at the yth."),
    agg("count", "* | x", "integer", "Number of rows, or of non-NULL values of x."),
    agg("group_concat", "x [, y]", "text", "The non-NULL values concatenated, separated by y or a comma."),
    agg("total", "x", "real", "Sum of the non-NULL values as a float, 0.0 when there are none."),
    win("lag", "expr [, offset [, default]]", "any", "expr at the row offset rows before the current one."),
    win("row_number", "", "integer", "Number of the current row within its partition, from 1."),
    val("CURRENT_DATE", "text", "The current UTC date as YYYY-MM-DD."),
    val("CURRENT_TIME", "text", "The current UTC time as HH:MM:SS."),
    val("CURRENT_TIMESTAMP", "text", "The current UTC date and time as YYYY-MM-DD HH:MM:SS."),
    val("FALSE", "integer", "Alias for 0."),
    val("NULL", "any", "The missing value."),
    val("TRUE", "integer", "Alias for 1."),
];

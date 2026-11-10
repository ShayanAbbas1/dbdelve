use super::{Builtin, agg, func, val, win};

#[rustfmt::skip]
pub(crate) const BUILTINS: &[Builtin] = &[
    func("ABS", "x numeric", "numeric", "Absolute value of x."),
    func("COALESCE", "value [, ...]", "any", "The first argument that is not NULL."),
    func("LOWER", "s text", "text", "s in lower case."),
    func("NULLIF", "a, b", "any", "NULL if a equals b, otherwise a."),
    func("UPPER", "s text", "text", "s in upper case."),
    agg("AVG", "x numeric", "numeric", "Average of the non-NULL values."),
    agg("COUNT", "* | x", "bigint", "Number of rows, or of non-NULL values of x."),
    agg("MAX", "x", "any", "Largest non-NULL value."),
    agg("MIN", "x", "any", "Smallest non-NULL value."),
    agg("SUM", "x numeric", "numeric", "Sum of the non-NULL values."),
    win("RANK", "", "bigint", "Rank of the current row within its partition, with gaps."),
    win("ROW_NUMBER", "", "bigint", "Number of the current row within its partition, from 1."),
    val("CURRENT_DATE", "date", "The current date."),
    val("CURRENT_TIMESTAMP", "timestamp with time zone", "The current date and time."),
    val("CURRENT_USER", "text", "Name of the user the session runs as."),
    val("FALSE", "boolean", "Boolean false."),
    val("NULL", "any", "The missing value."),
    val("TRUE", "boolean", "Boolean true."),
];

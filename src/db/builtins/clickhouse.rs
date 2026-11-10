use super::{Builtin, agg, func, val, win};

#[rustfmt::skip]
pub(crate) const BUILTINS: &[Builtin] = &[
    func("formatDateTime", "time, format [, timezone]", "String", "time formatted by the % specifiers in format."),
    func("length", "x", "UInt64", "Length of a string in bytes, or of an array."),
    func("now", "[timezone]", "DateTime", "Current date and time, fixed for the query."),
    func("toDate", "expr", "Date", "expr converted to a Date."),
    func("toString", "value [, timezone]", "String", "value converted to text."),
    func("today", "", "Date", "The current date."),
    agg("count", "[expr]", "UInt64", "Number of rows, or of non-NULL values of expr."),
    agg("groupArray", "x", "Array", "The values collected into an array."),
    agg("uniq", "x [, ...]", "UInt64", "Approximate number of distinct values."),
    win("lagInFrame", "x [, offset [, default]]", "type of x", "x at the row offset rows before the current one, within the frame."),
    win("row_number", "", "UInt64", "Number of the current row within its partition, from 1."),
    val("FALSE", "Bool", "Boolean false."),
    val("NULL", "Nullable(Nothing)", "The missing value."),
    val("TRUE", "Bool", "Boolean true."),
];

use super::{Builtin, agg, func, val, win};

#[rustfmt::skip]
pub(crate) const BUILTINS: &[Builtin] = &[
    func("DATEADD", "datepart, number, date", "type of date", "date with number units of datepart added."),
    func("DATEDIFF", "datepart, startdate, enddate", "int", "Count of datepart boundaries crossed between startdate and enddate."),
    func("GETDATE", "", "datetime", "The server's current date and time."),
    func("ISNULL", "check_expression, replacement_value", "type of check_expression", "check_expression if it is not NULL, otherwise replacement_value."),
    func("LEN", "string_expression", "int", "Characters in a string, not counting trailing spaces."),
    func("NEWID", "", "uniqueidentifier", "A new random GUID."),
    agg("COUNT", "* | [ALL | DISTINCT] expression", "int", "Number of rows, or of non-NULL values of expression."),
    agg("STRING_AGG", "expression, separator", "nvarchar | varchar", "The non-NULL values concatenated, separated by separator."),
    win("LAG", "scalar_expression [, offset [, default]]", "type of scalar_expression", "scalar_expression at the row offset rows before the current one."),
    win("ROW_NUMBER", "", "bigint", "Number of the current row within its partition, from 1."),
    val("CURRENT_TIMESTAMP", "datetime", "The server's current date and time."),
    val("CURRENT_USER", "sysname", "Name of the current database user."),
    val("NULL", "any", "The missing value."),
    val("SYSTEM_USER", "nvarchar(128)", "Login name of the session."),
];

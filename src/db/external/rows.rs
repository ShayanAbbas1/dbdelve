//! A plugin's Arrow batches as DBDelve's rows, on the caller's thread: every
//! value becomes the text the grid shows, so no Arrow type leaves `src/db/`.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::{
    Array, BinaryArray, FixedSizeBinaryArray, LargeBinaryArray, LargeStringArray, RecordBatch,
    StringArray, StringViewArray,
};
use arrow_buffer::NullBuffer;
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::{DataType, SchemaRef};
use dbdelve_plugin_protocol::EXACT;

use super::host::failure;
use crate::db::{Column, DbError, Row, RowBuilder};

/// The field metadata a plugin names the driver's own type in.
const NATIVE_TYPE: &str = "dbdelve.type";

/// The spellings the built-in engines show: no `T` between a date and its
/// time, and a fraction only when there is one.
const SPELLING: FormatOptions<'static> = FormatOptions::new()
    .with_date_format(Some("%Y-%m-%d"))
    .with_datetime_format(Some("%Y-%m-%d %H:%M:%S%.f"))
    .with_timestamp_format(Some("%Y-%m-%d %H:%M:%S%.f"))
    .with_timestamp_tz_format(Some("%Y-%m-%d %H:%M:%S%.f%:z"))
    .with_time_format(Some("%H:%M:%S%.f"));

/// One result set's columns, and the buffers its rows are built in, reused
/// from row to row.
pub(super) struct Rows {
    columns: Vec<Column>,
    /// Which columns the plugin sends as text the server reads back as the
    /// same value, so a row can be found by them. Kept here rather than on
    /// [`Column`], since nothing outside the plugin host asks.
    exact: Vec<bool>,
    row: RowBuilder,
    cell: String,
}

impl Rows {
    pub(super) fn new(schema: &SchemaRef) -> Self {
        let columns = schema
            .fields()
            .iter()
            .map(|field| Column {
                name: field.name().clone(),
                data_type: Some(
                    field
                        .metadata()
                        .get(NATIVE_TYPE)
                        .cloned()
                        .unwrap_or_else(|| field.data_type().to_string()),
                ),
            })
            .collect();
        let exact = schema
            .fields()
            .iter()
            .map(|field| {
                field
                    .metadata()
                    .get(EXACT)
                    .is_some_and(|value| value == "true")
            })
            .collect();
        Self {
            columns,
            exact,
            row: RowBuilder::default(),
            cell: String::new(),
        }
    }

    pub(super) fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub(super) fn exact(&self) -> &[bool] {
        &self.exact
    }

    /// Every row of `batch`, in order, each handed to `keep`; the rows seen.
    pub(super) fn push_batch(
        &mut self,
        batch: &RecordBatch,
        mut keep: impl FnMut(Row),
    ) -> Result<usize, DbError> {
        let cells = batch
            .columns()
            .iter()
            .map(Cells::new)
            .collect::<Result<Vec<_>, _>>()?;
        for index in 0..batch.num_rows() {
            for column in &cells {
                column.push(index, &mut self.row, &mut self.cell);
            }
            keep(self.row.finish()?);
        }
        Ok(batch.num_rows())
    }
}

/// One column of one batch, ready to render.
struct Cells<'a> {
    /// Logical, because a `Null` column has no validity buffer of its own and
    /// would otherwise read as every value present.
    nulls: Option<NullBuffer>,
    values: Values<'a>,
}

enum Values<'a> {
    Utf8(&'a StringArray),
    LargeUtf8(&'a LargeStringArray),
    Utf8View(&'a StringViewArray),
    Binary(&'a BinaryArray),
    LargeBinary(&'a LargeBinaryArray),
    FixedSizeBinary(&'a FixedSizeBinaryArray),
    Display(ArrayFormatter<'a>),
}

impl<'a> Cells<'a> {
    fn new(array: &'a Arc<dyn Array>) -> Result<Self, DbError> {
        let values = match array.data_type() {
            DataType::Utf8 => Values::Utf8(array.as_string()),
            DataType::LargeUtf8 => Values::LargeUtf8(array.as_string()),
            DataType::Utf8View => Values::Utf8View(array.as_string_view()),
            DataType::Binary => Values::Binary(array.as_binary()),
            DataType::LargeBinary => Values::LargeBinary(array.as_binary()),
            DataType::FixedSizeBinary(_) => Values::FixedSizeBinary(array.as_fixed_size_binary()),
            _ => Values::Display(
                ArrayFormatter::try_new(array.as_ref(), &SPELLING)
                    .map_err(|error| failure(error.to_string()))?,
            ),
        };
        Ok(Self {
            nulls: array.logical_nulls(),
            values,
        })
    }

    fn push(&self, index: usize, row: &mut RowBuilder, cell: &mut String) {
        if self
            .nulls
            .as_ref()
            .is_some_and(|nulls| nulls.is_null(index))
        {
            row.push(None);
            return;
        }
        let text = match &self.values {
            Values::Utf8(array) => array.value(index),
            Values::LargeUtf8(array) => array.value(index),
            Values::Utf8View(array) => array.value(index),
            Values::Binary(array) => hex(array.value(index), cell),
            Values::LargeBinary(array) => hex(array.value(index), cell),
            Values::FixedSizeBinary(array) => hex(array.value(index), cell),
            Values::Display(formatter) => {
                cell.clear();
                // Safe mode writes a value it cannot format as its error's
                // text, so this cannot fail.
                let _ = formatter.value(index).write(cell);
                cell
            }
        };
        row.push(Some(text));
    }
}

/// `0x` and upper-case hex, as the built-in engines show bytes.
fn hex<'a>(bytes: &[u8], cell: &'a mut String) -> &'a str {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    cell.clear();
    cell.reserve(2 + 2 * bytes.len());
    cell.push_str("0x");
    for byte in bytes {
        cell.push(DIGITS[usize::from(byte >> 4)].into());
        cell.push(DIGITS[usize::from(byte & 0xf)].into());
    }
    cell
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use arrow_array::*;
    use arrow_buffer::{IntervalDayTime, IntervalMonthDayNano, i256};
    use arrow_schema::{Field, Schema};

    use super::*;

    fn rendered(columns: Vec<(&str, ArrayRef)>) -> (Vec<Column>, Vec<Row>) {
        let batch = RecordBatch::try_from_iter(columns).unwrap();
        let mut rows = Rows::new(&batch.schema());
        let mut kept = Vec::new();
        let seen = rows.push_batch(&batch, |row| kept.push(row)).unwrap();
        assert_eq!(seen, batch.num_rows());
        (rows.columns().to_vec(), kept)
    }

    #[test]
    fn every_carried_type_renders_in_dbdelves_spelling_and_null_as_none() {
        let columns: Vec<(&str, ArrayRef, &str)> = vec![
            ("null", Arc::new(NullArray::new(2)), ""),
            (
                "boolean",
                Arc::new(BooleanArray::from(vec![Some(true), None])),
                "true",
            ),
            (
                "int8",
                Arc::new(Int8Array::from(vec![Some(-8), None])),
                "-8",
            ),
            (
                "int64",
                Arc::new(Int64Array::from(vec![Some(-64), None])),
                "-64",
            ),
            (
                "uint64",
                Arc::new(UInt64Array::from(vec![Some(u64::MAX), None])),
                "18446744073709551615",
            ),
            (
                "float64",
                Arc::new(Float64Array::from(vec![Some(2.5), None])),
                "2.5",
            ),
            (
                "decimal128",
                Arc::new(
                    Decimal128Array::from(vec![Some(1250), None])
                        .with_precision_and_scale(10, 2)
                        .unwrap(),
                ),
                "12.50",
            ),
            (
                "decimal256",
                Arc::new(
                    Decimal256Array::from(vec![Some(i256::from_i128(-1250)), None])
                        .with_precision_and_scale(40, 2)
                        .unwrap(),
                ),
                "-12.50",
            ),
            (
                "utf8",
                Arc::new(StringArray::from(vec![Some("text"), None])),
                "text",
            ),
            (
                "large_utf8",
                Arc::new(LargeStringArray::from(vec![Some("large"), None])),
                "large",
            ),
            (
                "utf8_view",
                Arc::new(StringViewArray::from(vec![
                    Some("longer than twelve bytes"),
                    None,
                ])),
                "longer than twelve bytes",
            ),
            (
                "binary",
                Arc::new(BinaryArray::from(vec![Some(&b"\x00\xff"[..]), None])),
                "0x00FF",
            ),
            (
                "large_binary",
                Arc::new(LargeBinaryArray::from(vec![Some(&b""[..]), None])),
                "0x",
            ),
            (
                "fixed_size_binary",
                Arc::new(
                    FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                        [Some([0xab, 0xcd]), None].into_iter(),
                        2,
                    )
                    .unwrap(),
                ),
                "0xABCD",
            ),
            (
                "date32",
                Arc::new(Date32Array::from(vec![Some(19782), None])),
                "2024-02-29",
            ),
            (
                "date64",
                Arc::new(Date64Array::from(vec![Some(1_709_210_096_000), None])),
                "2024-02-29 12:34:56",
            ),
            (
                "time32",
                Arc::new(Time32SecondArray::from(vec![Some(45_296), None])),
                "12:34:56",
            ),
            (
                "time64",
                Arc::new(Time64MicrosecondArray::from(vec![
                    Some(45_296_500_000),
                    None,
                ])),
                "12:34:56.500",
            ),
            (
                "timestamp",
                Arc::new(TimestampMicrosecondArray::from(vec![
                    Some(1_709_210_096_000_000),
                    None,
                ])),
                "2024-02-29 12:34:56",
            ),
            (
                "timestamp_tz",
                Arc::new(
                    TimestampSecondArray::from(vec![Some(1_709_210_096), None])
                        .with_timezone("+05:00"),
                ),
                "2024-02-29 17:34:56+05:00",
            ),
            (
                "timestamp_utc",
                Arc::new(
                    TimestampSecondArray::from(vec![Some(1_709_210_096), None])
                        .with_timezone("+00:00"),
                ),
                "2024-02-29 12:34:56+00:00",
            ),
            (
                "duration",
                Arc::new(DurationSecondArray::from(vec![Some(90), None])),
                "PT90S",
            ),
            (
                "interval_ym",
                Arc::new(IntervalYearMonthArray::from(vec![Some(14), None])),
                "1 years 2 mons",
            ),
            (
                "interval_dt",
                Arc::new(IntervalDayTimeArray::from(vec![
                    Some(IntervalDayTime::new(1, 2)),
                    None,
                ])),
                "1 days 0.002 secs",
            ),
            (
                "interval_mdn",
                Arc::new(IntervalMonthDayNanoArray::from(vec![
                    Some(IntervalMonthDayNano::new(1, 2, 3)),
                    None,
                ])),
                "1 mons 2 days 0.000000003 secs",
            ),
        ];
        let expected: Vec<Option<&str>> = columns
            .iter()
            .map(|(name, _, text)| (*name != "null").then_some(*text))
            .collect();
        let (_, rows) = rendered(
            columns
                .into_iter()
                .map(|(name, column, _)| (name, column))
                .collect(),
        );
        assert_eq!(rows[0].iter().collect::<Vec<_>>(), expected);
        assert!(rows[1].iter().all(|cell| cell.is_none()), "{:?}", rows[1]);
    }

    #[test]
    fn the_drivers_type_name_wins_over_arrows() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("native", DataType::Int32, true).with_metadata(HashMap::from([(
                NATIVE_TYPE.to_owned(),
                "INTEGER".to_owned(),
            )])),
            Field::new("plain", DataType::Int32, true),
        ]));
        let columns = Rows::new(&schema).columns().to_vec();
        assert_eq!(
            columns,
            [
                Column {
                    name: "native".into(),
                    data_type: Some("INTEGER".into()),
                },
                Column {
                    name: "plain".into(),
                    data_type: Some("Int32".into()),
                },
            ]
        );
    }

    #[test]
    fn only_a_column_marked_exact_reads_back_exactly() {
        let field = |name: &str, exact: Option<&str>| {
            let metadata = exact.map(|value| (EXACT.to_owned(), value.to_owned()));
            Field::new(name, DataType::Utf8, true)
                .with_metadata(metadata.into_iter().collect::<HashMap<_, _>>())
        };
        let schema = Arc::new(Schema::new(vec![
            field("marked", Some("true")),
            field("other", Some("yes")),
            field("plain", None),
        ]));
        assert_eq!(Rows::new(&schema).exact(), [true, false, false]);
    }

    #[test]
    fn a_sliced_batch_renders_only_its_own_rows() {
        let column: ArrayRef = Arc::new(Int32Array::from(vec![Some(1), None, Some(3), Some(4)]));
        let batch = RecordBatch::try_from_iter([("n", column)]).unwrap();
        let mut rows = Rows::new(&batch.schema());
        let mut kept = Vec::new();
        rows.push_batch(&batch.slice(1, 2), |row| kept.push(row))
            .unwrap();
        assert_eq!(
            kept,
            crate::db::rows(vec![vec![None], vec![Some("3".into())]])
        );
    }
}

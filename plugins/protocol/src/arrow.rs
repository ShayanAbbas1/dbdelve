//! Result data on the wire: a result set's first data frame holds one Arrow IPC
//! encapsulated schema message, and each later one exactly one record batch
//! message. No compression, no dictionaries and no nested types in protocol 1,
//! so a batch decodes to the size its frame declares and nothing is kept
//! between batches.
//!
//! ponytail: nested types are refused; they render through `arrow-cast` once
//! ADBC needs them.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_buffer::Buffer;
use arrow_ipc::convert::try_fb_to_schema;
use arrow_ipc::reader::RecordBatchDecoder;
use arrow_ipc::writer::{
    DictionaryTracker, IpcDataGenerator, IpcWriteContext, IpcWriteOptions, write_message,
};
use arrow_ipc::{Message, MessageHeader, root_as_message};
use arrow_schema::{ArrowError, DataType, Schema, SchemaRef};

use crate::Violation;

/// What opens every encapsulated message since Arrow 0.15; arrow-ipc keeps its
/// own copy private.
const CONTINUATION_MARKER: [u8; 4] = [0xff; 4];

pub fn encode_schema(schema: &Schema) -> Vec<u8> {
    let options = IpcWriteOptions::default();
    let encoded = IpcDataGenerator::default().schema_to_bytes_with_dictionary_tracker(
        schema,
        &mut DictionaryTracker::new(false),
        &options,
    );
    let mut payload = Vec::new();
    write_message(&mut payload, encoded, &options).expect("a schema message carries no body");
    payload
}

/// Refuses a column of a type protocol 1 does not carry, so a plugin learns it
/// here rather than from the host killing it.
pub fn encode_batch(batch: &RecordBatch) -> Result<Vec<u8>, ArrowError> {
    if let Some(field) = refused(batch.schema_ref()) {
        return Err(ArrowError::InvalidArgumentError(format!(
            "column {} is {}, which protocol 1 cannot carry",
            field.name(),
            field.data_type()
        )));
    }
    let options = IpcWriteOptions::default();
    let (_, encoded) = IpcDataGenerator::default().encode(
        batch,
        &mut DictionaryTracker::new(false),
        &options,
        &mut IpcWriteContext::default(),
    )?;
    let mut payload = Vec::new();
    write_message(&mut payload, encoded, &options)?;
    Ok(payload)
}

pub fn decode_schema(payload: &[u8]) -> Result<SchemaRef, Violation> {
    guarded(|| {
        let (message, body) = message(payload)?;
        let schema = match message.header_type() {
            MessageHeader::Schema => message.header_as_schema(),
            other => return Err(arrow(format!("a {other:?} message where a schema belongs"))),
        }
        .ok_or_else(|| arrow("a schema message without its schema"))?;
        if !body.is_empty() {
            return Err(arrow("a schema message with a body"));
        }
        let schema = try_fb_to_schema(schema).map_err(|error| arrow(error.to_string()))?;
        accept(&schema)?;
        Ok(Arc::new(schema))
    })
}

/// Validates every buffer against the schema before anything reads it, and
/// catches a panic in the decoder as a violation: `arrow-ipc` has panicked on
/// malformed input before, and a plugin's bad frame must never take DBDelve
/// down.
pub fn decode_batch(schema: &SchemaRef, payload: &[u8]) -> Result<RecordBatch, Violation> {
    guarded(|| {
        accept(schema)?;
        let (message, body) = message(payload)?;
        let batch = match message.header_type() {
            MessageHeader::RecordBatch => message.header_as_record_batch(),
            MessageHeader::DictionaryBatch => {
                return Err(arrow("a dictionary batch, which protocol 1 does not carry"));
            }
            other => {
                return Err(arrow(format!(
                    "a {other:?} message where a record batch belongs"
                )));
            }
        }
        .ok_or_else(|| arrow("a record batch message without its batch"))?;
        if batch.compression().is_some() {
            return Err(arrow("a compressed batch, which protocol 1 does not carry"));
        }
        // Columns of type Null take no buffer at all, so without this a batch
        // of them could claim more rows than the host would ever finish
        // rendering. Any other column spends at least a bit per row.
        let rows = batch.length();
        if rows < 0 || rows as u64 > 8 * payload.len() as u64 {
            return Err(arrow(format!(
                "a batch claiming {rows} rows in {} bytes",
                payload.len()
            )));
        }
        let body = Buffer::from(body);
        RecordBatchDecoder::try_new(
            &body,
            batch,
            schema.clone(),
            &HashMap::new(),
            &message.version(),
        )
        .and_then(|decoder| decoder.with_require_alignment(false).read_record_batch())
        .map_err(|error| arrow(error.to_string()))
    })
}

/// Splits an encapsulated message into its header and its body, refusing a
/// body length the payload does not hold exactly before anything is decoded.
fn message(payload: &[u8]) -> Result<(Message<'_>, &[u8]), Violation> {
    let rest = payload
        .strip_prefix(&CONTINUATION_MARKER)
        .ok_or_else(|| arrow("a message without the continuation marker"))?;
    let (len, rest) = rest
        .split_first_chunk::<4>()
        .ok_or_else(|| arrow("a message cut short"))?;
    let len = usize::try_from(i32::from_le_bytes(*len))
        .map_err(|_| arrow("a message with a negative header length"))?;
    if len > rest.len() {
        return Err(arrow("a message header longer than its frame"));
    }
    let (header, body) = rest.split_at(len);
    let message = root_as_message(header).map_err(|error| arrow(error.to_string()))?;
    if usize::try_from(message.bodyLength()) != Ok(body.len()) {
        return Err(arrow(format!(
            "a message declaring a {}-byte body in a frame holding {}",
            message.bodyLength(),
            body.len()
        )));
    }
    Ok((message, body))
}

fn accept(schema: &Schema) -> Result<(), Violation> {
    match refused(schema) {
        Some(field) => Err(arrow(format!(
            "column {} is {}, which protocol 1 does not carry",
            field.name(),
            field.data_type()
        ))),
        None => Ok(()),
    }
}

fn refused(schema: &Schema) -> Option<&arrow_schema::Field> {
    schema
        .fields()
        .iter()
        .find(|field| !carried(field.data_type()))
        .map(AsRef::as_ref)
}

fn carried(data_type: &DataType) -> bool {
    use DataType::*;
    matches!(
        data_type,
        Null | Boolean
            | Int8
            | Int16
            | Int32
            | Int64
            | UInt8
            | UInt16
            | UInt32
            | UInt64
            | Float16
            | Float32
            | Float64
            | Decimal128(..)
            | Decimal256(..)
            | Utf8
            | LargeUtf8
            | Utf8View
            | Binary
            | LargeBinary
            | FixedSizeBinary(_)
            | Date32
            | Date64
            | Time32(_)
            | Time64(_)
            | Timestamp(..)
            | Duration(_)
            | Interval(_)
    )
}

fn guarded<T>(decode: impl FnOnce() -> Result<T, Violation>) -> Result<T, Violation> {
    panic::catch_unwind(AssertUnwindSafe(decode))
        .unwrap_or_else(|_| Err(arrow("result data that made the decoder panic")))
}

fn arrow(message: impl Into<String>) -> Violation {
    Violation::Arrow(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    use arrow_array::*;
    use arrow_buffer::i256;
    use arrow_ipc::{
        BodyCompressionBuilder, BodyCompressionMethod, CompressionType, FieldNode, MessageBuilder,
        MetadataVersion, RecordBatchBuilder,
    };
    use arrow_schema::Field;

    fn int64_batch() -> RecordBatch {
        let column: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), None, Some(3)]));
        RecordBatch::try_from_iter([("n", column)]).unwrap()
    }

    fn refused_as_arrow<T: std::fmt::Debug>(result: Result<T, Violation>) {
        assert!(matches!(result, Err(Violation::Arrow(_))), "{result:?}");
    }

    /// A record batch message built by hand, as a plugin that ignored the rules
    /// would: one Int64 column of `rows` rows, its values in `body`.
    fn hand_built(rows: i64, compressed: bool, body: &[u8]) -> Vec<u8> {
        let mut fbb = flatbuffers::FlatBufferBuilder::new();
        let nodes = fbb.create_vector(&[FieldNode::new(rows, 0)]);
        let buffers = fbb.create_vector(&[
            arrow_ipc::Buffer::new(0, 0),
            arrow_ipc::Buffer::new(0, body.len() as i64),
        ]);
        let compression = compressed.then(|| {
            let mut compression = BodyCompressionBuilder::new(&mut fbb);
            compression.add_codec(CompressionType::LZ4_FRAME);
            compression.add_method(BodyCompressionMethod::BUFFER);
            compression.finish()
        });
        let mut batch = RecordBatchBuilder::new(&mut fbb);
        batch.add_length(rows);
        batch.add_nodes(nodes);
        batch.add_buffers(buffers);
        if let Some(compression) = compression {
            batch.add_compression(compression);
        }
        let batch = batch.finish();
        let mut message = MessageBuilder::new(&mut fbb);
        message.add_version(MetadataVersion::V5);
        message.add_header_type(MessageHeader::RecordBatch);
        message.add_header(batch.as_union_value());
        message.add_bodyLength(body.len() as i64);
        let message = message.finish();
        fbb.finish(message, None);

        let mut header = fbb.finished_data().to_vec();
        header.resize(header.len().next_multiple_of(8), 0);
        let mut payload = CONTINUATION_MARKER.to_vec();
        payload.extend((header.len() as i32).to_le_bytes());
        payload.extend(header);
        payload.extend(body);
        payload
    }

    fn int64_schema() -> SchemaRef {
        Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, true)]))
    }

    #[test]
    fn a_hand_built_batch_decodes_when_it_keeps_the_rules() {
        let body: Vec<u8> = [7i64, 8].iter().flat_map(|n| n.to_le_bytes()).collect();
        let batch = decode_batch(&int64_schema(), &hand_built(2, false, &body)).unwrap();
        assert_eq!(batch.num_rows(), 2);
    }

    #[test]
    fn a_compressed_batch_is_refused() {
        let body = [0; 16];
        refused_as_arrow(decode_batch(&int64_schema(), &hand_built(2, true, &body)));
    }

    #[test]
    fn a_batch_claiming_more_rows_than_its_bytes_could_hold_is_refused() {
        let schema = Arc::new(Schema::new(vec![Field::new("z", DataType::Null, true)]));
        refused_as_arrow(decode_batch(&schema, &hand_built(1 << 40, false, &[])));
    }

    #[test]
    fn a_body_longer_than_the_payload_is_refused() {
        let payload = encode_batch(&int64_batch()).unwrap();
        refused_as_arrow(decode_batch(&int64_schema(), &payload[..payload.len() - 8]));
    }

    #[test]
    fn a_dictionary_is_refused_as_a_field_and_as_a_message() {
        let dictionary: DictionaryArray<types::Int32Type> =
            vec!["a", "b", "a"].into_iter().collect();
        let batch = RecordBatch::try_from_iter([("d", Arc::new(dictionary) as ArrayRef)]).unwrap();
        assert!(encode_batch(&batch).is_err());

        refused_as_arrow(decode_schema(&encode_schema(&batch.schema())));
        refused_as_arrow(decode_batch(
            &batch.schema(),
            &encode_batch(&int64_batch()).unwrap(),
        ));

        let options = IpcWriteOptions::default();
        let generator = IpcDataGenerator::default();
        let mut tracker = DictionaryTracker::new(false);
        generator.schema_to_bytes_with_dictionary_tracker(&batch.schema(), &mut tracker, &options);
        let (dictionaries, _) = generator
            .encode(
                &batch,
                &mut tracker,
                &options,
                &mut IpcWriteContext::default(),
            )
            .unwrap();
        let mut payload = Vec::new();
        let dictionary = dictionaries.into_iter().next().unwrap();
        write_message(&mut payload, dictionary, &options).unwrap();
        refused_as_arrow(decode_batch(&int64_schema(), &payload));
    }

    #[test]
    fn a_nested_type_is_refused() {
        let list = ListArray::from_iter_primitive::<types::Int32Type, _, _>([Some(vec![Some(1)])]);
        let batch = RecordBatch::try_from_iter([("l", Arc::new(list) as ArrayRef)]).unwrap();
        assert!(encode_batch(&batch).is_err());
        refused_as_arrow(decode_schema(&encode_schema(&batch.schema())));
        refused_as_arrow(decode_batch(
            &batch.schema(),
            &encode_batch(&int64_batch()).unwrap(),
        ));
    }

    #[test]
    fn garbage_is_refused_never_a_panic() {
        let schema = int64_schema();
        let mut marked = CONTINUATION_MARKER.to_vec();
        marked.extend(64i32.to_le_bytes());
        marked.extend((0..64u8).map(|n| n.wrapping_mul(37)));
        for payload in [&b""[..], b"garbage", &[0xff; 64], &marked] {
            refused_as_arrow(decode_batch(&schema, payload));
            refused_as_arrow(decode_schema(payload));
        }
    }

    #[test]
    fn a_schema_where_a_batch_belongs_is_refused_and_the_reverse() {
        let schema = int64_schema();
        refused_as_arrow(decode_batch(&schema, &encode_schema(&schema)));
        refused_as_arrow(decode_schema(&encode_batch(&int64_batch()).unwrap()));
    }

    fn every_carried_type() -> RecordBatch {
        let float16 = Float16Array::from(
            arrow_data::ArrayData::builder(DataType::Float16)
                .len(2)
                .add_buffer(Buffer::from_slice_ref([0x3c00u16, 0]))
                .null_bit_buffer(Some(Buffer::from([0b01u8])))
                .build()
                .unwrap(),
        );
        let columns: Vec<(&str, ArrayRef)> = vec![
            ("null", Arc::new(NullArray::new(2))),
            (
                "boolean",
                Arc::new(BooleanArray::from(vec![Some(true), None])),
            ),
            ("int8", Arc::new(Int8Array::from(vec![Some(-8), None]))),
            ("int16", Arc::new(Int16Array::from(vec![Some(-16), None]))),
            ("int32", Arc::new(Int32Array::from(vec![Some(-32), None]))),
            ("int64", Arc::new(Int64Array::from(vec![Some(-64), None]))),
            ("uint8", Arc::new(UInt8Array::from(vec![Some(8), None]))),
            ("uint16", Arc::new(UInt16Array::from(vec![Some(16), None]))),
            ("uint32", Arc::new(UInt32Array::from(vec![Some(32), None]))),
            ("uint64", Arc::new(UInt64Array::from(vec![Some(64), None]))),
            ("float16", Arc::new(float16)),
            (
                "float32",
                Arc::new(Float32Array::from(vec![Some(1.5), None])),
            ),
            (
                "float64",
                Arc::new(Float64Array::from(vec![Some(2.5), None])),
            ),
            (
                "decimal128",
                Arc::new(
                    Decimal128Array::from(vec![Some(1250), None])
                        .with_precision_and_scale(10, 2)
                        .unwrap(),
                ),
            ),
            (
                "decimal256",
                Arc::new(
                    Decimal256Array::from(vec![Some(i256::from_i128(1250)), None])
                        .with_precision_and_scale(40, 2)
                        .unwrap(),
                ),
            ),
            ("utf8", Arc::new(StringArray::from(vec![Some("a"), None]))),
            (
                "large_utf8",
                Arc::new(LargeStringArray::from(vec![Some("b"), None])),
            ),
            (
                "utf8_view",
                Arc::new(StringViewArray::from(vec![
                    Some("longer than twelve bytes, so out of line"),
                    None,
                ])),
            ),
            (
                "binary",
                Arc::new(BinaryArray::from(vec![Some(&b"\x00\xff"[..]), None])),
            ),
            (
                "large_binary",
                Arc::new(LargeBinaryArray::from(vec![Some(&b"\x01"[..]), None])),
            ),
            (
                "fixed_size_binary",
                Arc::new(
                    FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                        [Some([1u8, 2]), None].into_iter(),
                        2,
                    )
                    .unwrap(),
                ),
            ),
            (
                "date32",
                Arc::new(Date32Array::from(vec![Some(19782), None])),
            ),
            (
                "date64",
                Arc::new(Date64Array::from(vec![Some(1_709_164_800_000), None])),
            ),
            (
                "time32",
                Arc::new(Time32SecondArray::from(vec![Some(45296), None])),
            ),
            (
                "time32_ms",
                Arc::new(Time32MillisecondArray::from(vec![Some(45_296_000), None])),
            ),
            (
                "time64",
                Arc::new(Time64MicrosecondArray::from(vec![
                    Some(45_296_000_000),
                    None,
                ])),
            ),
            (
                "time64_ns",
                Arc::new(Time64NanosecondArray::from(vec![Some(1), None])),
            ),
            (
                "timestamp",
                Arc::new(
                    TimestampMicrosecondArray::from(vec![Some(1_709_210_096_000_000), None])
                        .with_timezone("+05:00"),
                ),
            ),
            (
                "duration",
                Arc::new(DurationNanosecondArray::from(vec![Some(1_000), None])),
            ),
            (
                "interval_ym",
                Arc::new(IntervalYearMonthArray::from(vec![Some(14), None])),
            ),
            (
                "interval_dt",
                Arc::new(IntervalDayTimeArray::from(vec![
                    Some(arrow_buffer::IntervalDayTime::new(1, 2)),
                    None,
                ])),
            ),
            (
                "interval_mdn",
                Arc::new(IntervalMonthDayNanoArray::from(vec![
                    Some(arrow_buffer::IntervalMonthDayNano::new(1, 2, 3)),
                    None,
                ])),
            ),
        ];
        RecordBatch::try_from_iter(columns).unwrap()
    }

    #[test]
    fn every_carried_type_round_trips_with_nulls() {
        let batch = every_carried_type();
        let mut fields: Vec<Field> = batch
            .schema()
            .fields()
            .iter()
            .map(|field| field.as_ref().clone())
            .collect();
        fields[0] = fields[0].clone().with_metadata(HashMap::from([(
            "dbdelve.type".to_string(),
            "NULL".to_string(),
        )]));
        let batch = batch.with_schema(Arc::new(Schema::new(fields))).unwrap();

        let schema = decode_schema(&encode_schema(&batch.schema())).unwrap();
        assert_eq!(schema, batch.schema());
        let decoded = decode_batch(&schema, &encode_batch(&batch).unwrap()).unwrap();
        assert_eq!(decoded, batch);
        for column in decoded.columns().iter().skip(1) {
            assert!(column.is_null(1), "{}", column.data_type());
        }
    }
}

//! Framed encode / decode for wire records.
//!
//! Every encoded record is `header || body`:
//!
//! ```text
//! offset 0  u16 LE  schema version   (must equal R::SCHEMA_VERSION)
//! offset 2  u16 LE  wire tag         (must equal R::TAG)
//! offset 4  ...      postcard body of R
//! ```
//!
//! Decoding checks the input length against the channel limit *before* it
//! parses anything, so an oversized or hostile frame cannot drive an
//! allocation. After a successful parse, [`Record::validate`] runs; a record
//! that decodes structurally but violates a count/range/finiteness rule is
//! rejected.

use crate::limits::{MAX_BULK_PART, MAX_CONTROL_RECORD, MAX_DATAGRAM_PAYLOAD, SizeLimitError};
use crate::records::{Record, RecordError, WireTag};

/// Header length in bytes: `u16` schema version + `u16` tag.
pub const HEADER_LEN: usize = 4;

/// Worst-case postcard metadata: transfer u64 (10), index u32 (5), hash (32),
/// payload length u64 (10), plus the protocol header. Payload bytes are capped
/// separately by BaselinePart::validate.
pub const BULK_FRAME_OVERHEAD: usize = HEADER_LEN + 10 + 5 + 32 + 10;

pub fn encode_bulk(record: &crate::BaselinePart) -> Result<Vec<u8>, CodecError> {
    encode(record, MAX_BULK_PART + BULK_FRAME_OVERHEAD, "bulk part")
}

pub fn decode_bulk(bytes: &[u8]) -> Result<crate::BaselinePart, CodecError> {
    decode(bytes, MAX_BULK_PART + BULK_FRAME_OVERHEAD)
}

/// World consumers must validate material references before applying a record.
/// Generic decode_control only checks structural validity, without a registry.
pub fn decode_topology(
    bytes: &[u8],
    manifest: &spall_core::MaterialManifest,
) -> Result<crate::TopologyTransaction, CodecError> {
    let record: crate::TopologyTransaction = decode_control(bytes)?;
    record.validate_against(manifest)?;
    Ok(record)
}

/// Failure encoding or decoding a framed record.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CodecError {
    #[error("frame is {actual} bytes; channel limit is {limit}")]
    FrameTooLarge { actual: usize, limit: usize },
    #[error("frame is {0} bytes; need at least {HEADER_LEN} for a header")]
    Truncated(usize),
    #[error("schema version {found} does not match expected {expected}")]
    SchemaVersion { expected: u16, found: u16 },
    #[error("wire tag {found} does not match expected {expected:?} ({expected_num})")]
    TagMismatch {
        expected: WireTag,
        expected_num: u16,
        found: u16,
    },
    #[error("body did not decode: {0}")]
    Body(String),
    #[error("trailing {0} bytes after the record body")]
    TrailingBytes(usize),
    #[error(transparent)]
    Invalid(#[from] RecordError),
    #[error(transparent)]
    Size(#[from] SizeLimitError),
}

fn encode<R: Record>(
    record: &R,
    limit: usize,
    channel: &'static str,
) -> Result<Vec<u8>, CodecError> {
    record.validate()?;
    let body = postcard::to_stdvec(record).map_err(|e| CodecError::Body(e.to_string()))?;
    let total = HEADER_LEN + body.len();
    if total > limit {
        return Err(CodecError::Size(SizeLimitError::bytes(
            channel, total, limit,
        )));
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&R::SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&R::TAG.to_u16().to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

fn decode<R: Record>(bytes: &[u8], limit: usize) -> Result<R, CodecError> {
    // Bound the allocation risk first: refuse before touching the contents.
    if bytes.len() > limit {
        return Err(CodecError::FrameTooLarge {
            actual: bytes.len(),
            limit,
        });
    }
    if bytes.len() < HEADER_LEN {
        return Err(CodecError::Truncated(bytes.len()));
    }
    let schema = u16::from_le_bytes([bytes[0], bytes[1]]);
    if schema != R::SCHEMA_VERSION {
        return Err(CodecError::SchemaVersion {
            expected: R::SCHEMA_VERSION,
            found: schema,
        });
    }
    let tag = u16::from_le_bytes([bytes[2], bytes[3]]);
    if tag != R::TAG.to_u16() {
        return Err(CodecError::TagMismatch {
            expected: R::TAG,
            expected_num: R::TAG.to_u16(),
            found: tag,
        });
    }
    let (record, rest): (R, &[u8]) = postcard::take_from_bytes(&bytes[HEADER_LEN..])
        .map_err(|e| CodecError::Body(e.to_string()))?;
    if !rest.is_empty() {
        return Err(CodecError::TrailingBytes(rest.len()));
    }
    record.validate()?;
    Ok(record)
}

/// Encodes a reliable control / topology record. Fails if the frame would
/// exceed [`MAX_CONTROL_RECORD`].
pub fn encode_control<R: Record>(record: &R) -> Result<Vec<u8>, CodecError> {
    encode(record, MAX_CONTROL_RECORD, "control record")
}

/// Decodes a reliable control / topology record. Refuses input longer than
/// [`MAX_CONTROL_RECORD`] before parsing.
pub fn decode_control<R: Record>(bytes: &[u8]) -> Result<R, CodecError> {
    decode(bytes, MAX_CONTROL_RECORD)
}

/// Encodes an unreliable datagram record (input / motion). Fails above
/// [`MAX_DATAGRAM_PAYLOAD`].
pub fn encode_datagram<R: Record>(record: &R) -> Result<Vec<u8>, CodecError> {
    encode(record, MAX_DATAGRAM_PAYLOAD, "datagram")
}

/// Decodes an unreliable datagram record. Refuses input longer than
/// [`MAX_DATAGRAM_PAYLOAD`] before parsing.
pub fn decode_datagram<R: Record>(bytes: &[u8]) -> Result<R, CodecError> {
    decode(bytes, MAX_DATAGRAM_PAYLOAD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::{
        ActionOutcome, ActionStatus, DurableThrough, InputSeq, MotionSnapshot, RequestId,
        SnapshotSeq,
    };
    use spall_core::{EntityId, JournalSeq, Pose, QuantizedQuat, Revision, Tick};

    #[test]
    fn maximum_bulk_payload_round_trips_with_metadata() {
        let payload = vec![17; MAX_BULK_PART];
        let part = crate::BaselinePart {
            transfer_id: crate::TransferId(u64::MAX),
            part_index: 0,
            part_hash: crate::Hash32::of(&payload),
            payload,
        };
        assert!(encode_control(&part).is_err());
        let bytes = encode_bulk(&part).unwrap();
        assert_eq!(decode_bulk(&bytes).unwrap(), part);
        assert!(decode_bulk(&vec![0; MAX_BULK_PART + BULK_FRAME_OVERHEAD + 1]).is_err());
    }

    #[test]
    fn postcard_cannot_construct_an_invalid_brush() {
        #[derive(serde::Serialize)]
        struct RawBrush {
            centre: spall_core::BrushPoint,
            radius_units: i64,
        }
        for radius in [-1, 256 * 256 + 1, i64::MAX] {
            let bytes = postcard::to_stdvec(&RawBrush {
                centre: spall_core::BrushPoint::from_units(0, 0, 0),
                radius_units: radius,
            })
            .unwrap();
            assert!(postcard::from_bytes::<spall_core::SphereBrush>(&bytes).is_err());
        }
    }

    #[test]
    fn control_record_round_trips() {
        let status = ActionStatus {
            request_id: RequestId(0x0102_0304_0506_0708),
            outcome: ActionOutcome::Queued,
        };
        let bytes = encode_control(&status).unwrap();
        let back: ActionStatus = decode_control(&bytes).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn durable_through_has_stable_golden_bytes() {
        let record = DurableThrough {
            journal_seq: JournalSeq(300),
        };
        let bytes = encode_control(&record).unwrap();
        // header: schema=1, tag=11 (DurableThrough); body: postcard varint of 300 = 0xAC 0x02
        assert_eq!(bytes, [0x01, 0x00, 0x0B, 0x00, 0xAC, 0x02]);
        let back: DurableThrough = decode_control(&bytes).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn oversize_input_is_refused_before_parsing() {
        let huge = vec![0u8; MAX_CONTROL_RECORD + 1];
        let err = decode_control::<ActionStatus>(&huge).unwrap_err();
        assert!(matches!(err, CodecError::FrameTooLarge { .. }));
    }

    #[test]
    fn wrong_tag_and_schema_are_rejected() {
        let record = DurableThrough {
            journal_seq: JournalSeq(1),
        };
        let mut bytes = encode_control(&record).unwrap();
        // Decoding as the wrong record type sees a tag mismatch.
        assert!(matches!(
            decode_control::<ActionStatus>(&bytes),
            Err(CodecError::TagMismatch { .. })
        ));
        // Corrupt the schema version.
        bytes[0] = 0x09;
        assert!(matches!(
            decode_control::<DurableThrough>(&bytes),
            Err(CodecError::SchemaVersion { .. })
        ));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let record = DurableThrough {
            journal_seq: JournalSeq(1),
        };
        let mut bytes = encode_control(&record).unwrap();
        bytes.push(0xFF);
        assert!(matches!(
            decode_control::<DurableThrough>(&bytes),
            Err(CodecError::TrailingBytes(1))
        ));
    }

    fn a_snapshot() -> MotionSnapshot {
        MotionSnapshot {
            server_tick: Tick(1),
            snapshot_seq: SnapshotSeq(1),
            acked_input: InputSeq(0),
            body: EntityId::new(1).unwrap(),
            topology_revision: Revision(1),
            pose: Pose {
                translation_m: [1.0, 2.0, 3.0],
                rotation: QuantizedQuat::from_unit(0.0, 0.0, 0.0, 1.0).unwrap(),
            },
            linear_velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
            sleeping: false,
        }
    }

    #[test]
    fn datagram_round_trips_and_rejects_non_finite_pose() {
        let snap = a_snapshot();
        let bytes = encode_datagram(&snap).unwrap();
        assert_eq!(decode_datagram::<MotionSnapshot>(&bytes).unwrap(), snap);

        // Encode-side: validation refuses a NaN translation.
        let mut broken = a_snapshot();
        broken.pose.translation_m[1] = f64::NAN;
        assert!(matches!(
            encode_datagram(&broken),
            Err(CodecError::Invalid(RecordError::NonFinite(_)))
        ));

        // Decode-side: splice a NaN over the first translation lane (the
        // `1.0f64` little-endian window) and confirm post-parse validation
        // catches it rather than the value flowing through.
        let mut corrupt = bytes.clone();
        let one = 1.0f64.to_le_bytes();
        let pos = corrupt
            .windows(8)
            .position(|w| w == one)
            .expect("first translation lane present");
        corrupt[pos..pos + 8].copy_from_slice(&f64::NAN.to_le_bytes());
        assert!(matches!(
            decode_datagram::<MotionSnapshot>(&corrupt),
            Err(CodecError::Invalid(RecordError::NonFinite(_)))
        ));
    }
}

//! Bounded typed bulk attachments for one ordered topology transaction.
//!
//! This is a pure protocol candidate decoder. It does not dispatch a baseline
//! transfer or apply a transaction; those remain the responsibility of the
//! owning simulation/replica after the complete set has been validated.

use crate::records::WIRE_SCHEMA_VERSION;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use serde::{Deserialize, Serialize};
use spall_core::{EntityId, TransactionId, VolumeId};

use crate::lifecycle_bulk::LifecycleAttachmentWork;
use crate::{
    BaselineOwner, BaselineWorld, CreatureLifecycleBulkPayloadV4, Hash32, TopologyOp,
    TopologyTransaction,
};

pub const MAX_TRANSACTION_ATTACHMENT_BYTES: usize = crate::limits::MAX_ASSEMBLED_TRANSFER;
pub const MAX_TRANSACTION_GEOMETRY_DECODED_BYTES: usize =
    crate::limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED;
pub const MAX_TRANSACTION_LIFECYCLE_DECODED_BYTES: usize =
    crate::creature_owner::MAX_CREATURE_OWNER_SET_BYTES;

#[derive(Debug, Clone, PartialEq)]
pub struct TransactionAttachmentSetV1 {
    pub schema: u16,
    pub transaction_id: TransactionId,
    pub attachments: Vec<TransactionAttachmentV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TransactionAttachmentV1 {
    Geometry {
        transfer_id: u64,
        world: BaselineWorld,
    },
    CreatureLifecycle {
        transfer_id: u64,
        payload: Box<CreatureLifecycleBulkPayloadV4>,
    },
}

/// A validated, owned candidate. Fields are private so callers cannot bypass
/// transaction correlation after validation.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedTransactionAttachmentSetV1 {
    transaction_id: TransactionId,
    transaction_replay_identity: Hash32,
    attachments: BTreeMap<u64, ValidatedTransactionAttachmentV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ValidatedTransactionAttachmentV1 {
    Geometry(BaselineWorld),
    CreatureLifecycle(Box<CreatureLifecycleBulkPayloadV4>),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransactionAttachmentError {
    #[error("attachment-set schema or transaction id is invalid")]
    Header,
    #[error("attachment count does not match transaction markers")]
    Count,
    #[error("attachment encoding is malformed: {0}")]
    Encoding(String),
    #[error("attachment compressed-byte budget exceeded")]
    CompressedBudget,
    #[error("attachment decoded-work budget exceeded")]
    DecodedBudget,
    #[error("attachment marker family, transfer id, or payload correlation differs")]
    Correlation,
    #[error("duplicate or unreferenced attachment")]
    DuplicateOrExtra,
    #[error("baseline payload is invalid: {0}")]
    Baseline(String),
    #[error("lifecycle payload is invalid: {0}")]
    Lifecycle(String),
}

#[derive(Serialize, Deserialize)]
struct AttachmentPrefixV1 {
    schema: u16,
    transaction_id: TransactionId,
    count: u32,
}

impl TransactionAttachmentSetV1 {
    /// Encode a typed set only after validating it against its transaction.
    /// The returned bytes use a small prefix followed by length-delimited
    /// payloads, so the decoder can compare correlation before nested decode.
    pub fn encode_for_transaction(
        &self,
        transaction: &TopologyTransaction,
    ) -> Result<Vec<u8>, TransactionAttachmentError> {
        validate_typed_set(self, transaction)?;
        let count =
            u32::try_from(self.attachments.len()).map_err(|_| TransactionAttachmentError::Count)?;
        if count as usize > crate::limits::MAX_TRANSACTION_OPS {
            return Err(TransactionAttachmentError::Count);
        }
        let prefix = postcard::to_stdvec(&AttachmentPrefixV1 {
            schema: self.schema,
            transaction_id: self.transaction_id,
            count,
        })
        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?;
        let mut output = Vec::new();
        append_bounded(&mut output, &prefix)?;
        let mut geometry_budget = MAX_TRANSACTION_GEOMETRY_DECODED_BYTES;
        let mut lifecycle_budget = MAX_TRANSACTION_LIFECYCLE_DECODED_BYTES;
        for attachment in &self.attachments {
            let (id, kind, bytes) = match attachment {
                TransactionAttachmentV1::Geometry { transfer_id, world } => {
                    crate::baseline_decode::preflight_world_storage(world, geometry_budget)?;
                    world
                        .validate()
                        .map_err(|e| TransactionAttachmentError::Baseline(e.to_string()))?;
                    let raw_size = postcard::experimental::serialized_size(world)
                        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?;
                    let _minimum_cost = crate::baseline_decode::preflight_world_size(
                        world,
                        raw_size,
                        geometry_budget,
                    )?;
                    let raw = postcard::to_stdvec(world)
                        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?;
                    let raw_capacity_cost =
                        crate::baseline_decode::raw_buffer_cost(raw.len(), raw.capacity())?;
                    let heap = crate::baseline_decode::preflight_world_storage(
                        world,
                        geometry_budget.saturating_sub(raw_capacity_cost),
                    )?;
                    let decoded_cost = raw_capacity_cost
                        .checked_add(heap)
                        .filter(|cost| *cost <= geometry_budget)
                        .ok_or(TransactionAttachmentError::DecodedBudget)?;
                    geometry_budget = geometry_budget
                        .checked_sub(decoded_cost)
                        .ok_or(TransactionAttachmentError::DecodedBudget)?;
                    let compressed_remaining = MAX_TRANSACTION_ATTACHMENT_BYTES
                        .checked_sub(output.len())
                        .and_then(|remaining| remaining.checked_sub(32))
                        .ok_or(TransactionAttachmentError::CompressedBudget)?;
                    let bytes = encode_zstd_bounded(&raw, compressed_remaining)?;
                    (*transfer_id, 0u8, bytes)
                }
                TransactionAttachmentV1::CreatureLifecycle {
                    transfer_id,
                    payload,
                } => {
                    let raw_size = payload
                        .decoded_size_for_attachment_set()
                        .map_err(|e| TransactionAttachmentError::Lifecycle(e.to_string()))?;
                    lifecycle_budget = lifecycle_budget
                        .checked_sub(raw_size)
                        .ok_or(TransactionAttachmentError::DecodedBudget)?;
                    let compressed_remaining = MAX_TRANSACTION_ATTACHMENT_BYTES
                        .checked_sub(output.len())
                        .and_then(|remaining| remaining.checked_sub(32))
                        .ok_or(TransactionAttachmentError::CompressedBudget)?;
                    let bytes = payload
                        .encode_compressed_with_limit(compressed_remaining)
                        .map_err(|e| TransactionAttachmentError::Lifecycle(e.to_string()))?;
                    (*transfer_id, 1u8, bytes)
                }
            };
            append_u64(&mut output, id)?;
            append_u8(&mut output, kind)?;
            let len = u32::try_from(bytes.len())
                .map_err(|_| TransactionAttachmentError::CompressedBudget)?;
            append_u32(&mut output, len)?;
            append_bounded(&mut output, &bytes)?;
        }
        Ok(output)
    }
}

impl ValidatedTransactionAttachmentSetV1 {
    pub fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Check that this candidate still belongs to the exact ordered parent
    /// transaction used during bounded decode. This validates the parent and
    /// recomputes its existing replay identity; it creates no additional ledger.
    pub fn matches_transaction(&self, transaction: &TopologyTransaction) -> bool {
        transaction.transaction_id == self.transaction_id
            && transaction
                .replay_identity_hash()
                .is_ok_and(|identity| identity == self.transaction_replay_identity)
    }

    pub fn get(&self, transfer_id: u64) -> Option<&ValidatedTransactionAttachmentV1> {
        self.attachments.get(&transfer_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, &ValidatedTransactionAttachmentV1)> {
        self.attachments.iter().map(|(id, value)| (*id, value))
    }
}

/// Decode a wire set against its already validated parent transaction. The
/// prefix count and transaction id are checked before any attachment body is
/// sliced or deserialized.
pub fn decode_for_transaction(
    bytes: &[u8],
    transaction: &TopologyTransaction,
) -> Result<ValidatedTransactionAttachmentSetV1, TransactionAttachmentError> {
    if bytes.len() > MAX_TRANSACTION_ATTACHMENT_BYTES {
        return Err(TransactionAttachmentError::CompressedBudget);
    }
    let transaction_replay_identity = transaction
        .replay_identity_hash()
        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?;
    let (prefix, mut rest): (AttachmentPrefixV1, &[u8]) = postcard::take_from_bytes(bytes)
        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?;
    let expected = expected_markers(transaction)?;
    if prefix.schema != WIRE_SCHEMA_VERSION || prefix.transaction_id != transaction.transaction_id {
        return Err(TransactionAttachmentError::Header);
    }
    if prefix.count as usize != expected.len()
        || prefix.count as usize > crate::limits::MAX_TRANSACTION_OPS
    {
        return Err(TransactionAttachmentError::Count);
    }

    let mut candidate = BTreeMap::new();
    let mut compressed_remaining = MAX_TRANSACTION_ATTACHMENT_BYTES;
    let mut geometry_remaining = MAX_TRANSACTION_GEOMETRY_DECODED_BYTES;
    let mut lifecycle_work = LifecycleAttachmentWork::capacity();
    for _ in 0..prefix.count {
        let id: u64 = take(&mut rest)?;
        let kind: u8 = take(&mut rest)?;
        let len: u32 = take(&mut rest)?;
        let len = len as usize;
        compressed_remaining = compressed_remaining
            .checked_sub(len)
            .ok_or(TransactionAttachmentError::CompressedBudget)?;
        if len > rest.len() {
            return Err(TransactionAttachmentError::Encoding(
                "truncated attachment payload".into(),
            ));
        }
        let (payload_bytes, trailing) = rest.split_at(len);
        rest = trailing;
        let expected_kind = expected
            .get(&id)
            .ok_or(TransactionAttachmentError::DuplicateOrExtra)?;
        if *expected_kind != kind || candidate.contains_key(&id) {
            return Err(TransactionAttachmentError::Correlation);
        }
        let value = match kind {
            0 => {
                let world = crate::baseline_decode::decode_compressed(
                    payload_bytes,
                    &mut geometry_remaining,
                )?;
                validate_geometry_group(transaction, id, &world)?;
                ValidatedTransactionAttachmentV1::Geometry(world)
            }
            1 => {
                let marker = lifecycle_marker(transaction, id)?;
                let (payload, decoded_size) = crate::lifecycle_bulk::CreatureLifecycleBulkPayloadV4::decode_compressed_with_budget(
                    payload_bytes,
                    transaction.transaction_id,
                    marker.operation_ordinal,
                    marker.commit_hash,
                    lifecycle_work.decoded_bytes,
                    &mut lifecycle_work,
                )
                .map_err(|e| TransactionAttachmentError::Lifecycle(e.to_string()))?;
                let _ = decoded_size;
                ValidatedTransactionAttachmentV1::CreatureLifecycle(Box::new(payload))
            }
            _ => return Err(TransactionAttachmentError::Correlation),
        };
        candidate.insert(id, value);
    }
    if !rest.is_empty() || candidate.len() != expected.len() {
        return Err(TransactionAttachmentError::DuplicateOrExtra);
    }
    Ok(ValidatedTransactionAttachmentSetV1 {
        transaction_id: transaction.transaction_id,
        transaction_replay_identity,
        attachments: candidate,
    })
}

fn validate_typed_set(
    set: &TransactionAttachmentSetV1,
    transaction: &TopologyTransaction,
) -> Result<(), TransactionAttachmentError> {
    transaction
        .replay_identity_hash()
        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?;
    if set.schema != WIRE_SCHEMA_VERSION || set.transaction_id != transaction.transaction_id {
        return Err(TransactionAttachmentError::Header);
    }
    let expected = expected_markers(transaction)?;
    if set.attachments.len() != expected.len()
        || set.attachments.len() > crate::limits::MAX_TRANSACTION_OPS
    {
        return Err(TransactionAttachmentError::Count);
    }
    let mut seen = BTreeSet::new();
    let mut lifecycle_work = LifecycleAttachmentWork::capacity();
    for attachment in &set.attachments {
        let (id, kind) = match attachment {
            TransactionAttachmentV1::Geometry { transfer_id, .. } => (*transfer_id, 0),
            TransactionAttachmentV1::CreatureLifecycle { transfer_id, .. } => (*transfer_id, 1),
        };
        if !expected.contains_key(&id) || !seen.insert(id) {
            return Err(TransactionAttachmentError::DuplicateOrExtra);
        }
        if expected.get(&id) != Some(&kind) {
            return Err(TransactionAttachmentError::Correlation);
        }
        match attachment {
            TransactionAttachmentV1::Geometry { world, .. } => {
                crate::baseline_decode::preflight_world_storage(
                    world,
                    MAX_TRANSACTION_GEOMETRY_DECODED_BYTES,
                )?;
                validate_geometry_group(transaction, id, world)?;
            }
            TransactionAttachmentV1::CreatureLifecycle { payload, .. } => {
                let marker = lifecycle_marker(transaction, id)?;
                if payload.schema != crate::WIRE_SCHEMA_VERSION
                    || payload.transaction_id != transaction.transaction_id
                    || payload.operation_ordinal != marker.operation_ordinal
                    || payload.commit_hash != marker.commit_hash
                {
                    return Err(TransactionAttachmentError::Correlation);
                }
                lifecycle_work
                    .charge(
                        payload
                            .work_for_attachment_set()
                            .map_err(|e| TransactionAttachmentError::Lifecycle(e.to_string()))?,
                    )
                    .map_err(|e| TransactionAttachmentError::Lifecycle(e.to_string()))?;
            }
        }
    }
    Ok(())
}

fn expected_markers(
    transaction: &TopologyTransaction,
) -> Result<BTreeMap<u64, u8>, TransactionAttachmentError> {
    let mut expected = BTreeMap::new();
    for op in &transaction.ops {
        match op {
            TopologyOp::SplitOffBulkBaseline { transfer_id, .. }
            | TopologyOp::SourcePatchBulkBaseline { transfer_id, .. } => {
                if *transfer_id == 0 {
                    return Err(TransactionAttachmentError::Correlation);
                }
                if expected.insert(*transfer_id, 0).is_some_and(|old| old != 0) {
                    return Err(TransactionAttachmentError::Correlation);
                }
            }
            TopologyOp::CreatureLifecycleBulk { transfer_id, .. } => {
                if *transfer_id == 0 {
                    return Err(TransactionAttachmentError::Correlation);
                }
                if expected.insert(*transfer_id, 1).is_some() {
                    return Err(TransactionAttachmentError::Correlation);
                }
            }
            _ => {}
        }
    }
    if expected.len() > crate::limits::MAX_TRANSACTION_OPS {
        return Err(TransactionAttachmentError::Count);
    }
    Ok(expected)
}

fn lifecycle_marker(
    transaction: &TopologyTransaction,
    transfer_id: u64,
) -> Result<LifecycleMarker, TransactionAttachmentError> {
    transaction
        .ops
        .iter()
        .find_map(|op| match op {
            TopologyOp::CreatureLifecycleBulk {
                operation_ordinal,
                payload_schema,
                commit_hash,
                transfer_id: marker_id,
            } if *marker_id == transfer_id && *payload_schema == crate::WIRE_SCHEMA_VERSION => {
                Some(LifecycleMarker {
                    operation_ordinal: *operation_ordinal,
                    commit_hash: *commit_hash,
                })
            }
            _ => None,
        })
        .ok_or(TransactionAttachmentError::Correlation)
}

#[derive(Clone, Copy)]
struct LifecycleMarker {
    operation_ordinal: u32,
    commit_hash: Hash32,
}

fn validate_geometry_group(
    transaction: &TopologyTransaction,
    transfer_id: u64,
    world: &BaselineWorld,
) -> Result<(), TransactionAttachmentError> {
    if world.checkpoint_tick != transaction.server_tick.0 {
        return Err(TransactionAttachmentError::Correlation);
    }
    let mut expected = BTreeMap::<VolumeId, Option<EntityId>>::new();
    let mut child_count = 0usize;
    for op in &transaction.ops {
        match op {
            TopologyOp::SplitOffBulkBaseline {
                child,
                child_entity,
                transfer_id: marker_id,
                ..
            } if *marker_id == transfer_id => {
                child_count = child_count
                    .checked_add(1)
                    .ok_or(TransactionAttachmentError::Count)?;
                if expected.insert(*child, Some(*child_entity)).is_some() {
                    return Err(TransactionAttachmentError::Correlation);
                }
            }
            TopologyOp::SourcePatchBulkBaseline {
                source,
                transfer_id: marker_id,
            } if *marker_id == transfer_id => {
                if expected.contains_key(source) {
                    return Err(TransactionAttachmentError::Correlation);
                }
                expected.insert(*source, None);
            }
            _ => {}
        }
    }
    if child_count == 0 || world.volumes.len() != expected.len() {
        return Err(TransactionAttachmentError::Correlation);
    }
    for volume in &world.volumes {
        match expected.get(&volume.volume_id) {
            Some(Some(entity)) if volume.owner == BaselineOwner::Body(*entity) => {}
            Some(None) => {}
            _ => return Err(TransactionAttachmentError::Correlation),
        }
    }
    Ok(())
}

fn encode_zstd_bounded(raw: &[u8], limit: usize) -> Result<Vec<u8>, TransactionAttachmentError> {
    let writer = LimitedVecWriter {
        bytes: Vec::new(),
        limit,
    };
    let mut encoder = zstd::stream::Encoder::new(writer, 0)
        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?;
    encoder
        .write_all(raw)
        .map_err(|_| TransactionAttachmentError::CompressedBudget)?;
    let bytes = encoder
        .finish()
        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?
        .bytes;
    if bytes.len() > limit {
        return Err(TransactionAttachmentError::CompressedBudget);
    }
    Ok(bytes)
}

struct LimitedVecWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedVecWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|len| len > self.limit)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "compressed attachment exceeds remaining budget",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn take<'a, T: Deserialize<'a>>(rest: &mut &'a [u8]) -> Result<T, TransactionAttachmentError> {
    let (value, trailing) = postcard::take_from_bytes(rest)
        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?;
    *rest = trailing;
    Ok(value)
}

fn append_u8(out: &mut Vec<u8>, value: u8) -> Result<(), TransactionAttachmentError> {
    append_bounded(out, &[value])
}

fn append_u32(out: &mut Vec<u8>, value: u32) -> Result<(), TransactionAttachmentError> {
    let encoded = postcard::to_stdvec(&value)
        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?;
    append_bounded(out, &encoded)
}

fn append_u64(out: &mut Vec<u8>, value: u64) -> Result<(), TransactionAttachmentError> {
    let encoded = postcard::to_stdvec(&value)
        .map_err(|e| TransactionAttachmentError::Encoding(e.to_string()))?;
    append_bounded(out, &encoded)
}

fn append_bounded(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), TransactionAttachmentError> {
    if out
        .len()
        .checked_add(bytes.len())
        .is_none_or(|total| total > MAX_TRANSACTION_ATTACHMENT_BYTES)
    {
        return Err(TransactionAttachmentError::CompressedBudget);
    }
    out.extend_from_slice(bytes);
    Ok(())
}

#[allow(dead_code)]
fn _transaction_id_type_is_copy(_: TransactionId) {}

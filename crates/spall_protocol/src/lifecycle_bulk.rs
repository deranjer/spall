//! Typed framing for oversized lifecycle commits on the ordered bulk stream.
//!
//! This module defines the payload and bounded codec only. Network dispatch and
//! transaction release remain closed until simulation and journal consumers
//! validate and publish the complete attachment atomically.

use serde::{Deserialize, Serialize};
use spall_core::TransactionId;
use std::io::{Read, Write};

use crate::creature_owner::{
    MAX_CREATURE_OWNER_SET_BYTES, MAX_CREATURE_OWNER_SET_DENSE_CELLS,
    MAX_CREATURE_OWNER_SET_RECORDS, MAX_CREATURE_OWNER_SET_SOLID_CELLS,
};
use crate::{
    CreatureLifecycleCommitV4, Hash32, MAX_CREATURE_OWNER_BRICKS, MAX_CREATURE_OWNER_COMPONENTS,
    MAX_CREATURE_OWNER_DENSE_CELLS, MAX_CREATURE_OWNER_MASK_BRICKS, MAX_CREATURE_OWNER_MASK_CELLS,
    MAX_CREATURE_OWNER_PARTS, MAX_CREATURE_OWNER_RECORD_BYTES, MAX_CREATURE_OWNER_SOLID_CELLS,
};

pub const CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA: u16 = 5;
pub const MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED: usize = MAX_CREATURE_OWNER_RECORD_BYTES + 64;

/// Cumulative work charged by one lifecycle attachment to its transaction set.
/// The limits mirror `CreatureOwnershipSetV4::validate`, with explicit bounds
/// for motion and death-witness arrays carried outside the owner record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LifecycleAttachmentWork {
    pub decoded_bytes: usize,
    pub records: usize,
    pub bricks: usize,
    pub dense_cells: usize,
    pub solid_cells: usize,
    pub masks: usize,
    pub mask_cells: usize,
    pub overlay_poses: usize,
    pub part_velocities: usize,
    pub witness_poses: usize,
}

impl LifecycleAttachmentWork {
    pub(crate) const fn capacity() -> Self {
        Self {
            decoded_bytes: MAX_CREATURE_OWNER_SET_BYTES,
            records: MAX_CREATURE_OWNER_SET_RECORDS,
            bricks: MAX_CREATURE_OWNER_BRICKS,
            dense_cells: MAX_CREATURE_OWNER_SET_DENSE_CELLS,
            solid_cells: MAX_CREATURE_OWNER_SET_SOLID_CELLS,
            masks: MAX_CREATURE_OWNER_MASK_BRICKS,
            mask_cells: MAX_CREATURE_OWNER_SET_SOLID_CELLS,
            part_velocities: 2 * MAX_CREATURE_OWNER_SET_RECORDS * MAX_CREATURE_OWNER_PARTS,
            overlay_poses: MAX_CREATURE_OWNER_SET_RECORDS
                * (MAX_CREATURE_OWNER_PARTS + MAX_CREATURE_OWNER_COMPONENTS),
            witness_poses: MAX_CREATURE_OWNER_SET_RECORDS * MAX_CREATURE_OWNER_PARTS,
        }
    }

    pub(crate) fn charge(&mut self, used: Self) -> Result<(), CreatureLifecycleBulkError> {
        macro_rules! charge_field {
            ($field:ident) => {
                self.$field = self
                    .$field
                    .checked_sub(used.$field)
                    .ok_or(CreatureLifecycleBulkError::WorkLimit)?;
            };
        }
        charge_field!(decoded_bytes);
        charge_field!(records);
        charge_field!(bricks);
        charge_field!(dense_cells);
        charge_field!(solid_cells);
        charge_field!(masks);
        charge_field!(mask_cells);
        charge_field!(overlay_poses);
        charge_field!(part_velocities);
        charge_field!(witness_poses);
        Ok(())
    }
}

/// Correlated lifecycle payload carried as the compressed body of a world-version-9
/// BaselineBegin/BaselinePart/BaselineEnd transfer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreatureLifecycleBulkPayloadV4 {
    pub schema: u16,
    pub transaction_id: TransactionId,
    pub operation_ordinal: u32,
    pub commit_hash: Hash32,
    pub commit: CreatureLifecycleCommitV4,
}

#[derive(Serialize, Deserialize)]
struct PayloadPrefixV1 {
    schema: u16,
    transaction_id: TransactionId,
    operation_ordinal: u32,
    commit_hash: Hash32,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CreatureLifecycleBulkError {
    #[error("compressed lifecycle payload is {actual} bytes; limit is {limit}")]
    CompressedLimit { actual: usize, limit: usize },
    #[error("decompressed lifecycle payload exceeds {0} bytes")]
    DecompressedLimit(usize),
    #[error("lifecycle payload schema is unsupported")]
    Schema,
    #[error("lifecycle payload transaction, ordinal, or hash does not match its marker")]
    Correlation,
    #[error("lifecycle commit hash does not match its bounded payload")]
    CommitHash,
    #[error("lifecycle payload exceeds cumulative owner work limits")]
    WorkLimit,
    #[error("postcard lifecycle payload: {0}")]
    Postcard(String),
    #[error("zstd lifecycle payload: {0}")]
    Zstd(String),
}

impl CreatureLifecycleBulkPayloadV4 {
    pub fn new(
        transaction_id: TransactionId,
        operation_ordinal: u32,
        commit: CreatureLifecycleCommitV4,
    ) -> Result<Self, CreatureLifecycleBulkError> {
        check_commit_work(&commit)?;
        if commit.change.transaction_id != transaction_id
            || commit.change.operation_ordinal != operation_ordinal
        {
            return Err(CreatureLifecycleBulkError::Correlation);
        }
        let size = postcard::experimental::serialized_size(&commit)
            .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        if size > MAX_CREATURE_OWNER_RECORD_BYTES {
            return Err(CreatureLifecycleBulkError::DecompressedLimit(
                MAX_CREATURE_OWNER_RECORD_BYTES,
            ));
        }
        let commit_hash = commit.canonical_hash();
        Ok(Self {
            schema: CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA,
            transaction_id,
            operation_ordinal,
            commit_hash,
            commit,
        })
    }

    /// Preflight decoded-byte cost for a containing attachment set. Structural
    /// owner work is checked before serialized sizing so a caller can reject a
    /// cumulative budget overrun before hashing or encoding this payload.
    pub(crate) fn decoded_size_for_attachment_set(
        &self,
    ) -> Result<usize, CreatureLifecycleBulkError> {
        if self.schema != CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA {
            return Err(CreatureLifecycleBulkError::Schema);
        }
        check_commit_work(&self.commit)?;
        if self.commit.change.transaction_id != self.transaction_id
            || self.commit.change.operation_ordinal != self.operation_ordinal
        {
            return Err(CreatureLifecycleBulkError::Correlation);
        }
        let commit_size = postcard::experimental::serialized_size(&self.commit)
            .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        if commit_size > MAX_CREATURE_OWNER_RECORD_BYTES {
            return Err(CreatureLifecycleBulkError::DecompressedLimit(
                MAX_CREATURE_OWNER_RECORD_BYTES,
            ));
        }
        let prefix_size = postcard::experimental::serialized_size(&PayloadPrefixV1 {
            schema: self.schema,
            transaction_id: self.transaction_id,
            operation_ordinal: self.operation_ordinal,
            commit_hash: self.commit_hash,
        })
        .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        let total = prefix_size.checked_add(commit_size).ok_or(
            CreatureLifecycleBulkError::DecompressedLimit(MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED),
        )?;
        if total > MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED {
            return Err(CreatureLifecycleBulkError::DecompressedLimit(
                MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED,
            ));
        }
        Ok(total)
    }

    pub(crate) fn work_for_attachment_set(
        &self,
    ) -> Result<LifecycleAttachmentWork, CreatureLifecycleBulkError> {
        let mut work = commit_work(&self.commit)?;
        work.decoded_bytes = self.decoded_size_for_attachment_set()?;
        Ok(work)
    }

    /// Bounded postcard + zstd representation used by the existing bulk stream.
    pub fn encode_compressed(&self) -> Result<Vec<u8>, CreatureLifecycleBulkError> {
        self.encode_compressed_with_limit(crate::limits::MAX_ASSEMBLED_TRANSFER)
    }

    pub(crate) fn encode_compressed_with_limit(
        &self,
        compressed_limit: usize,
    ) -> Result<Vec<u8>, CreatureLifecycleBulkError> {
        if self.schema != CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA {
            return Err(CreatureLifecycleBulkError::Schema);
        }
        check_commit_work(&self.commit)?;
        if self.commit.change.transaction_id != self.transaction_id
            || self.commit.change.operation_ordinal != self.operation_ordinal
        {
            return Err(CreatureLifecycleBulkError::Correlation);
        }
        let commit_size = postcard::experimental::serialized_size(&self.commit)
            .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        if commit_size > MAX_CREATURE_OWNER_RECORD_BYTES {
            return Err(CreatureLifecycleBulkError::DecompressedLimit(
                MAX_CREATURE_OWNER_RECORD_BYTES,
            ));
        }
        if self.commit.canonical_hash() != self.commit_hash {
            return Err(CreatureLifecycleBulkError::CommitHash);
        }
        let prefix = PayloadPrefixV1 {
            schema: self.schema,
            transaction_id: self.transaction_id,
            operation_ordinal: self.operation_ordinal,
            commit_hash: self.commit_hash,
        };
        let prefix_size = postcard::experimental::serialized_size(&prefix)
            .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        let raw_size = prefix_size.checked_add(commit_size).ok_or(
            CreatureLifecycleBulkError::DecompressedLimit(MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED),
        )?;
        if raw_size > MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED {
            return Err(CreatureLifecycleBulkError::DecompressedLimit(
                MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED,
            ));
        }
        let prefix_bytes = postcard::to_stdvec(&prefix)
            .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        let commit_bytes = postcard::to_stdvec(&self.commit)
            .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        let mut raw = Vec::with_capacity(raw_size);
        raw.extend_from_slice(&prefix_bytes);
        raw.extend_from_slice(&commit_bytes);
        let limit = compressed_limit.min(crate::limits::MAX_ASSEMBLED_TRANSFER);
        let writer = LimitedVecWriter {
            bytes: Vec::new(),
            limit,
        };
        let mut encoder = zstd::stream::Encoder::new(writer, 0)
            .map_err(|e| CreatureLifecycleBulkError::Zstd(e.to_string()))?;
        encoder
            .write_all(&raw)
            .map_err(|_e| CreatureLifecycleBulkError::CompressedLimit {
                actual: limit.saturating_add(1),
                limit,
            })?;
        let compressed = encoder
            .finish()
            .map_err(|e| CreatureLifecycleBulkError::Zstd(e.to_string()))?
            .bytes;
        if compressed.len() > limit {
            return Err(CreatureLifecycleBulkError::CompressedLimit {
                actual: compressed.len(),
                limit,
            });
        }
        Ok(compressed)
    }

    /// Decompress and check marker correlation before decoding the nested commit.
    pub fn decode_compressed(
        bytes: &[u8],
        transaction_id: TransactionId,
        operation_ordinal: u32,
        commit_hash: Hash32,
    ) -> Result<Self, CreatureLifecycleBulkError> {
        Self::decode_compressed_with_budget(
            bytes,
            transaction_id,
            operation_ordinal,
            commit_hash,
            MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED,
            &mut LifecycleAttachmentWork::capacity(),
        )
        .map(|(payload, _)| payload)
    }

    /// Decode with a caller-supplied remaining aggregate byte budget. The
    /// budget is checked on decompressed bytes before decoding the nested owner
    /// commit; the returned count lets a set decoder charge it exactly once.
    pub(crate) fn decode_compressed_with_budget(
        bytes: &[u8],
        transaction_id: TransactionId,
        operation_ordinal: u32,
        commit_hash: Hash32,
        remaining_budget: usize,
        remaining_work: &mut LifecycleAttachmentWork,
    ) -> Result<(Self, usize), CreatureLifecycleBulkError> {
        if bytes.len() > crate::limits::MAX_ASSEMBLED_TRANSFER {
            return Err(CreatureLifecycleBulkError::CompressedLimit {
                actual: bytes.len(),
                limit: crate::limits::MAX_ASSEMBLED_TRANSFER,
            });
        }
        let decompressed_cap = MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED.min(remaining_budget);
        let decoder = zstd::stream::Decoder::new(bytes)
            .map_err(|e| CreatureLifecycleBulkError::Zstd(e.to_string()))?;
        let mut raw = Vec::new();
        decoder
            .take(decompressed_cap as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|e| CreatureLifecycleBulkError::Zstd(e.to_string()))?;
        if raw.len() > decompressed_cap {
            return Err(CreatureLifecycleBulkError::DecompressedLimit(
                decompressed_cap,
            ));
        }
        let decoded_bytes = raw.len();
        let (prefix, commit_bytes): (PayloadPrefixV1, &[u8]) = postcard::take_from_bytes(&raw)
            .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        if prefix.schema != CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA {
            return Err(CreatureLifecycleBulkError::Schema);
        }
        if prefix.transaction_id != transaction_id
            || prefix.operation_ordinal != operation_ordinal
            || prefix.commit_hash != commit_hash
        {
            return Err(CreatureLifecycleBulkError::Correlation);
        }
        // The exact commit tail is bounded before deserialization; it is decoded
        // only after all marker correlation fields have matched.
        if commit_bytes.len() > MAX_CREATURE_OWNER_RECORD_BYTES {
            return Err(CreatureLifecycleBulkError::DecompressedLimit(
                MAX_CREATURE_OWNER_RECORD_BYTES,
            ));
        }
        let (commit, rest): (CreatureLifecycleCommitV4, &[u8]) =
            postcard::take_from_bytes(commit_bytes)
                .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        if !rest.is_empty() {
            return Err(CreatureLifecycleBulkError::Postcard(
                "trailing bytes after nested lifecycle commit".into(),
            ));
        }
        let payload = CreatureLifecycleBulkPayloadV4 {
            schema: prefix.schema,
            transaction_id: prefix.transaction_id,
            operation_ordinal: prefix.operation_ordinal,
            commit_hash: prefix.commit_hash,
            commit,
        };
        if payload.commit.change.transaction_id != transaction_id
            || payload.commit.change.operation_ordinal != operation_ordinal
        {
            return Err(CreatureLifecycleBulkError::Correlation);
        }
        let mut work = commit_work(&payload.commit)?;
        work.decoded_bytes = decoded_bytes;
        remaining_work.charge(work)?;
        if payload.commit.canonical_hash() != payload.commit_hash {
            return Err(CreatureLifecycleBulkError::CommitHash);
        }
        Ok((payload, decoded_bytes))
    }
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
                "compressed lifecycle payload exceeds remaining budget",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn check_commit_work(commit: &CreatureLifecycleCommitV4) -> Result<(), CreatureLifecycleBulkError> {
    commit_work(commit).map(|_| ())
}

fn commit_work(
    commit: &CreatureLifecycleCommitV4,
) -> Result<LifecycleAttachmentWork, CreatureLifecycleBulkError> {
    if commit.schema != crate::CREATURE_LIFECYCLE_COMMIT_SCHEMA
        || commit.change.schema != crate::CREATURE_CHANGE_SCHEMA
    {
        return Err(CreatureLifecycleBulkError::Schema);
    }
    if commit.pre_motion.as_ref().is_some_and(|motion| {
        motion.part_velocities.len() > MAX_CREATURE_OWNER_PARTS
            || motion.parts.len() > MAX_CREATURE_OWNER_PARTS
            || motion.components.len() > MAX_CREATURE_OWNER_COMPONENTS
    }) || commit
        .death_witness
        .as_ref()
        .is_some_and(|witness| witness.frozen_local_poses.len() > MAX_CREATURE_OWNER_PARTS)
    {
        return Err(CreatureLifecycleBulkError::WorkLimit);
    }
    let record = match &commit.change.operation {
        crate::CreatureLifecycleOperationV4::Birth { after }
        | crate::CreatureLifecycleOperationV4::Death { after } => after,
    };
    if record.schema != crate::CREATURE_OWNER_SCHEMA
        || record.state.schema != crate::CREATURE_STATE_SCHEMA
    {
        return Err(CreatureLifecycleBulkError::Schema);
    }
    if record.part_velocities.len() > MAX_CREATURE_OWNER_PARTS
        || record.parts.len() > MAX_CREATURE_OWNER_PARTS
        || record.components.len() > MAX_CREATURE_OWNER_COMPONENTS
    {
        return Err(CreatureLifecycleBulkError::WorkLimit);
    }
    let mut bricks = 0usize;
    let mut dense_cells = 0usize;
    let mut solid_cells = 0usize;
    for part in &record.parts {
        if part.bricks.len() > MAX_CREATURE_OWNER_BRICKS {
            return Err(CreatureLifecycleBulkError::WorkLimit);
        }
        bricks = bricks
            .checked_add(part.bricks.len())
            .ok_or(CreatureLifecycleBulkError::WorkLimit)?;
        if bricks > MAX_CREATURE_OWNER_BRICKS {
            return Err(CreatureLifecycleBulkError::WorkLimit);
        }
        for brick in &part.bricks {
            match &brick.cells {
                crate::BaselineCells::Uniform(material) if *material != 0 => {
                    solid_cells = solid_cells
                        .checked_add(32 * 32 * 32)
                        .ok_or(CreatureLifecycleBulkError::WorkLimit)?;
                    if solid_cells > MAX_CREATURE_OWNER_SOLID_CELLS {
                        return Err(CreatureLifecycleBulkError::WorkLimit);
                    }
                }
                crate::BaselineCells::Dense(cells) => {
                    if cells.len() != 32 * 32 * 32 {
                        return Err(CreatureLifecycleBulkError::WorkLimit);
                    }
                    dense_cells = dense_cells
                        .checked_add(cells.len())
                        .ok_or(CreatureLifecycleBulkError::WorkLimit)?;
                    if dense_cells > MAX_CREATURE_OWNER_DENSE_CELLS {
                        return Err(CreatureLifecycleBulkError::WorkLimit);
                    }
                    solid_cells = solid_cells
                        .checked_add(cells.iter().filter(|id| **id != 0).count())
                        .ok_or(CreatureLifecycleBulkError::WorkLimit)?;
                    if solid_cells > MAX_CREATURE_OWNER_SOLID_CELLS {
                        return Err(CreatureLifecycleBulkError::WorkLimit);
                    }
                }
                _ => {}
            }
        }
    }
    if bricks > MAX_CREATURE_OWNER_BRICKS
        || dense_cells > MAX_CREATURE_OWNER_DENSE_CELLS
        || solid_cells > MAX_CREATURE_OWNER_SOLID_CELLS
    {
        return Err(CreatureLifecycleBulkError::WorkLimit);
    }
    let mut mask_bricks = 0usize;
    let mut mask_cells = 0usize;
    for component in &record.components {
        if component.source_masks.len() > MAX_CREATURE_OWNER_MASK_BRICKS {
            return Err(CreatureLifecycleBulkError::WorkLimit);
        }
        mask_bricks = mask_bricks
            .checked_add(component.source_masks.len())
            .ok_or(CreatureLifecycleBulkError::WorkLimit)?;
        if mask_bricks > MAX_CREATURE_OWNER_MASK_BRICKS {
            return Err(CreatureLifecycleBulkError::WorkLimit);
        }
        for mask in &component.source_masks {
            if mask.cells.len() > MAX_CREATURE_OWNER_MASK_CELLS {
                return Err(CreatureLifecycleBulkError::WorkLimit);
            }
            mask_cells = mask_cells
                .checked_add(mask.cells.len())
                .ok_or(CreatureLifecycleBulkError::WorkLimit)?;
            if mask_cells > MAX_CREATURE_OWNER_MASK_CELLS
                || mask_cells > MAX_CREATURE_OWNER_SOLID_CELLS
            {
                return Err(CreatureLifecycleBulkError::WorkLimit);
            }
        }
    }
    if mask_bricks > MAX_CREATURE_OWNER_MASK_BRICKS
        || mask_cells > MAX_CREATURE_OWNER_MASK_CELLS
        || mask_cells > MAX_CREATURE_OWNER_SOLID_CELLS
    {
        return Err(CreatureLifecycleBulkError::WorkLimit);
    }
    let mut work = LifecycleAttachmentWork {
        decoded_bytes: 0,
        records: 1,
        bricks,
        dense_cells,
        solid_cells,
        masks: mask_bricks,
        mask_cells,
        overlay_poses: 0,
        part_velocities: record.part_velocities.len(),
        witness_poses: 0,
    };
    if let Some(overlay) = &commit.pre_motion {
        work.part_velocities += overlay.part_velocities.len();
        work.overlay_poses = overlay
            .parts
            .len()
            .checked_add(overlay.components.len())
            .ok_or(CreatureLifecycleBulkError::WorkLimit)?;
    }
    if let Some(witness) = &commit.death_witness {
        work.witness_poses = witness.frozen_local_poses.len();
    }
    Ok(work)
}

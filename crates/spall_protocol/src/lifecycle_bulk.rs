//! Typed framing for oversized lifecycle commits on the ordered bulk stream.
//!
//! This module defines the payload and bounded codec only. Network dispatch and
//! transaction release remain closed until simulation and journal consumers
//! validate and publish the complete attachment atomically.

use serde::{Deserialize, Serialize};
use spall_core::TransactionId;
use std::io::Read;

use crate::{
    CreatureLifecycleCommitV1, Hash32, MAX_CREATURE_OWNER_BRICKS, MAX_CREATURE_OWNER_COMPONENTS,
    MAX_CREATURE_OWNER_DENSE_CELLS, MAX_CREATURE_OWNER_MASK_BRICKS, MAX_CREATURE_OWNER_MASK_CELLS,
    MAX_CREATURE_OWNER_PARTS, MAX_CREATURE_OWNER_RECORD_BYTES, MAX_CREATURE_OWNER_SOLID_CELLS,
};

pub const CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA: u16 = 1;
pub const MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED: usize = MAX_CREATURE_OWNER_RECORD_BYTES + 64;

/// Correlated lifecycle payload carried as the compressed body of a world-version-6
/// BaselineBegin/BaselinePart/BaselineEnd transfer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreatureLifecycleBulkPayloadV1 {
    pub schema: u16,
    pub transaction_id: TransactionId,
    pub operation_ordinal: u32,
    pub commit_hash: Hash32,
    pub commit: CreatureLifecycleCommitV1,
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

impl CreatureLifecycleBulkPayloadV1 {
    pub fn new(
        transaction_id: TransactionId,
        operation_ordinal: u32,
        commit: CreatureLifecycleCommitV1,
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

    /// Bounded postcard + zstd representation used by the existing bulk stream.
    pub fn encode_compressed(&self) -> Result<Vec<u8>, CreatureLifecycleBulkError> {
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
        let compressed = zstd::stream::encode_all(raw.as_slice(), 0)
            .map_err(|e| CreatureLifecycleBulkError::Zstd(e.to_string()))?;
        if compressed.len() > crate::limits::MAX_ASSEMBLED_TRANSFER {
            return Err(CreatureLifecycleBulkError::CompressedLimit {
                actual: compressed.len(),
                limit: crate::limits::MAX_ASSEMBLED_TRANSFER,
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
        if bytes.len() > crate::limits::MAX_ASSEMBLED_TRANSFER {
            return Err(CreatureLifecycleBulkError::CompressedLimit {
                actual: bytes.len(),
                limit: crate::limits::MAX_ASSEMBLED_TRANSFER,
            });
        }
        let decoder = zstd::stream::Decoder::new(bytes)
            .map_err(|e| CreatureLifecycleBulkError::Zstd(e.to_string()))?;
        let mut raw = Vec::new();
        decoder
            .take(MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|e| CreatureLifecycleBulkError::Zstd(e.to_string()))?;
        if raw.len() > MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED {
            return Err(CreatureLifecycleBulkError::DecompressedLimit(
                MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED,
            ));
        }
        let (prefix, commit_bytes): (PayloadPrefixV1, &[u8]) = postcard::take_from_bytes(&raw)
            .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        if prefix.schema != CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA
            || prefix.transaction_id != transaction_id
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
        let (commit, rest): (CreatureLifecycleCommitV1, &[u8]) =
            postcard::take_from_bytes(commit_bytes)
                .map_err(|e| CreatureLifecycleBulkError::Postcard(e.to_string()))?;
        if !rest.is_empty() {
            return Err(CreatureLifecycleBulkError::Postcard(
                "trailing bytes after nested lifecycle commit".into(),
            ));
        }
        let payload = CreatureLifecycleBulkPayloadV1 {
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
        check_commit_work(&payload.commit)?;
        if payload.commit.canonical_hash() != payload.commit_hash {
            return Err(CreatureLifecycleBulkError::CommitHash);
        }
        Ok(payload)
    }
}

fn check_commit_work(commit: &CreatureLifecycleCommitV1) -> Result<(), CreatureLifecycleBulkError> {
    if commit.schema != crate::CREATURE_LIFECYCLE_COMMIT_SCHEMA
        || commit.change.schema != crate::CREATURE_CHANGE_SCHEMA
    {
        return Err(CreatureLifecycleBulkError::Schema);
    }
    if commit.pre_motion.as_ref().is_some_and(|motion| {
        motion.parts.len() > MAX_CREATURE_OWNER_PARTS
            || motion.components.len() > MAX_CREATURE_OWNER_COMPONENTS
    }) || commit
        .death_witness
        .as_ref()
        .is_some_and(|witness| witness.frozen_local_poses.len() > MAX_CREATURE_OWNER_PARTS)
    {
        return Err(CreatureLifecycleBulkError::WorkLimit);
    }
    let record = match &commit.change.operation {
        crate::CreatureLifecycleOperationV1::Birth { after }
        | crate::CreatureLifecycleOperationV1::Death { after } => after,
    };
    if record.schema != crate::CREATURE_OWNER_SCHEMA
        || record.state.schema != crate::CREATURE_STATE_SCHEMA
    {
        return Err(CreatureLifecycleBulkError::Schema);
    }
    if record.parts.len() > MAX_CREATURE_OWNER_PARTS
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
    Ok(())
}

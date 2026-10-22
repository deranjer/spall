//! Bounded helpers for geometry payloads nested in a transaction attachment.
//!
//! The preflight walks postcard's fixed `BaselineWorld` field order without
//! constructing vectors. It charges raw bytes and conservative owned vector
//! storage before the regular model decoder is allowed to allocate.

use std::io::Read;

use serde::Deserialize;
use spall_core::{EntityId, VolumeId};

use crate::{
    BaselineBrick, BaselineCells, BaselineVolume, BaselineWorld, TransactionAttachmentError,
};

pub(crate) fn decode_compressed(
    bytes: &[u8],
    remaining_budget: &mut usize,
) -> Result<BaselineWorld, TransactionAttachmentError> {
    if bytes.len() > crate::limits::MAX_ASSEMBLED_TRANSFER {
        return Err(TransactionAttachmentError::CompressedBudget);
    }
    // Limit logical decompressed bytes before reading, then charge the actual
    // retained Vec capacity (with a 2x logical floor) plus the eventual owned
    // BaselineWorld against the same aggregate budget used by encode.
    let cap = (*remaining_budget / 3).min(crate::limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED);
    let decoder = zstd::stream::Decoder::new(bytes)
        .map_err(|e| TransactionAttachmentError::Baseline(e.to_string()))?;
    let mut raw = Vec::new();
    decoder
        .take(cap as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| TransactionAttachmentError::Baseline(e.to_string()))?;
    if raw.len() > cap {
        return Err(TransactionAttachmentError::DecodedBudget);
    }
    let raw_capacity_cost = raw_buffer_cost(raw.len(), raw.capacity())?;
    let heap = preflight_raw(&raw, remaining_budget.saturating_sub(raw_capacity_cost))?;
    let charge = raw_capacity_cost
        .checked_add(heap)
        .ok_or(TransactionAttachmentError::DecodedBudget)?;
    *remaining_budget = remaining_budget
        .checked_sub(charge)
        .ok_or(TransactionAttachmentError::DecodedBudget)?;
    BaselineWorld::decode(&raw).map_err(|e| TransactionAttachmentError::Baseline(e.to_string()))
}

/// Preflight cost for an already-owned world on encode. The raw postcard size
/// and the same conservative in-memory vector estimate used by decode count
/// against the aggregate geometry budget.
pub(crate) fn preflight_world_size(
    world: &BaselineWorld,
    raw_size: usize,
    remaining: usize,
) -> Result<usize, TransactionAttachmentError> {
    let heap = preflight_world_storage(world, remaining)?;
    raw_size
        .checked_mul(2)
        .and_then(|raw_capacity| raw_capacity.checked_add(heap))
        .filter(|total| *total <= remaining)
        .ok_or(TransactionAttachmentError::DecodedBudget)
}

pub(crate) fn raw_buffer_cost(
    logical_len: usize,
    capacity: usize,
) -> Result<usize, TransactionAttachmentError> {
    logical_len
        .checked_mul(2)
        .map(|floor| capacity.max(floor))
        .ok_or(TransactionAttachmentError::DecodedBudget)
}

pub(crate) fn preflight_world_storage(
    world: &BaselineWorld,
    remaining: usize,
) -> Result<usize, TransactionAttachmentError> {
    if world.volumes.len() > crate::limits::MAX_TRANSACTION_OPS {
        return Err(TransactionAttachmentError::DecodedBudget);
    }
    let mut heap = std::mem::size_of::<BaselineWorld>();
    heap = charge_vec::<BaselineVolume>(heap, world.volumes.len(), remaining)?;
    let mut bricks = 0usize;
    let mut dense_cells = 0usize;
    for volume in &world.volumes {
        bricks = bricks
            .checked_add(volume.bricks.len())
            .ok_or(TransactionAttachmentError::DecodedBudget)?;
        heap = charge_vec::<BaselineBrick>(heap, volume.bricks.len(), remaining)?;
        for brick in &volume.bricks {
            if let BaselineCells::Dense(cells) = &brick.cells {
                if cells.len() != spall_core::CELLS_PER_BRICK {
                    return Err(TransactionAttachmentError::Baseline(
                        "dense brick has an invalid cell count".into(),
                    ));
                }
                dense_cells = dense_cells
                    .checked_add(cells.len())
                    .ok_or(TransactionAttachmentError::DecodedBudget)?;
                heap = charge_vec::<u16>(heap, cells.len(), remaining)?;
            }
        }
    }
    if bricks > crate::limits::MAX_BASELINE_REGIONS.saturating_mul(512)
        || dense_cells > crate::limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED / 2
    {
        return Err(TransactionAttachmentError::DecodedBudget);
    }
    Ok(heap)
}

fn preflight_raw(raw: &[u8], remaining: usize) -> Result<usize, TransactionAttachmentError> {
    let mut input = raw;
    let _schema: u16 = take(&mut input)?;
    let _tick: u64 = take(&mut input)?;
    let volume_count: usize = take(&mut input)?;
    if volume_count > crate::limits::MAX_TRANSACTION_OPS {
        return Err(TransactionAttachmentError::DecodedBudget);
    }
    let mut heap = std::mem::size_of::<BaselineWorld>();
    heap = charge_vec::<BaselineVolume>(heap, volume_count, remaining)?;
    let mut total_bricks = 0usize;
    let mut total_dense = 0usize;
    for _ in 0..volume_count {
        let _volume_id: VolumeId = take(&mut input)?;
        let _cell_size: u8 = take(&mut input)?;
        match take::<u32>(&mut input)? {
            0 => {}
            1 => {
                let _owner: EntityId = take(&mut input)?;
            }
            _ => {
                return Err(TransactionAttachmentError::Baseline(
                    "invalid owner tag".into(),
                ));
            }
        }
        match take::<u8>(&mut input)? {
            0 => {}
            1 => {
                for _ in 0..6 {
                    let _: i64 = take(&mut input)?;
                }
            }
            _ => {
                return Err(TransactionAttachmentError::Baseline(
                    "invalid bounds tag".into(),
                ));
            }
        }
        let brick_count: usize = take(&mut input)?;
        total_bricks = total_bricks
            .checked_add(brick_count)
            .ok_or(TransactionAttachmentError::DecodedBudget)?;
        if total_bricks > crate::limits::MAX_BASELINE_REGIONS.saturating_mul(512) {
            return Err(TransactionAttachmentError::DecodedBudget);
        }
        heap = charge_vec::<BaselineBrick>(heap, brick_count, remaining)?;
        for _ in 0..brick_count {
            for _ in 0..3 {
                let _: i64 = take(&mut input)?;
            }
            let _: u64 = take(&mut input)?;
            let _: bool = take(&mut input)?;
            match take::<u32>(&mut input)? {
                0 => {
                    let _: u16 = take(&mut input)?;
                }
                1 => {
                    let count: usize = take(&mut input)?;
                    if count != spall_core::CELLS_PER_BRICK {
                        return Err(TransactionAttachmentError::Baseline(
                            "dense brick has an invalid cell count".into(),
                        ));
                    }
                    total_dense = total_dense
                        .checked_add(count)
                        .ok_or(TransactionAttachmentError::DecodedBudget)?;
                    if total_dense > remaining / std::mem::size_of::<u16>() {
                        return Err(TransactionAttachmentError::DecodedBudget);
                    }
                    heap = charge_vec::<u16>(heap, count, remaining)?;
                    for _ in 0..count {
                        let _: u16 = take(&mut input)?;
                    }
                }
                2 => {
                    let _: [u8; 32] = take(&mut input)?;
                    let _: u32 = take(&mut input)?;
                }
                _ => {
                    return Err(TransactionAttachmentError::Baseline(
                        "invalid cells tag".into(),
                    ));
                }
            }
        }
    }
    if !input.is_empty() {
        return Err(TransactionAttachmentError::Baseline(
            "trailing bytes after baseline world".into(),
        ));
    }
    Ok(heap)
}

fn charge_vec<T>(
    current: usize,
    count: usize,
    remaining: usize,
) -> Result<usize, TransactionAttachmentError> {
    // Charge twice the element storage to conservatively include Vec capacity
    // growth and allocator rounding without relying on postcard size hints.
    let bytes = count
        .checked_mul(std::mem::size_of::<T>())
        .and_then(|n| n.checked_mul(2))
        .ok_or(TransactionAttachmentError::DecodedBudget)?;
    let next = current
        .checked_add(bytes)
        .ok_or(TransactionAttachmentError::DecodedBudget)?;
    if next > remaining {
        return Err(TransactionAttachmentError::DecodedBudget);
    }
    Ok(next)
}

fn take<'a, T: Deserialize<'a>>(input: &mut &'a [u8]) -> Result<T, TransactionAttachmentError> {
    let (value, rest) = postcard::take_from_bytes(input)
        .map_err(|e| TransactionAttachmentError::Baseline(e.to_string()))?;
    *input = rest;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_count_over_limit_rejects_before_reading_any_volume() {
        let mut raw = Vec::new();
        raw.extend(postcard::to_stdvec(&crate::WIRE_SCHEMA_VERSION).unwrap());
        raw.extend(postcard::to_stdvec(&12_u64).unwrap());
        raw.extend(postcard::to_stdvec(&(crate::limits::MAX_TRANSACTION_OPS + 1)).unwrap());
        assert!(matches!(
            preflight_raw(&raw, crate::limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED),
            Err(TransactionAttachmentError::DecodedBudget)
        ));
    }

    #[test]
    fn dense_length_over_limit_rejects_before_reading_any_cell() {
        let mut raw = Vec::new();
        raw.extend(postcard::to_stdvec(&crate::WIRE_SCHEMA_VERSION).unwrap());
        raw.extend(postcard::to_stdvec(&12_u64).unwrap());
        raw.extend(postcard::to_stdvec(&1_usize).unwrap());
        raw.extend(postcard::to_stdvec(&1_u64).unwrap()); // volume id
        raw.extend(postcard::to_stdvec(&0_u8).unwrap()); // cell-size code
        raw.extend(postcard::to_stdvec(&0_u32).unwrap()); // Terrain owner
        raw.extend(postcard::to_stdvec(&0_u8).unwrap()); // no bounds
        raw.extend(postcard::to_stdvec(&1_usize).unwrap()); // one brick
        for _ in 0..3 {
            raw.extend(postcard::to_stdvec(&0_i64).unwrap());
        }
        raw.extend(postcard::to_stdvec(&1_u64).unwrap()); // revision
        raw.extend(postcard::to_stdvec(&false).unwrap());
        raw.extend(postcard::to_stdvec(&1_u32).unwrap()); // Dense cells
        raw.extend(postcard::to_stdvec(&(spall_core::CELLS_PER_BRICK + 1)).unwrap());
        assert!(matches!(
            preflight_raw(&raw, crate::limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED),
            Err(TransactionAttachmentError::Baseline(_))
        ));
    }
}

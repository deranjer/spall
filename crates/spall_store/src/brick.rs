//! Compressed brick payloads.
//!
//! A resident brick's authoritative material layer is `32768` `u16` ids. A
//! uniform brick (the common case — untouched terrain, or a mined-out
//! tombstone) stores a single id; anything else stores a bounded zstd frame
//! over the little-endian id array. Decompression is always capped at exactly
//! one brick so a corrupt or hostile row cannot force a large allocation
//! (`docs/protocol.md`: "decompress with output bounds").

use spall_core::CELLS_PER_BRICK;

use crate::dto::{BrickPayload, MAX_STORED_BRICK_BYTES};

/// Bytes in a fully dense brick material layer (`32768 * u16`).
pub const DENSE_CELL_BYTES: usize = CELLS_PER_BRICK * 2;

/// zstd level for brick frames. Level 3 is the zstd default: fast, and terrain
/// material ids compress well regardless.
const ZSTD_LEVEL: i32 = 3;

/// Why a brick payload could not be encoded or decoded.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BrickCodecError {
    #[error("compressed brick payload is {0} bytes, over the {cap}-byte cap", cap = MAX_STORED_BRICK_BYTES)]
    TooLarge(usize),
    #[error("zstd codec failure: {0}")]
    Zstd(String),
    #[error("decompressed brick is {got} bytes, expected {expected}", expected = DENSE_CELL_BYTES)]
    BadLength { got: usize },
}

/// Encode a brick's `32768` material ids, choosing `Uniform` when every cell is
/// equal and a zstd frame otherwise.
pub fn encode_cells(cells: &[u16]) -> Result<BrickPayload, BrickCodecError> {
    assert_eq!(
        cells.len(),
        CELLS_PER_BRICK,
        "a brick layer is exactly {CELLS_PER_BRICK} cells"
    );
    let first = cells[0];
    if cells.iter().all(|&c| c == first) {
        return Ok(BrickPayload::Uniform(first));
    }
    let mut raw = Vec::with_capacity(DENSE_CELL_BYTES);
    for &c in cells {
        raw.extend_from_slice(&c.to_le_bytes());
    }
    let frame =
        zstd::bulk::compress(&raw, ZSTD_LEVEL).map_err(|e| BrickCodecError::Zstd(e.to_string()))?;
    if frame.len() > MAX_STORED_BRICK_BYTES {
        return Err(BrickCodecError::TooLarge(frame.len()));
    }
    Ok(BrickPayload::DenseZstd(frame))
}

/// Decode a brick payload back to `32768` material ids, with a hard output
/// bound of one brick.
pub fn decode_cells(payload: &BrickPayload) -> Result<Vec<u16>, BrickCodecError> {
    match payload {
        BrickPayload::Uniform(id) => Ok(vec![*id; CELLS_PER_BRICK]),
        BrickPayload::DenseZstd(frame) => {
            if frame.len() > MAX_STORED_BRICK_BYTES {
                return Err(BrickCodecError::TooLarge(frame.len()));
            }
            let raw = zstd::bulk::decompress(frame, DENSE_CELL_BYTES)
                .map_err(|e| BrickCodecError::Zstd(e.to_string()))?;
            if raw.len() != DENSE_CELL_BYTES {
                return Err(BrickCodecError::BadLength { got: raw.len() });
            }
            Ok(raw
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_layer_stays_uniform() {
        let cells = vec![7u16; CELLS_PER_BRICK];
        assert_eq!(encode_cells(&cells).unwrap(), BrickPayload::Uniform(7));
        assert_eq!(decode_cells(&BrickPayload::Uniform(7)).unwrap(), cells);
    }

    #[test]
    fn dense_layer_round_trips() {
        let mut cells = vec![1u16; CELLS_PER_BRICK];
        for (i, c) in cells.iter_mut().enumerate() {
            *c = (i % 5) as u16;
        }
        let payload = encode_cells(&cells).unwrap();
        assert!(matches!(payload, BrickPayload::DenseZstd(_)));
        assert_eq!(decode_cells(&payload).unwrap(), cells);
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        let junk = vec![0u8; MAX_STORED_BRICK_BYTES + 1];
        assert_eq!(
            decode_cells(&BrickPayload::DenseZstd(junk)),
            Err(BrickCodecError::TooLarge(MAX_STORED_BRICK_BYTES + 1))
        );
    }

    #[test]
    fn corrupt_frame_is_a_clean_error() {
        let bad = BrickPayload::DenseZstd(vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(matches!(decode_cells(&bad), Err(BrickCodecError::Zstd(_))));
    }
}

//! The authoritative geometry payload carried by a baseline transfer (T17).
//!
//! `docs/protocol.md`: "Initial baselines transfer authoritative terrain rather
//! than trusting client-side generation to be bit-identical" and a late join
//! must "obtain a consistent current world" without "replaying all edits since
//! world creation". The [`BaselinePart`](crate::BaselinePart) stream therefore
//! carries the postcard bytes of one [`BaselineWorld`]: every resident brick of
//! every volume with its **authoritative revision and material layer**, so a
//! late-joining replica reconstructs the exact canonical topology hash — not an
//! approximation a later transaction would still have to repair.
//!
//! Motion (pose / velocity / sleep) is deliberately *not* here. Once the
//! catch-up barrier is reached the server sends a current
//! [`MotionSnapshot`](crate::MotionSnapshot) keyframe per body, exactly as
//! `docs/protocol.md` step 4 of "Late join" describes.

use serde::{Deserialize, Serialize};
use spall_core::{CELLS_PER_BRICK, EntityId, VolumeId};

/// Schema version of the [`BaselineWorld`] payload. Independent of the wire
/// record schema and the save schema (`docs/protocol.md`: "Version the wire
/// schema independently of the world save schema").
pub const BASELINE_WORLD_SCHEMA: u16 = 1;

/// Who owns a baseline volume's cells: a serializable twin of
/// [`crate::CanonicalOwner`] (which is a pure hashing type and intentionally not
/// `Serialize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BaselineOwner {
    Terrain,
    Body(EntityId),
}

/// One brick's authoritative material layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BaselineCells {
    /// Every cell holds this raw material id (air included).
    Uniform(u16),
    /// Exactly [`CELLS_PER_BRICK`] raw material ids in linear-index order.
    Dense(Vec<u16>),
}

/// One persisted brick of a baseline volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineBrick {
    pub coord: [i64; 3],
    /// The authoritative brick revision — carried so the canonical topology
    /// hash (which includes revision) matches exactly after install.
    pub revision: u64,
    /// The modified-air tombstone flag.
    pub edited: bool,
    pub cells: BaselineCells,
}

/// One volume in a baseline: terrain or a detached body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineVolume {
    pub volume_id: VolumeId,
    /// `spall_core::CellSizeCode as u8`.
    pub cell_size_code: u8,
    pub owner: BaselineOwner,
    /// Inclusive brick-coordinate bounds `[min, max]` when the volume is
    /// bounded (every split-off child volume is); `None` for an unbounded grid.
    pub bounds: Option<[[i64; 3]; 2]>,
    /// Every resident brick, ascending by `(z, y, x)`.
    pub bricks: Vec<BaselineBrick>,
}

/// A whole authoritative world's geometry at one immutable tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineWorld {
    pub schema: u16,
    /// The server tick this snapshot is coherent with.
    pub checkpoint_tick: u64,
    /// Volumes ascending by [`VolumeId`]; the terrain volume is first.
    pub volumes: Vec<BaselineVolume>,
}

/// Why a [`BaselineWorld`] blob could not be decoded / trusted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BaselineDecodeError {
    #[error("postcard: {0}")]
    Postcard(String),
    #[error("baseline world schema {0} is not supported")]
    Schema(u16),
    #[error("dense brick carries {0} cells, expected {CELLS_PER_BRICK}")]
    BrickLen(usize),
    #[error("baseline volume {0} has no bricks")]
    EmptyVolume(u64),
    #[error("baseline volumes are not sorted / unique by id")]
    Unsorted,
}

impl BaselineWorld {
    /// Postcard bytes for the transfer payload.
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("BaselineWorld serializes")
    }

    /// Decode + validate a reassembled transfer payload.
    pub fn decode(bytes: &[u8]) -> Result<Self, BaselineDecodeError> {
        let world: Self = postcard::from_bytes(bytes)
            .map_err(|e| BaselineDecodeError::Postcard(e.to_string()))?;
        world.validate()?;
        Ok(world)
    }

    /// Structural checks run on every decode: schema, dense-brick length, one
    /// brick minimum per volume, and the volume sort order the canonical hash
    /// assumes.
    pub fn validate(&self) -> Result<(), BaselineDecodeError> {
        if self.schema != BASELINE_WORLD_SCHEMA {
            return Err(BaselineDecodeError::Schema(self.schema));
        }
        let mut last_id = 0u64;
        for v in &self.volumes {
            if v.volume_id.get() <= last_id {
                return Err(BaselineDecodeError::Unsorted);
            }
            last_id = v.volume_id.get();
            if v.bricks.is_empty() {
                return Err(BaselineDecodeError::EmptyVolume(v.volume_id.get()));
            }
            for b in &v.bricks {
                if let BaselineCells::Dense(cells) = &b.cells
                    && cells.len() != CELLS_PER_BRICK
                {
                    return Err(BaselineDecodeError::BrickLen(cells.len()));
                }
            }
        }
        Ok(())
    }

    /// Total resident brick count across every volume (a memory-bound helper for
    /// the transfer/catch-up limits).
    pub fn brick_count(&self) -> usize {
        self.volumes.iter().map(|v| v.bricks.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_world() -> BaselineWorld {
        BaselineWorld {
            schema: BASELINE_WORLD_SCHEMA,
            checkpoint_tick: 42,
            volumes: vec![
                BaselineVolume {
                    volume_id: VolumeId::new(1).unwrap(),
                    cell_size_code: 2,
                    owner: BaselineOwner::Terrain,
                    bounds: None,
                    bricks: vec![BaselineBrick {
                        coord: [-1, 0, 2],
                        revision: 7,
                        edited: true,
                        cells: BaselineCells::Uniform(0),
                    }],
                },
                BaselineVolume {
                    volume_id: VolumeId::new(2).unwrap(),
                    cell_size_code: 2,
                    owner: BaselineOwner::Body(EntityId::new(5).unwrap()),
                    bounds: Some([[0, 0, 0], [0, 0, 0]]),
                    bricks: vec![BaselineBrick {
                        coord: [0, 0, 0],
                        revision: 1,
                        edited: false,
                        cells: BaselineCells::Dense(vec![1u16; CELLS_PER_BRICK]),
                    }],
                },
            ],
        }
    }

    #[test]
    fn baseline_world_round_trips() {
        let world = a_world();
        let bytes = world.encode();
        assert_eq!(BaselineWorld::decode(&bytes).unwrap(), world);
    }

    #[test]
    fn a_short_dense_brick_is_rejected() {
        let mut world = a_world();
        world.volumes[1].bricks[0].cells = BaselineCells::Dense(vec![0u16; 10]);
        let bytes = world.encode();
        assert!(matches!(
            BaselineWorld::decode(&bytes),
            Err(BaselineDecodeError::BrickLen(10))
        ));
    }

    #[test]
    fn out_of_order_volumes_are_rejected() {
        let mut world = a_world();
        world.volumes.reverse();
        let bytes = world.encode();
        assert!(matches!(
            BaselineWorld::decode(&bytes),
            Err(BaselineDecodeError::Unsorted)
        ));
    }
}

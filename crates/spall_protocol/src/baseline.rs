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
use spall_core::{CELLS_PER_BRICK, EntityId, MaterialId, VolumeId};

use crate::limits::{MAX_SPLIT_BASELINE_BLOB, MAX_SPLIT_BASELINE_DECOMPRESSED};

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
    #[error("split baseline blob is {bytes} compressed bytes; the cap is {cap}")]
    BlobTooLarge { bytes: usize, cap: usize },
    #[error("zstd: {0}")]
    Zstd(String),
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
            v.validate()?;
        }
        Ok(())
    }

    /// Total resident brick count across every volume (a memory-bound helper for
    /// the transfer/catch-up limits).
    pub fn brick_count(&self) -> usize {
        self.volumes.iter().map(|v| v.bricks.len()).sum()
    }

    /// zstd-compressed postcard bytes — the form a giant bulk split's geometry
    /// is stored in a `spall_store` `TopologyBulkSplit` journal payload (T17
    /// increment 2). Panics only on an internal encoder error.
    pub fn encode_compressed(&self) -> Vec<u8> {
        zstd::stream::encode_all(self.encode().as_slice(), 0).expect("zstd encodes")
    }

    /// Decompress + decode + validate. Bounded by
    /// [`crate::limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED`] — a DoS bound on
    /// the decompressed size, independent of the *compressed* wire-transfer
    /// cap ([`crate::limits::MAX_ASSEMBLED_TRANSFER`]) `transfer_from_world`
    /// enforces on what actually crossed the wire.
    pub fn decode_compressed(bytes: &[u8]) -> Result<Self, BaselineDecodeError> {
        use std::io::Read;
        let decoder = zstd::stream::Decoder::new(bytes)
            .map_err(|e| BaselineDecodeError::Zstd(e.to_string()))?;
        let mut raw = Vec::new();
        decoder
            .take(crate::limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|e| BaselineDecodeError::Zstd(e.to_string()))?;
        if raw.len() > crate::limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED {
            return Err(BaselineDecodeError::BlobTooLarge {
                bytes: raw.len(),
                cap: crate::limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED,
            });
        }
        Self::decode(&raw)
    }
}

impl BaselineVolume {
    /// Structural checks for one volume: at least one brick, and every `Dense`
    /// brick exactly [`CELLS_PER_BRICK`] long.
    pub fn validate(&self) -> Result<(), BaselineDecodeError> {
        if self.bricks.is_empty() {
            return Err(BaselineDecodeError::EmptyVolume(self.volume_id.get()));
        }
        for b in &self.bricks {
            if let BaselineCells::Dense(cells) = &b.cells
                && cells.len() != CELLS_PER_BRICK
            {
                return Err(BaselineDecodeError::BrickLen(cells.len()));
            }
        }
        Ok(())
    }

    /// Every distinct material id referenced by this volume's bricks — used to
    /// validate a split baseline op blob against the world manifest.
    pub fn material_ids(&self) -> impl Iterator<Item = MaterialId> + '_ {
        self.bricks.iter().flat_map(|b| match &b.cells {
            BaselineCells::Uniform(id) => vec![MaterialId(*id)],
            BaselineCells::Dense(ids) => {
                let mut seen: Vec<u16> = ids.to_vec();
                seen.sort_unstable();
                seen.dedup();
                seen.into_iter().map(MaterialId).collect()
            }
        })
    }

    /// zstd-compressed postcard bytes for a `SplitOffBaseline` /
    /// `SourcePatchBaseline` op blob. Panics only on an internal encoder error
    /// (the input is a plain owned struct).
    pub fn encode_compressed(&self) -> Vec<u8> {
        let raw = postcard::to_stdvec(self).expect("BaselineVolume serializes");
        zstd::stream::encode_all(raw.as_slice(), 0).expect("zstd encodes")
    }

    /// Decompress + decode + validate a split baseline op blob. Enforces the
    /// compressed cap ([`MAX_SPLIT_BASELINE_BLOB`]) and a decompressed DoS bound
    /// ([`MAX_SPLIT_BASELINE_DECOMPRESSED`]) before trusting the payload.
    pub fn decode_compressed(bytes: &[u8]) -> Result<Self, BaselineDecodeError> {
        use std::io::Read;
        if bytes.len() > MAX_SPLIT_BASELINE_BLOB {
            return Err(BaselineDecodeError::BlobTooLarge {
                bytes: bytes.len(),
                cap: MAX_SPLIT_BASELINE_BLOB,
            });
        }
        // Bounded decode: stop reading decompressed output at the DoS cap + 1 so
        // a compression bomb can never allocate past the limit.
        let decoder = zstd::stream::Decoder::new(bytes)
            .map_err(|e| BaselineDecodeError::Zstd(e.to_string()))?;
        let mut raw = Vec::new();
        decoder
            .take(MAX_SPLIT_BASELINE_DECOMPRESSED as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|e| BaselineDecodeError::Zstd(e.to_string()))?;
        if raw.len() > MAX_SPLIT_BASELINE_DECOMPRESSED {
            return Err(BaselineDecodeError::BlobTooLarge {
                bytes: raw.len(),
                cap: MAX_SPLIT_BASELINE_DECOMPRESSED,
            });
        }
        let volume: Self =
            postcard::from_bytes(&raw).map_err(|e| BaselineDecodeError::Postcard(e.to_string()))?;
        volume.validate()?;
        Ok(volume)
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

    fn a_volume() -> BaselineVolume {
        BaselineVolume {
            volume_id: VolumeId::new(7).unwrap(),
            cell_size_code: 2,
            owner: BaselineOwner::Body(EntityId::new(9).unwrap()),
            bounds: Some([[0, 0, 0], [1, 1, 1]]),
            bricks: vec![
                BaselineBrick {
                    coord: [0, 0, 0],
                    revision: 3,
                    edited: true,
                    cells: BaselineCells::Uniform(1),
                },
                BaselineBrick {
                    coord: [1, 1, 1],
                    revision: 4,
                    edited: true,
                    cells: BaselineCells::Dense({
                        let mut c = vec![0u16; CELLS_PER_BRICK];
                        c[0] = 2;
                        c
                    }),
                },
            ],
        }
    }

    #[test]
    fn split_baseline_volume_compressed_round_trips() {
        let v = a_volume();
        let blob = v.encode_compressed();
        assert!(
            blob.len() < MAX_SPLIT_BASELINE_BLOB,
            "sparse volume compresses small"
        );
        assert_eq!(BaselineVolume::decode_compressed(&blob).unwrap(), v);

        let ids: Vec<u16> = v.material_ids().map(|m| m.raw()).collect();
        assert!(ids.contains(&0) && ids.contains(&1) && ids.contains(&2));
    }

    #[test]
    fn split_baseline_blob_rejects_garbage_and_over_cap() {
        assert!(matches!(
            BaselineVolume::decode_compressed(&[0xAB; 32]),
            Err(BaselineDecodeError::Zstd(_))
        ));
        assert!(matches!(
            BaselineVolume::decode_compressed(&vec![0u8; MAX_SPLIT_BASELINE_BLOB + 1]),
            Err(BaselineDecodeError::BlobTooLarge { .. })
        ));

        // A structurally-invalid volume (empty bricks) is refused on decode.
        let mut v = a_volume();
        v.bricks.clear();
        let blob = v.encode_compressed();
        assert!(matches!(
            BaselineVolume::decode_compressed(&blob),
            Err(BaselineDecodeError::EmptyVolume(7))
        ));
    }
}

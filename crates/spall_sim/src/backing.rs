//! T23 / G3 row 7, slice C — a durable brick source for reloading evicted
//! geometry.
//!
//! [`SimWorld`](crate::SimWorld) keeps a `(revision, content_hash)` digest for
//! every brick it evicts (slice B), enough to hash and account for it but not
//! to *edit* it. When a staged / committing edit needs an evicted brick's
//! cells, the pipeline asks a [`BrickBacking`] for them, reinstalls the brick,
//! verifies it against the retained digest, and retries the edit.
//!
//! The trait is pure — an implementation does the I/O. `spall_server` bridges
//! its residency backing to this in the serve-loop wiring (slice D); tests use
//! [`MemoryBacking`].

use std::collections::{BTreeMap, BTreeSet};

use spall_core::{BrickCoord, CELLS_PER_BRICK, LocalCell, MaterialId, Revision, VolumeId};
use spall_voxel::{Brick, Volume};

/// One brick as offered by the backing.
#[derive(Debug, Clone)]
pub enum BackingBrick {
    /// The durable brick, ready to reinstall.
    Loaded(Brick),
    /// The slot is durably known to hold no geometry — a modified-air
    /// tombstone. Reinstall an air brick at the retained revision / edited
    /// flag; this adds no *new* canonical brick because the digest already
    /// counts one here.
    KnownEmpty { revision: Revision, edited: bool },
    /// No durable record. The reload cannot proceed; the edit is rejected with
    /// a bounded explicit failure.
    Unavailable,
}

/// A durable source of authoritative brick geometry, keyed by `(volume, brick
/// coord)`.
pub trait BrickBacking: Send + Sync {
    fn load(&self, volume: VolumeId, coord: BrickCoord) -> BackingBrick;
}

/// In-memory backing for fixtures and tests: an owned [`Brick`] per key.
#[derive(Debug, Default, Clone)]
pub struct MemoryBacking {
    bricks: BTreeMap<(u64, i64, i64, i64), Brick>,
    known_empty: BTreeMap<(u64, i64, i64, i64), (Revision, bool)>,
    unavailable: BTreeSet<(u64, i64, i64, i64)>,
}

fn key(volume: VolumeId, coord: BrickCoord) -> (u64, i64, i64, i64) {
    (volume.get(), coord.x, coord.y, coord.z)
}

impl MemoryBacking {
    /// Captures every resident brick of `volume` as a reload record — the
    /// durable state a checkpoint would hold.
    pub fn from_volume(volume: &Volume) -> Self {
        let mut backing = Self::default();
        for coord in volume.resident_brick_coords() {
            let snap = volume
                .snapshot_brick(coord)
                .ok()
                .flatten()
                .expect("coord came from the resident set");
            let cells: Vec<MaterialId> = (0..CELLS_PER_BRICK as u16)
                .map(|i| snap.get(LocalCell::from_linear_index(i).expect("i < 32768")))
                .collect();
            backing.bricks.insert(
                key(volume.id(), coord),
                Brick::restored(&cells, snap.revision(), snap.is_edited()),
            );
        }
        backing
    }

    pub fn insert(&mut self, volume: VolumeId, coord: BrickCoord, brick: Brick) {
        self.bricks.insert(key(volume, coord), brick);
    }

    pub fn mark_known_empty(
        &mut self,
        volume: VolumeId,
        coord: BrickCoord,
        rev: Revision,
        edited: bool,
    ) {
        self.bricks.remove(&key(volume, coord));
        self.known_empty.insert(key(volume, coord), (rev, edited));
    }

    /// Force `load` to report `Unavailable` for this key (a lost / corrupt
    /// durable record).
    pub fn mark_unavailable(&mut self, volume: VolumeId, coord: BrickCoord) {
        self.unavailable.insert(key(volume, coord));
    }
}

impl BrickBacking for MemoryBacking {
    fn load(&self, volume: VolumeId, coord: BrickCoord) -> BackingBrick {
        let k = key(volume, coord);
        if self.unavailable.contains(&k) {
            return BackingBrick::Unavailable;
        }
        if let Some(brick) = self.bricks.get(&k) {
            return BackingBrick::Loaded(brick.clone());
        }
        if let Some(&(revision, edited)) = self.known_empty.get(&k) {
            return BackingBrick::KnownEmpty { revision, edited };
        }
        BackingBrick::Unavailable
    }
}

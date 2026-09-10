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
use std::sync::Mutex;

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

type Key = (u64, i64, i64, i64);

fn key(volume: VolumeId, coord: BrickCoord) -> Key {
    (volume.get(), coord.x, coord.y, coord.z)
}

#[derive(Debug, Default)]
struct MemoryBackingInner {
    bricks: BTreeMap<Key, Brick>,
    known_empty: BTreeMap<Key, (Revision, bool)>,
    unavailable: BTreeSet<Key>,
}

/// In-memory brick source for fixtures, tests, and the default-off serve
/// residency pass. Interior-mutable so the pass (which updates it on every
/// commit) and [`SimWorld`](crate::SimWorld) (which reads it on reload) can
/// share one `Arc`.
#[derive(Debug, Default)]
pub struct MemoryBacking {
    inner: Mutex<MemoryBackingInner>,
}

impl MemoryBacking {
    /// Captures every resident brick of `volume` as a reload record — the
    /// durable state a checkpoint would hold.
    pub fn from_volume(volume: &Volume) -> Self {
        let backing = Self::default();
        for coord in volume.resident_brick_coords() {
            backing.capture(volume, coord);
        }
        backing
    }

    /// Records `volume`'s current geometry at `coord` (must be resident). Called
    /// after every committed edit so a later reload gets the current revision.
    pub fn capture(&self, volume: &Volume, coord: BrickCoord) {
        let Ok(Some(snap)) = volume.snapshot_brick(coord) else {
            return;
        };
        let cells: Vec<MaterialId> = (0..CELLS_PER_BRICK as u16)
            .map(|i| snap.get(LocalCell::from_linear_index(i).expect("i < 32768")))
            .collect();
        self.insert(
            volume.id(),
            coord,
            Brick::restored(&cells, snap.revision(), snap.is_edited()),
        );
    }

    pub fn insert(&self, volume: VolumeId, coord: BrickCoord, brick: Brick) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.unavailable.remove(&key(volume, coord));
        g.known_empty.remove(&key(volume, coord));
        g.bricks.insert(key(volume, coord), brick);
    }

    pub fn mark_known_empty(
        &self,
        volume: VolumeId,
        coord: BrickCoord,
        rev: Revision,
        edited: bool,
    ) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.bricks.remove(&key(volume, coord));
        g.known_empty.insert(key(volume, coord), (rev, edited));
    }

    /// Force `load` to report `Unavailable` for this key (a lost / corrupt
    /// durable record).
    pub fn mark_unavailable(&self, volume: VolumeId, coord: BrickCoord) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unavailable
            .insert(key(volume, coord));
    }
}

impl BrickBacking for MemoryBacking {
    fn load(&self, volume: VolumeId, coord: BrickCoord) -> BackingBrick {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let k = key(volume, coord);
        if g.unavailable.contains(&k) {
            return BackingBrick::Unavailable;
        }
        if let Some(brick) = g.bricks.get(&k) {
            return BackingBrick::Loaded(brick.clone());
        }
        if let Some(&(revision, edited)) = g.known_empty.get(&k) {
            return BackingBrick::KnownEmpty { revision, edited };
        }
        BackingBrick::Unavailable
    }
}

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

/// A [`BrickBacking`] that can also be written to -- the "ack-before-evict"
/// half of the T23/G3 row-7 contract (`docs/reports/G3.md` increment 27):
/// `ResidencyPass` captures a brick's exact current geometry through this
/// trait immediately before evicting it, and again on every committed edit
/// that touches it, so a later `load` always returns the right revision.
///
/// This is also this repo's one concrete step of row 7's residency-mechanism
/// unification (still open otherwise -- see `docs/reports/G3.md`): both
/// `MemoryBacking` (in-process, the historical default) and a real
/// disk-backed store (`spall_server::disk_backing::DiskBrickBacking`) satisfy
/// the same trait, so `ResidencyPass`/`SimWorld` are written once against it
/// and do not care which is installed. `ResidencyPass::install` keeps using
/// `MemoryBacking`; `ResidencyPass::install_with_backing` accepts any
/// `Arc<dyn BrickBackingWriter>`.
pub trait BrickBackingWriter: BrickBacking {
    /// Captures `volume`'s current geometry at `coord` (must be resident) as
    /// a reload record. `true` if the backing now reflects the live brick;
    /// `false` if it was not resident to snapshot, or a fault was injected.
    fn capture(&self, volume: &Volume, coord: BrickCoord) -> bool;

    /// The backing's on-disk footprint in bytes, for a backing that is
    /// actually disk-resident (T23/G3 row 7 item 3's durable-side memory
    /// evidence). `None` by default -- `MemoryBacking` has nothing on disk;
    /// `spall_server::disk_backing::DiskBrickBacking` overrides this.
    fn disk_bytes(&self) -> Option<u64> {
        None
    }

    /// T23 / G3 row 7 (ENG-30 row 7 increment 13): the backing's own retained
    /// **in-process memory** in bytes, distinct from [`Self::disk_bytes`] —
    /// the frozen contract's "backing memory" line item
    /// (`docs/reports/G3-residency-hash.md`), which the previous
    /// `resident_dense_bytes_*` figure did not cover because it only walked
    /// the *live* `SimWorld`, not the durable backing's own captured copies.
    /// `None` when the backing keeps nothing resident in this process (a pure
    /// disk-backed store with no cache); `MemoryBacking` overrides this with
    /// its real captured-brick payload total.
    fn resident_bytes(&self) -> Option<u64> {
        None
    }
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
    /// T23 / G3 row 7 (ack-before-evict coverage): keys whose *next* `capture`
    /// call must fail, as if the brick had unexpectedly stopped being
    /// resident. One-shot -- consumed by the failing call. Test-only fault
    /// injection; empty in every non-test run.
    poisoned_captures: BTreeSet<Key>,
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
    /// after every committed edit so a later reload gets the current revision,
    /// and (T23 / G3 row 7) again, gated, immediately before the residency
    /// pass evicts the brick -- ack-before-evict. `true` if the record now
    /// reflects the live brick; `false` (nothing written) if the brick was not
    /// resident to snapshot, or a test poisoned this call via
    /// [`Self::fail_next_capture`].
    pub fn capture(&self, volume: &Volume, coord: BrickCoord) -> bool {
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if g.poisoned_captures.remove(&key(volume.id(), coord)) {
                return false;
            }
        }
        let Ok(Some(snap)) = volume.snapshot_brick(coord) else {
            return false;
        };
        let cells: Vec<MaterialId> = (0..CELLS_PER_BRICK as u16)
            .map(|i| snap.get(LocalCell::from_linear_index(i).expect("i < 32768")))
            .collect();
        self.insert(
            volume.id(),
            coord,
            Brick::restored(&cells, snap.revision(), snap.is_edited()),
        );
        true
    }

    /// T23 / G3 row 7 test-only fault injection: makes the *next* [`Self::capture`]
    /// call for `(volume, coord)` fail, as if the brick had unexpectedly
    /// stopped being resident, without touching any currently-held record.
    /// One-shot; a later `capture` for the same key succeeds normally. `pub`
    /// (not `#[cfg(test)]`) so an integration test in another crate can drive
    /// it, matching this workspace's `spall_store::inject` convention.
    #[doc(hidden)]
    pub fn fail_next_capture(&self, volume: VolumeId, coord: BrickCoord) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .poisoned_captures
            .insert(key(volume, coord));
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

impl BrickBackingWriter for MemoryBacking {
    /// Delegates to the inherent [`MemoryBacking::capture`] (inherent methods
    /// take priority over a trait method of the same name/signature, so this
    /// is not recursive) -- kept so every existing call site that already
    /// holds a concrete `MemoryBacking`/`Arc<MemoryBacking>` and calls
    /// `.capture(...)` directly compiles unchanged.
    fn capture(&self, volume: &Volume, coord: BrickCoord) -> bool {
        MemoryBacking::capture(self, volume, coord)
    }

    /// Sum of every captured `Dense` brick's material-layer payload
    /// (`spall_voxel::brick::DENSE_LAYER_BYTES` each; `Uniform` bricks cost
    /// metadata only, matching `spall_voxel::MemoryReport`'s convention). This
    /// is the real bytes this in-process backing keeps alive, independent of
    /// whether the same brick is *also* resident in the live `SimWorld`.
    fn resident_bytes(&self) -> Option<u64> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Some(
            g.bricks
                .values()
                .filter(|b| b.is_dense())
                .map(|_| spall_voxel::MemoryReport::DENSE_BRICK_BYTES as u64)
                .sum(),
        )
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

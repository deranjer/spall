//! Cache-placement-independent logical topology (T23 / G3 row 7, slice A).
//!
//! A volume's **logical brick set** is its resident bricks plus the bricks that
//! have been evicted from the live cache but whose exact `(revision,
//! content_hash)` was retained. Every logical key `(VolumeId, BrickCoord)`
//! contributes exactly once, and a missing cache entry is *neither* a deletion
//! *nor* proof of air (see [`Volume::evict_brick`] — "absence is not air").
//!
//! [`logical_bricks`] yields that set as `(coord, revision, content_hash)`
//! triples in canonical `(z, y, x)` order, so the simulation commit path and the
//! client candidate validator can build the same `spall_protocol` canonical
//! records they build today from resident snapshots — without importing the
//! server, and without the value changing when cache contents differ.
//!
//! This module is the digest record and its lifecycle only. Cache policy,
//! durable acknowledgement, and I/O stay in `spall_server`; canonical encoding
//! and hashing stay in `spall_protocol`.

use crate::chunk_store::{ChunkKey, ChunkStore};

use spall_core::{BrickCoord, CELLS_PER_BRICK, LocalCell, Revision};

use crate::brick::{Brick, BrickHash};
use crate::volume::{AccessError, Volume};

/// The exact per-brick topology contribution retained when geometry leaves the
/// live cache. Reproduces the brick's part of the canonical topology hash and
/// of whole-world solid-cell conservation without holding its cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrickDigest {
    /// The durable revision the digest was captured at.
    pub revision: Revision,
    /// BLAKE3 content hash — the same bytes the canonical topology hash folds.
    pub content_hash: BrickHash,
    /// Solid (non-air) cells — conservation metadata (`0` for a modified-air
    /// tombstone).
    pub solid_cells: u32,
    /// `true` when the retained brick is an edited-to-air record, not a natural
    /// empty. It still occupies a logical brick slot and must not be dropped to
    /// make a hash match.
    pub modified_air: bool,
}

impl BrickDigest {
    /// Captures the digest of one **resident** brick of `volume`. `Err` if the
    /// coord is not resident — a digest is taken from real geometry, never
    /// guessed.
    pub fn capture(volume: &Volume, coord: BrickCoord) -> Result<Self, DigestError> {
        let snap = volume
            .snapshot_brick(coord)?
            .ok_or(DigestError::NotResident(coord))?;
        let mut solid_cells = 0u32;
        for index in 0..CELLS_PER_BRICK as u16 {
            let cell = LocalCell::from_linear_index(index).expect("index < CELLS_PER_BRICK");
            if !snap.get(cell).is_air() {
                solid_cells += 1;
            }
        }
        Ok(Self {
            revision: snap.revision(),
            content_hash: snap.content_hash(),
            solid_cells,
            modified_air: snap.is_modified_air(),
        })
    }

    /// Captures a digest from a candidate brick before it is published into a
    /// volume. Reload paths use this to validate backing data without briefly
    /// creating an invalid resident-and-evicted logical membership.
    pub fn capture_brick(brick: &Brick) -> Self {
        let mut solid_cells = 0u32;
        for index in 0..CELLS_PER_BRICK as u16 {
            let cell = LocalCell::from_linear_index(index).expect("index < CELLS_PER_BRICK");
            if !brick.get(cell).is_air() {
                solid_cells += 1;
            }
        }
        Self {
            revision: brick.revision(),
            content_hash: brick.content_hash(),
            solid_cells,
            modified_air: brick.is_modified_air(),
        }
    }
}

/// One resolved logical brick — the topology contribution of a `(coord,
/// revision, content_hash)` triple, whether its cells are resident or only
/// digested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalBrick {
    pub coord: BrickCoord,
    pub revision: Revision,
    pub content_hash: BrickHash,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DigestError {
    #[error("brick access: {0}")]
    Access(#[from] AccessError),
    #[error("brick {0:?} is not resident; a digest must come from real geometry")]
    NotResident(BrickCoord),
    #[error("logical conflict: brick {0:?} is both resident and evicted")]
    ResidentEvictedConflict(BrickCoord),
    /// The caller's configuration cannot support eviction of this volume (for example terrain
    /// with per-brick physics colliders, which have no residency lifecycle).
    #[error("eviction unsupported: {0}")]
    Unsupported(&'static str),
    #[error(
        "brick {coord:?} digest conflict: retained rev {retained:?} / {retained_hash}, \
         offered rev {offered:?} / {offered_hash}"
    )]
    Conflict {
        coord: BrickCoord,
        retained: Revision,
        retained_hash: BrickHash,
        offered: Revision,
        offered_hash: BrickHash,
    },
    #[error(
        "brick {coord:?} reload mismatch: retained rev {retained:?} / {retained_hash}, \
         reloaded rev {reloaded:?} / {reloaded_hash}"
    )]
    ReloadMismatch {
        coord: BrickCoord,
        retained: Revision,
        retained_hash: BrickHash,
        reloaded: Revision,
        reloaded_hash: BrickHash,
    },
    /// The logical transition was valid, but derived physics could not be
    /// rebuilt for the new resident geometry. The caller must keep the
    /// digest / geometry transition unpublished.
    #[error("resident collider rebuild failed: {0}")]
    ColliderBuild(String),
    #[error("no retained digest for brick {0:?}")]
    NoRetained(BrickCoord),
}

/// The evicted-brick digests retained for **one** volume. Scoped by its owner to
/// the current world/session generation (stable ids are unique within a world);
/// a full baseline or recovery replaces the whole namespace.
#[derive(Debug, Default, Clone)]
pub struct EvictedBricks {
    digests: ChunkStore<BrickDigest>,
}

fn key_of(coord: BrickCoord) -> (i64, i64, i64) {
    (coord.x, coord.y, coord.z)
}

impl EvictedBricks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.digests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.digests.len() == 0
    }

    pub fn get(&self, coord: BrickCoord) -> Option<BrickDigest> {
        self.digests.get(&key_of(coord)).copied()
    }

    pub fn contains(&self, coord: BrickCoord) -> bool {
        self.digests.get(&key_of(coord)).is_some()
    }

    /// Every chunk that holds a retained digest, with its stamp (see
    /// [`Volume::chunk_stamps`]). Cloning the set shares chunks, so a staging snapshot of a
    /// large evicted set is cheap.
    pub fn chunk_stamps(&self) -> impl Iterator<Item = (ChunkKey, u64)> + '_ {
        self.digests.chunk_stamps()
    }

    /// The retained digests of one chunk, in no particular order.
    pub fn chunk_digests(
        &self,
        chunk: ChunkKey,
    ) -> impl Iterator<Item = (BrickCoord, BrickDigest)> + '_ {
        self.digests
            .chunk_entries(chunk)
            .map(|(&(x, y, z), &digest)| (BrickCoord::new(x, y, z), digest))
    }

    /// Digests in canonical `(z, y, x)` order.
    pub fn iter(&self) -> impl Iterator<Item = (BrickCoord, BrickDigest)> + '_ {
        let mut items: Vec<_> = self
            .digests
            .iter()
            .map(|(&(x, y, z), &d)| (BrickCoord::new(x, y, z), d))
            .collect();
        items.sort_by_key(|(c, _)| c.sort_key());
        items.into_iter()
    }

    /// Total solid cells across every retained digest — the evicted half of the
    /// whole-world conservation sum.
    pub fn total_solid_cells(&self) -> u64 {
        self.digests
            .values()
            .map(|d| u64::from(d.solid_cells))
            .sum()
    }

    /// **Evict** lifecycle. Records `digest` for `coord`. Idempotent for an
    /// identical re-record; `Err(Conflict)` if a *different* `(revision,
    /// content_hash)` is already retained (a resident update must supersede an
    /// old digest through [`Self::supersede`], not a silent overwrite).
    ///
    /// The caller removes the live geometry only after this returns `Ok`, on the
    /// owning thread; a failure here leaves geometry available.
    pub fn record(&mut self, coord: BrickCoord, digest: BrickDigest) -> Result<(), DigestError> {
        match self.digests.get(&key_of(coord)) {
            Some(existing) if *existing == digest => Ok(()),
            Some(existing) => Err(DigestError::Conflict {
                coord,
                retained: existing.revision,
                retained_hash: existing.content_hash,
                offered: digest.revision,
                offered_hash: digest.content_hash,
            }),
            None => {
                self.digests.insert(key_of(coord), digest);
                Ok(())
            }
        }
    }

    /// Convenience: capture `coord`'s digest from `volume` and [`record`] it.
    ///
    /// [`record`]: Self::record
    pub fn record_from(&mut self, volume: &Volume, coord: BrickCoord) -> Result<(), DigestError> {
        let digest = BrickDigest::capture(volume, coord)?;
        self.record(coord, digest)
    }

    /// **Reload** lifecycle, step 1: check that the brick `volume` now holds at
    /// `coord` matches the retained digest exactly. On `Ok` the caller calls
    /// [`Self::clear`]; on `Err(ReloadMismatch)` the retained state is preserved
    /// and the reload stays pending/error.
    pub fn verify_reload(&self, volume: &Volume, coord: BrickCoord) -> Result<(), DigestError> {
        let retained = self
            .digests
            .get(&key_of(coord))
            .ok_or(DigestError::NoRetained(coord))?;
        let reloaded = BrickDigest::capture(volume, coord)?;
        if reloaded.revision == retained.revision && reloaded.content_hash == retained.content_hash
        {
            Ok(())
        } else {
            Err(DigestError::ReloadMismatch {
                coord,
                retained: retained.revision,
                retained_hash: retained.content_hash,
                reloaded: reloaded.revision,
                reloaded_hash: reloaded.content_hash,
            })
        }
    }

    /// Validates a candidate backing brick before it becomes resident. This is
    /// the atomic reload precondition: an error preserves both the absent live
    /// slot and the retained digest.
    pub fn verify_candidate(&self, coord: BrickCoord, brick: &Brick) -> Result<(), DigestError> {
        let retained = self
            .digests
            .get(&key_of(coord))
            .ok_or(DigestError::NoRetained(coord))?;
        let reloaded = BrickDigest::capture_brick(brick);
        if reloaded.revision == retained.revision && reloaded.content_hash == retained.content_hash
        {
            Ok(())
        } else {
            Err(DigestError::ReloadMismatch {
                coord,
                retained: retained.revision,
                retained_hash: retained.content_hash,
                reloaded: reloaded.revision,
                reloaded_hash: reloaded.content_hash,
            })
        }
    }

    /// **Reload** lifecycle, step 2: drop the retained digest once geometry is
    /// installed. `Err(NoRetained)` if nothing was retained for `coord`.
    pub fn clear(&mut self, coord: BrickCoord) -> Result<BrickDigest, DigestError> {
        self.digests
            .remove(&key_of(coord))
            .ok_or(DigestError::NoRetained(coord))
    }

    /// **Repair / commit** lifecycle: drop every retained digest whose brick is
    /// now resident in `volume` again — the live geometry supersedes it, whether
    /// it came back at the same revision (a traversal reload) or a newer one (an
    /// authoritative repair patch that healed a `before` gap). Returns how many
    /// digests were dropped. Idempotent; no error when nothing overlapped.
    pub fn drop_resident(&mut self, volume: &Volume) -> usize {
        let before = self.digests.len();
        for coord in volume.resident_brick_coords() {
            self.digests.remove(&key_of(coord));
        }
        before - self.digests.len()
    }

    /// **Edit / split** lifecycle: a validated resident write legitimately
    /// replaces an older retained digest for the same coord. Unlike [`record`],
    /// this overwrites without a conflict error, but only for a coord that *is*
    /// currently retained; call it in the same atomic step that publishes the
    /// new geometry.
    ///
    /// [`record`]: Self::record
    pub fn supersede(&mut self, coord: BrickCoord, digest: BrickDigest) -> Result<(), DigestError> {
        if self.digests.get(&key_of(coord)).is_none() {
            return Err(DigestError::NoRetained(coord));
        }
        self.digests.insert(key_of(coord), digest);
        Ok(())
    }

    /// **Full baseline / recovery** lifecycle: drop the entire namespace.
    pub fn clear_all(&mut self) {
        self.digests = ChunkStore::default();
    }
}

/// Every logical brick of `volume` — resident bricks plus retained evicted
/// digests — each key exactly once, in canonical `(z, y, x)` order.
///
/// `Err(ResidentEvictedConflict)` if any coord is *both* resident and retained:
/// a resident update must clear the digest in the same atomic transition, so an
/// overlap is a lifecycle bug, never resolved by silently preferring one side.
pub fn logical_bricks(
    volume: &Volume,
    evicted: &EvictedBricks,
) -> Result<Vec<LogicalBrick>, DigestError> {
    let mut out: Vec<LogicalBrick> =
        Vec::with_capacity(volume.resident_brick_count() + evicted.len());

    volume.for_each_resident_brick(|coord, brick| {
        out.push(LogicalBrick {
            coord,
            revision: brick.revision(),
            content_hash: brick.content_hash(),
        });
    });

    for (coord, digest) in evicted.iter() {
        out.push(LogicalBrick {
            coord,
            revision: digest.revision,
            content_hash: digest.content_hash,
        });
    }

    // Keys are unique within each source, so a coordinate that is both resident and retained
    // appears twice, adjacent, once sorted: the first such pair is the smallest conflicting
    // coordinate in canonical order.
    out.sort_unstable_by_key(|b| b.coord.sort_key());
    if let Some(pair) = out.windows(2).find(|pair| pair[0].coord == pair[1].coord) {
        return Err(DigestError::ResidentEvictedConflict(pair[0].coord));
    }
    Ok(out)
}

/// Diagnostic: writes one line per logical brick, `x y z revision content-hash-hex`, in canonical
/// order, so two replicas of the same volume can be compared brick by brick with a text diff
/// when their volume hashes disagree.
pub fn write_logical_brick_listing(
    path: &std::path::Path,
    volume: &Volume,
    evicted: &EvictedBricks,
) -> std::io::Result<()> {
    use std::io::Write;
    let bricks = logical_bricks(volume, evicted)
        .map_err(|e| std::io::Error::other(format!("logical brick listing: {e}")))?;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    for b in bricks {
        write!(
            out,
            "{} {} {} {} ",
            b.coord.x, b.coord.y, b.coord.z, b.revision.0
        )?;
        for byte in b.content_hash.as_bytes() {
            write!(out, "{byte:02x}")?;
        }
        writeln!(out)?;
    }
    out.flush()
}

/// Whole-volume solid-cell count over the logical brick set: resident solid
/// cells plus every retained digest's `solid_cells`. This is the conservation
/// left-hand side that must be independent of cache placement.
pub fn logical_solid_cells(volume: &Volume, evicted: &EvictedBricks) -> Result<u64, DigestError> {
    let mut total = evicted.total_solid_cells();
    let mut conflict: Option<BrickCoord> = None;
    volume.for_each_resident_brick(|coord, brick| {
        if evicted.contains(coord) {
            // Report the smallest conflicting coordinate in canonical order.
            if conflict.is_none_or(|c| coord.sort_key() < c.sort_key()) {
                conflict = Some(coord);
            }
        }
        total += u64::from(brick.solid_cells());
    });
    match conflict {
        Some(coord) => Err(DigestError::ResidentEvictedConflict(coord)),
        None => Ok(total),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};

    use crate::brick::BrickSnapshot;
    use crate::edit::EditPlan;

    const STONE: MaterialId = MaterialId(1);
    const DIRT: MaterialId = MaterialId(2);

    fn vid() -> VolumeId {
        VolumeId::new(1).unwrap()
    }

    /// A volume with solid boxes at a few brick coords, including negative ones,
    /// plus a mined-to-air tombstone brick.
    fn scene() -> Volume {
        let id = vid();
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        for (a, b, m) in [
            // brick (0,0,0)
            (GlobalCell::new(1, 1, 1), GlobalCell::new(20, 3, 4), STONE),
            // brick (1,0,0)
            (GlobalCell::new(40, 0, 0), GlobalCell::new(50, 2, 2), DIRT),
            // brick (-1,0,-1): negative coordinates
            (
                GlobalCell::new(-30, 0, -10),
                GlobalCell::new(-20, 1, -5),
                STONE,
            ),
            // brick (0,2,0)
            (GlobalCell::new(2, 70, 2), GlobalCell::new(6, 72, 6), STONE),
        ] {
            v.apply_edit(&EditPlan::filled_box(id, a, b, m)).unwrap();
        }
        // Mine one of those bricks' cells back to air -> a modified-air brick
        // stays resident with a real (non-zero) revision.
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(40, 0, 0),
            GlobalCell::new(50, 2, 2),
            MaterialId::AIR,
        ))
        .unwrap();
        v
    }

    fn digest_of(v: &Volume, e: &EvictedBricks) -> Vec<(BrickCoord, Revision, BrickHash)> {
        logical_bricks(v, e)
            .unwrap()
            .into_iter()
            .map(|b| (b.coord, b.revision, b.content_hash))
            .collect()
    }

    #[test]
    fn logical_view_with_no_evictions_matches_the_resident_set_exactly() {
        let v = scene();
        let empty = EvictedBricks::new();
        let logical = digest_of(&v, &empty);

        let resident: Vec<_> = v
            .resident_brick_coords()
            .into_iter()
            .map(|c| {
                let s = v.snapshot_brick(c).unwrap().unwrap();
                (c, s.revision(), s.content_hash())
            })
            .collect();

        assert_eq!(logical, resident);
        assert!(!logical.is_empty());
    }

    #[test]
    fn evicting_any_subset_in_any_order_preserves_the_logical_view() {
        let full = scene();
        let baseline = digest_of(&full, &EvictedBricks::new());

        let coords = full.resident_brick_coords();
        // Try several eviction subsets, each applied in two different orders.
        for subset in [
            vec![coords[0]],
            vec![coords[1], coords[3]],
            coords.clone(),
            vec![coords[2], coords[0], coords[1]],
        ] {
            for order in [subset.clone(), subset.iter().rev().copied().collect()] {
                let mut v = scene();
                let mut e = EvictedBricks::new();
                for c in &order {
                    e.record_from(&v, *c).unwrap();
                    v.evict_brick(*c);
                }
                assert_eq!(
                    digest_of(&v, &e),
                    baseline,
                    "subset {subset:?} order {order:?}"
                );
                assert_eq!(
                    logical_solid_cells(&v, &e).unwrap(),
                    logical_solid_cells(&full, &EvictedBricks::new()).unwrap()
                );
            }
        }
    }

    #[test]
    fn a_modified_air_brick_digest_round_trips_and_is_not_dropped() {
        let mut v = scene();
        // The (1,0,0) brick was mined to air above; find it.
        let air_coord = v
            .resident_brick_coords()
            .into_iter()
            .find(|&c| v.snapshot_brick(c).unwrap().unwrap().is_modified_air())
            .expect("scene has a modified-air brick");

        let digest = BrickDigest::capture(&v, air_coord).unwrap();
        assert!(digest.modified_air);
        assert_eq!(digest.solid_cells, 0);

        let baseline = digest_of(&v, &EvictedBricks::new());
        let mut e = EvictedBricks::new();
        e.record_from(&v, air_coord).unwrap();
        v.evict_brick(air_coord);

        // Still a logical brick, same revision + hash — the tombstone survived.
        assert_eq!(digest_of(&v, &e), baseline);
        assert!(e.get(air_coord).unwrap().modified_air);
    }

    #[test]
    fn multiple_volumes_are_independent() {
        let mut a = scene();
        let mut b = Volume::new(VolumeId::new(2).unwrap(), CellSizeCode::Quarter);
        b.apply_edit(&EditPlan::filled_box(
            VolumeId::new(2).unwrap(),
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(5, 5, 5),
            STONE,
        ))
        .unwrap();

        let a_base = digest_of(&a, &EvictedBricks::new());
        let b_base = digest_of(&b, &EvictedBricks::new());

        let mut ea = EvictedBricks::new();
        let ca = a.resident_brick_coords()[0];
        ea.record_from(&a, ca).unwrap();
        a.evict_brick(ca);

        // b untouched; a still whole logically.
        assert_eq!(digest_of(&b, &EvictedBricks::new()), b_base);
        assert_eq!(digest_of(&a, &ea), a_base);
    }

    #[test]
    fn a_resident_and_evicted_coord_is_a_conflict_not_a_silent_choice() {
        let v = scene();
        let c = v.resident_brick_coords()[0];
        let mut e = EvictedBricks::new();
        // Record a digest but do NOT evict the live brick.
        e.record_from(&v, c).unwrap();

        assert_eq!(
            logical_bricks(&v, &e).unwrap_err(),
            DigestError::ResidentEvictedConflict(c)
        );
        assert_eq!(
            logical_solid_cells(&v, &e).unwrap_err(),
            DigestError::ResidentEvictedConflict(c)
        );
    }

    #[test]
    fn several_conflicts_report_the_first_in_canonical_order() {
        let v = scene();
        let coords = v.resident_brick_coords();
        assert!(coords.len() >= 3, "the scene needs several bricks");
        let mut e = EvictedBricks::new();
        // Retain the last and the middle bricks without evicting them; the smallest
        // conflicting coordinate in canonical order is the middle one.
        e.record_from(&v, coords[coords.len() - 1]).unwrap();
        e.record_from(&v, coords[1]).unwrap();
        assert_eq!(
            logical_bricks(&v, &e).unwrap_err(),
            DigestError::ResidentEvictedConflict(coords[1])
        );
        assert_eq!(
            logical_solid_cells(&v, &e).unwrap_err(),
            DigestError::ResidentEvictedConflict(coords[1])
        );
    }

    #[test]
    fn logical_bricks_are_canonically_ordered_with_each_coord_once() {
        let mut v = scene();
        let mut e = EvictedBricks::new();
        let coords = v.resident_brick_coords();
        e.record_from(&v, coords[0]).unwrap();
        v.evict_brick(coords[0]);
        let logical = logical_bricks(&v, &e).unwrap();
        assert_eq!(
            logical.iter().map(|b| b.coord).collect::<Vec<_>>(),
            coords,
            "resident and retained bricks interleave in (z, y, x) order, once each"
        );
    }

    #[test]
    fn record_is_idempotent_but_a_different_digest_is_rejected() {
        let v = scene();
        let c = v.resident_brick_coords()[0];
        let d = BrickDigest::capture(&v, c).unwrap();
        let mut e = EvictedBricks::new();
        e.record(c, d).unwrap();
        e.record(c, d).unwrap(); // idempotent

        let bogus = BrickDigest {
            revision: Revision(d.revision.get() + 1),
            ..d
        };
        assert!(matches!(
            e.record(c, bogus),
            Err(DigestError::Conflict { coord, .. }) if coord == c
        ));
        // The original is untouched.
        assert_eq!(e.get(c), Some(d));
    }

    #[test]
    fn capture_of_a_non_resident_brick_is_an_error_never_a_guess() {
        let v = scene();
        let absent = BrickCoord::new(9, 9, 9);
        assert_eq!(
            BrickDigest::capture(&v, absent),
            Err(DigestError::NotResident(absent))
        );
    }

    #[test]
    fn reload_verifies_key_revision_and_content_before_clearing() {
        let original = scene();
        let c = original.resident_brick_coords()[0];

        let mut v = scene();
        let mut e = EvictedBricks::new();
        e.record_from(&v, c).unwrap();
        v.evict_brick(c);

        // A correct reload: reinstall the identical brick.
        let snap = original.snapshot_brick(c).unwrap().unwrap();
        v.insert_brick(c, brick_from_snapshot(&snap)).unwrap();
        e.verify_reload(&v, c).unwrap();
        let cleared = e.clear(c).unwrap();
        assert_eq!(cleared.revision, snap.revision());
        assert!(e.is_empty());
        assert!(logical_bricks(&v, &e).is_ok());
    }

    #[test]
    fn a_mismatched_reload_keeps_the_retained_digest() {
        let mut v = scene();
        let c = v.resident_brick_coords()[0];
        e_and_evict(&mut v, c);
        let mut e = EvictedBricks::new();
        e.record_from(&scene(), c).unwrap();

        // Reinstall a *different* brick at c (wrong content).
        let mut wrong = Volume::new(vid(), CellSizeCode::Quarter);
        wrong
            .apply_edit(&EditPlan::filled_box(
                vid(),
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(0, 0, 0),
                DIRT,
            ))
            .unwrap();
        let wsnap = wrong
            .snapshot_brick(BrickCoord::new(0, 0, 0))
            .unwrap()
            .unwrap();
        v.insert_brick(c, brick_from_snapshot(&wsnap)).unwrap();

        assert!(matches!(
            e.verify_reload(&v, c),
            Err(DigestError::ReloadMismatch { coord, .. }) if coord == c
        ));
        // Retained digest preserved; clear still works if the caller insists,
        // but verify_reload refused.
        assert!(e.contains(c));
    }

    #[test]
    fn clear_and_supersede_require_a_retained_entry() {
        let v = scene();
        let c = v.resident_brick_coords()[0];
        let mut e = EvictedBricks::new();
        assert_eq!(e.clear(c), Err(DigestError::NoRetained(c)));
        let d = BrickDigest::capture(&v, c).unwrap();
        assert_eq!(e.supersede(c, d), Err(DigestError::NoRetained(c)));
        e.record(c, d).unwrap();
        assert!(e.supersede(c, d).is_ok());
    }

    #[test]
    fn drop_resident_clears_only_digests_whose_brick_is_back() {
        let mut v = scene();
        let coords = v.resident_brick_coords();
        let (a, b) = (coords[0], coords[1]);

        let mut e = EvictedBricks::new();
        // `a` still has its geometry (a reload landed); `b` is genuinely gone.
        e.record_from(&v, a).unwrap();
        e.record_from(&v, b).unwrap();
        v.evict_brick(b);
        assert_eq!(e.len(), 2);
        // `a` resident *and* retained is exactly the invariant `drop_resident`
        // repairs after a reload path forgets to clear the digest.
        assert!(matches!(
            logical_bricks(&v, &e),
            Err(DigestError::ResidentEvictedConflict(c)) if c == a
        ));

        assert_eq!(e.drop_resident(&v), 1);
        assert!(!e.contains(a));
        assert!(e.contains(b));
        assert!(logical_bricks(&v, &e).is_ok());
        // Idempotent — a second pass finds nothing to drop.
        assert_eq!(e.drop_resident(&v), 0);
    }

    // --- test helpers ---------------------------------------------------------

    fn e_and_evict(v: &mut Volume, c: BrickCoord) {
        v.evict_brick(c);
    }

    fn brick_from_snapshot(snap: &BrickSnapshot) -> crate::brick::Brick {
        // Rebuild a brick with the snapshot's cells + revision so a reload can
        // be exercised without a backing store.
        let mut brick = crate::brick::Brick::empty();
        for index in 0..CELLS_PER_BRICK as u16 {
            let cell = LocalCell::from_linear_index(index).unwrap();
            brick.set_cell(cell, snap.get(cell));
        }
        brick.collapse();
        if snap.is_edited() {
            brick.mark_edited();
        }
        brick.set_revision(snap.revision());
        brick
    }
}

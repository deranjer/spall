//! A sparse voxel volume: a `VolumeId`, a fixed cell size, an optional brick
//! bounding box, and a sparse map of bricks.
//!
//! Sampling distinguishes four states, never collapsing them: a brick can be
//! *absent* (not in the stored set), *failed* (a load was attempted and
//! failed), resident-and-*empty* (air), or resident-and-*filled*. "Not loaded"
//! is never silently treated as air.

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, LocalCell, MaterialId, Revision, VolumeId};

use crate::accounting::MemoryReport;
use crate::brick::{Brick, BrickSnapshot};
use crate::chunk_store::{ChunkKey, ChunkStore, fresh_stamp};

/// Inclusive brick-coordinate bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrickBounds {
    pub min: BrickCoord,
    pub max: BrickCoord,
}

impl BrickBounds {
    pub fn new(min: BrickCoord, max: BrickCoord) -> Option<Self> {
        (min.x <= max.x && min.y <= max.y && min.z <= max.z).then_some(Self { min, max })
    }

    pub fn contains(&self, coord: BrickCoord) -> bool {
        (self.min.x..=self.max.x).contains(&coord.x)
            && (self.min.y..=self.max.y).contains(&coord.y)
            && (self.min.z..=self.max.z).contains(&coord.z)
    }
}

/// Why a resident brick is not available. Used where a *cell* sample cannot be
/// produced because the brick is not loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Residency {
    /// Not in the stored set. May or may not exist in the persistent world.
    Absent,
    /// A load was attempted and failed. Distinct from absent and from empty.
    Failed,
}

/// The full state of a brick slot, including the resident case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrickState {
    Absent,
    Failed,
    Resident { revision: Revision, edited: bool },
}

/// One slot in a volume's sparse brick map.
#[derive(Debug, Clone)]
enum BrickSlot {
    Failed,
    Resident(Brick),
}

/// Result of sampling one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample {
    /// The brick is not resident; the caller must not treat this as air.
    Unknown(Residency),
    /// Resident brick, this cell is air. `modified` is true if the brick has
    /// been authoritatively edited (a modified-air cell that must not
    /// regenerate).
    Empty { modified: bool },
    /// Resident brick, this cell holds a solid material.
    Filled(MaterialId),
}

/// Checked-access failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AccessError {
    #[error("brick {coord:?} is outside the volume bounds")]
    OutOfBounds { coord: BrickCoord },
    #[error("brick {coord:?} has revision u64::MAX, leaving no later revision for edits")]
    RevisionExhausted { coord: BrickCoord },
    #[error("linear cell index {0} is out of range 0..32768")]
    BadCellIndex(u32),
}

fn fresh_state_stamp() -> u64 {
    fresh_stamp()
}

/// A sparse voxel volume.
#[derive(Debug, Clone)]
pub struct Volume {
    id: VolumeId,
    cell_size: CellSizeCode,
    bounds: Option<BrickBounds>,
    bricks: ChunkStore<BrickSlot>,
    next_revision: Revision,
    /// Replaced with a fresh process-unique value by every mutation; a clone keeps its source's
    /// until it is itself mutated. See [`Self::state_stamp`].
    stamp: u64,
}

impl Volume {
    /// An unbounded sparse volume with no bricks.
    pub fn new(id: VolumeId, cell_size: CellSizeCode) -> Self {
        Self {
            id,
            cell_size,
            bounds: None,
            bricks: ChunkStore::default(),
            next_revision: Revision(1),
            stamp: fresh_state_stamp(),
        }
    }

    /// A volume whose bricks must lie within `bounds`; access outside fails.
    pub fn bounded(id: VolumeId, cell_size: CellSizeCode, bounds: BrickBounds) -> Self {
        Self {
            bounds: Some(bounds),
            ..Self::new(id, cell_size)
        }
    }

    pub fn id(&self) -> VolumeId {
        self.id
    }

    pub fn cell_size(&self) -> CellSizeCode {
        self.cell_size
    }

    pub fn bounds(&self) -> Option<BrickBounds> {
        self.bounds
    }

    /// The next revision this volume will hand out to an edited brick.
    pub fn next_revision(&self) -> Revision {
        self.next_revision
    }

    pub fn resident_brick_count(&self) -> usize {
        self.bricks
            .values()
            .filter(|slot| matches!(slot, BrickSlot::Resident(_)))
            .count()
    }

    /// Coordinates of every resident brick, in canonical `(z, y, x)` order.
    /// Failed slots are excluded.
    pub fn resident_brick_coords(&self) -> Vec<BrickCoord> {
        let mut coords: Vec<BrickCoord> = self
            .bricks
            .iter()
            .filter_map(|(&(x, y, z), slot)| {
                matches!(slot, BrickSlot::Resident(_)).then_some(BrickCoord::new(x, y, z))
            })
            .collect();
        coords.sort_by_key(|c| c.sort_key());
        coords
    }

    /// Every chunk that holds a slot, with its stamp. A chunk's stamp is replaced by every
    /// change to it and shared by clones until either side is written, so two equal stamps
    /// mean identical slots; anything derived from a chunk (the topology hash's per-chunk
    /// digest) can be reused while its stamp is unchanged.
    pub fn chunk_stamps(&self) -> impl Iterator<Item = (ChunkKey, u64)> + '_ {
        self.bricks.chunk_stamps()
    }

    /// The resident bricks of one chunk, in no particular order. Slots whose load failed are
    /// not resident and are skipped.
    pub fn chunk_resident_bricks(
        &self,
        chunk: ChunkKey,
    ) -> impl Iterator<Item = (BrickCoord, &Brick)> + '_ {
        self.bricks
            .chunk_entries(chunk)
            .filter_map(|(&(x, y, z), slot)| match slot {
                BrickSlot::Resident(brick) => Some((BrickCoord::new(x, y, z), brick)),
                BrickSlot::Failed => None,
            })
    }

    /// Visits every resident brick in storage order (not canonical order), without the
    /// per-brick lookup and handle clone of [`Self::snapshot_brick`].
    pub fn for_each_resident_brick(&self, mut visit: impl FnMut(BrickCoord, &Brick)) {
        for (&(x, y, z), slot) in self.bricks.iter() {
            if let BrickSlot::Resident(brick) = slot {
                visit(BrickCoord::new(x, y, z), brick);
            }
        }
    }

    fn check_bounds(&self, coord: BrickCoord) -> Result<(), AccessError> {
        match self.bounds {
            Some(bounds) if !bounds.contains(coord) => Err(AccessError::OutOfBounds { coord }),
            _ => Ok(()),
        }
    }

    /// Installs a resident brick and advances the volume revision high-water
    /// mark past its revision. A brick at `u64::MAX` is rejected because no
    /// subsequent edit can receive a strictly newer revision.
    pub fn insert_brick(&mut self, coord: BrickCoord, brick: Brick) -> Result<(), AccessError> {
        self.check_bounds(coord)?;
        let next_after_brick = brick
            .revision()
            .checked_next()
            .map_err(|_| AccessError::RevisionExhausted { coord })?;
        if next_after_brick > self.next_revision {
            self.next_revision = next_after_brick;
        }
        self.stamp = fresh_state_stamp();
        self.bricks
            .insert((coord.x, coord.y, coord.z), BrickSlot::Resident(brick));
        Ok(())
    }

    /// Records that loading the brick at `coord` failed.
    pub fn mark_failed(&mut self, coord: BrickCoord) -> Result<(), AccessError> {
        self.check_bounds(coord)?;
        self.stamp = fresh_state_stamp();
        self.bricks
            .insert((coord.x, coord.y, coord.z), BrickSlot::Failed);
        Ok(())
    }

    /// Drops a brick from the stored set, returning it to `Absent`.
    pub fn evict_brick(&mut self, coord: BrickCoord) {
        self.stamp = fresh_state_stamp();
        self.bricks.remove(&(coord.x, coord.y, coord.z));
    }

    /// A process-unique value that changes on every mutation of this volume (brick insert,
    /// eviction, failure mark, edit). Two volumes with the same stamp hold identical state, so
    /// work derived from a volume (a structure index) can be reused exactly when its stamp has
    /// not changed. A clone shares its source's stamp until either is mutated.
    pub fn state_stamp(&self) -> u64 {
        self.stamp
    }

    fn slot(&self, coord: BrickCoord) -> Option<&BrickSlot> {
        self.bricks.get(&(coord.x, coord.y, coord.z))
    }

    pub(crate) fn put_resident(&mut self, coord: BrickCoord, brick: Brick) {
        self.stamp = fresh_state_stamp();
        self.bricks
            .insert((coord.x, coord.y, coord.z), BrickSlot::Resident(brick));
    }

    pub(crate) fn take_resident(&mut self, coord: BrickCoord) -> Option<Brick> {
        self.stamp = fresh_state_stamp();
        match self.bricks.remove(&(coord.x, coord.y, coord.z)) {
            Some(BrickSlot::Resident(brick)) => Some(brick),
            other => {
                if let Some(slot) = other {
                    self.bricks.insert((coord.x, coord.y, coord.z), slot);
                }
                None
            }
        }
    }

    pub(crate) fn allocate_revision(&mut self) -> Result<Revision, spall_core::IdError> {
        self.stamp = fresh_state_stamp();
        let value = self.next_revision;
        self.next_revision = self.next_revision.checked_next()?;
        Ok(value)
    }

    #[cfg(test)]
    pub(crate) fn set_next_revision_for_test(&mut self, revision: Revision) {
        self.stamp = fresh_state_stamp();
        self.next_revision = revision;
    }

    /// Full state of the brick that owns `coord`.
    pub fn brick_state(&self, coord: BrickCoord) -> Result<BrickState, AccessError> {
        self.check_bounds(coord)?;
        Ok(match self.slot(coord) {
            None => BrickState::Absent,
            Some(BrickSlot::Failed) => BrickState::Failed,
            Some(BrickSlot::Resident(brick)) => BrickState::Resident {
                revision: brick.revision(),
                edited: brick.is_edited(),
            },
        })
    }

    /// The revision of a resident brick, or `None` if it is not resident.
    pub fn brick_revision(&self, coord: BrickCoord) -> Result<Option<Revision>, AccessError> {
        self.check_bounds(coord)?;
        Ok(match self.slot(coord) {
            Some(BrickSlot::Resident(brick)) => Some(brick.revision()),
            _ => None,
        })
    }

    /// An immutable snapshot of a resident brick.
    pub fn snapshot_brick(&self, coord: BrickCoord) -> Result<Option<BrickSnapshot>, AccessError> {
        self.check_bounds(coord)?;
        Ok(match self.slot(coord) {
            Some(BrickSlot::Resident(brick)) => Some(brick.snapshot()),
            _ => None,
        })
    }

    /// Samples one global cell. `Err` only for a checked-access violation
    /// (outside bounds); an unloaded brick is `Ok(Sample::Unknown(..))`.
    pub fn sample(&self, cell: GlobalCell) -> Result<Sample, AccessError> {
        let (coord, local) = cell.split();
        self.check_bounds(coord)?;
        Ok(match self.slot(coord) {
            None => Sample::Unknown(Residency::Absent),
            Some(BrickSlot::Failed) => Sample::Unknown(Residency::Failed),
            Some(BrickSlot::Resident(brick)) => classify(brick, local),
        })
    }

    /// Samples by brick coordinate and in-brick linear index, checking the
    /// index range explicitly.
    pub fn sample_local(
        &self,
        coord: BrickCoord,
        linear_index: u32,
    ) -> Result<Sample, AccessError> {
        self.check_bounds(coord)?;
        let local = u16::try_from(linear_index)
            .ok()
            .and_then(LocalCell::from_linear_index)
            .ok_or(AccessError::BadCellIndex(linear_index))?;
        Ok(match self.slot(coord) {
            None => Sample::Unknown(Residency::Absent),
            Some(BrickSlot::Failed) => Sample::Unknown(Residency::Failed),
            Some(BrickSlot::Resident(brick)) => classify(brick, local),
        })
    }
}

impl Volume {
    /// Tallies the volume's storage footprint. See [`MemoryReport`].
    pub fn memory_report(&self) -> MemoryReport {
        let mut report = MemoryReport::default();
        for slot in self.bricks.values() {
            match slot {
                BrickSlot::Failed => report.failed_slots += 1,
                BrickSlot::Resident(brick) if !brick.is_dense() => report.uniform_bricks += 1,
                BrickSlot::Resident(brick) => {
                    report.dense_bricks += 1;
                    if brick.dense_payload_shared() {
                        report.snapshot_shared_bricks += 1;
                        report.snapshot_shared_bytes += brick.material_storage_bytes();
                    } else {
                        report.owned_dense_bytes += brick.material_storage_bytes();
                    }
                }
            }
        }
        report
    }
}

fn classify(brick: &Brick, local: LocalCell) -> Sample {
    let material = brick.get(local);
    if material.is_air() {
        Sample::Empty {
            modified: brick.is_edited(),
        }
    } else {
        Sample::Filled(material)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brick::Brick;

    fn vol() -> Volume {
        Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter)
    }

    /// Bricks on both sides of chunk boundaries, including negative coordinates.
    fn spread_coords() -> Vec<BrickCoord> {
        let mut coords = Vec::new();
        for x in [-9, -8, -1, 0, 7, 8, 17] {
            for y in [-8, -1, 0, 8] {
                for z in [-17, -1, 0, 7, 8] {
                    coords.push(BrickCoord::new(x, y, z));
                }
            }
        }
        coords
    }

    #[test]
    fn chunked_storage_keeps_every_brick_findable_across_chunk_boundaries() {
        let mut v = vol();
        let coords = spread_coords();
        for (i, &c) in coords.iter().enumerate() {
            v.insert_brick(
                c,
                Brick::uniform(MaterialId(1 + (i % 5) as u16), Revision(1)),
            )
            .unwrap();
        }
        assert_eq!(v.resident_brick_count(), coords.len());
        let mut expected = coords.clone();
        expected.sort_by_key(|c| c.sort_key());
        assert_eq!(v.resident_brick_coords(), expected);
        for (i, &c) in coords.iter().enumerate() {
            let cell = GlobalCell::new(c.x * 32, c.y * 32, c.z * 32);
            assert_eq!(
                v.sample(cell).unwrap(),
                Sample::Filled(MaterialId(1 + (i % 5) as u16)),
                "{c:?}"
            );
        }
    }

    #[test]
    fn a_cloned_volume_is_independent_of_its_source_in_both_directions() {
        let mut original = vol();
        let coords = spread_coords();
        for &c in &coords {
            original
                .insert_brick(c, Brick::uniform(MaterialId(1), Revision(1)))
                .unwrap();
        }
        let mut copy = original.clone();
        assert_eq!(copy.state_stamp(), original.state_stamp());

        // Edits to the copy leave the original alone...
        let victim = coords[3];
        copy.evict_brick(victim);
        copy.insert_brick(
            BrickCoord::new(100, 100, 100),
            Brick::uniform(MaterialId(2), Revision(1)),
        )
        .unwrap();
        assert_eq!(original.resident_brick_count(), coords.len());
        assert!(matches!(
            original.brick_state(victim).unwrap(),
            BrickState::Resident { .. }
        ));
        assert!(matches!(
            original
                .brick_state(BrickCoord::new(100, 100, 100))
                .unwrap(),
            BrickState::Absent
        ));
        assert_ne!(copy.state_stamp(), original.state_stamp());

        // ...and edits to the original leave the copy alone.
        let other = coords[10];
        original.mark_failed(other).unwrap();
        assert!(matches!(
            copy.brick_state(other).unwrap(),
            BrickState::Resident { .. }
        ));
        assert_eq!(copy.resident_brick_count(), coords.len());
    }

    #[test]
    fn removing_the_last_brick_of_a_chunk_and_an_absent_brick_are_both_harmless() {
        let mut v = vol();
        let lone = BrickCoord::new(40, 40, 40);
        v.insert_brick(lone, Brick::uniform(MaterialId(1), Revision(1)))
            .unwrap();
        let snapshot = v.clone();
        v.evict_brick(lone);
        v.evict_brick(lone); // already gone
        v.evict_brick(BrickCoord::new(-300, 5, 5)); // never existed, in a chunk that never existed
        assert_eq!(v.resident_brick_count(), 0);
        assert_eq!(snapshot.resident_brick_count(), 1);
        v.insert_brick(lone, Brick::uniform(MaterialId(2), Revision(2)))
            .unwrap();
        assert_eq!(v.resident_brick_count(), 1);
    }

    #[test]
    fn unloaded_absent_and_failed_are_distinct_from_empty() {
        let mut v = vol();
        let c = GlobalCell::new(10, 10, 10);
        assert_eq!(v.sample(c).unwrap(), Sample::Unknown(Residency::Absent));

        v.mark_failed(c.split().0).unwrap();
        assert_eq!(v.sample(c).unwrap(), Sample::Unknown(Residency::Failed));

        v.insert_brick(c.split().0, Brick::uniform(MaterialId::AIR, Revision(1)))
            .unwrap();
        assert_eq!(v.sample(c).unwrap(), Sample::Empty { modified: false });
    }

    #[test]
    fn filled_and_empty_reflect_brick_contents() {
        let mut v = vol();
        let c = GlobalCell::new(-5, 2, 40);
        v.insert_brick(c.split().0, Brick::uniform(MaterialId(3), Revision(1)))
            .unwrap();
        assert_eq!(v.sample(c).unwrap(), Sample::Filled(MaterialId(3)));
    }

    #[test]
    fn bounded_volume_rejects_out_of_range_access() {
        let bounds = BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(1, 1, 1)).unwrap();
        let mut v = Volume::bounded(VolumeId::new(2).unwrap(), CellSizeCode::Quarter, bounds);

        // brick (2,0,0) is outside 0..=1
        let outside = GlobalCell::new(64, 0, 0);
        assert_eq!(
            v.sample(outside),
            Err(AccessError::OutOfBounds {
                coord: BrickCoord::new(2, 0, 0)
            })
        );
        assert!(
            v.insert_brick(BrickCoord::new(2, 0, 0), Brick::empty())
                .is_err()
        );

        // inside is fine
        assert!(v.sample(GlobalCell::new(0, 0, 0)).is_ok());
    }

    #[test]
    fn sample_local_checks_the_index_range() {
        let v = vol();
        assert_eq!(
            v.sample_local(BrickCoord::new(0, 0, 0), 32_768),
            Err(AccessError::BadCellIndex(32_768))
        );
        assert!(v.sample_local(BrickCoord::new(0, 0, 0), 32_767).is_ok());
    }

    #[test]
    fn installing_a_brick_advances_the_edit_revision_high_water_mark() {
        let mut v = vol();
        v.insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId(2), Revision(4)),
        )
        .unwrap();
        assert_eq!(v.next_revision(), Revision(5));
    }

    #[test]
    fn installing_an_exhausted_brick_is_rejected_without_mutation() {
        let mut v = vol();
        let coord = BrickCoord::new(0, 0, 0);
        assert_eq!(
            v.insert_brick(coord, Brick::uniform(MaterialId(2), Revision(u64::MAX))),
            Err(AccessError::RevisionExhausted { coord })
        );
        assert_eq!(v.brick_state(coord).unwrap(), BrickState::Absent);
        assert_eq!(v.next_revision(), Revision(1));
    }
}

//! Copy-on-write edits with before/after revision records.
//!
//! An [`EditPlan`] is a set of `(cell, material)` writes for one volume. Applying
//! it groups the writes by brick, copies each touched brick's dense payload on
//! write (snapshots keep their old payload), bumps that brick's revision, marks
//! it edited, and returns an [`EditOutcome`] with the before/after revision and
//! content hash of every affected brick. T02 provides the mechanism only — no
//! server loop calls it yet.

use std::collections::BTreeMap;

use spall_core::{BrickCoord, GlobalCell, LocalCell, MaterialId, Revision, VolumeId};

use crate::brick::{Brick, BrickHash};
use crate::volume::{AccessError, BrickState, Volume};

/// One cell write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellEdit {
    pub cell: GlobalCell,
    pub material: MaterialId,
}

/// A batch of cell writes targeting one volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPlan {
    pub volume: VolumeId,
    pub writes: Vec<CellEdit>,
}

impl EditPlan {
    pub fn new(volume: VolumeId) -> Self {
        Self {
            volume,
            writes: Vec::new(),
        }
    }

    pub fn set(&mut self, cell: GlobalCell, material: MaterialId) -> &mut Self {
        self.writes.push(CellEdit { cell, material });
        self
    }
}

/// Per-brick before/after record produced by applying an [`EditPlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrickRevisionRecord {
    pub coord: BrickCoord,
    /// Whether the brick was already resident before the edit.
    pub existed_before: bool,
    pub before_revision: Revision,
    pub after_revision: Revision,
    pub before_hash: BrickHash,
    pub after_hash: BrickHash,
    /// The brick is now edited and contains only air.
    pub now_modified_air: bool,
    pub cells_changed: u32,
}

/// The result of applying an [`EditPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutcome {
    pub volume: VolumeId,
    /// One record per touched brick, ordered by `(z, y, x)`.
    pub bricks: Vec<BrickRevisionRecord>,
}

impl EditOutcome {
    pub fn total_cells_changed(&self) -> u64 {
        self.bricks.iter().map(|b| u64::from(b.cells_changed)).sum()
    }
}

/// Why an [`EditPlan`] could not be applied. On error the volume is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("edit plan targets volume {plan} but was applied to volume {target}")]
    WrongVolume { plan: VolumeId, target: VolumeId },
    #[error("edit touches brick {coord:?} which is outside the volume bounds")]
    OutOfBounds { coord: BrickCoord },
    #[error("edit touches brick {coord:?} whose data failed to load")]
    TouchesFailedBrick { coord: BrickCoord },
    #[error("volume revision counter is exhausted")]
    RevisionExhausted,
}

impl From<AccessError> for EditError {
    fn from(value: AccessError) -> Self {
        match value {
            AccessError::OutOfBounds { coord } => Self::OutOfBounds { coord },
            AccessError::BadCellIndex(_) => {
                unreachable!("edit writes use LocalCell, which is always in range")
            }
        }
    }
}

impl Volume {
    /// Applies `plan` transactionally: it either updates every touched brick
    /// and returns an [`EditOutcome`], or changes nothing and returns
    /// [`EditError`].
    pub fn apply_edit(&mut self, plan: &EditPlan) -> Result<EditOutcome, EditError> {
        if plan.volume != self.id() {
            return Err(EditError::WrongVolume {
                plan: plan.volume,
                target: self.id(),
            });
        }

        // Group writes by brick, preserving per-brick write order.
        let mut by_brick: BTreeMap<(i64, i64, i64), Vec<(LocalCell, MaterialId)>> = BTreeMap::new();
        for write in &plan.writes {
            let (coord, local) = write.cell.split();
            by_brick
                .entry((coord.x, coord.y, coord.z))
                .or_default()
                .push((local, write.material));
        }

        // Pre-flight: bounds and failed-brick checks, so a rejected plan
        // leaves the volume untouched.
        for &(x, y, z) in by_brick.keys() {
            let coord = BrickCoord::new(x, y, z);
            match self.brick_state(coord)? {
                BrickState::Failed => return Err(EditError::TouchesFailedBrick { coord }),
                BrickState::Absent | BrickState::Resident { .. } => {}
            }
        }
        // Enough revisions left for every touched brick? The last one handed
        // out would be `next_revision + needed - 1`.
        let needed = by_brick.len() as u64;
        if needed > 0 && self.next_revision().get().checked_add(needed - 1).is_none() {
            return Err(EditError::RevisionExhausted);
        }

        let mut records = Vec::with_capacity(by_brick.len());
        for ((x, y, z), writes) in by_brick {
            let coord = BrickCoord::new(x, y, z);

            let (mut brick, existed_before) = match self.take_resident(coord) {
                Some(brick) => (brick, true),
                None => (Brick::empty(), false),
            };
            let before_revision = brick.revision();
            let before_hash = brick.content_hash();

            let mut cells_changed = 0u32;
            for (local, material) in writes {
                if brick.set_cell(local, material) {
                    cells_changed += 1;
                }
            }
            brick.collapse();
            brick.mark_edited();

            let after_revision = self
                .allocate_revision()
                .map_err(|_| EditError::RevisionExhausted)?;
            brick.set_revision(after_revision);
            let after_hash = brick.content_hash();
            let now_modified_air = brick.is_modified_air();

            self.put_resident(coord, brick);
            records.push(BrickRevisionRecord {
                coord,
                existed_before,
                before_revision,
                after_revision,
                before_hash,
                after_hash,
                now_modified_air,
                cells_changed,
            });
        }

        Ok(EditOutcome {
            volume: self.id(),
            bricks: records,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brick::Brick;
    use crate::volume::{BrickBounds, Sample};
    use spall_core::CellSizeCode;

    fn vol() -> Volume {
        Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter)
    }

    #[test]
    fn edit_creates_bricks_records_revisions_and_hashes() {
        let mut v = vol();
        let mut plan = EditPlan::new(v.id());
        plan.set(GlobalCell::new(1, 1, 1), MaterialId(5))
            .set(GlobalCell::new(40, 1, 1), MaterialId(6)); // different brick

        let outcome = v.apply_edit(&plan).unwrap();
        assert_eq!(outcome.bricks.len(), 2);
        assert_eq!(outcome.total_cells_changed(), 2);

        let first = &outcome.bricks[0];
        assert!(!first.existed_before);
        assert_eq!(first.before_revision, Revision::ZERO);
        assert_eq!(first.after_revision, Revision(1));
        assert_ne!(first.before_hash, first.after_hash);
        assert!(!first.now_modified_air);

        assert_eq!(
            v.sample(GlobalCell::new(1, 1, 1)).unwrap(),
            Sample::Filled(MaterialId(5))
        );
        assert_eq!(v.next_revision(), Revision(3));
    }

    #[test]
    fn re_editing_a_brick_advances_its_revision_from_the_previous_value() {
        let mut v = vol();
        v.insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId(1), Revision(4)),
        )
        .unwrap();

        let mut plan = EditPlan::new(v.id());
        plan.set(GlobalCell::new(2, 2, 2), MaterialId(0));
        let outcome = v.apply_edit(&plan).unwrap();

        let rec = &outcome.bricks[0];
        assert!(rec.existed_before);
        assert_eq!(rec.before_revision, Revision(4));
        assert_eq!(rec.after_revision, Revision(1));
        assert_eq!(rec.cells_changed, 1);
    }

    #[test]
    fn emptying_a_brick_marks_modified_air() {
        let mut v = vol();
        v.insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId(2), Revision(1)),
        )
        .unwrap();

        let mut plan = EditPlan::new(v.id());
        for z in 0..32 {
            for y in 0..32 {
                for x in 0..32 {
                    plan.set(GlobalCell::new(x, y, z), MaterialId::AIR);
                }
            }
        }
        let outcome = v.apply_edit(&plan).unwrap();
        assert!(outcome.bricks[0].now_modified_air);
        assert_eq!(
            v.sample(GlobalCell::new(0, 0, 0)).unwrap(),
            Sample::Empty { modified: true }
        );
    }

    #[test]
    fn wrong_volume_and_failed_brick_are_rejected_without_mutation() {
        let mut v = vol();
        let other = EditPlan::new(VolumeId::new(99).unwrap());
        assert!(matches!(
            v.apply_edit(&other),
            Err(EditError::WrongVolume { .. })
        ));

        v.mark_failed(BrickCoord::new(0, 0, 0)).unwrap();
        let mut plan = EditPlan::new(v.id());
        plan.set(GlobalCell::new(0, 0, 0), MaterialId(1));
        assert!(matches!(
            v.apply_edit(&plan),
            Err(EditError::TouchesFailedBrick { .. })
        ));
        // Untouched: still failed, revision counter not advanced.
        assert_eq!(v.next_revision(), Revision(1));
    }

    #[test]
    fn out_of_bounds_edit_is_rejected() {
        let bounds = BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(0, 0, 0)).unwrap();
        let mut v = Volume::bounded(VolumeId::new(1).unwrap(), CellSizeCode::Quarter, bounds);
        let mut plan = EditPlan::new(v.id());
        plan.set(GlobalCell::new(100, 0, 0), MaterialId(1));
        assert!(matches!(
            v.apply_edit(&plan),
            Err(EditError::OutOfBounds { .. })
        ));
    }
}

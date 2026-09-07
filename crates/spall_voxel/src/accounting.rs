//! Memory accounting for a volume.
//!
//! Dense payloads uniquely owned by the volume and dense payloads still shared
//! with an outstanding [`crate::brick::BrickSnapshot`] are reported separately,
//! so a caller can see how much memory is "live storage" versus "retained for
//! in-flight jobs". The tally itself lives on [`Volume::memory_report`].

use crate::brick::DENSE_LAYER_BYTES;

/// A volume's storage footprint. Byte counts cover material-layer payloads
/// only; the small per-brick metadata is not included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryReport {
    /// Resident bricks stored as `Uniform` (metadata only, no cell array).
    pub uniform_bricks: usize,
    /// Resident bricks stored as `Dense`.
    pub dense_bricks: usize,
    /// Dense payload bytes uniquely owned by the volume.
    pub owned_dense_bytes: usize,
    /// Dense bricks whose payload is shared with a live snapshot.
    pub snapshot_shared_bricks: usize,
    /// Payload bytes of those shared bricks (counted once each).
    pub snapshot_shared_bytes: usize,
    /// Slots recorded as failed loads.
    pub failed_slots: usize,
}

impl MemoryReport {
    /// Bytes of one dense material layer (`32768` cells x `u16` = 64 KiB).
    pub const DENSE_BRICK_BYTES: usize = DENSE_LAYER_BYTES;

    /// Total material-layer bytes the volume is responsible for, owned plus
    /// snapshot-shared.
    pub fn total_dense_bytes(&self) -> usize {
        self.owned_dense_bytes + self.snapshot_shared_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brick::Brick;
    use crate::edit::EditPlan;
    use crate::volume::Volume;
    use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};

    fn vol() -> Volume {
        Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter)
    }

    #[test]
    fn uniform_bricks_use_no_dense_bytes() {
        let mut v = vol();
        v.insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId(1), Revision(1)),
        )
        .unwrap();
        v.insert_brick(
            BrickCoord::new(1, 0, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )
        .unwrap();
        let report = v.memory_report();
        assert_eq!(report.uniform_bricks, 2);
        assert_eq!(report.dense_bricks, 0);
        assert_eq!(report.total_dense_bytes(), 0);
    }

    #[test]
    fn dense_brick_bytes_move_to_the_snapshot_column_while_a_snapshot_is_held() {
        let mut v = vol();
        let mut plan = EditPlan::new(v.id());
        plan.set(GlobalCell::new(0, 0, 0), MaterialId(1));
        plan.set(GlobalCell::new(1, 0, 0), MaterialId(2));
        v.apply_edit(&plan).unwrap();

        let before = v.memory_report();
        assert_eq!(before.dense_bricks, 1);
        assert_eq!(before.owned_dense_bytes, MemoryReport::DENSE_BRICK_BYTES);
        assert_eq!(before.snapshot_shared_bytes, 0);

        let snap = v.snapshot_brick(BrickCoord::new(0, 0, 0)).unwrap().unwrap();
        let during = v.memory_report();
        assert_eq!(during.owned_dense_bytes, 0);
        assert_eq!(during.snapshot_shared_bricks, 1);
        assert_eq!(
            during.snapshot_shared_bytes,
            MemoryReport::DENSE_BRICK_BYTES
        );

        drop(snap);
        assert_eq!(
            v.memory_report().owned_dense_bytes,
            MemoryReport::DENSE_BRICK_BYTES
        );
    }

    #[test]
    fn failed_slots_are_counted() {
        let mut v = vol();
        v.mark_failed(BrickCoord::new(3, 3, 3)).unwrap();
        assert_eq!(v.memory_report().failed_slots, 1);
    }
}

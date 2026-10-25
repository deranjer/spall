//! A bounded tap on the terrain edits the replica applies, so presentation
//! (sound) can tell that a material was written somewhere without diffing the
//! volume. Nothing here is state the replica depends on.

use super::*;
use spall_voxel::Sample;

/// Most edits held between two drains; the oldest are dropped.
const MAX_EDITS: usize = 64;
/// Cell runs examined per transaction; a huge edit reports its first runs.
const MAX_RUNS_PER_TRANSACTION: usize = 256;

/// One cell run's first cell, before and after a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerrainEdit {
    pub cell: GlobalCell,
    pub from: MaterialId,
    pub to: MaterialId,
}

/// What was applied since the last [`ReplicaWorld::take_terrain_edits`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerrainEdits {
    pub edits: Vec<TerrainEdit>,
    /// Transactions that split a body off the terrain.
    pub splits: u32,
}

impl TerrainEdits {
    /// Notes the edits of `ops` against the terrain as it is before they
    /// apply.
    pub(super) fn observe(
        &mut self,
        ops: &[TopologyOp],
        volumes: &BTreeMap<u64, Volume>,
        terrain: VolumeId,
    ) {
        let Some(volume) = volumes.get(&terrain.get()) else {
            return;
        };
        let mut runs = 0;
        let mut split = false;
        for op in ops {
            match op {
                TopologyOp::CellRun {
                    volume: id,
                    start,
                    len,
                    material,
                } if *id == terrain && *len > 0 && runs < MAX_RUNS_PER_TRANSACTION => {
                    runs += 1;
                    let from = match volume.sample(*start) {
                        Ok(Sample::Filled(material)) => material,
                        Ok(Sample::Empty { .. }) => MaterialId::AIR,
                        _ => continue,
                    };
                    if from != *material {
                        if self.edits.len() >= MAX_EDITS {
                            self.edits.remove(0);
                        }
                        self.edits.push(TerrainEdit {
                            cell: *start,
                            from,
                            to: *material,
                        });
                    }
                }
                TopologyOp::SplitOff { source, .. }
                | TopologyOp::SplitOffBaseline { source, .. }
                | TopologyOp::SplitOffBulkBaseline { source, .. }
                    if *source == terrain =>
                {
                    split = true;
                }
                _ => {}
            }
        }
        self.splits += u32::from(split);
    }
}

impl ReplicaWorld {
    /// The terrain edits applied since the last call, oldest first.
    pub fn take_terrain_edits(&mut self) -> TerrainEdits {
        std::mem::take(&mut self.edit_events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::CellSizeCode;

    fn run(volume: VolumeId, x: i64, material: u16) -> TopologyOp {
        TopologyOp::CellRun {
            volume,
            start: GlobalCell::new(x, 0, 0),
            len: 1,
            material: MaterialId(material),
        }
    }

    #[test]
    fn a_run_reports_the_material_it_replaces_and_the_tap_stays_bounded() {
        let id = VolumeId::new(1).unwrap();
        let mut terrain = Volume::new(id, CellSizeCode::Quarter);
        terrain
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(99, 0, 0),
                MaterialId(223),
            ))
            .unwrap();
        let volumes = BTreeMap::from([(id.get(), terrain)]);
        let mut tap = TerrainEdits::default();
        // Ice cracks, then breaks; writing the material already there is not an edit.
        tap.observe(&[run(id, 0, 224), run(id, 1, 223)], &volumes, id);
        tap.observe(&[run(id, 2, 0)], &volumes, id);
        assert_eq!(
            tap.edits
                .iter()
                .map(|e| (e.from.0, e.to.0))
                .collect::<Vec<_>>(),
            [(223, 224), (223, 0)]
        );
        // Another volume's runs and a split off another volume are not noted.
        let other = VolumeId::new(2).unwrap();
        tap.observe(&[run(other, 3, 0)], &volumes, id);
        assert_eq!(tap.edits.len(), 2);
        assert_eq!(tap.splits, 0);
        // A flood keeps the newest and examines a bounded number of runs.
        let flood: Vec<_> = (0..100).map(|x| run(id, x, 224)).collect();
        for _ in 0..4 {
            tap.observe(&flood, &volumes, id);
        }
        assert_eq!(tap.edits.len(), MAX_EDITS);
    }
}

//! Deterministic physics benchmark fixtures for the T06 feasibility scenarios.
//!
//! Each is a pure function returning a [`spall_voxel::Volume`] (or a list of
//! them) plus the metadata a physics scene needs. They are intentionally small
//! enough to build and step on CPU CI; the heavier percentile sweep in
//! [`crate::report`] and the `collision-bench` binary repeats them many times
//! rather than making any single one large.

use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_voxel::{EditPlan, Volume, fixtures as vox};

/// Fixture material density, kg/m³ (roughly granite).
pub const STONE_DENSITY: f32 = 2600.0;
/// Cell edge for every fixture here: the 0.25 m terrain cell.
pub const CELL_M: f32 = 0.25;

/// The `hollow_tower` volume from `spall_voxel` — a 2-cell-thick stone shell,
/// `12 x 32 x 12` cells, straddling the eight-brick corner. Dropped as one
/// dynamic body it must keep its interior void (no convex-hull seal).
pub fn hollow_building(id: VolumeId) -> Volume {
    vox::hollow_tower(id)
}

/// A flat stone floor slab, `nx x nz` bricks wide and `thickness_cells` cells
/// thick on Y (its top surface at `y = thickness_cells * 0.25 m`). Used as the
/// fixed terrain every drop scene lands on; a shallow slab keeps the native
/// voxel collider build cheap.
pub fn floor_slab(id: VolumeId, nx_bricks: i64, nz_bricks: i64, thickness_cells: i64) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    let max = GlobalCell::new(nx_bricks * 32 - 1, thickness_cells - 1, nz_bricks * 32 - 1);
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(0, 0, 0),
        max,
        vox::STONE,
    ))
    .expect("floor edit");
    v
}

/// `count` small connected pieces for the "256 active pieces settle" scenario.
/// Each piece is a `2 x 2 x 2`-cell solid stone cube in its own single-brick
/// volume; the caller stacks them in a loose grid above the floor.
pub fn debris_pieces(first_id: u64, count: usize, edge_cells: i64) -> Vec<(VolumeId, Volume)> {
    (0..count)
        .map(|i| {
            let id = VolumeId::new(first_id + i as u64).expect("nonzero id");
            let mut v = Volume::new(id, CellSizeCode::Quarter);
            v.apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(edge_cells - 1, edge_cells - 1, edge_cells - 1),
                vox::STONE,
            ))
            .expect("piece edit");
            (id, v)
        })
        .collect()
}

/// A single connected body spanning `bricks_x * bricks_y * bricks_z` bricks for
/// the "64-brick connected body" stress case. `hollow` cuts a large interior
/// void so the body stays concave (and the merged-cuboid path stays honest)
/// while the greedy decomposition still collapses it to a handful of boxes.
pub fn connected_multibrick(id: VolumeId, bricks: [i64; 3], hollow: bool) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    let max = GlobalCell::new(bricks[0] * 32 - 1, bricks[1] * 32 - 1, bricks[2] * 32 - 1);
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(0, 0, 0),
        max,
        vox::STONE,
    ))
    .expect("solid edit");
    if hollow {
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(4, 4, 4),
            GlobalCell::new(bricks[0] * 32 - 5, bricks[1] * 32 - 5, bricks[2] * 32 - 5),
            MaterialId::AIR,
        ))
        .expect("hollow edit");
    }
    v
}

/// A thin upright stone wall, `thickness` cells on X, for the fast-object /
/// tunnelling test. One single-brick volume.
pub fn thin_wall(id: VolumeId, thickness: i64) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    v.apply_edit(&EditPlan::filled_box(
        id,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(thickness - 1, 23, 23),
        vox::STONE,
    ))
    .expect("wall edit");
    v
}

/// Uniform density lookup for the single-material fixtures.
pub fn stone_density(_m: MaterialId) -> f64 {
    STONE_DENSITY as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::occupancy::OccupancyGrid;

    fn vid(n: u64) -> VolumeId {
        VolumeId::new(n).unwrap()
    }

    #[test]
    fn debris_pieces_are_independent_small_cubes() {
        let pieces = debris_pieces(1000, 8, 2);
        assert_eq!(pieces.len(), 8);
        for (_, v) in &pieces {
            let g = OccupancyGrid::from_volume(v).unwrap().unwrap();
            assert_eq!(g.dims(), [2, 2, 2]);
            assert_eq!(g.solid_count(), 8);
        }
    }

    #[test]
    fn connected_multibrick_is_one_resident_body() {
        // 2 x 2 x 2 bricks = 8; keep the CI fixture small.
        let v = connected_multibrick(vid(1), [2, 2, 2], true);
        let g = OccupancyGrid::from_volume(&v).unwrap().unwrap();
        assert_eq!(g.dims(), [64, 64, 64]);
        // Hollow: strictly fewer solids than the full box.
        assert!(g.solid_count() < 64 * 64 * 64);
        assert!(g.solid_count() > 0);
    }

    #[test]
    fn floor_slab_is_solid() {
        let v = floor_slab(vid(2), 2, 2, 4);
        let g = OccupancyGrid::from_volume(&v).unwrap().unwrap();
        assert_eq!(g.solid_count(), 64 * 4 * 64);
        assert_eq!(g.dims(), [64, 4, 64]);
    }
}

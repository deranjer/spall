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

/// Where [`ladder_wall`] puts things, metres (`CELL_M` cells).
pub mod ladder {
    /// Floor top (feet height standing on the floor).
    pub const FLOOR_TOP_M: f64 = 1.0;
    /// Platform top: the top of the wall block and of the ladder.
    pub const PLATFORM_TOP_M: f64 = 5.0;
    /// The ladder's outer face (`x`); the wall face is one cell further `+x`.
    pub const LADDER_FACE_X_M: f64 = 7.75;
    /// Centre of the ladder's width on `z`.
    pub const LADDER_CENTRE_Z_M: f64 = 4.0;
    /// A feet spawn on the floor, 0.75 m in front of the ladder; face `+x`.
    pub const SPAWN_M: [f64; 3] = [7.0, FLOOR_TOP_M, LADDER_CENTRE_Z_M];
}

/// ENG-263's ladder scene: a 16 m x 8 m stone floor (top at
/// [`ladder::FLOOR_TOP_M`]), a 2 m thick, 4 m tall stone wall block whose top
/// is the platform (x 8..10 m, z 2..6 m, top at [`ladder::PLATFORM_TOP_M`]),
/// and a four-cell-wide (1 m) column of `ladder` material on the wall's
/// `-x` face (x 7.75..8 m, z 3.5..4.5 m) from the floor to the platform top.
/// Air is resident up to y = 8 m.
pub fn ladder_wall(id: VolumeId, ladder: MaterialId) -> Volume {
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    for (a, b, m) in [
        ((0, 0, 0), (63, 3, 31), vox::STONE),
        ((0, 4, 0), (63, 31, 31), MaterialId::AIR),
        ((32, 4, 8), (39, 19, 23), vox::STONE),
        ((31, 4, 14), (31, 19, 17), ladder),
    ] {
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(a.0, a.1, a.2),
            GlobalCell::new(b.0, b.1, b.2),
            m,
        ))
        .expect("ladder wall edit");
    }
    v
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

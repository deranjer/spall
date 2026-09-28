//! Client-local sky occupancy: the camera-local 128^3 x 0.5 m grid that makes
//! skylight visibility-aware (`spall_render::SkyVisibility`).
//!
//! It is *derived, render-only* data built from the replica's terrain volume.
//! It never feeds collision, topology, replication or persistence, and it is
//! rebuilt from scratch on each terrain rebuild rather than patched.
//!
//! Every solid voxel marks the cache cell that contains it (a cell is occupied
//! if *any* voxel in it is, so a one-voxel wall still blocks: over-occlusion,
//! never leakage). A brick that is not resident is **unknown**, not air:
//! depending on `absent_is_open` it is either declared open sky (a complete,
//! non-streamed world, where an absent brick is one that was never populated
//! because it is empty) or marked [`UNKNOWN_CELL`] so it blocks light instead
//! of leaking it.

use glam::Vec3;
use spall_core::{BRICK_EDGE, BrickCoord, LocalCell, MaterialId};
use spall_render::indirect::{LIGHT_CELL_SIZE_METRES, LIGHT_VOLUME_DIM, LightingVolume};
use spall_render::{UNKNOWN_CELL, cache_origin_around, mark_world_box};
use spall_voxel::Volume;

/// What a build found, for reporting and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkyOccupancyStats {
    pub resident_bricks: usize,
    pub unknown_bricks: usize,
    pub solid_cells: usize,
    pub unknown_cells: usize,
}

/// Build the occupancy grid around `center_m` from `volume`. `absent_is_open`
/// says whether a non-resident brick may be treated as air (see the module
/// docs).
pub fn build_sky_occupancy(
    volume: &Volume,
    center_m: [f64; 3],
    absent_is_open: bool,
) -> (LightingVolume, SkyOccupancyStats) {
    let center = Vec3::new(center_m[0] as f32, center_m[1] as f32, center_m[2] as f32);
    let origin = cache_origin_around(center);
    let mut grid = LightingVolume::empty(origin);
    let mut stats = SkyOccupancyStats::default();

    let cell_m = volume.cell_size().metres();
    let brick_m = cell_m * f64::from(BRICK_EDGE);
    let extent_m = f64::from(LIGHT_VOLUME_DIM) * f64::from(LIGHT_CELL_SIZE_METRES);
    let lo_m = [
        f64::from(origin.x),
        f64::from(origin.y),
        f64::from(origin.z),
    ];
    let brick_range = |axis: usize| {
        let lo = (lo_m[axis] / brick_m).floor() as i64;
        let hi = ((lo_m[axis] + extent_m) / brick_m).ceil() as i64;
        lo..hi
    };

    for bz in brick_range(2) {
        for by in brick_range(1) {
            for bx in brick_range(0) {
                let coord = BrickCoord {
                    x: bx,
                    y: by,
                    z: bz,
                };
                let brick_min = [
                    bx as f64 * brick_m,
                    by as f64 * brick_m,
                    bz as f64 * brick_m,
                ];
                let brick_box = |grid: &mut LightingVolume, marker: u32| {
                    mark_world_box(
                        grid,
                        Vec3::new(
                            brick_min[0] as f32,
                            brick_min[1] as f32,
                            brick_min[2] as f32,
                        ),
                        Vec3::new(
                            (brick_min[0] + brick_m) as f32,
                            (brick_min[1] + brick_m) as f32,
                            (brick_min[2] + brick_m) as f32,
                        ),
                        marker,
                    );
                };
                // Out of the volume's bounds counts as not resident.
                let Some(snapshot) = volume.snapshot_brick(coord).ok().flatten() else {
                    stats.unknown_bricks += 1;
                    if !absent_is_open {
                        brick_box(&mut grid, UNKNOWN_CELL);
                        stats.unknown_cells += 1;
                    }
                    continue;
                };
                stats.resident_bricks += 1;
                if !snapshot.is_dense() {
                    // Uniform brick: one material for every cell.
                    let material = snapshot.get(LocalCell::new(0, 0, 0).expect("in range"));
                    if material != MaterialId::AIR {
                        brick_box(&mut grid, u32::from(material.0));
                        stats.solid_cells += 1;
                    }
                    continue;
                }
                for index in 0..(BRICK_EDGE * BRICK_EDGE * BRICK_EDGE) as u16 {
                    let Some(local) = LocalCell::from_linear_index(index) else {
                        continue;
                    };
                    let material = snapshot.get(local);
                    if material == MaterialId::AIR {
                        continue;
                    }
                    let min = Vec3::new(
                        (brick_min[0] + f64::from(local.x()) * cell_m) as f32,
                        (brick_min[1] + f64::from(local.y()) * cell_m) as f32,
                        (brick_min[2] + f64::from(local.z()) * cell_m) as f32,
                    );
                    mark_world_box(
                        &mut grid,
                        min,
                        min + Vec3::splat(cell_m as f32),
                        u32::from(material.0),
                    );
                    stats.solid_cells += 1;
                }
            }
        }
    }
    (grid, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, Revision, VolumeId};
    use spall_voxel::Brick;

    fn volume() -> Volume {
        Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter)
    }

    fn at(grid: &LightingVolume, p: Vec3) -> u32 {
        let cell = grid.world_to_cell(p);
        let d = LIGHT_VOLUME_DIM as i32;
        assert!(
            cell.cmpge(glam::IVec3::ZERO).all() && cell.cmplt(glam::IVec3::splat(d)).all(),
            "{p:?} is outside the cache"
        );
        grid.cells()[(cell.x + d * (cell.y + d * cell.z)) as usize]
    }

    #[test]
    fn a_solid_brick_marks_its_cells_and_a_resident_air_brick_stays_open() {
        let mut v = volume();
        // Brick (0,0,0) covers [0, 8) m; make it solid stone. Brick (1,0,0)
        // covers [8, 16) m; resident air.
        v.insert_brick(
            BrickCoord { x: 0, y: 0, z: 0 },
            Brick::uniform(MaterialId(1), Revision::ZERO),
        )
        .unwrap();
        v.insert_brick(BrickCoord { x: 1, y: 0, z: 0 }, Brick::empty())
            .unwrap();
        let (grid, stats) = build_sky_occupancy(&v, [4.0, 1.0, 4.0], true);
        assert_eq!(at(&grid, Vec3::new(4.0, 4.0, 4.0)), 1);
        assert_eq!(
            at(&grid, Vec3::new(12.0, 4.0, 4.0)),
            0,
            "resident air is open"
        );
        assert_eq!(stats.resident_bricks, 2);
    }

    #[test]
    fn identical_inputs_build_identical_grids_so_idle_rebuilds_cost_nothing() {
        let mut v = volume();
        let mut brick = Brick::empty();
        brick.set_cell(LocalCell::new(3, 3, 3).unwrap(), MaterialId(2));
        v.insert_brick(BrickCoord { x: 0, y: 0, z: 0 }, brick)
            .unwrap();
        let (a, _) = build_sky_occupancy(&v, [4.0, 1.0, 4.0], true);
        let (b, _) = build_sky_occupancy(&v, [4.0, 1.0, 4.0], true);
        assert!(a.origin() == b.origin() && a.cells() == b.cells());
        // An edit changes the grid.
        let mut edited = Brick::empty();
        edited.set_cell(LocalCell::new(3, 3, 3).unwrap(), MaterialId(2));
        edited.set_cell(LocalCell::new(9, 9, 9).unwrap(), MaterialId(2));
        let mut v2 = volume();
        v2.insert_brick(BrickCoord { x: 0, y: 0, z: 0 }, edited)
            .unwrap();
        let (c, _) = build_sky_occupancy(&v2, [4.0, 1.0, 4.0], true);
        assert!(c.cells() != a.cells());
    }

    #[test]
    fn a_single_voxel_wall_marks_exactly_one_cache_cell() {
        let mut v = volume();
        let mut brick = Brick::empty();
        brick.set_cell(LocalCell::new(9, 3, 5).unwrap(), MaterialId(2));
        v.insert_brick(BrickCoord { x: 0, y: 0, z: 0 }, brick)
            .unwrap();
        let (grid, _) = build_sky_occupancy(&v, [4.0, 1.0, 4.0], true);
        // Voxel (9,3,5) at 0.25 m is world [2.25, 2.5) x [0.75, 1.0) x [1.25, 1.5).
        assert_eq!(at(&grid, Vec3::new(2.3, 0.8, 1.3)), 2);
        assert_eq!(grid.cells().iter().filter(|c| **c != 0).count(), 1);
    }

    #[test]
    fn absent_bricks_are_open_only_when_the_world_is_complete() {
        let mut v = volume();
        v.insert_brick(BrickCoord { x: 0, y: 0, z: 0 }, Brick::empty())
            .unwrap();
        // Brick (1,0,0) is absent.
        let probe = Vec3::new(12.0, 4.0, 4.0);
        let (open, open_stats) = build_sky_occupancy(&v, [4.0, 1.0, 4.0], true);
        assert_eq!(at(&open, probe), 0);
        let (closed, closed_stats) = build_sky_occupancy(&v, [4.0, 1.0, 4.0], false);
        assert_eq!(
            at(&closed, probe),
            UNKNOWN_CELL,
            "unknown must block, not leak"
        );
        assert_eq!(
            at(&closed, Vec3::new(4.0, 4.0, 4.0)),
            0,
            "resident air stays open"
        );
        assert!(closed_stats.unknown_cells > 0 && open_stats.unknown_cells == 0);
        assert_eq!(open_stats.unknown_bricks, closed_stats.unknown_bricks);
    }
}

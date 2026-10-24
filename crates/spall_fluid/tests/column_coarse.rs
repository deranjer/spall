//! Water at 1 m columns (4 x 4 terrain cells each, see
//! `ColumnConfig::column_cells`). A wall one terrain cell thick still holds, a
//! canal one column wide carries water, and volume is conserved.

use std::sync::Arc;

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::columns::{COARSE_CELLS, CoarseVolumeTerrain, ColumnConfig, ColumnWorld};
use spall_voxel::{Brick, EditPlan, Volume};

const DT: f64 = 1.0 / 60.0;
/// Terrain cells: 128 x 64 x 128 (one coarse region of 32 x 32 columns).
const N: i64 = 128;
const NY: i64 = 64;

fn terrain(solid: impl Fn(i64, i64, i64) -> bool) -> Arc<Volume> {
    let id = VolumeId::new(0x1c0).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    for bz in 0..N / 32 {
        for by in 0..NY / 32 {
            for bx in 0..N / 32 {
                volume
                    .insert_brick(
                        BrickCoord::new(bx, by, bz),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )
                    .unwrap();
            }
        }
    }
    let mut plan = EditPlan::new(id);
    for z in 0..N {
        for y in 0..NY {
            for x in 0..N {
                let rim = x == 0 || z == 0 || x == N - 1 || z == N - 1;
                if y < 2 || (rim && y < 40) || solid(x, y, z) {
                    plan.set(GlobalCell::new(x, y, z), MaterialId(1));
                }
            }
        }
    }
    volume.apply_edit(&plan).unwrap();
    Arc::new(volume)
}

fn coarse(volume: Arc<Volume>) -> ColumnWorld {
    let config = ColumnConfig {
        column_cells: COARSE_CELLS as u32,
        ..ColumnConfig::default()
    };
    ColumnWorld::with_terrain(
        Arc::new(CoarseVolumeTerrain::new(volume, 0, NY as i32)),
        config,
    )
}

/// Fills coarse columns `x0..x1` (all z inside the rim) to height `top`.
fn fill(world: &mut ColumnWorld, x0: i64, x1: i64, top: f64) {
    world.allocate_rect(0, 0, N / COARSE_CELLS - 1, N / COARSE_CELLS - 1);
    for z in 1..N / COARSE_CELLS - 1 {
        for x in x0..x1 {
            world.fill_column_to(x, z, top);
        }
    }
}

fn surface(world: &ColumnWorld, x: i64, z: i64) -> f64 {
    world
        .spans_in_column(x, z)
        .iter()
        .map(|s| s.floor_y as f64 + s.depth)
        .fold(f64::MIN, f64::max)
}

fn run(world: &mut ColumnWorld, seconds: f64) {
    for _ in 0..(seconds / DT) as u32 {
        world.step(DT);
    }
}

#[test]
fn a_wall_one_terrain_cell_thick_holds_water() {
    // A wall at terrain x = 62 (inside coarse column 15), up to y = 30.
    let mut world = coarse(terrain(|x, y, _| x == 62 && y < 30));
    fill(&mut world, 1, 15, 20.0);
    let start = world.total_volume();
    run(&mut world, 20.0);
    assert!(
        world.column_volume(20, 16) == 0.0,
        "water leaked past the wall: {}",
        world.column_volume(20, 16)
    );
    assert!((world.total_volume() - start).abs() < 1e-9 * start);
}

#[test]
fn a_canal_one_column_wide_carries_water_and_volume_is_conserved() {
    // A dividing wall at terrain x 62..64 with a gap one column (4 cells,
    // aligned) wide at z 64..68.
    let mut world = coarse(terrain(|x, y, z| {
        (62..64).contains(&x) && y < 30 && !(64..68).contains(&z)
    }));
    fill(&mut world, 1, 15, 20.0);
    let start = world.total_volume();
    run(&mut world, 300.0);
    let (left, right) = (surface(&world, 8, 8), surface(&world, 24, 24));
    assert!(
        right > 2.5,
        "water came through the canal: right side at {right}"
    );
    assert!(
        (left - right).abs() < 0.5,
        "the two sides are levelling: {left} against {right}"
    );
    assert!((world.total_volume() - start).abs() < 1e-9 * start);
}

#[test]
fn a_spring_adds_its_volume_in_terrain_cells() {
    let mut world = coarse(terrain(|_, _, _| false));
    world.allocate_rect(0, 0, N / COARSE_CELLS - 1, N / COARSE_CELLS - 1);
    let spring = GlobalCell::new(10, 2, 10);
    for _ in 0..600 {
        world.add_flow(spring, 16.0 * DT).unwrap();
        world.step(DT);
    }
    // 160 terrain cells^3 over about 30 x 30 columns of 16 cells^2 each.
    assert!(
        (world.total_volume() - 160.0).abs() < 1e-6,
        "{}",
        world.total_volume()
    );
}

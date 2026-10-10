//! Bodies float: buoyancy and drag from the column water (ENG-168 follow-up).

use glam::DQuat;
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_sim::column_water::ColumnWaterSetup;
use spall_sim::playground::spawn_box_body;
use spall_sim::{BodyPose, Simulation, SimulationConfig, fixtures};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, EditPlan, Volume};

const WOOD: MaterialId = MaterialId(3);
const STONE: MaterialId = MaterialId(1);

/// 16 m x 8 m x 4 m basin with 2.5 m of water over a 0.5 m floor.
fn pool_sim() -> Simulation {
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    for bx in 0..2 {
        terrain
            .insert_brick(
                BrickCoord::new(bx, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
    }
    let mut plan = EditPlan::new(terrain.id());
    let mut water = Vec::new();
    for z in 0..16 {
        for y in 0..32 {
            for x in 0..64 {
                let wall = x == 0 || x == 63 || z == 0 || z == 15;
                if y < 2 || (wall && y < 24) {
                    plan.set(GlobalCell::new(x, y, z), STONE);
                } else if y < 12 {
                    water.push(GlobalCell::new(x, y, z));
                }
            }
        }
    }
    terrain.apply_edit(&plan).unwrap();
    let mut world = fixtures::flat_terrain_setup();
    world.terrain = terrain;
    world.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 31, 15));
    world.anchor = AnchorPlane::at(0);
    let mut config = SimulationConfig::new(world);
    config.column_water = Some(ColumnWaterSetup::new(0, 32, water));
    Simulation::new(config).unwrap()
}

fn y_of(sim: &Simulation, id: spall_core::EntityId) -> f64 {
    sim.world()
        .bodies()
        .find(|b| b.entity == Some(id))
        .expect("body exists")
        .pose
        .translation_m[1]
}

/// Runs `ticks` more ticks and returns how far the body's height wandered.
fn height_range_over(sim: &mut Simulation, id: spall_core::EntityId, ticks: u32) -> f64 {
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for _ in 0..ticks {
        sim.tick().unwrap();
        let y = y_of(sim, id);
        lo = lo.min(y);
        hi = hi.max(y);
    }
    hi - lo
}

fn tilt_deg(sim: &Simulation, id: spall_core::EntityId) -> f64 {
    let b = sim
        .world()
        .bodies()
        .find(|b| b.entity == Some(id))
        .expect("body exists");
    (b.pose.rotation * glam::DVec3::Y)
        .dot(glam::DVec3::Y)
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
}

/// A flat 4 m x 3 m x 0.5 m raft of wood (700 kg/m3) floats flat with 70% of
/// its thickness below the surface; a stone block (2600 kg/m3) sinks.
#[test]
fn a_wood_raft_floats_flat_at_its_density_and_stone_sinks() {
    let mut sim = pool_sim();
    let raft = spawn_box_body(
        sim.world_mut(),
        [16, 2, 12],
        WOOD,
        700.0,
        BodyPose::new(DQuat::IDENTITY, [3.0, 4.0, 0.5]),
    )
    .unwrap();
    let stone = spawn_box_body(
        sim.world_mut(),
        [6, 6, 6],
        STONE,
        2600.0,
        BodyPose::new(DQuat::IDENTITY, [10.0, 4.0, 2.0]),
    )
    .unwrap();
    let mut max_in_water = 0;
    for _ in 0..(60 * 20) {
        let report = sim.tick().unwrap();
        max_in_water = max_in_water.max(report.buoyancy.unwrap().bodies_in_water);
    }
    assert!(max_in_water >= 2, "both bodies met the water");
    // The surface is at 3.0 m; the raft is 0.5 m thick, so its base sits 0.35 m under.
    let base = y_of(&sim, raft);
    assert!(
        (base - (3.0 - 0.35)).abs() < 0.08,
        "the raft floats with its base at {base} m (expected ~2.65)"
    );
    assert!(
        tilt_deg(&sim, raft) < 3.0,
        "and stays flat: {} deg",
        tilt_deg(&sim, raft)
    );
    // Settled: its height does not wander over the next two seconds.
    let wander = height_range_over(&mut sim, raft, 120);
    assert!(wander < 0.02, "the raft still moves {wander} m");
    let stone_base = y_of(&sim, stone);
    assert!(
        stone_base < 0.7,
        "stone sinks to the floor: base at {stone_base} m"
    );
}

/// A raft dropped from height plunges, bobs, and comes to rest.
#[test]
fn bobbing_is_damped_and_a_dry_body_feels_nothing() {
    let mut sim = pool_sim();
    let raft = spawn_box_body(
        sim.world_mut(),
        [16, 2, 12],
        WOOD,
        700.0,
        BodyPose::new(DQuat::IDENTITY, [3.0, 5.5, 0.5]),
    )
    .unwrap();
    let first = sim.tick().unwrap().buoyancy.unwrap();
    assert_eq!(first.bodies_in_water, 0, "nothing in water before it lands");
    for _ in 0..(60 * 20) {
        sim.tick().unwrap();
    }
    let wander = height_range_over(&mut sim, raft, 300);
    assert!(
        wander < 0.02,
        "still bobbing after 20 s: moved {wander} m in 5 s"
    );
    assert!(tilt_deg(&sim, raft) < 3.0);
}

/// A load on one end sinks that end: buoyancy acts at the centre of the
/// submerged volume, so it produces a torque.
#[test]
fn an_off_centre_load_tilts_the_raft() {
    let mut sim = pool_sim();
    let raft = spawn_box_body(
        sim.world_mut(),
        [16, 2, 12],
        WOOD,
        700.0,
        BodyPose::new(DQuat::IDENTITY, [3.0, 3.2, 0.5]),
    )
    .unwrap();
    // A dense body welded to nothing: instead make the raft itself lopsided by
    // dropping a stone block on one end after it settles.
    for _ in 0..(60 * 8) {
        sim.tick().unwrap();
    }
    assert!(tilt_deg(&sim, raft) < 3.0);
    let _stone = spawn_box_body(
        sim.world_mut(),
        [4, 4, 4],
        STONE,
        2600.0,
        BodyPose::new(DQuat::IDENTITY, [3.2, 3.5, 1.0]),
    )
    .unwrap();
    let mut worst = 0.0f64;
    for _ in 0..(60 * 10) {
        sim.tick().unwrap();
        worst = worst.max(tilt_deg(&sim, raft));
    }
    assert!(
        worst > 3.0,
        "a heavy block on one end must tilt the raft: {worst} deg"
    );
}

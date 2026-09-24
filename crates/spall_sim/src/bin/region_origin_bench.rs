//! Measures one authoritative SimWorld at the origin and in a distant local
//! physics frame, including localized terrain collision and body pose sync.

use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_physics::{PhysicsConfig, PhysicsOrigin};
use spall_sim::fixtures::{STONE, stone_manifest};
use spall_sim::{BodyPose, Simulation, SimulationConfig, WorldSetup};
use spall_structure::AnchorPlane;
use spall_voxel::{EditPlan, Volume};

const CELL_M: f64 = 0.25;
const WORLD_OFFSET_M: f64 = 100_000.0;
const CELL_OFFSET: i64 = 400_000;
const SETTLE_STEPS: usize = 240;

fn main() -> std::process::ExitCode {
    let baseline = run_world(0, PhysicsOrigin::ZERO);
    let rebased_origin = PhysicsOrigin::new([WORLD_OFFSET_M, 0.0, 0.0]).unwrap();
    let rebased = run_world(CELL_OFFSET, rebased_origin);

    let rebased_relative_position = [
        rebased.position_m[0] - WORLD_OFFSET_M,
        rebased.position_m[1],
        rebased.position_m[2],
    ];
    let position_error_m = distance(rebased_relative_position, baseline.position_m);
    let velocity_error_m_s = distance(rebased.velocity_m_s, baseline.velocity_m_s);
    let passed = baseline.contact_pairs > 0
        && rebased.contact_pairs > 0
        && baseline.finite
        && rebased.finite
        && position_error_m < 0.01
        && velocity_error_m_s < 0.002;

    println!(
        "{{\"scenario\":\"sim_world_local_physics_origin\",\"passed\":{passed},\
         \"world_offset_m\":{WORLD_OFFSET_M},\"settle_steps\":{SETTLE_STEPS},\
         \"solid_cells\":{},\"baseline_contact_pairs\":{},\
         \"rebased_contact_pairs\":{},\"relative_position_error_m\":{position_error_m},\
         \"velocity_error_m_s\":{velocity_error_m_s},\
         \"baseline_position_m\":{},\"rebased_position_m\":{},\
         \"finite\":{}}}",
        baseline.solid_cells,
        baseline.contact_pairs,
        rebased.contact_pairs,
        vector(baseline.position_m),
        vector(rebased.position_m),
        baseline.finite && rebased.finite,
    );

    if passed {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

struct ResultRow {
    position_m: [f64; 3],
    velocity_m_s: [f64; 3],
    solid_cells: u64,
    contact_pairs: usize,
    finite: bool,
}

fn run_world(cell_offset: i64, origin: PhysicsOrigin) -> ResultRow {
    let min = GlobalCell::new(cell_offset, 0, 0);
    let max = GlobalCell::new(cell_offset + 95, 9, 23);
    let terrain_id = VolumeId::new(1).unwrap();
    let mut terrain = Volume::new(terrain_id, CellSizeCode::Quarter);
    terrain
        .apply_edit(&EditPlan::filled_box(
            terrain_id,
            min,
            GlobalCell::new(cell_offset + 95, 1, 23),
            STONE,
        ))
        .unwrap();
    terrain
        .apply_edit(&EditPlan::filled_box(
            terrain_id,
            GlobalCell::new(cell_offset, 2, 0),
            max,
            MaterialId::AIR,
        ))
        .unwrap();
    let setup = WorldSetup {
        terrain,
        terrain_collider_region: (min, max),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    };
    let mut sim = Simulation::new_with_physics_origin(SimulationConfig::new(setup), origin)
        .expect("world setup");
    let entity = sim
        .world_mut()
        .spawn_body(
            |id| {
                let mut body = Volume::new(id, CellSizeCode::Quarter);
                body.apply_edit(&EditPlan::filled_box(
                    id,
                    GlobalCell::new(0, 0, 0),
                    GlobalCell::new(3, 3, 3),
                    STONE,
                ))
                .unwrap();
                body
            },
            BodyPose::new(
                glam::DQuat::IDENTITY,
                [cell_offset as f64 * CELL_M + 10.0, 4.0, 2.0],
            ),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .expect("spawn body in physics frame");

    let mut contact_pairs = 0;
    let mut finite = true;
    for _ in 0..SETTLE_STEPS {
        sim.step_physics_only();
        contact_pairs = sim.world().physics().contact_pair_count();
        let body = sim.world().body(entity).unwrap();
        finite &= body.pose.translation_m.iter().all(|v| v.is_finite())
            && body.linvel_m_s.iter().all(|v| v.is_finite());
    }
    let body = sim.world().body(entity).unwrap();
    ResultRow {
        position_m: body.pose.translation_m,
        velocity_m_s: body.linvel_m_s,
        solid_cells: spall_sim::solid_cells(&body.volume),
        contact_pairs,
        finite,
    }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.into_iter()
        .zip(b)
        .map(|(a, b)| (a - b).powi(2))
        .sum::<f64>()
        .sqrt()
}

fn vector(value: [f64; 3]) -> String {
    format!("[{},{},{}]", value[0], value[1], value[2])
}

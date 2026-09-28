use spall_physics::fixtures::{CELL_M, STONE_DENSITY, debris_pieces};
use spall_physics::{
    BodyKind, BodySpec, OccupancyGrid, PhysicsConfig, PhysicsOrigin, PhysicsRegionSet,
    Representation,
};

const BEFORE_STEPS: usize = 20;
const AFTER_STEPS: usize = 60;

fn main() -> std::process::ExitCode {
    let (_, volume) = debris_pieces(301, 1, 4).pop().expect("fixture body");
    let grid = OccupancyGrid::from_volume(&volume)
        .expect("extract occupancy")
        .expect("solid body");
    let config = PhysicsConfig::default();
    let mut regions = PhysicsRegionSet::new();
    regions
        .create_region(
            1,
            PhysicsOrigin::new([100_000.0, 0.0, 0.0]).unwrap(),
            config,
        )
        .unwrap();
    regions
        .create_region(
            2,
            PhysicsOrigin::new([100_112.0, 0.0, 0.0]).unwrap(),
            config,
        )
        .unwrap();
    regions
        .create_region(
            3,
            PhysicsOrigin::new([100_000.0, 0.0, 0.0]).unwrap(),
            config,
        )
        .unwrap();
    let source_world_pose = [100_124.25, 40.0, 10.0];
    let linear_velocity = [1.25, -0.5, 0.75];
    let angular_velocity = [0.1, 0.2, -0.15];
    let source_id = regions
        .add_body(
            1,
            body_spec(grid.clone(), linear_velocity),
            source_world_pose,
        )
        .unwrap();
    regions
        .set_body_velocity(source_id, linear_velocity, angular_velocity)
        .unwrap();
    let control_id = regions
        .add_body(
            3,
            body_spec(grid.clone(), linear_velocity),
            source_world_pose,
        )
        .unwrap();
    regions
        .set_body_velocity(control_id, linear_velocity, angular_velocity)
        .unwrap();
    for _ in 0..BEFORE_STEPS {
        regions.step_all();
    }
    let source_before = regions.state(source_id).unwrap();
    let destination_id = regions
        .transfer_body(
            source_id,
            2,
            body_spec(grid.clone(), linear_velocity),
            0.002,
            0.0001,
        )
        .unwrap();
    for _ in 0..AFTER_STEPS {
        regions.step_all();
    }
    let moved = regions.state(destination_id).unwrap();
    let control = regions.state(control_id).unwrap();
    let position_error = distance(moved.world_translation_m, control.world_translation_m);
    let velocity_error = distance(
        moved.local.linvel_m_s.map(f64::from),
        control.local.linvel_m_s.map(f64::from),
    );
    let angular_velocity_error = distance(
        moved.local.angvel_rad_s.map(f64::from),
        control.local.angvel_rad_s.map(f64::from),
    );
    let rotation_error = distance4(
        moved.local.rotation.map(f64::from),
        control.local.rotation.map(f64::from),
    );
    let exact_one_owner = regions.active_body_count(1).unwrap() == 0
        && regions.active_body_count(2).unwrap() == 1
        && regions.active_body_count(3).unwrap() == 1;
    let disjoint_ids =
        source_id != destination_id && source_id != control_id && destination_id != control_id;
    let passed = exact_one_owner
        && disjoint_ids
        && moved.local.is_finite()
        && position_error < 0.02
        && velocity_error < 0.002
        && angular_velocity_error < 0.002
        && rotation_error < 0.001
        && (source_before.world_translation_m[0] - 100_124.25).abs() < 1.0;
    println!(
        "{{\"scenario\":\"simultaneous_rebased_regions_and_transfer\",\"passed\":{passed},\
         \"regions_stepped\":3,\"before_handoff_steps\":{BEFORE_STEPS},\
         \"after_handoff_steps\":{AFTER_STEPS},\"source_to_destination_position_error_m\":{position_error},\
         \"velocity_error_m_s\":{velocity_error},\"angular_velocity_error_rad_s\":{angular_velocity_error},\
         \"rotation_error\":{rotation_error},\"exactly_one_active_owner\":{exact_one_owner},\
         \"region_body_ids_disjoint\":{disjoint_ids},\"destination_namespace\":{}}}",
        destination_id.namespace(),
    );
    if passed {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn body_spec(grid: OccupancyGrid, linvel_m_s: [f32; 3]) -> BodySpec {
    BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid,
        cell_m: CELL_M,
        density_kg_m3: STONE_DENSITY,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s,
    }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn distance4(a: [f64; 4], b: [f64; 4]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2) + (a[3] - b[3]).powi(2))
        .sqrt()
}

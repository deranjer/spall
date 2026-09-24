//! ENG-31 adapter-level spike for moving one voxel rigid body between local
//! physics origins. This does not transfer authoritative SimWorld ownership.

use spall_physics::collider::Representation;
use spall_physics::coordinates::PhysicsOrigin;
use spall_physics::fixtures::{CELL_M, STONE_DENSITY, debris_pieces};
use spall_physics::occupancy::OccupancyGrid;
use spall_physics::world::{BodyKind, BodySpec, BodyState, PhysicsConfig, PhysicsWorld};

const SOURCE_ORIGIN_M: [f64; 3] = [100_000.0, 0.0, 0.0];
const DESTINATION_ORIGIN_M: [f64; 3] = [100_112.0, 0.0, 0.0];
const DT_STEPS_BEFORE_HANDOFF: usize = 20;
const DT_STEPS_AFTER_HANDOFF: usize = 60;

fn main() -> std::process::ExitCode {
    let source_origin = PhysicsOrigin::new(SOURCE_ORIGIN_M).expect("finite source origin");
    let destination_origin =
        PhysicsOrigin::new(DESTINATION_ORIGIN_M).expect("finite destination origin");
    let (_, volume) = debris_pieces(301, 1, 4).pop().expect("one fixture body");
    let grid = OccupancyGrid::from_volume(&volume)
        .expect("extract fixture occupancy")
        .expect("fixture is solid");
    let source_geometry_digest = geometry_digest(&grid);
    let initial_world_pose = [100_124.25, 40.0, 10.0];
    let rotation = [0.0, 0.0, 0.1305262, 0.9914449];
    let linear_velocity = [1.25, -0.5, 0.75];
    let angular_velocity = [0.1, 0.2, -0.15];

    let mut source = PhysicsWorld::new_in_namespace(PhysicsConfig::default(), 1);
    let source_id = add_body(
        &mut source,
        grid.clone(),
        source_origin,
        initial_world_pose,
        rotation,
        linear_velocity,
        angular_velocity,
        CELL_M,
    );
    let mut control = PhysicsWorld::new_in_namespace(PhysicsConfig::default(), 2);
    let control_id = add_body(
        &mut control,
        grid.clone(),
        source_origin,
        initial_world_pose,
        rotation,
        linear_velocity,
        angular_velocity,
        CELL_M,
    );
    let mut destination = PhysicsWorld::new_in_namespace(PhysicsConfig::default(), 3);

    // A rejected destination spec must leave the source as the only owner.
    let rejected = stage_body(
        &mut destination,
        grid.clone(),
        destination_origin,
        source.body_state(source_id),
        source_origin,
        0.0,
    );
    let failure_preserved_source = rejected.is_err()
        && source.active_body_count() == 1
        && destination.active_body_count() == 0;
    if !failure_preserved_source {
        eprintln!("physics-transfer: injected destination rejection changed ownership");
        return std::process::ExitCode::FAILURE;
    }

    for _ in 0..DT_STEPS_BEFORE_HANDOFF {
        source.step();
        control.step();
    }
    let handoff_state = source.body_state(source_id);
    let destination_id = match stage_body(
        &mut destination,
        grid.clone(),
        destination_origin,
        handoff_state,
        source_origin,
        CELL_M,
    ) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("physics-transfer: valid destination rejected: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let staged = destination.body_state(destination_id);
    let staged_world_position = destination_origin.to_world_f64(staged.translation_m);
    let expected_world_position = source_origin.to_world_f64(handoff_state.translation_m);
    let pose_error_m = distance(staged_world_position, expected_world_position);
    let velocity_error_m_s = distance(
        staged.linvel_m_s.map(f64::from),
        handoff_state.linvel_m_s.map(f64::from),
    );
    let angular_velocity_error_rad_s = distance(
        staged.angvel_rad_s.map(f64::from),
        handoff_state.angvel_rad_s.map(f64::from),
    );
    let rotation_error = distance(
        staged.rotation.map(f64::from),
        handoff_state.rotation.map(f64::from),
    );
    let destination_geometry_digest = geometry_digest(&grid);
    if pose_error_m > 0.002
        || velocity_error_m_s > 0.0001
        || angular_velocity_error_rad_s > 0.0001
        || rotation_error > 0.0001
        || destination_geometry_digest != source_geometry_digest
    {
        eprintln!("physics-transfer: staged state or geometry differs from source");
        destination.retire_body(destination_id);
        return std::process::ExitCode::FAILURE;
    }

    // Inject a rejection after destination construction but before source
    // retirement. Roll back the staged collider and prove the source remains.
    destination.retire_body(destination_id);
    let rollback_preserved_source =
        source.active_body_count() == 1 && destination.active_body_count() == 0;
    if !rollback_preserved_source {
        eprintln!("physics-transfer: staged rollback did not preserve source ownership");
        return std::process::ExitCode::FAILURE;
    }
    let destination_id = match stage_body(
        &mut destination,
        grid.clone(),
        destination_origin,
        handoff_state,
        source_origin,
        CELL_M,
    ) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("physics-transfer: retry after rollback failed: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Destination collider and kinematics are validated before this retirement.
    source.retire_body(source_id);
    for _ in 0..DT_STEPS_AFTER_HANDOFF {
        destination.step();
        control.step();
    }
    let transferred = destination.body_state(destination_id);
    let control_state = control.body_state(control_id);
    let final_position_error_m = distance(
        destination_origin.to_world_f64(transferred.translation_m),
        source_origin.to_world_f64(control_state.translation_m),
    );
    let final_velocity_error_m_s = distance(
        transferred.linvel_m_s.map(f64::from),
        control_state.linvel_m_s.map(f64::from),
    );
    let exactly_one_active_owner = source.active_body_count() == 0
        && destination.active_body_count() == 1
        && control.active_body_count() == 1;
    let region_ids_are_disjoint = source_id.namespace() != destination_id.namespace()
        && source_id != destination_id
        && control_id != destination_id;
    let passed = exactly_one_active_owner
        && region_ids_are_disjoint
        && rollback_preserved_source
        && source_geometry_digest == destination_geometry_digest
        && final_position_error_m < 0.02
        && final_velocity_error_m_s < 0.002
        && transferred.is_finite();

    println!(
        "{{\"scenario\":\"two_origin_body_transfer\",\"passed\":{passed},\
         \"source_origin_m\":{},\"destination_origin_m\":{},\
         \"solid_cells\":{},\"geometry_digest\":\"{source_geometry_digest:016x}\",\
         \"invalid_spec_preserved_source\":{failure_preserved_source},\
         \"staged_rollback_preserved_source\":{rollback_preserved_source},\
         \"pose_error_m\":{pose_error_m},\"velocity_error_m_s\":{velocity_error_m_s},\
         \"angular_velocity_error_rad_s\":{angular_velocity_error_rad_s},\
         \"rotation_error\":{rotation_error},\
         \"final_control_position_error_m\":{final_position_error_m},\
         \"final_control_velocity_error_m_s\":{final_velocity_error_m_s},\
         \"exactly_one_active_owner_after_handoff\":{exactly_one_active_owner},\
         \"region_body_ids_disjoint\":{region_ids_are_disjoint},\
         \"source_body_namespace\":{},\"destination_body_namespace\":{}}}",
        vector(source_origin.world_m()),
        vector(destination_origin.world_m()),
        grid.solid_count(),
        source_id.namespace(),
        destination_id.namespace(),
    );

    if passed {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn add_body(
    world: &mut PhysicsWorld,
    grid: OccupancyGrid,
    origin: PhysicsOrigin,
    world_position_m: [f64; 3],
    rotation: [f32; 4],
    linear_velocity: [f32; 3],
    angular_velocity: [f32; 3],
    cell_m: f32,
) -> spall_physics::BodyId {
    let id = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid,
        cell_m,
        density_kg_m3: STONE_DENSITY,
        mass_properties: None,
        translation_m: origin
            .to_local_f32(world_position_m)
            .expect("local pose fits f32"),
        linvel_m_s: linear_velocity,
    });
    world.set_body_pose(
        id,
        origin
            .to_local_f32(world_position_m)
            .expect("local pose fits f32"),
        rotation,
    );
    world.set_body_velocity(id, linear_velocity, angular_velocity);
    id
}

fn stage_body(
    destination: &mut PhysicsWorld,
    grid: OccupancyGrid,
    destination_origin: PhysicsOrigin,
    source_state: BodyState,
    source_origin: PhysicsOrigin,
    cell_m: f32,
) -> Result<spall_physics::BodyId, &'static str> {
    if !cell_m.is_finite() || cell_m <= 0.0 {
        return Err("invalid cell size");
    }
    if grid.solid_count() == 0 {
        return Err("empty body geometry");
    }
    let world_position = source_origin.to_world_f64(source_state.translation_m);
    let destination_position = destination_origin
        .to_local_f32(world_position)
        .ok_or("world pose is outside destination physics frame")?;
    let id = destination.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid,
        cell_m,
        density_kg_m3: STONE_DENSITY,
        mass_properties: None,
        translation_m: destination_position,
        linvel_m_s: source_state.linvel_m_s,
    });
    destination.set_body_pose(id, destination_position, source_state.rotation);
    destination.set_body_velocity(id, source_state.linvel_m_s, source_state.angvel_rad_s);
    Ok(id)
}

fn geometry_digest(grid: &OccupancyGrid) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for dimension in grid.dims() {
        for byte in dimension.to_le_bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    for z in 0..grid.dims()[2] {
        for y in 0..grid.dims()[1] {
            for x in 0..grid.dims()[0] {
                if let Some(material) = grid.material(x, y, z) {
                    for byte in [x, y, z]
                        .into_iter()
                        .flat_map(u32::to_le_bytes)
                        .chain(material.0.to_le_bytes())
                    {
                        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                    }
                }
            }
        }
    }
    hash
}

fn distance<const N: usize>(a: [f64; N], b: [f64; N]) -> f64 {
    a.into_iter()
        .zip(b)
        .map(|(a, b)| (a - b).powi(2))
        .sum::<f64>()
        .sqrt()
}

fn vector(value: [f64; 3]) -> String {
    format!("[{},{},{}]", value[0], value[1], value[2])
}

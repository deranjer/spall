use std::collections::BTreeMap;

use spall_physics::fixtures::{CELL_M, STONE_DENSITY, debris_pieces};
use spall_physics::{BodyKind, BodySpec, OccupancyGrid, PhysicsOrigin, Representation};
use spall_sim::{RegionCoordinator, RegionCoordinatorError};

fn main() {
    let mut coordinator = RegionCoordinator::default();
    let west = coordinator
        .create_region([0.0, 0.0, 0.0])
        .expect("west region");
    let east = coordinator
        .create_region([100_000.0, 0.0, 0.0])
        .expect("east region");
    let count = 512_u64;
    for entity in 1..=count {
        let region = if entity % 2 == 0 { east } else { west };
        coordinator
            .assign(entity, region)
            .expect("unique ownership");
    }
    let mut world_positions = (1..=count)
        .filter(|entity| entity % 2 == 0)
        .map(|entity| (entity, [100_000.0 + entity as f64, 4.0, -2.0]))
        .collect::<BTreeMap<_, _>>();
    let far_rejected = coordinator.merge(east, west, 100_000.0, 2.0, &world_positions);
    let far_rejected_atomically = matches!(
        far_rejected,
        Err(RegionCoordinatorError::RegionsTooFar { .. })
    ) && coordinator.region_count() == 2
        && coordinator.region_for(2) == Some(east);
    let missing_pose = coordinator.merge(east, west, 0.5, 2.0, &BTreeMap::new());
    let missing_pose_rejected_atomically = matches!(
        missing_pose,
        Err(RegionCoordinatorError::MissingWorldPose(_))
    ) && coordinator.region_count() == 2
        && coordinator.region_for(2) == Some(east);
    world_positions.insert(2, [1.0e300, 0.0, 0.0]);
    let out_of_range = coordinator.merge(east, west, 0.5, 2.0, &world_positions);
    let range_rejected_atomically = matches!(
        out_of_range,
        Err(RegionCoordinatorError::DestinationOutOfRange(2))
    ) && coordinator.region_count() == 2
        && coordinator.region_for(2) == Some(east);
    world_positions.insert(2, [100_002.0, 4.0, -2.0]);
    let plan = coordinator
        .merge(east, west, 0.5, 2.0, &world_positions)
        .expect("region merge plan");
    let unique = (1..=count).all(|entity| coordinator.region_for(entity) == Some(west));
    let midpoint_world = [100_000.0, 4.0, -2.0];
    let old_local = PhysicsOrigin::new([100_000.0, 0.0, 0.0])
        .and_then(|origin| origin.to_local_f32(midpoint_world));
    let new_local = plan.survivor_origin.to_local_f32(midpoint_world);
    let transfer_preserves_world = match (old_local, new_local) {
        (Some(a), Some(b)) => {
            plan.retired_origin.to_world_f64(a) == plan.survivor_origin.to_world_f64(b)
        }
        _ => false,
    };
    let passed = coordinator.region_count() == 1
        && coordinator.entity_count() == count as usize
        && plan.transferred_entities == count as usize / 2
        && unique
        && far_rejected_atomically
        && missing_pose_rejected_atomically
        && range_rejected_atomically
        && transfer_preserves_world;

    let live_transfer = live_transfer_scenario();
    let live_split = live_split_scenario();
    println!(
        "{{\"scenario\":\"region_coordination_merge_plan\",\"passed\":{passed},\
         \"regions_before\":2,\"regions_after\":{},\"entities\":{},\
         \"transferred_entities\":{},\"unique_authoritative_owners\":{unique},\
         \"world_pose_conversion_matches\":{transfer_preserves_world},\
         \"distance_threshold_atomic\":{far_rejected_atomically},\
         \"missing_pose_preflight_atomic\":{missing_pose_rejected_atomically},\
         \"range_preflight_atomic\":{range_rejected_atomically}}}",
        coordinator.region_count(),
        coordinator.entity_count(),
        plan.transferred_entities,
    );
    if !passed || !live_transfer || !live_split {
        std::process::exit(1);
    }
}

fn live_split_scenario() -> bool {
    let (_, volume) = debris_pieces(902, 1, 4).pop().expect("fixture body");
    let grid = OccupancyGrid::from_volume(&volume)
        .expect("occupancy extraction")
        .expect("solid body");
    let mut coordinator = RegionCoordinator::default();
    let shared = coordinator.create_region([100_000.0, 0.0, 0.0]).unwrap();
    let first = coordinator
        .add_body(
            91_001,
            shared,
            body_spec(grid.clone(), [0.0; 3]),
            [100_010.0, 10.0, 0.0],
        )
        .unwrap();
    let second = coordinator
        .add_body(
            91_002,
            shared,
            body_spec(grid.clone(), [0.0; 3]),
            [100_020.0, 10.0, 0.0],
        )
        .unwrap();
    let isolated = coordinator.create_region([100_016.0, 0.0, 0.0]).unwrap();
    let split = coordinator
        .transfer_body(91_002, isolated, body_spec(grid, [0.0; 3]), 0.002, 0.0001)
        .unwrap();
    let both_survive = coordinator.active_physics_bodies(shared).unwrap() == 1
        && coordinator.active_physics_bodies(isolated).unwrap() == 1
        && coordinator.region_for(91_001) == Some(shared)
        && coordinator.region_for(91_002) == Some(isolated)
        && first != second
        && second != split;
    println!(
        "{{\"scenario\":\"live_region_split\",\"passed\":{both_survive},\
         \"regions\":{},\"shared_region_bodies\":{},\"split_region_bodies\":{},\
         \"unique_stable_owners\":{both_survive}}}",
        coordinator.region_count(),
        coordinator.active_physics_bodies(shared).unwrap(),
        coordinator.active_physics_bodies(isolated).unwrap(),
    );
    both_survive
}

fn live_transfer_scenario() -> bool {
    let (_, volume) = debris_pieces(901, 1, 4).pop().expect("fixture body");
    let grid = OccupancyGrid::from_volume(&volume)
        .expect("occupancy extraction")
        .expect("solid body");
    let mut coordinator = RegionCoordinator::default();
    let source = coordinator.create_region([100_000.0, 0.0, 0.0]).unwrap();
    let destination = coordinator.create_region([100_112.0, 0.0, 0.0]).unwrap();
    let entity = 90_001;
    let position = [100_124.25, 40.0, 10.0];
    let velocity = [1.25, -0.5, 0.75];
    let source_id = coordinator
        .add_body(entity, source, body_spec(grid.clone(), velocity), position)
        .unwrap();
    for _ in 0..20 {
        coordinator.step_physics();
    }
    let before = coordinator.body_state(entity).unwrap();
    let moved_id = coordinator
        .transfer_body(
            entity,
            destination,
            body_spec(grid.clone(), velocity),
            0.002,
            0.0001,
        )
        .unwrap();
    let after = coordinator.body_state(entity).unwrap();
    let one_owner = coordinator.active_physics_bodies(source).unwrap() == 0
        && coordinator.active_physics_bodies(destination).unwrap() == 1
        && coordinator.region_for(entity) == Some(destination)
        && source_id != moved_id;
    let within_transfer_tolerance =
        distance(before.world_translation_m, after.world_translation_m) <= 0.002;
    let blocked_merge = coordinator.merge(
        source,
        destination,
        0.5,
        2.0,
        &BTreeMap::from([(entity, after.world_translation_m)]),
    );
    let live_body_merge_blocked_atomically = matches!(
        blocked_merge,
        Err(RegionCoordinatorError::LivePhysicsBodiesRequireTransfer)
    ) && coordinator.region_count() == 2
        && coordinator.region_for(entity) == Some(destination)
        && coordinator.active_physics_bodies(destination).unwrap() == 1;
    let survivor_id = coordinator
        .transfer_body(entity, source, body_spec(grid, velocity), 0.002, 0.0001)
        .unwrap();
    let survivor_state = coordinator.body_state(entity).unwrap();
    let merge = coordinator.merge(
        source,
        destination,
        0.5,
        2.0,
        &BTreeMap::from([(entity, survivor_state.world_translation_m)]),
    );
    let merged_once = merge.is_ok()
        && coordinator.region_count() == 1
        && coordinator.region_for(entity) == Some(source)
        && coordinator.active_physics_bodies(source).unwrap() == 1
        && coordinator.active_physics_bodies(destination).is_err();
    println!(
        "{{\"scenario\":\"live_region_transfer_then_merge\",\"passed\":{},\
         \"source_active_after_transfer\":{},\"destination_active_after_transfer\":{},\
         \"transfer_pose_error_m\":{},\"regions_after_merge\":{},\"stable_entity_owner\":{}}}",
        one_owner && within_transfer_tolerance && live_body_merge_blocked_atomically && merged_once,
        0,
        1,
        distance(before.world_translation_m, after.world_translation_m),
        coordinator.region_count(),
        coordinator.region_for(entity) == Some(source)
            && survivor_id.namespace() == 0
            && live_body_merge_blocked_atomically,
    );
    one_owner && within_transfer_tolerance && live_body_merge_blocked_atomically && merged_once
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

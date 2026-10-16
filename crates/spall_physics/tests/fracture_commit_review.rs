use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::{
    OccupancyGrid, PartTransform, PhysicsConfig, PhysicsWorld, Representation, SourcePart,
    prepare_fracture,
};

fn fixture() -> (PhysicsWorld, spall_physics::ArticulatedBodyId) {
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        disable_ccd: true,
        ..PhysicsConfig::default()
    });
    let source = SourcePart {
        part_id: 1,
        parent_id: None,
        volume_id: VolumeId::new(1201).unwrap(),
        source_revision: 9,
        grid: OccupancyGrid::from_solid_mask(
            GlobalCell::new(-2, 0, 0),
            [3, 1, 1],
            vec![true, false, true],
            vec![MaterialId(1); 3],
        )
        .unwrap(),
        cell_m: 1.0,
        local_transform: PartTransform::IDENTITY,
    };
    let id = world
        .add_dynamic_corpse_aggregate(
            [0.0; 3],
            PartTransform::IDENTITY.rotation_xyzw,
            5,
            vec![source],
            Representation::MergedCuboids,
            |_| 1.0,
        )
        .unwrap();
    (world, id)
}

#[test]
fn disconnected_same_part_splits_into_two_dynamic_components_and_preserves_queries() {
    let (mut world, id) = fixture();
    let snapshot = world
        .capture_dynamic_corpse_fracture_snapshot(id, 77, None)
        .unwrap();
    let plan = prepare_fracture(&snapshot).unwrap();
    assert_eq!(plan.components.len(), 2);
    let outcome = world
        .commit_dynamic_corpse_fracture(id, &snapshot, &plan, |_| 1.0)
        .unwrap();
    assert_eq!(outcome.source_cells, 2);
    assert_eq!(outcome.components.len(), 2);
    assert_eq!(world.articulated_rigid_body_count(id), 2);
    assert_eq!(world.articulated_collider_count(id), 2);
    let left = world
        .raycast_articulated(id, [-1.5, 0.5, -2.0], [0.0, 0.0, 1.0], 4.0)
        .unwrap();
    let right = world
        .raycast_articulated(id, [0.5, 0.5, -2.0], [0.0, 0.0, 1.0], 4.0)
        .unwrap();
    assert_eq!(left.cell, [-2, 0, 0]);
    assert_eq!(right.cell, [0, 0, 0]);
    assert_eq!(left.volume_id, VolumeId::new(1201).unwrap());
    assert_eq!(right.volume_id, VolumeId::new(1201).unwrap());
}

#[test]
fn malformed_partition_and_stale_pose_leave_the_original_owner_intact() {
    let (mut world, id) = fixture();
    let snapshot = world
        .capture_dynamic_corpse_fracture_snapshot(id, 77, None)
        .unwrap();
    let plan = prepare_fracture(&snapshot).unwrap();
    let mut malformed = plan.clone();
    malformed.components[0].cells.pop();
    assert!(
        world
            .commit_dynamic_corpse_fracture(id, &snapshot, &malformed, |_| 1.0)
            .is_err()
    );
    assert_eq!(world.articulated_rigid_body_count(id), 1);
    assert_eq!(world.articulated_collider_count(id), 1);
    world.step();
    assert!(
        world
            .commit_dynamic_corpse_fracture(id, &snapshot, &plan, |_| 1.0)
            .is_err()
    );
    assert_eq!(world.articulated_rigid_body_count(id), 1);
    assert_eq!(world.articulated_collider_count(id), 1);
}

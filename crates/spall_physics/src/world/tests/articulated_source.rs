use super::*;

#[test]
fn source_cell_witness_requires_a_unique_exterior_face_in_source_coordinates() {
    let one = one_cell_grid(GlobalCell::new(-3, 2, 1));
    for (point, normal) in [
        (Vector::new(1.0, 0.4, 0.6), Vector::X),
        (Vector::new(0.0, 0.4, 0.6), -Vector::X),
    ] {
        assert_eq!(
            resolve_source_cell_witness(&one, 1.0, point, normal, 0.0),
            ContactCellAttribution::Resolved {
                cell: [-3, 2, 1],
                material: fixtures::STONE,
            }
        );
    }

    let shared = OccupancyGrid::from_solid_mask(
        GlobalCell::new(-2, 0, 0),
        [2, 1, 1],
        vec![true, true],
        vec![fixtures::STONE; 2],
    )
    .unwrap();
    assert_eq!(
        resolve_source_cell_witness(&shared, 1.0, Vector::new(1.0, 0.5, 0.5), Vector::X, 0.0),
        ContactCellAttribution::Unattributed(ContactWitnessUnattributed::InternalSharedFace)
    );
    let holed = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        [2, 1, 1],
        vec![true, false],
        vec![fixtures::STONE, fixtures::STONE],
    )
    .unwrap();
    assert!(matches!(
        resolve_source_cell_witness(&holed, 1.0, Vector::new(1.0, 0.5, 0.5), Vector::X, 0.0),
        ContactCellAttribution::Resolved {
            cell: [0, 0, 0],
            ..
        }
    ));
    for (point, normal, separation, reason) in [
        (
            Vector::new(1.0, 0.0, 0.5),
            Vector::X,
            0.0,
            ContactWitnessUnattributed::EdgeOrCorner,
        ),
        (
            Vector::new(1.0, 0.00005, 0.5),
            Vector::X,
            0.0,
            ContactWitnessUnattributed::EdgeOrCorner,
        ),
        (
            Vector::new(1.0, 0.5, 0.5),
            Vector::new(1.0, 1.0, 0.0),
            0.0,
            ContactWitnessUnattributed::NotAxisAlignedFace,
        ),
        (
            Vector::new(1.0, 0.5, 0.5),
            Vector::X,
            -0.01,
            ContactWitnessUnattributed::Penetrating,
        ),
    ] {
        assert_eq!(
            resolve_source_cell_witness(&one, 1.0, point, normal, separation),
            ContactCellAttribution::Unattributed(reason)
        );
    }
}

#[test]
fn articulated_topology_rebuild_installs_new_source_occupancy_atomically() {
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0, -9.81, 0.0],
        ..PhysicsConfig::default()
    });
    let body = world
        .add_articulated_body(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            3,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 70, 1)],
            Representation::NativeVoxels,
        )
        .unwrap();
    let hit = world
        .raycast_articulated(body, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 2.0)
        .unwrap();
    assert_eq!(hit.source_revision, 1);
    let old_radius = world.articulated[&body.0].parts[0].root_surface_radius_m;
    assert!((old_radius - (3.0_f32.sqrt() * 0.25)).abs() < 1.0e-6);
    assert!(
        world
            .rebuild_articulated_part_grid(
                body,
                3,
                4,
                7,
                1,
                2,
                one_cell_grid(GlobalCell::new(4, 0, 0)),
            )
            .is_ok()
    );
    assert!(matches!(
        world.raycast_articulated(body, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 2.0),
        Err(crate::articulated::ArticulatedError::Miss)
    ));
    let moved = world
        .raycast_articulated(body, [0.5, 0.125, 0.125], [1.0, 0.0, 0.0], 2.0)
        .unwrap();
    assert_eq!(moved.source_revision, 2);
    assert_eq!(moved.topology_token, 4);
    let rebuilt_radius = world.articulated[&body.0].parts[0].root_surface_radius_m;
    assert!((rebuilt_radius - (1.25_f32.powi(2) + 2.0 * 0.25_f32.powi(2)).sqrt()).abs() < 1.0e-6);

    let stale = world.rebuild_articulated_part_grid(
        body,
        3,
        5,
        7,
        2,
        3,
        one_cell_grid(GlobalCell::new(8, 0, 0)),
    );
    assert_eq!(
        stale.unwrap_err(),
        crate::articulated::ArticulatedError::StaleTopology
    );
    let still_current = world
        .raycast_articulated(body, [0.5, 0.125, 0.125], [1.0, 0.0, 0.0], 2.0)
        .unwrap();
    assert_eq!(still_current.source_revision, 2);
    assert_eq!(still_current.topology_token, 4);
}

#[test]
fn articulated_source_rebuild_rejects_pose_staged_for_prior_topology() {
    use crate::articulated::{ArticulatedError, ArticulatedPose, PartTransform};

    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            solver_substeps: 4,
            ..PhysicsConfig::default()
        });
        let body = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                10,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 71, 3)],
                representation,
            )
            .unwrap();
        let initial_hit = world
            .raycast_articulated(body, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 3.0)
            .unwrap();
        assert_eq!(
            (initial_hit.topology_token, initial_hit.pose_token),
            (10, 0)
        );
        let initial_colliders = world.articulated_collider_count(body);
        let initial_bodies = world.articulated_rigid_body_count(body);

        world
            .update_articulated_pose(
                body,
                ArticulatedPose {
                    topology_token: 10,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(
                        7,
                        PartTransform {
                            translation_m: [0.01, 0.0, 0.0],
                            ..PartTransform::IDENTITY
                        },
                    )],
                },
            )
            .unwrap();
        assert_eq!(world.articulated[&body.0].topology_token, 10);
        assert_eq!(world.articulated[&body.0].pose_token, 0);
        assert_eq!(
            world.articulated[&body.0]
                .pending_pose
                .as_ref()
                .map(|pending| pending.pose_token),
            Some(1)
        );

        let rejected = world.rebuild_articulated_part_grid(
            body,
            10,
            11,
            7,
            3,
            4,
            one_cell_grid(GlobalCell::new(4, 0, 0)),
        );
        assert_eq!(rejected.unwrap_err(), ArticulatedError::StaleTopology);
        assert_eq!(world.articulated_collider_count(body), initial_colliders);
        assert_eq!(world.articulated_rigid_body_count(body), initial_bodies);
        let assembly = &world.articulated[&body.0];
        assert_eq!(assembly.topology_token, 10);
        assert_eq!(assembly.pose_token, 0);
        assert_eq!(assembly.parts[0].source_revision, 3);
        assert_eq!(assembly.parts[0].grid.origin(), GlobalCell::new(0, 0, 0));
        assert_eq!(
            assembly
                .pending_pose
                .as_ref()
                .map(|pending| pending.pose_token),
            Some(1)
        );
        let still_old = world
            .raycast_articulated(body, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 3.0)
            .unwrap();
        assert_eq!((still_old.topology_token, still_old.pose_token), (10, 0));

        world.step();
        let applied = world
            .raycast_articulated(body, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 3.0)
            .unwrap();
        assert_eq!((applied.topology_token, applied.pose_token), (10, 1));
        assert!((applied.toi_m - 1.01).abs() < 1.0e-4);

        world
            .rebuild_articulated_part_grid(
                body,
                10,
                11,
                7,
                3,
                4,
                one_cell_grid(GlobalCell::new(4, 0, 0)),
            )
            .unwrap();
        let rebuilt = world
            .raycast_articulated(body, [0.0, 0.125, 0.125], [1.0, 0.0, 0.0], 3.0)
            .unwrap();
        assert_eq!(rebuilt.source_revision, 4);
        assert_eq!((rebuilt.topology_token, rebuilt.pose_token), (11, 1));
        assert_eq!(rebuilt.cell, [4, 0, 0]);
    }
}

#[test]
fn occupied_source_corner_radius_handles_negative_off_center_nested_geometry() {
    let grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(-2, 1, -1),
        [2, 1, 1],
        vec![false, true],
        vec![fixtures::STONE; 2],
    )
    .unwrap();
    let local_radius = source_surface_radius_m(&grid, 0.25, [0.0; 3], None);
    assert!((local_radius - 0.375_f32.sqrt()).abs() < 1.0e-6);
    let parent = PartTransform {
        translation_m: [1.0, 0.0, 0.0],
        rotation_xyzw: [
            0.0,
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
        ],
    };
    let child = PartTransform {
        translation_m: [0.0, 1.0, 0.0],
        rotation_xyzw: PartTransform::IDENTITY.rotation_xyzw,
    };
    let flattened = compose(parent, child);
    let root_radius = source_surface_radius_m(&grid, 0.25, [0.0; 3], Some(flattened));
    assert!(
        (root_radius - 0.375_f32.sqrt()).abs() < 1.0e-6,
        "radius={root_radius}"
    );
}

#[test]
fn articulated_rejects_unrepresentable_source_and_shape_bounds_without_mutation() {
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    let body = world
        .add_articulated_body(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 72, 1)],
            Representation::NativeVoxels,
        )
        .unwrap();
    let out_of_source_range = one_cell_grid(GlobalCell::new(i32::MAX as i64 + 1, 0, 0));
    assert_eq!(
        world
            .rebuild_articulated_part_grid(body, 1, 2, 7, 1, 2, out_of_source_range)
            .unwrap_err(),
        crate::articulated::ArticulatedError::GeometryRange
    );
    let preserved = world
        .raycast_articulated(body, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 2.0)
        .unwrap();
    assert_eq!(preserved.source_revision, 1);
    assert_eq!(preserved.topology_token, 1);

    let mut impossible_scale = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 73, 1);
    impossible_scale.cell_m = f32::MAX;
    assert_eq!(
        world
            .add_articulated_body(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![impossible_scale],
                Representation::NativeVoxels,
            )
            .unwrap_err(),
        crate::articulated::ArticulatedError::GeometryRange
    );
}

#[test]
fn articulated_rebuild_enforces_aggregate_dense_budget_before_mutation() {
    fn sparse_grid(dims: [u32; 3]) -> OccupancyGrid {
        let count = (u64::from(dims[0]) * u64::from(dims[1]) * u64::from(dims[2])) as usize;
        let mut solid = vec![false; count];
        let mut material = vec![MaterialId::AIR; count];
        solid[0] = true;
        material[0] = fixtures::STONE;
        OccupancyGrid::from_solid_mask(GlobalCell::new(0, 0, 0), dims, solid, material).unwrap()
    }
    let mut root_part = one_part(sparse_grid([128, 128, 116]), 77, 1);
    root_part.part_id = 1;
    let mut child = one_part(sparse_grid([128, 128, 116]), 78, 1);
    child.part_id = 2;
    child.parent_id = Some(1);
    child.local_transform.translation_m[0] = 0.5;
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    let body = world
        .add_articulated_body(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![root_part, child],
            Representation::MergedCuboids,
        )
        .unwrap();
    let hit_before = world
        .raycast_articulated(body, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 2.0)
        .unwrap();
    let result =
        world.rebuild_articulated_part_grid(body, 1, 2, 1, 1, 2, sparse_grid([128, 128, 135]));
    assert_eq!(
        result.unwrap_err(),
        crate::articulated::ArticulatedError::WorkBudget
    );
    let hit_after = world
        .raycast_articulated(body, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 2.0)
        .unwrap();
    assert_eq!(hit_after.source_revision, hit_before.source_revision);
    assert_eq!(hit_after.topology_token, hit_before.topology_token);
    assert_eq!(world.articulated_collider_count(body), 2);
}

#[test]
fn articulated_source_volume_has_one_live_assembly_owner() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let first = world
        .add_articulated_body(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 79, 1)],
            Representation::NativeVoxels,
        )
        .unwrap();
    let duplicate = world.add_articulated_body(
        [2.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
        1,
        vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 79, 1)],
        Representation::NativeVoxels,
    );
    assert_eq!(
        duplicate.unwrap_err(),
        crate::articulated::ArticulatedError::DuplicateVolume
    );
    assert_eq!(world.articulated_collider_count(first), 1);
    world.retire_articulated_body(first);
    assert!(
        world
            .add_articulated_body(
                [2.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 79, 2)],
                Representation::NativeVoxels,
            )
            .is_ok()
    );
}

#[test]
fn direct_character_sweep_exclusion_covers_every_source_part_collider() {
    let dims = [1, 4, 1];
    let child_grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        dims,
        vec![true; 4],
        vec![fixtures::STONE; 4],
    )
    .unwrap();
    let mut root_part = one_part(child_grid.clone(), 82, 1);
    root_part.part_id = 1;
    root_part.local_transform.translation_m[0] = -5.0;
    let mut child_part = one_part(child_grid, 83, 1);
    child_part.part_id = 2;
    child_part.parent_id = Some(1);
    child_part.local_transform.translation_m[0] = 7.0;
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    let assembly = world
        .add_articulated_body(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![root_part, child_part],
            Representation::MergedCuboids,
        )
        .unwrap();
    world.sync_queries();
    let movement = world.sweep_character_excluding(
        crate::character::CharacterParams::DEFAULT,
        [0.0, 0.0, 0.125],
        [4.0, 0.0, 0.0],
        1.0 / 60.0,
        &[assembly.0],
    );
    assert!(
        movement.translation_m[0] > 3.99,
        "excluded assembly's non-primary child collider blocked the direct sweep: {movement:?}"
    );
}

#[test]
fn legacy_single_collider_lifecycle_calls_explicitly_reject_articulated_owners() {
    let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let assembly = world
        .add_articulated_body(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(grid.clone(), 84, 1)],
            Representation::NativeVoxels,
        )
        .unwrap();
    let owner = assembly.0;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| world.remove_collider(owner)))
            .is_err()
    );
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| world.rebuild_collider(
            owner,
            &grid,
            Representation::NativeVoxels
        )))
        .is_err()
    );
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| world.deactivate_body(owner)))
            .is_err()
    );
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| world.reactivate_body(
            owner,
            &grid,
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            [0.0; 3],
            [0.0; 3]
        )))
        .is_err()
    );
    assert_eq!(world.articulated_collider_count(assembly), 1);
    assert!(!world.is_retired(owner));
}

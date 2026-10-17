//! Independent coordinator checks of the public reconstruction adapter.
use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::{
    DerivedFractureComponentId, DynamicCorpseComponentRestore, DynamicCorpseRestore, OccupancyGrid,
    PartTransform, PhysicsConfig, PhysicsWorld, Representation, SourceCellRef, SourcePart,
};

fn bundle(owner_key: u128, volume: u64) -> DynamicCorpseRestore {
    let volume_id = VolumeId::new(volume).unwrap();
    DynamicCorpseRestore {
        owner_key,
        source_generation: 4,
        topology_revision: 3,
        pose_epoch: 29,
        next_component_serial: 100,
        source_parts: vec![SourcePart {
            part_id: 1,
            parent_id: None,
            volume_id,
            source_revision: 7,
            grid: OccupancyGrid::from_solid_mask(
                GlobalCell::new(-3, 0, 0),
                [1, 1, 1],
                vec![true],
                vec![MaterialId(1)],
            )
            .unwrap(),
            cell_m: 0.5,
            local_transform: PartTransform::IDENTITY,
        }],
        components: vec![DynamicCorpseComponentRestore {
            component_id: Some(DerivedFractureComponentId {
                owner_key,
                serial: 41,
            }),
            translation_m: [10.0, 0.0, 0.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [1.0, 2.0, 3.0],
            angular_velocity_rad_s: [0.25, 0.0, 0.5],
            cells: vec![SourceCellRef {
                volume_id,
                part_id: 1,
                cell: [-3, 0, 0],
                material: MaterialId(1),
                source_revision: 7,
            }],
        }],
    }
}

#[test]
fn public_restore_preserves_negative_source_coordinates_and_live_world_on_failure() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let key = DerivedFractureComponentId {
        owner_key: 77,
        serial: 41,
    };
    let owner = world
        .restore_dynamic_corpse(bundle(77, 101), Representation::MergedCuboids, |_| 900.0)
        .unwrap();
    let before = world.fracture_component_state(owner, key).unwrap();
    assert_eq!(before.translation_m, [10.0, 0.0, 0.0]);
    assert_eq!(before.linvel_m_s, [1.0, 2.0, 3.0]);
    assert_eq!(before.angvel_rad_s, [0.25, 0.0, 0.5]);
    assert!((before.mass_kg - 112.5).abs() < 1.0e-4);
    let hit = world
        .raycast_articulated(owner, [8.75, 0.25, -1.0], [0.0, 0.0, 1.0], 3.0)
        .unwrap();
    assert_eq!(hit.cell, [-3, 0, 0]);
    assert_eq!(hit.volume_id, VolumeId::new(101).unwrap());
    assert_eq!(hit.source_revision, 7);
    assert_eq!(hit.topology_token, 3);
    assert_eq!(hit.pose_token, 29);
    let count = world.body_count();
    for case in 0..3 {
        let mut invalid = bundle(78, 102);
        match case {
            0 => invalid.components[0].cells[0].material = MaterialId(2),
            1 => invalid.components[0].angular_velocity_rad_s[0] = f32::NAN,
            2 => invalid.components[0].component_id.as_mut().unwrap().serial = 100,
            _ => unreachable!(),
        }
        assert!(
            world
                .restore_dynamic_corpse(invalid, Representation::MergedCuboids, |_| 900.0)
                .is_err()
        );
        assert_eq!(world.body_count(), count);
        let after = world.fracture_component_state(owner, key).unwrap();
        assert_eq!(after.translation_m, before.translation_m);
        assert_eq!(after.linvel_m_s, before.linvel_m_s);
        assert_eq!(after.angvel_rad_s, before.angvel_rad_s);
        assert_eq!(after.mass_kg, before.mass_kg);
    }
    // The failed preparations must not consume the owner key or source ID.
    assert!(
        world
            .restore_dynamic_corpse(bundle(78, 102), Representation::MergedCuboids, |_| 900.0)
            .is_ok()
    );
}

#[test]
fn a_late_invalid_restore_or_cross_member_collision_publishes_no_owner() {
    for case in 0..4 {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let first = bundle(77, 101);
        let mut second = bundle(78, 102);
        match case {
            0 => second.components[0].linear_velocity_m_s[0] = f32::NAN,
            1 => {
                second.owner_key = 77;
                second.components[0]
                    .component_id
                    .as_mut()
                    .unwrap()
                    .owner_key = 77;
            }
            2 => {
                second.source_parts[0].volume_id = VolumeId::new(101).unwrap();
                second.components[0].cells[0].volume_id = VolumeId::new(101).unwrap();
            }
            3 => second.source_parts.push(SourcePart {
                part_id: 2,
                parent_id: Some(1),
                volume_id: VolumeId::new(101).unwrap(),
                source_revision: 7,
                cell_m: 0.5,
                local_transform: PartTransform::IDENTITY,
                grid: OccupancyGrid::from_solid_mask(
                    GlobalCell::new(0, 0, 0),
                    [1, 1, 1],
                    vec![false],
                    vec![MaterialId::AIR],
                )
                .unwrap(),
            }),
            _ => unreachable!(),
        }
        assert!(
            world
                .prepare_dynamic_corpse_restore_batch(
                    vec![first, second],
                    Representation::MergedCuboids,
                    |_| 900.0,
                )
                .is_err(),
            "batch case {case}"
        );
        assert_eq!(world.body_count(), 0);
        // Even a failure after preparing the first owner's shapes consumes no identity.
        assert!(
            world
                .restore_dynamic_corpse(bundle(77, 101), Representation::MergedCuboids, |_| 900.0)
                .is_ok()
        );
    }
}

#[test]
fn prepared_restores_are_world_bound_and_reject_stale_batches_before_insertion() {
    let mut source = PhysicsWorld::new(PhysicsConfig::default());
    let mut another = PhysicsWorld::new(PhysicsConfig::default());
    let prepared = source
        .prepare_dynamic_corpse_restore_batch(
            vec![bundle(77, 101), bundle(78, 102)],
            Representation::MergedCuboids,
            |_| 900.0,
        )
        .unwrap();
    assert_eq!(source.body_count(), 0, "preparation is pure");
    assert!(matches!(
        another.commit_dynamic_corpse_restore_batch(prepared),
        Err(spall_physics::ArticulatedError::StalePreparedBatch)
    ));
    assert_eq!(another.body_count(), 0);
    let prepared = source
        .prepare_dynamic_corpse_restore_batch(
            vec![bundle(77, 101), bundle(78, 102)],
            Representation::MergedCuboids,
            |_| 900.0,
        )
        .unwrap();
    source.step();
    assert!(matches!(
        source.commit_dynamic_corpse_restore_batch(prepared),
        Err(spall_physics::ArticulatedError::StalePreparedBatch)
    ));
    assert_eq!(source.body_count(), 0);

    let prepared = source
        .prepare_dynamic_corpse_restore_batch(
            vec![bundle(77, 101), bundle(78, 102)],
            Representation::MergedCuboids,
            |_| 900.0,
        )
        .unwrap();
    source
        .restore_dynamic_corpse(bundle(79, 103), Representation::MergedCuboids, |_| 900.0)
        .unwrap();
    assert!(matches!(
        source.commit_dynamic_corpse_restore_batch(prepared),
        Err(spall_physics::ArticulatedError::StalePreparedBatch)
    ));
    assert_eq!(
        source.body_count(),
        1,
        "no first-member insertion before detecting staleness"
    );
    let prepared = source
        .prepare_dynamic_corpse_restore_batch(
            vec![bundle(77, 101), bundle(78, 102)],
            Representation::MergedCuboids,
            |_| 900.0,
        )
        .unwrap();
    assert_eq!(
        source
            .commit_dynamic_corpse_restore_batch(prepared)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(source.body_count(), 3);
}

#[test]
fn one_prepared_commit_installs_all_exact_owners_and_preserves_their_identity() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let first = bundle(77, 101);
    let mut second = bundle(78, 102);
    second.components[0].translation_m[0] = 20.0;
    let prepared = world
        .prepare_dynamic_corpse_restore_batch(
            vec![first, second],
            Representation::MergedCuboids,
            |_| 900.0,
        )
        .unwrap();
    assert_eq!(world.body_count(), 0);
    let owners = world.commit_dynamic_corpse_restore_batch(prepared).unwrap();
    assert_eq!(owners.len(), 2);
    for (index, id) in owners.into_iter().enumerate() {
        let key = DerivedFractureComponentId {
            owner_key: 77 + index as u128,
            serial: 41,
        };
        let state = world.fracture_component_state(id, key).unwrap();
        assert_eq!(state.translation_m, [10.0 + 10.0 * index as f32, 0.0, 0.0]);
        assert!((state.mass_kg - 112.5).abs() < 1.0e-4);
        assert_eq!(state.linvel_m_s, [1.0, 2.0, 3.0]);
        let hit = world
            .raycast_articulated(
                id,
                [8.75 + 10.0 * index as f32, 0.25, -1.0],
                [0.0, 0.0, 1.0],
                3.0,
            )
            .unwrap();
        assert_eq!(hit.volume_id, VolumeId::new(101 + index as u64).unwrap());
        assert_eq!(hit.cell, [-3, 0, 0]);
        assert_eq!(hit.pose_token, 29);
    }
}

#[test]
fn reconstruction_owner_count_is_bounded_before_preparing_shapes() {
    let world = PhysicsWorld::new(PhysicsConfig::default());
    let inputs = vec![bundle(77, 101); spall_physics::MAX_CORPSE_RESTORE_BATCH_OWNERS + 1];
    assert!(matches!(
        world
            .prepare_dynamic_corpse_restore_batch(inputs, Representation::MergedCuboids, |_| 900.0),
        Err(spall_physics::ArticulatedError::WorkBudget)
    ));
    assert_eq!(world.body_count(), 0);
}

#[test]
fn a_batch_freezes_each_material_density_once_for_all_owners() {
    let world = PhysicsWorld::new(PhysicsConfig::default());
    let calls = std::cell::Cell::new(0_u32);
    let prepared = world
        .prepare_dynamic_corpse_restore_batch(
            vec![bundle(77, 101), bundle(78, 102)],
            Representation::MergedCuboids,
            |_| {
                calls.set(calls.get() + 1);
                900.0 * f64::from(calls.get())
            },
        )
        .unwrap();
    assert_eq!(
        calls.get(),
        1,
        "one immutable catalog snapshot across the batch"
    );
    let mut world = world;
    let owners = world.commit_dynamic_corpse_restore_batch(prepared).unwrap();
    for (index, owner) in owners.into_iter().enumerate() {
        let state = world
            .fracture_component_state(
                owner,
                DerivedFractureComponentId {
                    owner_key: 77 + index as u128,
                    serial: 41,
                },
            )
            .unwrap();
        assert!((state.mass_kg - 112.5).abs() < 1.0e-4);
    }
}

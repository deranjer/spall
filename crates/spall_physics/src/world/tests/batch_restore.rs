use super::*;

#[test]
fn articulated_batch_publishes_alive_birth_and_checked_transfer_together() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let alive = world
        .add_articulated_kinematic_parts(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            7,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 501, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    let before = world.body_count();
    let prepared = world
        .prepare_articulated_batch(
            vec![
                ArticulatedBatchOperation::AliveBirth {
                    operation_tag: 11,
                    translation_m: [2.0, 0.0, 0.0],
                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    topology_token: 9,
                    parts: vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 502, 1)],
                    representation: Representation::MergedCuboids,
                },
                ArticulatedBatchOperation::CheckedDeathTransfer {
                    operation_tag: 12,
                    id: alive,
                    expected_topology_token: 7,
                    expected_pose_token: 0,
                },
            ],
            mix_density,
        )
        .unwrap();
    assert_eq!(prepared.additional_body_slots(), 1);
    let transfer_witness = *prepared.transfer_outcome(12).unwrap();
    assert!(prepared.transfer_outcome(11).is_none());
    assert!(prepared.transfer_outcome(999).is_none());
    assert_eq!(
        world.body_count(),
        before,
        "prepare must not publish bodies"
    );
    let validated = world.validate_articulated_batch(prepared).unwrap();
    assert_eq!(
        world.body_count(),
        before,
        "validation must not publish bodies"
    );
    let outcomes = world.publish_validated_articulated_batch(validated);
    assert_eq!(
        outcomes.iter().map(|o| o.operation_tag).collect::<Vec<_>>(),
        vec![11, 12]
    );
    assert!(outcomes[1].transfer.is_some());
    assert_eq!(outcomes[1].transfer, Some(transfer_witness));
    let born = world.articulated.get(&outcomes[0].owner_id.0).unwrap();
    assert!(born.kinematic_part_bodies && !born.dynamic_corpse);
    let converted = world.articulated.get(&alive.0).unwrap();
    assert!(!converted.kinematic_part_bodies && converted.dynamic_corpse);
    assert!(
        converted
            .source_volume_ids
            .contains(&VolumeId::new(501).unwrap())
    );
}

#[test]
fn articulated_batch_prepares_two_deaths_birth_and_cold_restore_atomically() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let death_a = world
        .add_articulated_kinematic_parts(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            31,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 531, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    let death_b = world
        .add_articulated_kinematic_parts(
            [1.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            32,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 532, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    let restored_part = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 533, 4);
    let restored = crate::articulated::DynamicCorpseRestore {
        owner_key: 0x5eed,
        source_generation: 4,
        topology_revision: 3,
        pose_epoch: 8,
        next_component_serial: 1,
        source_parts: vec![restored_part.clone()],
        components: vec![restore_component(
            None,
            BodyState {
                translation_m: [4.0, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                linvel_m_s: [0.0; 3],
                angvel_rad_s: [0.0; 3],
                mass_kg: 0.0,
                sleeping: false,
            },
            source_cell_refs(&restored_part),
        )],
    };
    let before = world.body_count();
    let prepared = world
        .prepare_articulated_batch(
            vec![
                ArticulatedBatchOperation::AliveBirth {
                    operation_tag: 41,
                    translation_m: [2.0, 0.0, 0.0],
                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    topology_token: 40,
                    parts: vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 534, 1)],
                    representation: Representation::MergedCuboids,
                },
                ArticulatedBatchOperation::CheckedDeathTransfer {
                    operation_tag: 42,
                    id: death_a,
                    expected_topology_token: 31,
                    expected_pose_token: 0,
                },
                ArticulatedBatchOperation::ColdRestore {
                    operation_tag: 43,
                    restore: restored,
                    representation: Representation::MergedCuboids,
                },
                ArticulatedBatchOperation::CheckedDeathTransfer {
                    operation_tag: 44,
                    id: death_b,
                    expected_topology_token: 32,
                    expected_pose_token: 0,
                },
            ],
            mix_density,
        )
        .unwrap();
    assert_eq!(prepared.additional_body_slots(), 2);
    assert_eq!(
        world.body_count(),
        before,
        "prepare must not publish members"
    );
    let outcomes = world.commit_articulated_batch(prepared).unwrap();
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.operation_tag)
            .collect::<Vec<_>>(),
        vec![41, 42, 43, 44]
    );
    assert!(outcomes[1].transfer.is_some());
    assert!(outcomes[3].transfer.is_some());
    assert!(world.articulated.get(&death_a.0).unwrap().dynamic_corpse);
    assert!(world.articulated.get(&death_b.0).unwrap().dynamic_corpse);
    assert!(
        world
            .articulated
            .get(&outcomes[0].owner_id.0)
            .unwrap()
            .kinematic_part_bodies
    );
    assert_eq!(
        world
            .articulated_owner_snapshot(outcomes[2].owner_id)
            .unwrap()
            .pose_token,
        8
    );
    assert_eq!(world.body_count(), before + 2);
}

#[test]
fn articulated_batch_capacity_reservation_is_joint_and_overflow_is_atomic() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let make_birth = |tag, volume| ArticulatedBatchOperation::AliveBirth {
        operation_tag: tag,
        translation_m: [0.0; 3],
        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        topology_token: tag,
        parts: vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), volume, 1)],
        representation: Representation::MergedCuboids,
    };
    let before = world.body_count();
    let prepared = world
        .prepare_articulated_batch(vec![make_birth(61, 561)], mix_density)
        .unwrap();
    assert_eq!(prepared.additional_body_slots(), 1);
    let validated = world
        .validate_articulated_batch_with_reserved_body_slots(prepared, 2)
        .unwrap();
    assert_eq!(world.body_count(), before);
    let outcomes = world.publish_validated_articulated_batch(validated);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(world.body_count(), before + 1);

    let prepared = world
        .prepare_articulated_batch(vec![make_birth(62, 562)], mix_density)
        .unwrap();
    let before_overflow = world.body_count();
    assert!(matches!(
        world.validate_articulated_batch_with_reserved_body_slots(prepared, usize::MAX),
        Err(crate::articulated::ArticulatedError::WorkBudget)
    ));
    assert_eq!(world.body_count(), before_overflow);
    assert!(!world.articulated.values().any(|owner| {
        owner
            .source_volume_ids
            .contains(&VolumeId::new(562).unwrap())
    }));
}

#[test]
fn articulated_batch_rejects_same_step_transfer_drift_before_birth_publication() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let alive = world
        .add_articulated_kinematic_parts(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            17,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 511, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    let before = world.body_count();
    let prepared = world
        .prepare_articulated_batch(
            vec![
                ArticulatedBatchOperation::AliveBirth {
                    operation_tag: 21,
                    translation_m: [2.0, 0.0, 0.0],
                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    topology_token: 19,
                    parts: vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 512, 1)],
                    representation: Representation::MergedCuboids,
                },
                ArticulatedBatchOperation::CheckedDeathTransfer {
                    operation_tag: 22,
                    id: alive,
                    expected_topology_token: 17,
                    expected_pose_token: 0,
                },
            ],
            mix_density,
        )
        .unwrap();
    let live_body = world.entries[entry_index(alive.0, world.id_namespace)].body;
    world.bodies[live_body]
        .set_position(Pose::from_translation(Vector::new(0.001, 0.0, 0.0)), true);
    assert!(matches!(
        world.commit_articulated_batch(prepared),
        Err(crate::articulated::ArticulatedError::StalePreparedBatch)
    ));
    assert_eq!(
        world.body_count(),
        before,
        "stale last member must leave the first birth unpublished"
    );
    assert!(
        world
            .articulated
            .get(&alive.0)
            .unwrap()
            .kinematic_part_bodies
    );
    assert!(!world.articulated.values().any(|owner| {
        owner
            .source_volume_ids
            .contains(&VolumeId::new(512).unwrap())
    }));
}

#[test]
fn body_commit_fence_rejects_same_step_twist_changes() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let id = world
        .add_articulated_kinematic_parts(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            27,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 521, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    let fence = world.snapshot_body_commit_fence(&[id.0]).unwrap();
    assert!(world.validate_body_commit_fence(&fence));
    let live_body = world.entries[entry_index(id.0, world.id_namespace)].body;
    world.bodies[live_body]
        .set_position(Pose::from_translation(Vector::new(0.001, 0.0, 0.0)), true);
    assert!(!world.validate_body_commit_fence(&fence));
}

#[test]
fn body_commit_fence_accepts_and_tracks_absent_terrain_collider() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let id = world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
        cell_m: 0.25,
        density_kg_m3: 1.0,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });
    world.remove_collider(id);
    let fence = world.snapshot_body_commit_fence(&[id]).unwrap();
    assert!(world.validate_body_commit_fence(&fence));
    world.rebuild_collider(
        id,
        &one_cell_grid(GlobalCell::new(1, 0, 0)),
        Representation::MergedCuboids,
    );
    assert!(!world.validate_body_commit_fence(&fence));
}

#[test]
fn cold_restore_preserves_split_motion_mass_query_and_serial_highwater() {
    let cfg = PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        solver_substeps: 4,
        ..Default::default()
    };
    let mut live = PhysicsWorld::new(cfg);
    let part = one_part(grid_from_x_cells(&[0, 1, 2, 4], 5), 90, 7);
    let owner = live
        .add_dynamic_corpse_aggregate(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![part.clone()],
            Representation::MergedCuboids,
            mix_density,
        )
        .unwrap();
    let snapshot = live
        .capture_dynamic_corpse_fracture_snapshot(owner, 0xabc, None)
        .unwrap();
    let plan = prepare_fracture(&snapshot).unwrap();
    assert_eq!(plan.components.len(), 2);
    let committed = live
        .commit_dynamic_corpse_fracture(owner, &snapshot, &plan, mix_density)
        .unwrap();
    let ids: Vec<_> = committed.components.iter().map(|entry| entry.0).collect();
    assert_eq!(
        ids,
        vec![
            DerivedFractureComponentId {
                owner_key: 0xabc,
                serial: 1
            },
            DerivedFractureComponentId {
                owner_key: 0xabc,
                serial: 2
            },
        ]
    );
    live.set_dynamic_corpse_component_velocity(owner, Some(ids[0]), [1.0, 0.0, 0.0], [0.0; 3])
        .unwrap();
    live.set_dynamic_corpse_component_velocity(owner, Some(ids[1]), [-1.0, 0.0, 0.0], [0.0; 3])
        .unwrap();
    live.step();

    let components = ids
        .iter()
        .zip(&plan.components)
        .map(|(component_id, planned)| {
            restore_component(
                Some(*component_id),
                live.fracture_component_state(owner, *component_id).unwrap(),
                planned.cells.clone(),
            )
        })
        .collect();
    let mut cold = PhysicsWorld::new(cfg);
    let restored = cold
        .restore_dynamic_corpse(
            crate::articulated::DynamicCorpseRestore {
                owner_key: 0xabc,
                source_generation: 2,
                topology_revision: 2,
                pose_epoch: 77,
                next_component_serial: 3,
                source_parts: vec![part],
                components,
            },
            Representation::MergedCuboids,
            mix_density,
        )
        .unwrap();
    assert_eq!(cold.articulated_collider_count(restored), 2);
    assert_eq!(cold.articulated_rigid_body_count(restored), 2);
    assert_eq!(
        cold.articulated_owner_snapshot(restored)
            .unwrap()
            .pose_token,
        77
    );
    for &id in &ids {
        let expected = live.fracture_component_state(owner, id).unwrap();
        let actual = cold.fracture_component_state(restored, id).unwrap();
        assert!((expected.translation_m[0] - actual.translation_m[0]).abs() < 1.0e-5);
        assert!((expected.linvel_m_s[0] - actual.linvel_m_s[0]).abs() < 1.0e-5);
        assert!((expected.mass_kg - actual.mass_kg).abs() < 1.0e-3);
    }
    // The adapter restored the portable pose epoch without modifying the
    // world-global freshness counter used by fracture snapshots.
    assert_eq!(
        cold.capture_dynamic_corpse_fracture_snapshot(restored, 0xabc, Some(ids[0]))
            .unwrap()
            .token
            .pose_epoch,
        0
    );
    for x in [0.14_f32, 1.11_f32] {
        let hit = cold
            .raycast_articulated(restored, [x, 1.0, 0.125], [0.0, -1.0, 0.0], 2.0)
            .unwrap();
        assert_eq!(hit.volume_id, VolumeId::new(90).unwrap());
    }
    let fixed = cold.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
        cell_m: 0.25,
        density_kg_m3: 1_000.0,
        mass_properties: None,
        translation_m: [0.75, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });
    cold.step();
    assert!(cold.articulated_contact_witnesses().iter().any(|witness| {
        witness.bodies.contains(&restored.0)
            && witness.bodies.contains(&fixed)
            && witness.source_sides.iter().flatten().any(|side| {
                side.volume_id == VolumeId::new(90).unwrap() && side.normal_impulse_n_s.is_some()
            })
    }));

    // Removing the bridge from the restored first component creates two
    // new components; serials continue above the persisted high-water.
    let edited = grid_from_x_cells(&[0, 2], 3);
    cold.rebuild_dynamic_corpse_component_part_grid(
        restored,
        ids[0],
        2,
        3,
        7,
        7,
        8,
        edited,
        mix_density,
    )
    .unwrap();
    let edited_snapshot = cold
        .capture_dynamic_corpse_fracture_snapshot(restored, 0xabc, Some(ids[0]))
        .unwrap();
    let edited_plan = prepare_fracture(&edited_snapshot).unwrap();
    assert_eq!(edited_plan.components.len(), 2);
    let resplit = cold
        .commit_dynamic_corpse_fracture(restored, &edited_snapshot, &edited_plan, mix_density)
        .unwrap();
    assert_eq!(
        resplit
            .components
            .iter()
            .map(|entry| entry.0.serial)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert!(cold.fracture_component_state(restored, ids[0]).is_err());
}

#[test]
fn whole_owner_capture_roundtrips_components_empty_binding_and_serial_gap() {
    let cfg = PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..Default::default()
    };
    let mut root = one_part(grid_from_x_cells(&[0, 1, 4], 5), 8_201, 7);
    root.part_id = 1;
    root.local_transform = crate::articulated::PartTransform {
        translation_m: [0.25, -0.1, 0.0],
        rotation_xyzw: [0.0, 0.0, 0.38268343, 0.9238795],
    };
    let middle = crate::articulated::SourcePart {
        part_id: 3,
        parent_id: Some(1),
        volume_id: VolumeId::new(8_203).unwrap(),
        source_revision: 4,
        grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
        cell_m: 0.5,
        local_transform: crate::articulated::PartTransform {
            translation_m: [0.4, 0.0, 0.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        },
    };
    let empty = crate::articulated::SourcePart {
        part_id: 4,
        parent_id: Some(1),
        volume_id: VolumeId::new(8_202).unwrap(),
        source_revision: 9,
        grid: OccupancyGrid::from_solid_mask(
            GlobalCell::new(10, 0, 0),
            [1, 1, 1],
            vec![false],
            vec![MaterialId::AIR],
        )
        .unwrap(),
        cell_m: 0.25,
        local_transform: crate::articulated::PartTransform {
            translation_m: [0.0, 0.5, 0.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        },
    };
    let sources = vec![root.clone(), middle.clone(), empty.clone()];
    let root_cells = source_cell_refs(&root);
    let mut component_cells: Vec<Vec<SourceCellRef>> = vec![
        root_cells
            .iter()
            .copied()
            .filter(|cell| cell.cell[0] < 2)
            .collect(),
        root_cells
            .iter()
            .copied()
            .filter(|cell| cell.cell[0] == 4)
            .collect(),
    ];
    component_cells[0].extend(source_cell_refs(&middle));
    component_cells[0].sort_unstable();
    let live = PhysicsWorld::new(cfg);
    let first_id = DerivedFractureComponentId {
        owner_key: 0x8a01,
        serial: 1,
    };
    let third_id = DerivedFractureComponentId {
        owner_key: 0x8a01,
        serial: 3,
    };
    let mut restore_world = live;
    let owner = restore_world
        .restore_dynamic_corpse(
            crate::articulated::DynamicCorpseRestore {
                owner_key: 0x8a01,
                source_generation: 5,
                topology_revision: 12,
                pose_epoch: 27,
                next_component_serial: 4,
                source_parts: sources.clone(),
                // Deliberately make serial 3 the physical primary. Cold
                // restore must preserve this exact ordering even though
                // component identities themselves sort in the opposite order.
                components: vec![
                    crate::articulated::DynamicCorpseComponentRestore {
                        component_id: Some(third_id),
                        translation_m: [3.0, 0.0, 0.0],
                        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                        linear_velocity_m_s: [-0.5, 0.0, 0.0],
                        angular_velocity_rad_s: [0.0, 0.0, -0.2],
                        cells: component_cells[1].clone(),
                    },
                    crate::articulated::DynamicCorpseComponentRestore {
                        component_id: Some(first_id),
                        translation_m: [1.0, 0.0, 0.0],
                        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                        linear_velocity_m_s: [0.25, 0.0, 0.0],
                        angular_velocity_rad_s: [0.0, 0.0, 0.1],
                        cells: component_cells[0].clone(),
                    },
                ],
            },
            Representation::MergedCuboids,
            mix_density,
        )
        .unwrap();
    let before_capture = restore_world.body_count();
    let snapshot = restore_world.articulated_owner_snapshot(owner).unwrap();
    let captured = restore_world
        .capture_dynamic_corpse_owner(
            owner,
            crate::articulated::DynamicCorpseOwnerFence {
                owner_key: 0x8a01,
                source_generation: snapshot.source_generation,
                topology_revision: snapshot.topology_token,
                pose_token: snapshot.pose_token,
                next_component_serial: snapshot.next_component_serial,
            },
            &sources,
        )
        .unwrap();
    assert_eq!(restore_world.body_count(), before_capture);
    assert_eq!(captured.owner_key, 0x8a01);
    assert_eq!(captured.source_generation, 5);
    assert_eq!(captured.topology_revision, 12);
    assert_eq!(captured.pose_epoch, 27);
    assert_eq!(captured.next_component_serial, 4);
    assert_eq!(captured.source_parts.len(), 3);
    assert_eq!(captured.source_parts[2].grid.solid_count(), 0);
    assert_eq!(
        captured
            .components
            .iter()
            .map(|component| component.component_id.unwrap())
            .collect::<Vec<_>>(),
        vec![third_id, first_id]
    );
    assert_eq!(captured.components[0].cells, component_cells[1]);
    assert_eq!(captured.components[1].cells, component_cells[0]);

    let mut cold = PhysicsWorld::new(cfg);
    let restored = cold
        .restore_dynamic_corpse(captured, Representation::MergedCuboids, mix_density)
        .unwrap();
    assert_eq!(cold.articulated_collider_count(restored), 3);
    assert_eq!(cold.articulated_rigid_body_count(restored), 2);
    assert_eq!(
        cold.articulated_owner_snapshot(restored)
            .unwrap()
            .next_component_serial,
        4
    );
    for component_id in [first_id, third_id] {
        let before = restore_world
            .fracture_component_state(owner, component_id)
            .unwrap();
        let after = cold
            .fracture_component_state(restored, component_id)
            .unwrap();
        assert_eq!(before.translation_m, after.translation_m);
        assert_eq!(before.rotation, after.rotation);
        assert_eq!(before.linvel_m_s, after.linvel_m_s);
        assert_eq!(before.angvel_rad_s, after.angvel_rad_s);
        assert!((before.mass_kg - after.mass_kg).abs() < 1.0e-4);
    }
    let mut matching_rays = 0;
    for index in -8..40 {
        let x = index as f32 * 0.125;
        let live_hit = restore_world
            .raycast_articulated(owner, [x, 3.0, 0.125], [0.0, -1.0, 0.0], 6.0)
            .ok();
        let cold_hit = cold
            .raycast_articulated(restored, [x, 3.0, 0.125], [0.0, -1.0, 0.0], 6.0)
            .ok();
        assert_eq!(live_hit, cold_hit);
        matching_rays += usize::from(live_hit.is_some());
    }
    assert!(
        matching_rays > 0,
        "the restored source query should hit both shard sets"
    );

    let before_bad_capture = restore_world.body_count();
    let mut bad_source = sources.clone();
    bad_source[0].grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        [5, 1, 1],
        vec![true, true, false, false, true],
        vec![fixtures::DIRT; 5],
    )
    .unwrap();
    assert!(
        restore_world
            .capture_dynamic_corpse_owner(
                owner,
                crate::articulated::DynamicCorpseOwnerFence {
                    owner_key: 0x8a01,
                    source_generation: 5,
                    topology_revision: 12,
                    pose_token: 27,
                    next_component_serial: 4,
                },
                &bad_source,
            )
            .is_err()
    );
    assert!(
        restore_world
            .capture_dynamic_corpse_owner(
                owner,
                crate::articulated::DynamicCorpseOwnerFence {
                    owner_key: 0x8a01,
                    source_generation: 5,
                    topology_revision: 12,
                    pose_token: 26,
                    next_component_serial: 4,
                },
                &sources,
            )
            .is_err()
    );
    assert!(
        restore_world
            .capture_dynamic_corpse_owner(
                owner,
                crate::articulated::DynamicCorpseOwnerFence {
                    owner_key: 0x8a02,
                    source_generation: 5,
                    topology_revision: 12,
                    pose_token: 27,
                    next_component_serial: 4,
                },
                &sources,
            )
            .is_err()
    );
    restore_world
        .articulated
        .get_mut(&owner.0)
        .unwrap()
        .pending_root_pose_token = Some(28);
    assert!(
        restore_world
            .capture_dynamic_corpse_owner(
                owner,
                crate::articulated::DynamicCorpseOwnerFence {
                    owner_key: 0x8a01,
                    source_generation: 5,
                    topology_revision: 12,
                    pose_token: 27,
                    next_component_serial: 4,
                },
                &sources,
            )
            .is_err()
    );
    restore_world
        .articulated
        .get_mut(&owner.0)
        .unwrap()
        .pending_root_pose_token = None;
    assert_eq!(restore_world.body_count(), before_bad_capture);
    assert!(
        restore_world
            .fracture_component_state(owner, first_id)
            .is_ok()
    );
}

#[test]
fn whole_owner_capture_uses_caller_key_without_binding_for_intact_checked_death() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let mut source = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 8_211, 2);
    let owner = world
        .add_articulated_kinematic_parts(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            14,
            vec![source.clone()],
            Representation::MergedCuboids,
        )
        .unwrap();
    let frozen = crate::articulated::PartTransform {
        translation_m: [0.25, 0.0, 0.0],
        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
    };
    world
        .update_articulated_pose(
            owner,
            ArticulatedPose {
                topology_token: 14,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![(source.part_id, frozen)],
            },
        )
        .unwrap();
    world.step();
    world
        .transfer_articulated_to_dynamic_corpse(owner, 14, 1, mix_density)
        .unwrap();
    assert_eq!(
        world.articulated_owner_snapshot(owner).unwrap().owner_key,
        None
    );
    assert!(!world.fracture_owner_keys.contains_key(&0x8a11));
    source.local_transform = frozen;
    let before = world.body_count();
    let captured = world
        .capture_dynamic_corpse_owner(
            owner,
            crate::articulated::DynamicCorpseOwnerFence {
                owner_key: 0x8a11,
                source_generation: 1,
                topology_revision: 14,
                pose_token: 1,
                next_component_serial: 1,
            },
            &[source],
        )
        .unwrap();
    assert_eq!(world.body_count(), before);
    assert_eq!(captured.owner_key, 0x8a11);
    assert_eq!(captured.pose_epoch, 1);
    assert_eq!(captured.components.len(), 1);
    assert_eq!(captured.components[0].component_id, None);
    let mut cold = PhysicsWorld::new(PhysicsConfig::default());
    let restored = cold
        .restore_dynamic_corpse(captured, Representation::MergedCuboids, mix_density)
        .unwrap();
    assert_eq!(cold.articulated_rigid_body_count(restored), 1);
    assert_eq!(cold.articulated_collider_count(restored), 1);
}

#[test]
fn sparse_large_intact_owner_captures_and_cold_restores_with_existing_dense_limit() {
    let dims = [1024, 1024, 1];
    let mut mask = vec![false; dims[0] as usize * dims[1] as usize];
    mask[0] = true;
    mask[dims[0] as usize * (dims[1] as usize - 1) + dims[0] as usize - 1] = true;
    let grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        dims,
        mask,
        vec![fixtures::STONE; dims[0] as usize * dims[1] as usize],
    )
    .unwrap();
    assert!(u64::from(dims[0]) * u64::from(dims[1]) > 1_000_000);
    let source = one_part(grid, 8_221, 3);
    let bundle = crate::articulated::DynamicCorpseRestore {
        owner_key: 0x8a21,
        source_generation: 2,
        topology_revision: 8,
        pose_epoch: 11,
        next_component_serial: 1,
        components: vec![crate::articulated::DynamicCorpseComponentRestore {
            component_id: None,
            translation_m: [0.0; 3],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            cells: source_cell_refs(&source),
        }],
        source_parts: vec![source.clone()],
    };
    let mut live = PhysicsWorld::new(PhysicsConfig::default());
    let owner = live
        .restore_dynamic_corpse(bundle, Representation::MergedCuboids, mix_density)
        .unwrap();
    let snapshot = live.articulated_owner_snapshot(owner).unwrap();
    let captured = live
        .capture_dynamic_corpse_owner(
            owner,
            crate::articulated::DynamicCorpseOwnerFence {
                owner_key: 0x8a21,
                source_generation: snapshot.source_generation,
                topology_revision: snapshot.topology_token,
                pose_token: snapshot.pose_token,
                next_component_serial: snapshot.next_component_serial,
            },
            &[source],
        )
        .unwrap();
    let mut cold = PhysicsWorld::new(PhysicsConfig::default());
    let restored = cold
        .restore_dynamic_corpse(captured, Representation::MergedCuboids, mix_density)
        .unwrap();
    assert_eq!(live.articulated_collider_count(owner), 1);
    assert_eq!(cold.articulated_collider_count(restored), 1);
    let before = live.articulated_body_state(owner);
    let after = cold.articulated_body_state(restored);
    assert!((before.mass_kg - after.mass_kg).abs() < 1.0e-4);
    assert_eq!(before.translation_m, after.translation_m);
}

#[test]
fn whole_owner_capture_rejects_composed_part_outside_restore_range() {
    let mut source = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 8_231, 1);
    source.local_transform.translation_m = [900.0, 0.0, 0.0];
    let bundle = crate::articulated::DynamicCorpseRestore {
        owner_key: 0x8a31,
        source_generation: 2,
        topology_revision: 8,
        pose_epoch: 11,
        next_component_serial: 1,
        components: vec![crate::articulated::DynamicCorpseComponentRestore {
            component_id: None,
            translation_m: [0.0; 3],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            cells: source_cell_refs(&source),
        }],
        source_parts: vec![source.clone()],
    };
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let owner = world
        .restore_dynamic_corpse(bundle, Representation::MergedCuboids, mix_density)
        .unwrap();
    let owner_snapshot = world.articulated_owner_snapshot(owner).unwrap();
    let body = world.entries[entry_index(owner.0, world.id_namespace)].body;
    world
        .bodies
        .get_mut(body)
        .unwrap()
        .set_translation(Vector::new(200.0, 0.0, 0.0), false);

    assert!(matches!(
        world.capture_dynamic_corpse_owner(
            owner,
            crate::articulated::DynamicCorpseOwnerFence {
                owner_key: 0x8a31,
                source_generation: owner_snapshot.source_generation,
                topology_revision: owner_snapshot.topology_token,
                pose_token: owner_snapshot.pose_token,
                next_component_serial: owner_snapshot.next_component_serial,
            },
            &[source],
        ),
        Err(crate::articulated::ArticulatedError::GeometryRange)
    ));
}

#[test]
fn cold_restore_accepts_mixed_source_cell_sizes_and_empty_bindings() {
    let mut root = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 101, 1);
    let mut child = crate::articulated::SourcePart {
        part_id: 8,
        parent_id: Some(root.part_id),
        volume_id: VolumeId::new(102).unwrap(),
        source_revision: 3,
        grid: one_cell_grid(GlobalCell::new(4, 0, 0)),
        cell_m: 0.5,
        local_transform: crate::articulated::PartTransform {
            translation_m: [1.0, 0.0, 0.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        },
    };
    let mut empty = one_part(
        OccupancyGrid::from_solid_mask(
            GlobalCell::new(10, 0, 0),
            [1, 1, 1],
            vec![false],
            vec![MaterialId::AIR],
        )
        .unwrap(),
        103,
        1,
    );
    empty.part_id = 9;
    empty.parent_id = Some(root.part_id);
    root.local_transform.translation_m = [0.0, 0.0, 0.0];
    child.local_transform.translation_m = [1.0, 0.0, 0.0];
    let mut cells = source_cell_refs(&root);
    cells.extend(source_cell_refs(&child));
    cells.sort();
    let component = crate::articulated::DynamicCorpseComponentRestore {
        component_id: None,
        translation_m: [2.0, 3.0, 4.0],
        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        linear_velocity_m_s: [0.5, 0.0, 0.0],
        angular_velocity_rad_s: [0.0; 3],
        cells,
    };
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let mut incomplete = component.clone();
    incomplete.cells.pop();
    assert_eq!(
        world.restore_dynamic_corpse(
            crate::articulated::DynamicCorpseRestore {
                owner_key: 0xdef,
                source_generation: 5,
                topology_revision: 4,
                pose_epoch: 8,
                next_component_serial: 1,
                source_parts: vec![root.clone(), child.clone(), empty.clone()],
                components: vec![incomplete],
            },
            Representation::MergedCuboids,
            mix_density,
        ),
        Err(crate::articulated::ArticulatedError::InvalidRestore)
    );
    assert!(world.entries.is_empty());
    let id = world
        .restore_dynamic_corpse(
            crate::articulated::DynamicCorpseRestore {
                owner_key: 0xdef,
                source_generation: 5,
                topology_revision: 4,
                pose_epoch: 8,
                next_component_serial: 1,
                source_parts: vec![root, child, empty],
                components: vec![component],
            },
            Representation::MergedCuboids,
            mix_density,
        )
        .unwrap();
    assert_eq!(world.articulated_collider_count(id), 2);
    assert_eq!(world.articulated_rigid_body_count(id), 1);
    assert_eq!(world.articulated_owner_snapshot(id).unwrap().pose_token, 8);
    assert!((world.articulated_body_state(id).mass_kg - 15.625 - 125.0).abs() < 1.0e-2);
    let reused_empty_binding = one_part(one_cell_grid(GlobalCell::new(20, 0, 0)), 103, 2);
    assert_eq!(
        world.restore_dynamic_corpse(
            crate::articulated::DynamicCorpseRestore {
                owner_key: 0x123,
                source_generation: 1,
                topology_revision: 1,
                pose_epoch: 1,
                next_component_serial: 1,
                components: vec![crate::articulated::DynamicCorpseComponentRestore {
                    component_id: None,
                    translation_m: [0.0; 3],
                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    linear_velocity_m_s: [0.0; 3],
                    angular_velocity_rad_s: [0.0; 3],
                    cells: source_cell_refs(&reused_empty_binding),
                }],
                source_parts: vec![reused_empty_binding.clone()],
            },
            Representation::MergedCuboids,
            mix_density,
        ),
        Err(crate::articulated::ArticulatedError::DuplicateVolume)
    );
    assert_eq!(
        world.add_articulated_body(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![reused_empty_binding.clone()],
            Representation::MergedCuboids,
        ),
        Err(crate::articulated::ArticulatedError::DuplicateVolume)
    );
    world.retire_articulated_body(id);
    assert_eq!(world.fracture_owner_keys.get(&0xdef), Some(&id.0));
    assert!(
        world
            .add_articulated_body(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![reused_empty_binding],
                Representation::MergedCuboids,
            )
            .is_ok()
    );
}

#[test]
fn validated_articulated_batch_rejects_foreign_world_before_mutation() {
    let source = PhysicsWorld::new(PhysicsConfig::default());
    let prepared = source
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::AliveBirth {
                operation_tag: 25,
                translation_m: [0.0; 3],
                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                topology_token: 25,
                parts: vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 515, 1)],
                representation: Representation::MergedCuboids,
            }],
            mix_density,
        )
        .unwrap();
    let validated = source.validate_articulated_batch(prepared).unwrap();
    let mut other = PhysicsWorld::new(PhysicsConfig::default());
    let before_source = source.body_count();
    let before_other = other.body_count();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        other.publish_validated_articulated_batch(validated)
    }));
    assert!(result.is_err());
    assert_eq!(source.body_count(), before_source);
    assert_eq!(other.body_count(), before_other);
}

#[test]
fn restore_rejects_bad_identity_partition_and_density_before_publication() {
    let part = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 111, 4);
    let cells = source_cell_refs(&part);
    let make_intact = || crate::articulated::DynamicCorpseRestore {
        owner_key: 0x456,
        source_generation: 3,
        topology_revision: 2,
        pose_epoch: 1,
        next_component_serial: 1,
        source_parts: vec![part.clone()],
        components: vec![crate::articulated::DynamicCorpseComponentRestore {
            component_id: None,
            translation_m: [0.0; 3],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            cells: cells.clone(),
        }],
    };
    let mut invalid = Vec::new();
    let mut bad = make_intact();
    bad.owner_key = 0;
    invalid.push(bad);
    let mut bad = make_intact();
    bad.source_generation = 0;
    invalid.push(bad);
    let mut bad = make_intact();
    bad.topology_revision = 0;
    invalid.push(bad);
    let mut bad = make_intact();
    bad.pose_epoch = 0;
    invalid.push(bad);
    let mut bad = make_intact();
    bad.next_component_serial = 2;
    invalid.push(bad);
    let mut bad = make_intact();
    bad.source_parts[0].source_revision = 0;
    invalid.push(bad);
    let mut bad = make_intact();
    bad.components[0].component_id = Some(DerivedFractureComponentId {
        owner_key: 0x999,
        serial: 1,
    });
    bad.next_component_serial = 2;
    invalid.push(bad);
    let mut bad = make_intact();
    bad.components[0].component_id = Some(DerivedFractureComponentId {
        owner_key: 0x456,
        serial: 1,
    });
    bad.next_component_serial = 1;
    invalid.push(bad);

    let derived_component = |serial| crate::articulated::DynamicCorpseComponentRestore {
        component_id: Some(DerivedFractureComponentId {
            owner_key: 0x456,
            serial,
        }),
        translation_m: [0.0; 3],
        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        linear_velocity_m_s: [0.0; 3],
        angular_velocity_rad_s: [0.0; 3],
        cells: cells.clone(),
    };
    let mut overlapping = make_intact();
    overlapping.next_component_serial = 3;
    overlapping.components = vec![derived_component(1), derived_component(2)];
    invalid.push(overlapping);

    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let live_owner = world
        .add_articulated_body(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            19,
            vec![one_part(one_cell_grid(GlobalCell::new(30, 0, 0)), 110, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    for bundle in invalid {
        assert!(
            world
                .restore_dynamic_corpse(bundle, Representation::MergedCuboids, mix_density)
                .is_err()
        );
        assert_eq!(world.entries.len(), 1);
        assert!(world.articulated_owner_snapshot(live_owner).is_some());
        assert!(world.fracture_owner_keys.is_empty());
    }

    assert_eq!(
        world.restore_dynamic_corpse(make_intact(), Representation::MergedCuboids, |_| 0.0,),
        Err(crate::articulated::ArticulatedError::InvalidDensity)
    );
    assert_eq!(world.entries.len(), 1);
    assert!(world.articulated_owner_snapshot(live_owner).is_some());
    assert!(world.fracture_owner_keys.is_empty());
}

#[test]
fn restore_keeps_single_derived_component_highwater_for_next_split() {
    let part = one_part(grid_from_x_cells(&[0, 1, 2], 3), 112, 7);
    let component_id = DerivedFractureComponentId {
        owner_key: 0x789,
        serial: 4,
    };
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let owner = world
        .restore_dynamic_corpse(
            crate::articulated::DynamicCorpseRestore {
                owner_key: component_id.owner_key,
                source_generation: 9,
                topology_revision: 8,
                pose_epoch: 5,
                next_component_serial: 9,
                source_parts: vec![part.clone()],
                components: vec![crate::articulated::DynamicCorpseComponentRestore {
                    component_id: Some(component_id),
                    translation_m: [1.0, 2.0, 3.0],
                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    linear_velocity_m_s: [0.5, 0.0, 0.0],
                    angular_velocity_rad_s: [0.0; 3],
                    cells: source_cell_refs(&part),
                }],
            },
            Representation::MergedCuboids,
            mix_density,
        )
        .unwrap();
    assert_eq!(world.articulated_rigid_body_count(owner), 1);
    assert_eq!(
        world
            .fracture_component_state(owner, component_id)
            .unwrap()
            .translation_m,
        [1.0, 2.0, 3.0]
    );
    world
        .rebuild_dynamic_corpse_component_part_grid(
            owner,
            component_id,
            8,
            9,
            7,
            7,
            8,
            grid_from_x_cells(&[0, 2], 3),
            mix_density,
        )
        .unwrap();
    let snapshot = world
        .capture_dynamic_corpse_fracture_snapshot(owner, 0x789, Some(component_id))
        .unwrap();
    let plan = prepare_fracture(&snapshot).unwrap();
    let split = world
        .commit_dynamic_corpse_fracture(owner, &snapshot, &plan, mix_density)
        .unwrap();
    assert_eq!(
        split
            .components
            .iter()
            .map(|entry| entry.0.serial)
            .collect::<Vec<_>>(),
        vec![9, 10]
    );
}

use super::*;
use crate::{creatures::tests::FixtureCatalog, sim::creature_capture::tests::born};
use spall_core::{CellSizeCode, CreatureAllocatorSnapshotV1, Revision, VolumeId};
use spall_protocol::{BaselineCells, CreatureOwnershipSetV4, WIRE_SCHEMA_VERSION};

pub(super) fn target(snapshot: &CreatureSnapshotV2) -> Simulation {
    let mut sim =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    let c = sim.world.registry().counters();
    sim.world
        .resume_registry(c.0, snapshot.next_volume_id, c.2, c.3)
        .unwrap();
    sim
}

fn untouched(sim: &Simulation, bodies: usize, counters: (u64, u64, u64, u64), cursor: u64) {
    assert!(sim.creatures.is_none());
    assert_eq!(sim.world.creature_source_counts(), [0; 5]);
    assert_eq!(sim.world.physics().body_count(), bodies);
    assert_eq!(sim.world.registry().counters(), counters);
    assert_eq!(sim.journal.cursor(), cursor);
}

#[test]
fn joint_restore_mixed_alive_and_corpse_preserves_all_saved_highwaters() {
    let (mut live, fixture, state, _) = born();
    let id = VolumeId::new(live.world.registry().counters().1).unwrap();
    let mut second = fixture.state(2, id.get());
    second.root_translation_m = [20.0, 8.0, 10.0];
    let mut volume = spall_voxel::Volume::new(id, CellSizeCode::Quarter);
    volume
        .apply_edit(&spall_voxel::EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    live.commit_creature_birth_for_test(
        second.clone(),
        &fixture.view(),
        vec![CreatureBirthPartInput {
            part_id: 1,
            parent_id: None,
            local_transform: PartTransform::IDENTITY,
            volume,
        }],
        Representation::MergedCuboids,
    )
    .unwrap();
    live.commit_creature_death_for_test(
        second.key,
        second.state_revision,
        second.aggregate_topology_revision,
        0,
        &fixture.view(),
    )
    .unwrap();
    let mut snapshot = live
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    // Retired IDs/gaps are meaningful even when no current owner uses them.
    snapshot.allocator.next_creature_id = 100;
    snapshot.next_volume_id += 20;
    let mut restored = assert_joint_roundtrip(&snapshot, &fixture.view());
    assert_eq!(restored.creatures.as_ref().unwrap().record_count(), 2);
    let owner = restored.world.creature_physics_owner(state.key).unwrap();
    restored
        .world
        .physics_mut()
        .update_articulated_pose(
            owner,
            spall_physics::ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: state.pose_epoch,
                pose_token: state.pose_epoch + 1,
                parts: vec![(1, PartTransform::IDENTITY)],
            },
        )
        .unwrap();
}

#[test]
fn joint_restore_empty_population_preserves_retired_allocator_and_refuses_replacement() {
    let sim =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    let fixture = FixtureCatalog::with_materials(sim.world.materials().clone());
    let snapshot = CreatureSnapshotV2 {
        schema: WIRE_SCHEMA_VERSION,
        allocator: CreatureAllocatorSnapshotV1 {
            schema: spall_core::STORE_SCHEMA_VERSION_U16,
            world_id: WorldId::from_u128(10),
            world_epoch: 7,
            next_creature_id: 101,
        },
        next_volume_id: sim.world.registry().counters().1,
        owners: CreatureOwnershipSetV4 {
            schema: WIRE_SCHEMA_VERSION,
            records: vec![],
        },
    };
    let restored = assert_joint_roundtrip(&snapshot, &fixture.view());
    assert!(matches!(
        restored.prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &fixture.view()),
        Err(CreatureRestoreError::Context)
    ));
    assert_eq!(
        restored
            .capture_creature_snapshot(&fixture.view())
            .unwrap()
            .unwrap(),
        snapshot
    );
}

#[test]
fn joint_restore_stale_foreign_counter_catalog_and_source_collisions_are_atomic() {
    let (live, fixture, _, _) = born();
    let snapshot = live
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    for mode in 0..6 {
        let mut sim = target(&snapshot);
        let prepared = sim
            .prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &fixture.view())
            .unwrap();
        match mode {
            0 => sim.world.step_physics(),
            1 => {
                sim = target(&snapshot);
            }
            2 => {
                let c = sim.world.registry().counters();
                sim.world.resume_registry(c.0, c.1 + 1, c.2, c.3).unwrap();
            }
            3 => {
                let terrain = sim.world.terrain().volume.id();
                sim.world
                    .volume_body_mut(terrain)
                    .unwrap()
                    .volume
                    .resume_revision_counter(Revision(10))
                    .unwrap();
            }
            4 => {
                sim.initialize_creature_host(snapshot.allocator.world_id)
                    .unwrap();
            }
            _ => (),
        }
        let c = sim.world.registry().counters();
        let cursor = sim.journal.cursor();
        let bodies = sim.world.physics().body_count();
        let mut catalog = fixture.view();
        if mode == 5 {
            catalog.material_mapping_version += 1;
        }
        assert!(
            sim.commit_prepared_creature_restore(prepared, &catalog)
                .is_err(),
            "mode {mode}"
        );
        if mode == 4 {
            assert_eq!(sim.creatures.as_ref().unwrap().record_count(), 0);
            assert_eq!(sim.world.physics().body_count(), bodies);
            assert_eq!(sim.world.registry().counters(), c);
        } else {
            untouched(&sim, bodies, c, cursor);
        }
    }
    let sim = target(&snapshot);
    assert!(matches!(
        sim.prepare_creature_restore(WorldId::from_u128(99), &snapshot, &fixture.view()),
        Err(CreatureRestoreError::Context)
    ));
    let mut collision = snapshot.clone();
    let source = &mut collision.owners.records[0];
    let terrain = sim.world.terrain().volume.id();
    source.parts[0].source_volume_id = terrain;
    source.state.source_bindings[0].source_volume_id = terrain;
    source.components[0].source_masks[0].volume_id = terrain;
    source.parts[0].source_content_hash = source.parts[0].canonical_source_content_hash();
    collision.validate(&fixture.view()).unwrap();
    let c = sim.world.registry().counters();
    let n = sim.world.physics().body_count();
    let cursor = sim.journal.cursor();
    assert!(matches!(
        sim.prepare_creature_restore(snapshot.allocator.world_id, &collision, &fixture.view()),
        Err(CreatureRestoreError::Context)
    ));
    untouched(&sim, n, c, cursor);
    let mut mismatch = snapshot.clone();
    mismatch.next_volume_id += 1;
    assert!(matches!(
        sim.prepare_creature_restore(snapshot.allocator.world_id, &mismatch, &fixture.view()),
        Err(CreatureRestoreError::Context)
    ));
}

#[test]
fn joint_restore_sparse_span_budget_refuses_without_publishing_a_valid_first_owner() {
    let (live, fixture, _, _) = born();
    let mut snapshot = live
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    let mut second = snapshot.owners.records[0].clone();
    second.state.key.creature_id = spall_core::CreatureId::new(2).unwrap();
    second.owner_key += 1;
    second.parts[0].source_volume_id = VolumeId::new(snapshot.next_volume_id).unwrap();
    second.parts[0].bounds = None;
    second.state.source_bindings[0].source_volume_id = second.parts[0].source_volume_id;
    let mut brick = second.parts[0].bricks[0].clone();
    brick.coord = [125001, 0, 0];
    second.parts[0].bricks.push(brick);
    second.parts[0].source_content_hash = second.parts[0].canonical_source_content_hash();
    second.components[0].owner_key = second.owner_key;
    second.components[0].source_masks[0].volume_id = second.parts[0].source_volume_id;
    let mut mask = second.components[0].source_masks[0].clone();
    mask.coord = [125001, 0, 0];
    second.components[0].source_masks.push(mask);
    assert!(matches!(
        second.parts[0].bricks[0].cells,
        BaselineCells::Dense(_)
    ));
    snapshot.owners.records.push(second);
    snapshot.allocator.next_creature_id = 3;
    snapshot.next_volume_id += 1;
    snapshot.validate(&fixture.view()).unwrap();
    let sim = target(&snapshot);
    let c = sim.world.registry().counters();
    let n = sim.world.physics().body_count();
    let cursor = sim.journal.cursor();
    assert!(matches!(
        sim.prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &fixture.view()),
        Err(CreatureRestoreError::Limit)
    ));
    untouched(&sim, n, c, cursor);
}

#[test]
fn joint_restore_invalid_second_physical_owner_never_publishes_the_first() {
    let (live, fixture, _, _) = born();
    let mut snapshot = live
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    let mut second = snapshot.owners.records[0].clone();
    second.state.key.creature_id = spall_core::CreatureId::new(2).unwrap();
    second.owner_key += 1;
    let volume = VolumeId::new(snapshot.next_volume_id).unwrap();
    second.parts[0].source_volume_id = volume;
    second.state.source_bindings[0].source_volume_id = volume;
    second.components[0].source_masks[0].volume_id = volume;
    second.components[0].owner_key = second.owner_key;
    // Valid portable transform, outside the exact physics geometry extent.
    second.parts[0].frozen_local_pose.translation_m = [10000.0, 0.0, 0.0];
    second.parts[0].source_content_hash = second.parts[0].canonical_source_content_hash();
    snapshot.owners.records.push(second);
    snapshot.allocator.next_creature_id = 3;
    snapshot.next_volume_id += 1;
    snapshot.validate(&fixture.view()).unwrap();
    let sim = target(&snapshot);
    let c = sim.world.registry().counters();
    let n = sim.world.physics().body_count();
    let cursor = sim.journal.cursor();
    assert!(matches!(
        sim.prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &fixture.view()),
        Err(CreatureRestoreError::Physics(_))
    ));
    untouched(&sim, n, c, cursor);
}

#[test]
fn joint_restore_physics_owner_key_conflict_refuses_without_mutation() {
    let (live, fixture, _, _) = born();
    let snapshot = live
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    let record = &snapshot.owners.records[0];
    let mut sim = target(&snapshot);
    let volume_id = VolumeId::new(snapshot.next_volume_id + 10).unwrap();
    let mut volume = spall_voxel::Volume::new(volume_id, CellSizeCode::Quarter);
    volume
        .apply_edit(&spall_voxel::EditPlan::filled_box(
            volume_id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    let physical = sim
        .world
        .physics()
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::AliveRestore {
                operation_tag: 1,
                representation: Representation::MergedCuboids,
                restore: AliveArticulatedRestore {
                    owner_key: record.owner_key,
                    source_generation: 1,
                    topology_token: 2,
                    pose_token: 3,
                    next_component_serial: 1,
                    root_translation_m: [30.0, 8.0, 10.0],
                    root_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    root_linear_velocity_m_s: [0.0; 3],
                    root_angular_velocity_rad_s: [0.0; 3],
                    part_motion: vec![AliveArticulatedPartMotion {
                        part_id: 1,
                        linear_velocity_m_s: [0.0; 3],
                        angular_velocity_rad_s: [0.0; 3],
                    }],
                    parts: vec![SourcePart {
                        part_id: 1,
                        parent_id: None,
                        volume_id,
                        source_revision: 1,
                        grid: OccupancyGrid::from_volume(&volume).unwrap().unwrap(),
                        cell_m: 0.25,
                        local_transform: PartTransform::IDENTITY,
                    }],
                },
            }],
            |m| sim.world.density(m),
        )
        .unwrap();
    let existing = sim
        .world
        .physics_mut()
        .commit_articulated_batch(physical)
        .unwrap()[0]
        .owner_id;
    let prior = sim
        .world
        .physics()
        .articulated_owner_snapshot(existing)
        .unwrap();
    // This separate physics owner is deliberately unbound to a creature host.
    // Physics preparation already rejects its occupied owner key.
    let c = sim.world.registry().counters();
    let n = sim.world.physics().body_count();
    let cursor = sim.journal.cursor();
    assert!(matches!(
        sim.prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &fixture.view()),
        Err(CreatureRestoreError::Physics(_))
    ));
    untouched(&sim, n, c, cursor);
    let after = sim
        .world
        .physics()
        .articulated_owner_snapshot(existing)
        .unwrap();
    assert_eq!(after.root.translation_m, prior.root.translation_m);
    assert_eq!(after.root.rotation, prior.root.rotation);
    assert_eq!(after.root.linvel_m_s, prior.root.linvel_m_s);
    assert_eq!(after.root.angvel_rad_s, prior.root.angvel_rad_s);
    assert_eq!(after.owner_key, prior.owner_key);
}

#[test]
fn joint_restore_local_frame_roundtrip_and_rounding_refusal() {
    let (live, fixture, _, _) = born();
    let snapshot = live
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    let mut sim = target(&snapshot);
    sim.world
        .set_physics_origin_for_test(PhysicsOrigin::new([8.0, 0.0, 8.0]).unwrap());
    let prepared = sim
        .prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &fixture.view())
        .unwrap();
    sim.commit_prepared_creature_restore(prepared, &fixture.view())
        .unwrap();
    assert_eq!(
        sim.capture_creature_snapshot(&fixture.view())
            .unwrap()
            .unwrap(),
        snapshot
    );
    let mut sim = target(&snapshot);
    let prepared = sim
        .prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &fixture.view())
        .unwrap();
    sim.world
        .set_physics_origin_for_test(PhysicsOrigin::new([0.1, 0.0, 0.0]).unwrap());
    let c = sim.world.registry().counters();
    let n = sim.world.physics().body_count();
    let cursor = sim.journal.cursor();
    assert!(matches!(
        sim.commit_prepared_creature_restore(prepared, &fixture.view()),
        Err(CreatureRestoreError::Stale)
    ));
    assert!(matches!(
        sim.prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &fixture.view()),
        Err(CreatureRestoreError::Limit)
    ));
    untouched(&sim, n, c, cursor);
}

#[test]
fn joint_restore_empty_population_bounds_unused_catalog_before_fingerprinting() {
    let (live, fixture, _, _) = born();
    let mut snapshot = live
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    snapshot.owners.records.clear();
    let sim = target(&snapshot);
    let c = sim.world.registry().counters();
    let n = sim.world.physics().body_count();
    let cursor = sim.journal.cursor();
    let base = fixture.view();
    let too_many = vec![base.assets[0]; spall_protocol::MAX_CREATURE_CATALOG_ASSETS + 1];
    let oversized = CreatureValidationCatalog {
        assets: &too_many,
        ..fixture.view()
    };
    assert!(matches!(
        sim.prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &oversized),
        Err(CreatureRestoreError::Limit)
    ));
    let long_name = "x".repeat(spall_protocol::MAX_CREATURE_CLIP_NAME_BYTES + 1);
    let names = [long_name.as_str()];
    let assets = [spall_protocol::CreatureAssetCatalogEntry {
        clip_names: &names,
        ..base.assets[0]
    }];
    let oversized = CreatureValidationCatalog {
        assets: &assets,
        ..fixture.view()
    };
    assert!(matches!(
        sim.prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &oversized),
        Err(CreatureRestoreError::Limit)
    ));
    untouched(&sim, n, c, cursor);
}

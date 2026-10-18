use super::*;
use crate::creatures::tests::FixtureCatalog;
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, Revision, VolumeId};
use spall_physics::{
    ArticulatedBatchOperation, PartTransform, PhysicsConfig, PhysicsWorld, Representation,
};
use spall_voxel::{Brick, EditPlan};

fn born() -> (
    Simulation,
    FixtureCatalog,
    CreatureStateV1,
    spall_physics::ArticulatedBodyId,
) {
    let mut sim =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    sim.initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let fixture = FixtureCatalog::with_materials(sim.world.materials().clone());
    let volume_id = VolumeId::new(sim.world.registry().counters().1).unwrap();
    let mut state = fixture.state(1, volume_id.get());
    state.root_translation_m = [10.0, 8.0, 10.0];
    let mut source = Volume::new(volume_id, CellSizeCode::Quarter);
    source
        .apply_edit(&EditPlan::filled_box(
            volume_id,
            GlobalCell::new(-1, 0, 0),
            GlobalCell::new(-1, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    let owner = sim
        .commit_creature_birth_for_test(
            state.clone(),
            &fixture.view(),
            vec![CreatureBirthPartInput {
                part_id: 1,
                parent_id: None,
                local_transform: PartTransform::IDENTITY,
                volume: source,
            }],
            Representation::MergedCuboids,
        )
        .unwrap();
    (sim, fixture, state, owner)
}

fn replace_sources(sim: &mut Simulation, state: &CreatureStateV1, source: Volume) {
    let metadata = creature_source_freshness(&source, None, PartTransform::IDENTITY).unwrap();
    sim.world
        .publish_creature_sources_with_metadata(state.key, vec![(1, source, metadata)]);
}

#[test]
fn capture_distinguishes_uninitialized_and_empty_retired_allocator() {
    let mut sim =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    let fixture = FixtureCatalog::with_materials(sim.world.materials().clone());
    assert!(
        sim.capture_creature_snapshot(&fixture.view())
            .unwrap()
            .is_none()
    );
    let allocator = CreatureAllocatorSnapshotV1 {
        schema: 1,
        world_id: WorldId::from_u128(10),
        world_epoch: 7,
        next_creature_id: 101,
    };
    sim.restore_creature_allocator_snapshot(allocator).unwrap();
    let counters = sim.world.registry().counters();
    let captured = sim
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    assert_eq!(captured.allocator, allocator);
    assert_eq!(captured.next_volume_id, counters.1);
    assert!(captured.owners.records.is_empty());
    assert_eq!(sim.world.registry().counters(), counters);
    let encoded = postcard::to_stdvec(&captured).unwrap();
    assert_eq!(
        CreatureSnapshotV2::decode(&encoded, &fixture.view()).unwrap(),
        captured
    );
}

#[test]
fn initial_token_zero_and_checked_death_capture_map_the_logical_epoch() {
    let (mut sim, fixture, state, id) = born();
    assert_eq!(
        sim.world
            .physics()
            .articulated_owner_snapshot(id)
            .unwrap()
            .pose_token,
        0
    );
    let alive = sim
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    let record = &alive.owners.records[0];
    assert_eq!(record.state.pose_epoch, state.pose_epoch);
    assert_eq!(record.components[0].source_masks[0].coord, [-1, 0, 0]);
    assert_eq!(record.components[0].source_masks[0].cells[0].index, 31);
    assert_eq!(
        record.parts[0].initial_local_pose,
        local_pose(PartTransform::IDENTITY)
    );
    let counters = sim.world.registry().counters();
    let sources = sim
        .capture_physical_sources(state.key, &record.parts)
        .unwrap();
    let mut restored_alive = PhysicsWorld::new(PhysicsConfig::default());
    let prepared = restored_alive
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::AliveRestore {
                operation_tag: 1,
                representation: Representation::MergedCuboids,
                restore: spall_physics::AliveArticulatedRestore {
                    owner_key: record.owner_key,
                    source_generation: record.source_generation,
                    topology_token: record.state.aggregate_topology_revision,
                    pose_token: record.state.pose_epoch,
                    next_component_serial: record.next_component_serial,
                    root_translation_m: record.state.root_translation_m.map(|v| v as f32),
                    root_rotation_xyzw: record.state.root_rotation_xyzw,
                    root_linear_velocity_m_s: record.state.linear_velocity_m_s,
                    root_angular_velocity_rad_s: record.state.angular_velocity_rad_s,
                    parts: sources,
                    part_motion: record
                        .part_velocities
                        .iter()
                        .map(|p| spall_physics::AliveArticulatedPartMotion {
                            part_id: p.part_id,
                            linear_velocity_m_s: p.linear_velocity_m_s,
                            angular_velocity_rad_s: p.angular_velocity_rad_s,
                        })
                        .collect(),
                },
            }],
            |material| sim.world.density(material),
        )
        .unwrap();
    let restored_id = restored_alive.commit_articulated_batch(prepared).unwrap()[0].owner_id;
    assert_eq!(
        restored_alive
            .articulated_owner_snapshot(restored_id)
            .unwrap()
            .pose_token,
        state.pose_epoch
    );
    restored_alive
        .update_articulated_pose(
            restored_id,
            spall_physics::ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: state.pose_epoch,
                pose_token: state.pose_epoch + 1,
                parts: vec![(1, PartTransform::IDENTITY)],
            },
        )
        .unwrap();
    let host_revision = sim.creatures.as_ref().unwrap().revision();
    assert_eq!(
        sim.capture_creature_snapshot(&fixture.view())
            .unwrap()
            .unwrap(),
        alive
    );
    assert_eq!(sim.world.registry().counters(), counters);
    assert_eq!(sim.creatures.as_ref().unwrap().revision(), host_revision);

    sim.commit_creature_death_for_test(
        state.key,
        state.state_revision,
        state.aggregate_topology_revision,
        0,
        &fixture.view(),
    )
    .unwrap();
    sim.world.step_physics();
    let captured = sim
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    let record = &captured.owners.records[0];
    assert_eq!(record.state.pose_epoch, state.pose_epoch + 1);
    assert!(record.part_velocities.is_empty());
    let owner = sim.world.physics().articulated_owner_snapshot(id).unwrap();
    assert_eq!(owner.pose_token, 0);
    let sources = sim
        .capture_physical_sources(state.key, &record.parts)
        .unwrap();
    let runtime = sim
        .world
        .physics()
        .capture_dynamic_corpse_owner_at_epoch(
            id,
            DynamicCorpseOwnerFence {
                owner_key: record.owner_key,
                source_generation: owner.source_generation,
                topology_revision: owner.topology_token,
                pose_token: owner.pose_token,
                next_component_serial: owner.next_component_serial,
            },
            &sources,
            record.state.pose_epoch,
        )
        .unwrap();
    let mut restored = PhysicsWorld::new(PhysicsConfig::default());
    let prepared = restored
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::ColdRestore {
                operation_tag: 1,
                restore: runtime,
                representation: Representation::MergedCuboids,
            }],
            |material| sim.world.density(material),
        )
        .unwrap();
    let new_id = restored.commit_articulated_batch(prepared).unwrap()[0].owner_id;
    let new_owner = restored.articulated_owner_snapshot(new_id).unwrap();
    assert_eq!(new_owner.pose_token, record.state.pose_epoch);
    assert!((new_owner.root.mass_kg - owner.root.mass_kg).abs() < 1.0e-5);
    let second = restored
        .capture_dynamic_corpse_owner(
            new_id,
            DynamicCorpseOwnerFence {
                owner_key: record.owner_key,
                source_generation: new_owner.source_generation,
                topology_revision: new_owner.topology_token,
                pose_token: new_owner.pose_token,
                next_component_serial: new_owner.next_component_serial,
            },
            &sources,
        )
        .unwrap();
    assert_eq!(second.pose_epoch, record.state.pose_epoch);
    assert_eq!(
        second.components[0].translation_m.map(f64::from),
        record.components[0].translation_m
    );
    assert_eq!(
        second.components[0].linear_velocity_m_s,
        record.components[0].linear_velocity_m_s
    );
    assert_eq!(
        CreatureSnapshotV2::decode(&postcard::to_stdvec(&captured).unwrap(), &fixture.view())
            .unwrap(),
        captured
    );
}

#[test]
fn pending_and_source_mismatch_refuse_without_capture_mutation() {
    let (mut sim, fixture, state, id) = born();
    sim.world
        .physics_mut()
        .update_articulated_pose(
            id,
            spall_physics::ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![(1, PartTransform::IDENTITY)],
            },
        )
        .unwrap();
    let counters = sim.world.registry().counters();
    assert!(matches!(
        sim.capture_creature_snapshot(&fixture.view()),
        Err(CreatureCaptureError::Pending)
    ));
    assert_eq!(sim.world.registry().counters(), counters);
    sim.world.step_physics();
    let mut changed = sim
        .world
        .creature_source_part(state.key, 1)
        .unwrap()
        .clone();
    changed
        .apply_edit(&EditPlan::filled_box(
            changed.id(),
            GlobalCell::new(-1, 0, 0),
            GlobalCell::new(-1, 0, 0),
            MaterialId::AIR,
        ))
        .unwrap();
    changed
        .apply_edit(&EditPlan::filled_box(
            changed.id(),
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    replace_sources(&mut sim, &state, changed);
    let before = sim.world.creature_source_hash_for_test();
    assert!(matches!(
        sim.capture_creature_snapshot(&fixture.view()),
        Err(CreatureCaptureError::Physics(
            spall_physics::ArticulatedError::InvalidRestore
        ))
    ));
    assert_eq!(sim.world.creature_source_hash_for_test(), before);

    assert_eq!(sim.world.registry().counters(), counters);
}

#[test]
fn scalar_population_admission_precedes_stale_source_hashing() {
    let (mut sim, fixture, state, _) = born();
    let mut source = sim
        .world
        .creature_source_part(state.key, 1)
        .unwrap()
        .clone();
    for x in 0..MAX_CREATURE_OWNER_BRICKS {
        source
            .insert_brick(
                BrickCoord::new(x as i64, 1, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
    }
    // Deliberately keep stale metadata: scalar limit must win before hashing.
    sim.world
        .replace_creature_source(state.key, 1, source)
        .unwrap();
    let counters = sim.world.registry().counters();
    assert!(matches!(
        sim.capture_creature_snapshot(&fixture.view()),
        Err(CreatureCaptureError::Limit)
    ));
    assert_eq!(sim.world.registry().counters(), counters);
}

#[test]
fn source_native_capacity_is_admitted_before_copying_a_valid_size_sparse_population() {
    let (mut sim, fixture, state, _) = born();
    let mut source = sim
        .world
        .creature_source_part(state.key, 1)
        .unwrap()
        .clone();
    let mut cells = vec![MaterialId::AIR; spall_core::CELLS_PER_BRICK];
    cells[0] = MaterialId(1);
    let brick = Brick::restored_unpacked(&cells, Revision(1), true);
    // Exactly the per-owner 8 Mi dense-slot ceiling, but native output metadata
    // and masks make copying those arrays exceed the independent 16 MiB cap.
    for x in 0..255 {
        source
            .insert_brick(BrickCoord::new(x, 0, 0), brick.clone())
            .unwrap();
    }
    replace_sources(&mut sim, &state, source);
    let counters = sim.world.registry().counters();
    assert!(matches!(
        sim.capture_creature_snapshot(&fixture.view()),
        Err(CreatureCaptureError::Limit)
    ));
    assert_eq!(sim.world.registry().counters(), counters);
    assert_eq!(
        sim.world
            .creature_source_part(state.key, 1)
            .unwrap()
            .resident_brick_count(),
        256
    );
}

#[test]
fn complete_corpse_capture_retains_nonlowest_primary_empty_source_and_serial_gaps() {
    let mut sim =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    sim.initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let fixture = FixtureCatalog::with_materials(sim.world.materials().clone());
    let base = fixture.view();
    let assets = [CreatureAssetCatalogEntry {
        part_ids: &[1, 2],
        ..base.assets[0]
    }];
    let catalog = CreatureValidationCatalog {
        assets: &assets,
        species: base.species,
        materials: base.materials,
        material_mapping_version: base.material_mapping_version,
        material_mapping_hash: base.material_mapping_hash,
        material_mapping: base.material_mapping,
    };
    let first = sim.world.registry_mut().allocate_volume().unwrap();
    let empty = sim.world.registry_mut().allocate_volume().unwrap();
    let mut volume = Volume::new(first, CellSizeCode::Quarter);
    for x in [-1, 2] {
        volume
            .apply_edit(&EditPlan::filled_box(
                first,
                GlobalCell::new(x, 0, 0),
                GlobalCell::new(x, 0, 0),
                MaterialId(1),
            ))
            .unwrap();
    }
    let mut empty_volume = Volume::new(empty, CellSizeCode::Quarter);
    for material in [MaterialId(1), MaterialId::AIR] {
        empty_volume
            .apply_edit(&EditPlan::filled_box(
                empty,
                GlobalCell::new(320, 0, 0),
                GlobalCell::new(320, 0, 0),
                material,
            ))
            .unwrap();
    }
    let empty_next = empty_volume.next_revision();
    empty_volume.evict_brick(BrickCoord::new(10, 0, 0));
    let child = PartTransform {
        translation_m: [1.0, 0.0, 0.0],
        ..PartTransform::IDENTITY
    };
    let sources = vec![
        SourcePart {
            part_id: 1,
            parent_id: None,
            volume_id: first,
            source_revision: 1,
            grid: OccupancyGrid::from_volume(&volume).unwrap().unwrap(),
            cell_m: 0.25,
            local_transform: PartTransform::IDENTITY,
        },
        SourcePart {
            part_id: 2,
            parent_id: Some(1),
            volume_id: empty,
            source_revision: 1,
            cell_m: 0.25,
            local_transform: child,
            grid: OccupancyGrid::from_solid_mask(
                GlobalCell::new(0, 0, 0),
                [1; 3],
                vec![false],
                vec![MaterialId::AIR],
            )
            .unwrap(),
        },
    ];
    let owner_key = 0x218c_u128;
    let component =
        |serial, x, translation, velocity| spall_physics::DynamicCorpseComponentRestore {
            component_id: Some(spall_physics::DerivedFractureComponentId { owner_key, serial }),
            translation_m: translation,
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: velocity,
            angular_velocity_rad_s: [0.0, 0.0, 0.1],
            cells: vec![spall_physics::SourceCellRef {
                volume_id: first,
                part_id: 1,
                cell: [x, 0, 0],
                material: MaterialId(1),
                source_revision: 1,
            }],
        };
    let density = sim.world.density(MaterialId(1));
    let prepared = sim
        .world
        .physics()
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::ColdRestore {
                operation_tag: 1,
                representation: Representation::MergedCuboids,
                restore: spall_physics::DynamicCorpseRestore {
                    owner_key,
                    source_generation: 9,
                    topology_revision: 11,
                    pose_epoch: 3,
                    next_component_serial: 100,
                    source_parts: sources,
                    components: vec![
                        component(41, -1, [4.0, 8.0, 6.0], [1.0, 0.0, 0.0]),
                        component(7, 2, [10.0, 8.0, 6.0], [-1.0, 0.0, 0.0]),
                    ],
                },
            }],
            |_| density,
        )
        .unwrap();
    let owner = sim
        .world
        .physics_mut()
        .commit_articulated_batch(prepared)
        .unwrap()[0]
        .owner_id;
    let mut state = fixture.state(1, first.get());
    state.lifecycle = CreatureLifecycleState::Corpse;
    state.clip_mode = CreatureClipMode::Frozen;
    state.aggregate_topology_revision = 11;
    state.pose_epoch = 5;
    state.source_bindings.push(CreaturePartBinding {
        part_id: 2,
        source_volume_id: empty,
        source_revision: 1,
    });
    sim.creatures
        .as_mut()
        .unwrap()
        .register_validated(state.clone(), &catalog)
        .unwrap();
    let metadata = creature_source_freshness(&volume, None, PartTransform::IDENTITY).unwrap();
    let empty_metadata = creature_source_freshness(&empty_volume, Some(1), child).unwrap();
    sim.world.publish_creature_sources_with_metadata(
        state.key,
        vec![(1, volume, metadata), (2, empty_volume, empty_metadata)],
    );
    sim.world.set_creature_physics_owner(state.key, owner);
    sim.world
        .set_creature_frozen_poses(state.key, vec![(1, PartTransform::IDENTITY), (2, child)]);
    let captured = sim.capture_creature_snapshot(&catalog).unwrap().unwrap();
    let record = &captured.owners.records[0];
    assert_eq!(record.source_generation, 9);
    assert_eq!(record.next_component_serial, 100);
    assert_eq!(record.primary_component_serial, Some(41));
    assert_eq!(
        record
            .components
            .iter()
            .map(|c| c.serial)
            .collect::<Vec<_>>(),
        vec![Some(7), Some(41)]
    );
    assert_eq!(record.state.root_translation_m, [4.0, 8.0, 6.0]);
    assert_eq!(record.state.pose_epoch, 5);
    assert!(record.parts[1].bricks.is_empty());
    assert_eq!(record.parts[1].next_brick_revision, empty_next.get());
    let mut restored = crate::reconstruct_creature_sources(&captured, &catalog).unwrap();
    let empty_source = &mut restored[0].parts[1].1;
    assert_eq!(empty_source.next_revision(), empty_next);
    empty_source
        .apply_edit(&EditPlan::filled_box(
            empty,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    assert_eq!(
        empty_source
            .brick_revision(BrickCoord::new(0, 0, 0))
            .unwrap()
            .unwrap(),
        empty_next
    );
    assert_eq!(record.parts[1].parent_id, Some(1));
    assert_eq!(
        CreatureSnapshotV2::decode(&postcard::to_stdvec(&captured).unwrap(), &catalog).unwrap(),
        captured
    );
    assert_eq!(
        sim.world
            .physics()
            .articulated_owner_snapshot(owner)
            .unwrap()
            .pose_token,
        3
    );
}

#[test]
fn source_revision_gaps_roundtrip_and_the_next_edit_never_reuses_a_revision() {
    let (mut sim, fixture, state, _) = born();
    let mut original = sim
        .world
        .creature_source_part(state.key, 1)
        .unwrap()
        .clone();
    let retired = BrickCoord::new(10, 0, 0);
    // Real edits produce an edited-air tombstone at the highest revision; its
    // retirement must not rewind the allocator below that consumed revision.
    for material in [MaterialId(1), MaterialId::AIR] {
        original
            .apply_edit(&EditPlan::filled_box(
                original.id(),
                GlobalCell::new(320, 0, 0),
                GlobalCell::new(320, 0, 0),
                material,
            ))
            .unwrap();
    }
    assert!(
        original
            .snapshot_brick(retired)
            .unwrap()
            .unwrap()
            .is_modified_air()
    );
    let next = original.next_revision();
    original.evict_brick(retired);
    replace_sources(&mut sim, &state, original.clone());
    let counters = sim.world.registry().counters();
    let captured = sim
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    assert_eq!(
        captured.owners.records[0].parts[0].next_brick_revision,
        next.get()
    );
    let decoded =
        CreatureSnapshotV2::decode(&postcard::to_stdvec(&captured).unwrap(), &fixture.view())
            .unwrap();
    let mut restored = crate::reconstruct_creature_sources(&decoded, &fixture.view()).unwrap();
    assert_eq!(restored[0].key, state.key);
    let volume = &mut restored[0].parts[0].1;
    assert_eq!(volume.next_revision(), next);
    let metadata = creature_source_freshness(volume, None, PartTransform::IDENTITY).unwrap();
    assert_eq!(
        capture_source(
            volume,
            &state.source_bindings[0],
            &metadata,
            PartTransform::IDENTITY,
            1
        ),
        decoded.owners.records[0].parts[0]
    );
    let plan = EditPlan::filled_box(
        original.id(),
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(0, 0, 0),
        MaterialId(1),
    );
    original.apply_edit(&plan).unwrap();
    volume.apply_edit(&plan).unwrap();
    let coord = BrickCoord::new(0, 0, 0);
    assert_eq!(volume.brick_revision(coord).unwrap().unwrap(), next);
    assert_eq!(
        volume
            .snapshot_brick(coord)
            .unwrap()
            .unwrap()
            .content_hash(),
        original
            .snapshot_brick(coord)
            .unwrap()
            .unwrap()
            .content_hash()
    );
    assert_eq!(volume.next_revision(), original.next_revision());
    assert_eq!(sim.world.registry().counters(), counters);
    assert_eq!(
        sim.world
            .creature_source_part(state.key, 1)
            .unwrap()
            .next_revision(),
        next
    );
}

#[test]
fn exhausted_source_counter_reconstructs_without_reuse_and_invalid_candidates_do_not_publish() {
    let (mut sim, fixture, state, _) = born();
    let mut source = sim
        .world
        .creature_source_part(state.key, 1)
        .unwrap()
        .clone();
    source.resume_revision_counter(Revision(u64::MAX)).unwrap();
    replace_sources(&mut sim, &state, source);
    let snapshot = sim
        .capture_creature_snapshot(&fixture.view())
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot.owners.records[0].parts[0].next_brick_revision,
        u64::MAX
    );
    let decoded =
        CreatureSnapshotV2::decode(&postcard::to_stdvec(&snapshot).unwrap(), &fixture.view())
            .unwrap();
    let mut restored = crate::reconstruct_creature_sources(&decoded, &fixture.view()).unwrap();
    let volume = &mut restored[0].parts[0].1;
    let stamp = volume.state_stamp();
    assert_eq!(
        volume.apply_edit(&EditPlan::filled_box(
            volume.id(),
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1)
        )),
        Err(spall_voxel::EditError::RevisionExhausted)
    );
    assert_eq!(volume.next_revision(), Revision(u64::MAX));
    assert_eq!(volume.state_stamp(), stamp);
    assert_eq!(
        sim.world
            .creature_source_part(state.key, 1)
            .unwrap()
            .next_revision(),
        Revision(u64::MAX)
    );
    let mut invalid = snapshot;
    invalid.owners.records[0].parts[0].next_brick_revision = 1;
    assert!(crate::reconstruct_creature_sources(&invalid, &fixture.view()).is_err());
    assert_eq!(
        sim.world
            .creature_source_part(state.key, 1)
            .unwrap()
            .next_revision(),
        Revision(u64::MAX)
    );
}

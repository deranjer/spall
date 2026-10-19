//! Bridges the accepted ENG-216 test-only owner seam into portable records.
use super::*;
use crate::creatures::tests::FixtureCatalog;
use spall_core::{CellSizeCode, GlobalCell, TransactionId, VolumeId};
use spall_physics::{PartTransform, Representation};
use spall_protocol::creature_owner::CreatureDeathWitnessV1;
use spall_protocol::{
    BaselineCells, BaselineOwner, CREATURE_CHANGE_SCHEMA, CREATURE_OWNER_SCHEMA,
    CreatureLifecycleChangeV4, CreatureLifecycleContextV1, CreatureLifecycleOperationV4,
    CreatureLocalPoseV1, CreatureMaskCellV1, CreatureOwnedComponentV1, CreatureOwnershipRecordV4,
    CreaturePartBinding, CreatureReductionOutcome, CreatureSourceMaskBrickV1, CreatureSourcePartV2,
    Hash32, baseline::BaselineBrick, reduce_creature_lifecycle_candidate,
};
use spall_voxel::{EditPlan, Volume};

const OWNER_KEY: u128 = 0x217;

fn pose(value: PartTransform) -> CreatureLocalPoseV1 {
    CreatureLocalPoseV1 {
        translation_m: value.translation_m,
        rotation_xyzw: value.rotation_xyzw,
    }
}

fn record_from_sim(
    sim: &Simulation,
    state: spall_protocol::CreatureStateV1,
) -> CreatureOwnershipRecordV4 {
    let local_poses = if state.lifecycle == spall_protocol::CreatureLifecycleState::Corpse {
        sim.world.creature_frozen_poses(state.key).unwrap().to_vec()
    } else {
        let owner = sim.world.creature_physics_owner(state.key).unwrap();
        sim.world
            .physics()
            .articulated_pose_snapshot(owner)
            .unwrap()
            .parts
    };
    let mut parts = Vec::new();
    let mut masks = Vec::new();
    for entry in &state.source_bindings {
        let volume = sim
            .world
            .creature_source_part(state.key, entry.part_id)
            .unwrap();
        let baseline = crate::replication::baseline_volume_of(volume, BaselineOwner::Terrain, None);
        let mut source_masks = Vec::new();
        let bricks = baseline
            .bricks
            .into_iter()
            .map(|brick| {
                if let BaselineCells::Uniform(material) = brick.cells {
                    if material != 0 {
                        let cells = (0..32 * 32 * 32)
                            .map(|index| CreatureMaskCellV1 {
                                index: index as u16,
                                material_id: material,
                            })
                            .collect();
                        source_masks.push(CreatureSourceMaskBrickV1 {
                            volume_id: entry.source_volume_id,
                            part_id: entry.part_id,
                            coord: brick.coord,
                            source_revision: entry.source_revision,
                            brick_revision: brick.revision,
                            cells,
                        });
                    }
                    BaselineBrick {
                        cells: BaselineCells::Uniform(material),
                        ..brick
                    }
                } else {
                    let cells = match &brick.cells {
                        BaselineCells::Dense(ids) => ids
                            .iter()
                            .enumerate()
                            .filter_map(|(index, material_id)| {
                                (*material_id != 0).then_some(CreatureMaskCellV1 {
                                    index: index as u16,
                                    material_id: *material_id,
                                })
                            })
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    };
                    if !cells.is_empty() {
                        source_masks.push(CreatureSourceMaskBrickV1 {
                            volume_id: entry.source_volume_id,
                            part_id: entry.part_id,
                            coord: brick.coord,
                            source_revision: entry.source_revision,
                            brick_revision: brick.revision,
                            cells,
                        });
                    }
                    brick
                }
            })
            .collect::<Vec<_>>();
        let metadata = sim
            .world
            .creature_source_metadata(state.key, entry.part_id)
            .unwrap();
        let local_pose = local_poses
            .iter()
            .find(|(part_id, _)| *part_id == entry.part_id)
            .map(|(_, transform)| *transform)
            .unwrap();
        let mut part = CreatureSourcePartV2 {
            next_brick_revision: volume.next_revision().get(),
            part_id: entry.part_id,
            source_volume_id: entry.source_volume_id,
            source_generation: 1,
            source_revision: entry.source_revision,
            cell_size_code: volume.cell_size().to_u8(),
            bounds: volume
                .bounds()
                .map(|b| [[b.min.x, b.min.y, b.min.z], [b.max.x, b.max.y, b.max.z]]),
            source_content_hash: Hash32::ZERO,
            parent_id: metadata.parent_id,
            initial_local_pose: pose(metadata.initial_local_transform),
            frozen_local_pose: pose(local_pose),
            bricks,
        };
        part.source_content_hash = part.canonical_source_content_hash();
        parts.push(part);
        masks.extend(source_masks);
    }
    parts.sort_by_key(|part| part.part_id);
    masks.sort_by_key(|mask| {
        (
            mask.volume_id.get(),
            mask.part_id,
            mask.coord[2],
            mask.coord[1],
            mask.coord[0],
        )
    });
    let components = vec![CreatureOwnedComponentV1 {
        owner_key: OWNER_KEY,
        serial: None,
        translation_m: state.root_translation_m,
        rotation_xyzw: state.root_rotation_xyzw,
        linear_velocity_m_s: state.linear_velocity_m_s,
        angular_velocity_rad_s: state.angular_velocity_rad_s,
        source_masks: masks,
    }];
    let part_velocities = if state.lifecycle == spall_protocol::CreatureLifecycleState::Alive {
        sim.world
            .physics()
            .articulated_part_motion_snapshot(sim.world.creature_physics_owner(state.key).unwrap())
            .unwrap()
            .into_iter()
            .map(|motion| spall_protocol::CreaturePartVelocityV2 {
                part_id: motion.part_id,
                linear_velocity_m_s: motion.linear_velocity_m_s,
                angular_velocity_rad_s: motion.angular_velocity_rad_s,
            })
            .collect()
    } else {
        Vec::new()
    };
    CreatureOwnershipRecordV4 {
        primary_component_serial: None,
        part_velocities,
        schema: CREATURE_OWNER_SCHEMA,
        state,
        owner_key: OWNER_KEY,
        source_generation: 1,
        next_component_serial: 1,
        parts,
        components,
    }
}

#[test]
fn eng216_birth_death_values_roundtrip_through_portable_reducer() {
    let mut sim =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    sim.initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    use spall_protocol::{
        CreatureAssetCatalogEntry, CreatureSpeciesCatalogEntry, CreatureValidationCatalog,
    };
    let base_catalog = FixtureCatalog::with_materials(sim.world.materials().clone());
    let base_view = base_catalog.view();
    let next_volume_id = sim.world.registry().counters().1;
    let volume_ids = [
        VolumeId::new(next_volume_id).unwrap(),
        VolumeId::new(next_volume_id + 1).unwrap(),
        VolumeId::new(next_volume_id + 2).unwrap(),
    ];
    let assets = [CreatureAssetCatalogEntry {
        uuid: base_view.assets[0].uuid,
        logical_hash: base_view.assets[0].logical_hash,
        part_ids: &[1, 2, 3],
        clip_names: base_view.assets[0].clip_names,
        portable_material_keys: base_view.assets[0].portable_material_keys,
    }];
    let species = [CreatureSpeciesCatalogEntry {
        key: base_view.species[0].key,
        version: base_view.species[0].version,
        enabled: base_view.species[0].enabled,
        asset: base_view.species[0].asset,
    }];
    let catalog = CreatureValidationCatalog {
        assets: &assets,
        species: &species,
        materials: base_view.materials,
        material_mapping_version: base_view.material_mapping_version,
        material_mapping_hash: base_view.material_mapping_hash,
        material_mapping: base_view.material_mapping,
    };
    let before_allocator = sim.creature_allocator_snapshot().unwrap();
    let mut state = base_catalog.state(1, volume_ids[0].get());
    state.source_bindings = volume_ids
        .iter()
        .enumerate()
        .map(|(index, volume_id)| CreaturePartBinding {
            part_id: index as u32 + 1,
            source_volume_id: *volume_id,
            source_revision: 1,
        })
        .collect();
    state.root_translation_m = [10.0, 5.0, 10.0];
    state.root_rotation_xyzw = [
        0.0,
        0.0,
        std::f32::consts::FRAC_1_SQRT_2,
        std::f32::consts::FRAC_1_SQRT_2,
    ];
    let q90 = state.root_rotation_xyzw;
    let authored = [
        (None, PartTransform::IDENTITY),
        (
            Some(1),
            PartTransform {
                translation_m: [1.0, 0.0, 0.0],
                rotation_xyzw: q90,
            },
        ),
        (
            Some(2),
            PartTransform {
                translation_m: [1.0, 0.0, 0.0],
                rotation_xyzw: q90,
            },
        ),
    ];
    let source_parts = volume_ids
        .iter()
        .map(|volume_id| {
            let mut source = Volume::new(*volume_id, CellSizeCode::Quarter);
            source
                .apply_edit(&EditPlan::filled_box(
                    *volume_id,
                    GlobalCell::new(0, 0, 0),
                    GlobalCell::new(0, 0, 0),
                    MaterialId(1),
                ))
                .unwrap();
            source
        })
        .collect::<Vec<_>>();
    sim.commit_creature_birth_for_test(
        state.clone(),
        &catalog,
        (0..3)
            .map(|index| CreatureBirthPartInput {
                part_id: index as u32 + 1,
                parent_id: authored[index].0,
                local_transform: authored[index].1,
                volume: source_parts[index].clone(),
            })
            .collect(),
        Representation::MergedCuboids,
    )
    .unwrap();
    let alive = sim
        .creatures
        .as_ref()
        .unwrap()
        .record(state.key.creature_id)
        .unwrap()
        .clone();
    let before = record_from_sim(&sim, alive);
    before.validate(&catalog).unwrap();
    let transaction_id = TransactionId::new(1).unwrap();
    let birth = CreatureLifecycleChangeV4 {
        schema: CREATURE_CHANGE_SCHEMA,
        transaction_id,
        operation_ordinal: 0,
        expected_before_hash: Hash32::ZERO,
        expected_after_hash: before.canonical_hash(),
        operation: CreatureLifecycleOperationV4::Birth {
            after: before.clone(),
        },
    };
    let birth_candidate = reduce_creature_lifecycle_candidate(
        None,
        &birth,
        CreatureLifecycleContextV1 {
            transaction_id,
            operation_ordinal: 0,
            replay_receipt: None,
            allocator: before_allocator,
            next_volume_id,
            death_witness: None,
            existing_keys: &[],
            existing_owner_keys: &[],
            existing_source_volumes: &[],
        },
        &catalog,
    )
    .unwrap();
    assert_eq!(birth_candidate.outcome, CreatureReductionOutcome::Applied);
    assert_eq!(
        birth_candidate.record.canonical_hash(),
        before.canonical_hash()
    );
    assert_eq!(
        birth_candidate.next_creature_id,
        before_allocator.next_creature_id + 1
    );
    assert_eq!(birth_candidate.next_volume_id, next_volume_id + 3);

    let owner = sim.world.creature_physics_owner(state.key).unwrap();
    sim.world
        .physics_mut()
        .update_articulated_pose(
            owner,
            spall_physics::ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![
                    (1, PartTransform::IDENTITY),
                    (
                        2,
                        PartTransform {
                            translation_m: [1.25, 0.0, 0.0],
                            rotation_xyzw: q90,
                        },
                    ),
                    (
                        3,
                        PartTransform {
                            translation_m: [0.75, 0.5, 0.0],
                            rotation_xyzw: q90,
                        },
                    ),
                ],
            },
        )
        .unwrap();
    sim.world.step_physics();
    let applied_alive = sim
        .creatures
        .as_ref()
        .unwrap()
        .record(state.key.creature_id)
        .unwrap()
        .clone();
    let before = record_from_sim(&sim, applied_alive);
    before.validate(&catalog).unwrap();
    assert_eq!(before.part_velocities.len(), 3);
    let captured = sim.capture_creature_snapshot(&catalog).unwrap().unwrap();
    crate::sim::creature_restore::assert_joint_roundtrip(&captured, &catalog);
    assert_eq!(
        captured.owners.records[0].part_velocities,
        before.part_velocities
    );
    assert_eq!(captured.owners.records[0].parts, before.parts);
    assert_ne!(
        before.part_velocities[0].linear_velocity_m_s,
        before.part_velocities[1].linear_velocity_m_s
    );
    let bytes = before.encode(&catalog).unwrap();
    assert_eq!(
        CreatureOwnershipRecordV4::decode(&bytes, &catalog).unwrap(),
        before
    );
    let transfer = sim
        .commit_creature_death_for_test(
            state.key,
            state.state_revision,
            state.aggregate_topology_revision,
            1,
            &catalog,
        )
        .unwrap();
    assert!(transfer.mass_kg > 0.0);
    let corpse = sim
        .creatures
        .as_ref()
        .unwrap()
        .record(state.key.creature_id)
        .unwrap()
        .clone();
    let after = record_from_sim(&sim, corpse);
    after.validate(&catalog).unwrap();
    let captured = sim.capture_creature_snapshot(&catalog).unwrap().unwrap();
    crate::sim::creature_restore::assert_joint_roundtrip(&captured, &catalog);
    assert_eq!(captured.owners.records[0].state, after.state);
    assert_eq!(captured.owners.records[0].parts, after.parts);
    assert!(captured.owners.records[0].part_velocities.is_empty());
    let frozen_local_poses = sim
        .world
        .creature_frozen_poses(state.key)
        .unwrap()
        .iter()
        .map(|(id, transform)| (*id, pose(*transform)))
        .collect();
    let witness = CreatureDeathWitnessV1 {
        key: state.key,
        source_generation: before.source_generation,
        before_state_revision: before.state.state_revision,
        before_topology_revision: before.state.aggregate_topology_revision,
        before_pose_epoch: before.state.pose_epoch,
        root_translation_m: after.state.root_translation_m,
        root_rotation_xyzw: after.state.root_rotation_xyzw,
        installed_linear_velocity_m_s: transfer.linear_velocity_m_s.map(|value| value as f32),
        installed_angular_velocity_rad_s: transfer.angular_velocity_rad_s.map(|value| value as f32),
        frozen_local_poses,
    };
    assert_eq!(
        after.state.linear_velocity_m_s,
        witness.installed_linear_velocity_m_s
    );
    assert_eq!(
        after.state.angular_velocity_rad_s,
        witness.installed_angular_velocity_rad_s
    );
    let death = CreatureLifecycleChangeV4 {
        schema: CREATURE_CHANGE_SCHEMA,
        transaction_id,
        operation_ordinal: 1,
        expected_before_hash: before.canonical_hash(),
        expected_after_hash: after.canonical_hash(),
        operation: CreatureLifecycleOperationV4::Death {
            after: after.clone(),
        },
    };
    let snapshot = sim
        .world
        .physics()
        .articulated_owner_snapshot(owner)
        .unwrap();
    assert_eq!(
        snapshot.root.translation_m.map(f64::from),
        after.state.root_translation_m
    );
    assert_eq!(snapshot.root.rotation, after.state.root_rotation_xyzw);
    assert_eq!(snapshot.root.linvel_m_s, after.state.linear_velocity_m_s);
    assert_eq!(
        snapshot.root.angvel_rad_s,
        after.state.angular_velocity_rad_s
    );
    let existing_keys = [state.key];
    let existing_owner_keys = [OWNER_KEY];
    let existing_source_volumes = volume_ids;
    let reduced = reduce_creature_lifecycle_candidate(
        Some(&before),
        &death,
        CreatureLifecycleContextV1 {
            transaction_id,
            operation_ordinal: 1,
            replay_receipt: None,
            allocator: sim.creature_allocator_snapshot().unwrap(),
            next_volume_id: sim.world.registry().counters().1,
            death_witness: Some(&witness),
            existing_keys: &existing_keys,
            existing_owner_keys: &existing_owner_keys,
            existing_source_volumes: &existing_source_volumes,
        },
        &catalog,
    )
    .unwrap();
    assert_eq!(reduced.outcome, CreatureReductionOutcome::Applied);
    assert_eq!(reduced.record.canonical_hash(), after.canonical_hash());
    for ((before_part, after_part), reduced_part) in before
        .parts
        .iter()
        .zip(&after.parts)
        .zip(&reduced.record.parts)
    {
        assert_eq!(reduced_part.source_volume_id, before_part.source_volume_id);
        assert_eq!(reduced_part.bricks, before_part.bricks);
        assert_eq!(
            reduced_part.source_content_hash,
            before_part.source_content_hash
        );
        assert_eq!(reduced_part.parent_id, before_part.parent_id);
        assert_eq!(
            reduced_part.initial_local_pose,
            before_part.initial_local_pose
        );
        assert_eq!(reduced_part.frozen_local_pose, after_part.frozen_local_pose);
    }
    assert_eq!(
        before
            .parts
            .iter()
            .map(|part| part.parent_id)
            .collect::<Vec<_>>(),
        [None, Some(1), Some(2)]
    );
    assert_eq!(
        after
            .parts
            .iter()
            .map(|part| part.parent_id)
            .collect::<Vec<_>>(),
        [None, Some(1), Some(2)]
    );
}

#[test]
fn source_material_must_belong_to_selected_asset_keys() {
    use spall_protocol::{
        CreatureAssetCatalogEntry, CreatureAssetIdentity, CreatureMaterialMappingEntry,
        CreatureSpeciesCatalogEntry, CreatureValidationCatalog, creature_material_mapping_hash,
    };

    let mut sim =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    sim.initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    assert!(sim.world.materials().contains(MaterialId(2)));
    let base = FixtureCatalog::with_materials(sim.world.materials().clone());
    let volume_id = VolumeId::new(sim.world.registry().counters().1).unwrap();
    let mut state = base.state(1, volume_id.get());
    let mapping = [
        CreatureMaterialMappingEntry {
            portable_key: "fur",
            material_id: MaterialId(1),
        },
        CreatureMaterialMappingEntry {
            portable_key: "unrelated",
            material_id: MaterialId(2),
        },
    ];
    state.material_mapping_hash = creature_material_mapping_hash(&mapping).unwrap();
    let assets = [CreatureAssetCatalogEntry {
        uuid: [8; 16],
        logical_hash: Hash32([9; 32]),
        part_ids: &[1],
        clip_names: &["idle"],
        portable_material_keys: &["fur"],
    }];
    let species = [CreatureSpeciesCatalogEntry {
        key: "deer",
        version: 1,
        enabled: true,
        asset: CreatureAssetIdentity {
            uuid: [8; 16],
            logical_hash: Hash32([9; 32]),
        },
    }];
    let materials = sim.world.materials().clone();
    let catalog = CreatureValidationCatalog {
        assets: &assets,
        species: &species,
        materials: &materials,
        material_mapping_version: 1,
        material_mapping_hash: state.material_mapping_hash,
        material_mapping: &mapping,
    };
    let mut source = Volume::new(volume_id, CellSizeCode::Quarter);
    source
        .apply_edit(&EditPlan::filled_box(
            volume_id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    sim.commit_creature_birth_for_test(
        state.clone(),
        &catalog,
        vec![CreatureBirthPartInput {
            part_id: 1,
            parent_id: None,
            local_transform: PartTransform::IDENTITY,
            volume: source,
        }],
        Representation::MergedCuboids,
    )
    .unwrap();
    let mut record = record_from_sim(&sim, state);
    let BaselineCells::Dense(ids) = &mut record.parts[0].bricks[0].cells else {
        panic!("fixture brick should be dense");
    };
    let cell = ids.iter_mut().find(|material| **material != 0).unwrap();
    *cell = 2;
    record.parts[0].source_content_hash = record.parts[0].canonical_source_content_hash();
    record.components[0].source_masks[0].cells[0].material_id = 2;
    assert!(
        record.state.validate(&catalog).is_ok(),
        "the mapping itself is valid and contains both keys"
    );
    assert_eq!(
        record.validate(&catalog),
        Err(spall_protocol::CreatureOwnershipError::Material)
    );
}

#[test]
fn record_work_preflight_rejects_duplicate_masks_and_large_owned_sets() {
    use spall_protocol::CreatureOwnershipSetV4;
    use spall_protocol::creature_owner::MAX_CREATURE_OWNER_SET_SOLID_CELLS;
    let mut sim =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    sim.initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let catalog = FixtureCatalog::with_materials(sim.world.materials().clone());
    let volume_id = VolumeId::new(sim.world.registry().counters().1).unwrap();
    let state = catalog.state(1, volume_id.get());
    let mut source = Volume::new(volume_id, CellSizeCode::Quarter);
    source
        .apply_edit(&EditPlan::filled_box(
            volume_id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    sim.commit_creature_birth_for_test(
        state.clone(),
        &catalog.view(),
        vec![CreatureBirthPartInput {
            part_id: 1,
            parent_id: None,
            local_transform: PartTransform::IDENTITY,
            volume: source,
        }],
        Representation::MergedCuboids,
    )
    .unwrap();
    let record = record_from_sim(&sim, state);
    let mut duplicate = record.clone();
    let first = duplicate.components[0].source_masks[0].cells[0];
    duplicate.components[0].source_masks[0].cells.push(first);
    duplicate.components[0].source_masks[0]
        .cells
        .sort_by_key(|cell| cell.index);
    assert_eq!(
        duplicate.validate(&catalog.view()),
        Err(spall_protocol::CreatureOwnershipError::Mask)
    );

    let mut overflow = record.clone();
    overflow.parts[0].bounds = None;
    overflow.parts[0].bricks[0].coord = [i64::MAX, 0, 0];
    overflow.parts[0].source_content_hash = overflow.parts[0].canonical_source_content_hash();
    overflow.components[0].source_masks[0].coord = [i64::MAX, 0, 0];
    assert_eq!(
        overflow.validate(&catalog.view()),
        Err(spall_protocol::CreatureOwnershipError::Mask)
    );

    // The aggregate ceiling is checked before the record's content hash or
    // cell material scan. Each complete dense brick adds 32,768 potential
    // solids; the set-wide ceiling is exceeded after a small bounded fixture.
    let dense_count = MAX_CREATURE_OWNER_SET_SOLID_CELLS / (32 * 32 * 32) + 1;
    let mut oversized = record.clone();
    oversized.parts[0].bounds = None;
    oversized.parts[0].bricks = (0..dense_count)
        .map(|x| BaselineBrick {
            coord: [x as i64, 0, 0],
            revision: 1,
            edited: true,
            cells: BaselineCells::Dense(vec![1; 32 * 32 * 32]),
        })
        .collect();
    let set = CreatureOwnershipSetV4 {
        schema: CREATURE_OWNER_SCHEMA,
        records: vec![oversized],
    };
    assert_eq!(
        set.validate(&catalog.view()),
        Err(spall_protocol::CreatureOwnershipError::Limit)
    );
}

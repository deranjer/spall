use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use super::*;
use crate::creatures::{
    CreatureIntentWriter, CreatureRuleError, CreatureScheduleContext, CreatureScheduleSnapshot,
    CreatureSchedulingRules,
};
use spall_core::{CellSizeCode, GlobalCell, MaterialId};
use spall_physics::{ArticulatedPose, PartTransform, Representation};
use spall_voxel::{EditPlan, Volume};

struct CountingRules {
    calls: AtomicUsize,
    contexts: Mutex<Vec<CreatureScheduleContext>>,
}

fn source_volume(id: spall_core::VolumeId) -> Volume {
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    volume
        .apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    volume
}

#[test]
fn lifecycle_catalog_fence_hashes_mapping_entries_not_only_claimed_hash() {
    let fixture = crate::creatures::tests::FixtureCatalog::new();
    let original = fixture.view();
    let changed_mapping = [spall_protocol::CreatureMaterialMappingEntry {
        portable_key: "fur",
        material_id: MaterialId(2),
    }];
    let changed = spall_protocol::CreatureValidationCatalog {
        assets: original.assets,
        species: original.species,
        materials: original.materials,
        material_mapping_version: original.material_mapping_version,
        material_mapping_hash: original.material_mapping_hash,
        material_mapping: &changed_mapping,
    };
    assert_ne!(
        creature_catalog_fingerprint(&original),
        creature_catalog_fingerprint(&changed),
        "a stale claimed mapping hash must not hide changed catalog entries"
    );
}

#[test]
fn dormant_mixed_target_rejects_without_waking_or_publishing() {
    let mut simulation =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let entity = simulation
        .world
        .spawn_body(
            crate::fixtures::solid_block(2),
            crate::body::BodyPose::new(glam::DQuat::IDENTITY, [0.0, 3.0, 0.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    let body = simulation.world.body(entity).unwrap();
    let volume_id = body.volume_id;
    let intent = crate::intent::EditIntent::cut(
        RequestId(9),
        entity,
        EditTarget::Body(entity),
        SphereBrush::new(
            BrushPoint::from_units(BRUSH_UNIT / 2, BRUSH_UNIT / 2, BRUSH_UNIT / 2),
            BRUSH_UNIT,
        )
        .unwrap(),
    );
    let staged = crate::stage::stage_edit(&crate::stage::StageInput::new(
        &intent,
        volume_id,
        body.volume.clone(),
        spall_voxel::EvictedBricks::new(),
        simulation.world.anchor(),
        simulation.world.generation(),
        simulation.world.topology_epoch(),
    ))
    .unwrap();
    assert!(simulation.world.deactivate_body(entity));
    let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
        simulation.world.materials().clone(),
    );
    let source_id = simulation
        .world
        .registry()
        .clone()
        .allocate_volume()
        .unwrap();
    let state = catalog.state(1, source_id.get());
    let before = (
        simulation.world.world_hash(),
        simulation.world.registry().counters(),
        simulation.world.physics().body_count(),
        simulation.journal.cursor(),
        simulation.creatures.as_ref().unwrap().revision(),
    );
    let result = simulation.prepare_creature_lifecycle_transaction(
        Tick(1),
        spall_protocol::ControlSeq(1),
        vec![AuthoritativeCreatureIntent::Birth {
            state: Box::new(state),
            parts: vec![CreatureBirthPartInput {
                part_id: 1,
                parent_id: None,
                local_transform: PartTransform::IDENTITY,
                volume: source_volume(source_id),
            }],
            representation: Representation::MergedCuboids,
        }],
        Some(&staged),
        &catalog.view(),
    );
    assert!(matches!(result, Err(reason) if reason.contains("staged dormant-body reactivation")));
    assert!(simulation.world.body_is_dormant(entity));
    assert_eq!(
        (
            simulation.world.world_hash(),
            simulation.world.registry().counters(),
            simulation.world.physics().body_count(),
            simulation.journal.cursor(),
            simulation.creatures.as_ref().unwrap().revision(),
        ),
        before
    );
}

#[test]
fn ordered_single_part_birth_publishes_one_transaction_and_oversized_birth_rejects_atomically() {
    let mut simulation =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
        simulation.world.materials().clone(),
    );
    let mut candidate_registry = simulation.world.registry().clone();
    let volume_id = candidate_registry.allocate_volume().unwrap();
    let state = catalog.state(1, volume_id.get());
    let input = || CreatureBirthPartInput {
        part_id: 1,
        parent_id: None,
        local_transform: PartTransform::IDENTITY,
        volume: source_volume(volume_id),
    };
    let counters = simulation.world.registry().counters();
    let cursor = simulation.journal.cursor();
    let bodies = simulation.world.physics().body_count();
    let committed = simulation
        .commit_creature_lifecycle_transaction(
            Tick(1),
            spall_protocol::ControlSeq(1),
            vec![AuthoritativeCreatureIntent::Birth {
                state: Box::new(state.clone()),
                parts: vec![input()],
                representation: Representation::MergedCuboids,
            }],
            None,
            &catalog.view(),
        )
        .expect("one-part lifecycle record fits the existing inline frame");
    let inline_bytes = spall_protocol::encode_control(&committed.topology).unwrap();
    eprintln!(
        "one-part lifecycle control record: {} bytes",
        inline_bytes.len()
    );
    assert!(inline_bytes.len() <= spall_protocol::limits::MAX_CONTROL_RECORD);
    assert_eq!(committed.topology.ops.len(), 1);
    assert!(matches!(
        committed.topology.ops[0],
        spall_protocol::TopologyOp::CreatureLifecycle { .. }
    ));
    assert_eq!(simulation.journal.cursor(), cursor + 1);
    assert_eq!(simulation.journal.entries().len(), 1);
    assert_eq!(simulation.world.registry().counters().1, counters.1 + 1);
    assert_eq!(simulation.world.physics().body_count(), bodies + 1);
    assert!(
        simulation
            .world
            .creature_source_part(state.key, 1)
            .is_some()
    );

    let mut multi_simulation =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    multi_simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let materials = multi_simulation.world.materials().clone();
    let multi_catalog = HierarchyCatalog {
        materials: materials.clone(),
        asset: [spall_protocol::CreatureAssetCatalogEntry {
            uuid: [8; 16],
            logical_hash: spall_protocol::Hash32([9; 32]),
            part_ids: &[1, 2, 3],
            clip_names: &["idle"],
            portable_material_keys: &["fur", "hide"],
        }],
        species: [spall_protocol::CreatureSpeciesCatalogEntry {
            key: "deer",
            version: 1,
            enabled: true,
            asset: spall_protocol::CreatureAssetIdentity {
                uuid: [8; 16],
                logical_hash: spall_protocol::Hash32([9; 32]),
            },
        }],
        mapping: [
            spall_protocol::CreatureMaterialMappingEntry {
                portable_key: "fur",
                material_id: MaterialId(1),
            },
            spall_protocol::CreatureMaterialMappingEntry {
                portable_key: "hide",
                material_id: MaterialId(2),
            },
        ],
    };
    let mut multi_registry = multi_simulation.world.registry().clone();
    let volumes = [
        multi_registry.allocate_volume().unwrap(),
        multi_registry.allocate_volume().unwrap(),
        multi_registry.allocate_volume().unwrap(),
    ];
    let multi_state = multi_catalog.state(volumes);
    let inputs = volumes
        .into_iter()
        .enumerate()
        .map(|(index, volume)| CreatureBirthPartInput {
            part_id: index as u32 + 1,
            parent_id: match index {
                0 => None,
                1 => Some(1),
                _ => Some(2),
            },
            local_transform: PartTransform::IDENTITY,
            volume: crate::fixtures::noisy_source_volume(
                volume,
                if index == 1 {
                    MaterialId(2)
                } else {
                    MaterialId(1)
                },
            ),
        })
        .collect();
    let before = (
        multi_simulation.world.registry().counters(),
        multi_simulation.journal.cursor(),
        multi_simulation.world.physics().body_count(),
        multi_simulation.creatures.as_ref().unwrap().revision(),
    );
    let rejected = multi_simulation.prepare_creature_lifecycle_transaction(
        Tick(1),
        spall_protocol::ControlSeq(1),
        vec![AuthoritativeCreatureIntent::Birth {
            state: Box::new(multi_state.clone()),
            parts: inputs,
            representation: Representation::MergedCuboids,
        }],
        None,
        &multi_catalog.view(),
    );
    assert!(rejected.is_err(), "three-part source exceeds inline cap");
    assert_eq!(
        (
            multi_simulation.world.registry().counters(),
            multi_simulation.journal.cursor(),
            multi_simulation.world.physics().body_count(),
            multi_simulation.creatures.as_ref().unwrap().revision(),
        ),
        before,
        "oversized record rejection must not publish any candidate state"
    );
    assert!(
        multi_simulation
            .creatures
            .as_ref()
            .unwrap()
            .record(multi_state.key.creature_id)
            .is_none()
    );
}

#[test]
fn ordered_birth_applied_pose_and_checked_death_use_one_journal_entry_each() {
    let mut simulation =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
        simulation.world.materials().clone(),
    );
    let mut candidate_registry = simulation.world.registry().clone();
    let volume_id = candidate_registry.allocate_volume().unwrap();
    let state = catalog.state(1, volume_id.get());
    let birth = simulation
        .commit_creature_lifecycle_transaction(
            Tick(1),
            spall_protocol::ControlSeq(1),
            vec![AuthoritativeCreatureIntent::Birth {
                state: Box::new(state.clone()),
                parts: vec![CreatureBirthPartInput {
                    part_id: 1,
                    parent_id: None,
                    local_transform: PartTransform::IDENTITY,
                    volume: source_volume(volume_id),
                }],
                representation: Representation::MergedCuboids,
            }],
            None,
            &catalog.view(),
        )
        .unwrap();
    assert_eq!(birth.journal_seq.0, 1);
    let owner = simulation.world.creature_physics_owner(state.key).unwrap();
    let moved = PartTransform {
        translation_m: [0.5, 0.0, 0.0],
        ..PartTransform::IDENTITY
    };
    simulation
        .world
        .physics_mut()
        .update_articulated_pose(
            owner,
            ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![(1, moved)],
            },
        )
        .unwrap();
    simulation.world.step_physics();

    let before_stale_death = (
        simulation.journal.cursor(),
        simulation.world.registry().counters(),
        simulation.world.physics().body_count(),
        simulation.creatures.as_ref().unwrap().revision(),
    );
    assert!(
        simulation
            .commit_creature_lifecycle_transaction(
                Tick(2),
                spall_protocol::ControlSeq(2),
                vec![AuthoritativeCreatureIntent::Death {
                    key: state.key,
                    expected_state_revision: state.state_revision,
                    expected_topology_token: state.aggregate_topology_revision,
                    expected_pose_token: 0,
                }],
                None,
                &catalog.view(),
            )
            .is_err()
    );
    assert_eq!(
        (
            simulation.journal.cursor(),
            simulation.world.registry().counters(),
            simulation.world.physics().body_count(),
            simulation.creatures.as_ref().unwrap().revision(),
        ),
        before_stale_death
    );

    let measured = simulation
        .world
        .physics()
        .articulated_part_motion_snapshot(owner)
        .unwrap();
    let death = simulation
        .commit_creature_lifecycle_transaction(
            Tick(2),
            spall_protocol::ControlSeq(3),
            vec![AuthoritativeCreatureIntent::Death {
                key: state.key,
                expected_state_revision: state.state_revision,
                expected_topology_token: state.aggregate_topology_revision,
                expected_pose_token: 1,
            }],
            None,
            &catalog.view(),
        )
        .unwrap();
    assert_eq!(death.journal_seq.0, 2);
    let spall_protocol::TopologyOp::CreatureLifecycle { commit } = &death.topology.ops[0] else {
        panic!("typed death missing");
    };
    let overlay = commit.pre_motion.as_ref().unwrap();
    assert_eq!(overlay.part_velocities.len(), measured.len());
    for (serialized, physical) in overlay.part_velocities.iter().zip(&measured) {
        assert_eq!(serialized.part_id, physical.part_id);
        assert_eq!(serialized.linear_velocity_m_s, physical.linear_velocity_m_s);
        assert_eq!(
            serialized.angular_velocity_rad_s,
            physical.angular_velocity_rad_s
        );
    }
    let spall_protocol::CreatureLifecycleOperationV4::Death { after } = &commit.change.operation
    else {
        panic!("wrong lifecycle");
    };
    assert!(after.part_velocities.is_empty());
    assert_eq!(simulation.journal.len(), 2);
    assert_eq!(death.witnesses.len(), 1);
    assert!(matches!(
        death.topology.ops[0],
        spall_protocol::TopologyOp::CreatureLifecycle { .. }
    ));
    assert_eq!(
        simulation.world.creature_source_owner(volume_id),
        Some((state.key, 1))
    );
    assert_eq!(
        simulation.world.creature_frozen_poses(state.key).unwrap(),
        &[(1, moved)]
    );
    let physics = simulation
        .world
        .physics()
        .articulated_owner_snapshot(owner)
        .unwrap();
    let witness = &death.witnesses[0];
    let installed_root = simulation
        .world
        .physics_origin()
        .to_world_f64(physics.root.translation_m);
    for (actual, expected) in witness.root_translation_m.iter().zip(installed_root) {
        assert!((actual - expected).abs() < 1.0e-5);
    }
    assert_eq!(
        witness.installed_linear_velocity_m_s,
        physics.root.linvel_m_s
    );
    assert_eq!(
        witness.installed_angular_velocity_rad_s,
        physics.root.angvel_rad_s
    );
    let corpse = simulation
        .creatures
        .as_ref()
        .unwrap()
        .record(state.key.creature_id)
        .unwrap();
    assert_eq!(
        corpse.lifecycle,
        spall_protocol::CreatureLifecycleState::Corpse
    );
    assert_eq!(corpse.state_revision, state.state_revision + 1);
}

#[test]
fn two_part_birth_refuses_without_mutation_until_typed_bulk_publication_exists() {
    let mut simulation =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
        simulation.world.materials().clone(),
    );
    let mut candidate_registry = simulation.world.registry().clone();
    let volume_id = candidate_registry.allocate_volume().unwrap();
    let child_volume_id = candidate_registry.allocate_volume().unwrap();
    let mut state = catalog.state(1, volume_id.get());
    state
        .source_bindings
        .push(spall_protocol::CreaturePartBinding {
            part_id: 2,
            source_volume_id: child_volume_id,
            source_revision: 1,
        });
    let mut view = catalog.view();
    let assets = [spall_protocol::CreatureAssetCatalogEntry {
        uuid: view.assets[0].uuid,
        logical_hash: view.assets[0].logical_hash,
        part_ids: &[1, 2],
        clip_names: view.assets[0].clip_names,
        portable_material_keys: view.assets[0].portable_material_keys,
    }];
    view.assets = &assets;
    let before = (
        simulation.journal.cursor(),
        simulation.world.registry().counters(),
        simulation.world.physics().body_count(),
        simulation.creatures.as_ref().unwrap().revision(),
    );
    let birth = simulation
        .commit_creature_lifecycle_transaction(
            Tick(1),
            spall_protocol::ControlSeq(1),
            vec![AuthoritativeCreatureIntent::Birth {
                state: Box::new(state.clone()),
                parts: vec![
                    CreatureBirthPartInput {
                        part_id: 1,
                        parent_id: None,
                        local_transform: PartTransform::IDENTITY,
                        volume: crate::fixtures::noisy_source_volume(volume_id, MaterialId(1)),
                    },
                    CreatureBirthPartInput {
                        part_id: 2,
                        parent_id: Some(1),
                        local_transform: PartTransform {
                            translation_m: [2.0, 0.0, 0.0],
                            ..PartTransform::IDENTITY
                        },
                        volume: source_volume(child_volume_id),
                    },
                ],
                representation: Representation::MergedCuboids,
            }],
            None,
            &view,
        )
        .err()
        .expect("two-part inline birth must refuse");
    assert!(birth.contains("inline control-record ceiling"), "{birth}");
    assert_eq!(
        before,
        (
            simulation.journal.cursor(),
            simulation.world.registry().counters(),
            simulation.world.physics().body_count(),
            simulation.creatures.as_ref().unwrap().revision()
        )
    );
}

#[test]
fn mixed_staged_geometry_and_birth_publish_atomically_under_one_transaction() {
    use crate::stage::StageInput;
    use spall_jobs::{Generation, TopologyEpoch};
    use spall_structure::AnchorPlane;
    use spall_voxel::EvictedBricks;

    let setup = crate::fixtures::bridged_terrain_setup();
    let mut simulation = Simulation::new(SimulationConfig::new(setup)).unwrap();
    simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
        simulation.world.materials().clone(),
    );
    let terrain_id = simulation.world.terrain_volume_id();
    let terrain = simulation.world.volume_ref(terrain_id).unwrap().clone();
    let half = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            10 * BRUSH_UNIT + half,
            4 * BRUSH_UNIT + half,
            BRUSH_UNIT + half,
        ),
        2 * BRUSH_UNIT,
    )
    .unwrap();
    let intent = crate::intent::EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        crate::intent::EditTarget::Terrain,
        brush,
    );
    let staged = crate::stage::stage_edit(&StageInput::new(
        &intent,
        terrain_id,
        terrain,
        EvictedBricks::new(),
        AnchorPlane::at(0),
        Generation::START,
        TopologyEpoch::START,
    ))
    .unwrap();
    assert!(staged.splits());
    let geometry = match crate::commit::prepare_geometry(
        &simulation.world,
        &simulation.journal,
        &staged,
        Tick(1),
        spall_protocol::ControlSeq(1),
    )
    .unwrap()
    {
        crate::commit::PrepareGeometryOutcome::Ready(prepared) => prepared,
        crate::commit::PrepareGeometryOutcome::Stale(_) => {
            panic!("fresh mixed geometry fixture is stale")
        }
    };
    let mut candidate_registry = geometry.registry.clone();
    let source_id = candidate_registry.allocate_volume().unwrap();
    let state = catalog.state(1, source_id.get());
    let invalid = AuthoritativeCreatureIntent::Birth {
        state: Box::new(state.clone()),
        parts: vec![CreatureBirthPartInput {
            part_id: 1,
            parent_id: None,
            local_transform: PartTransform {
                rotation_xyzw: [0.0; 4],
                ..PartTransform::IDENTITY
            },
            volume: source_volume(source_id),
        }],
        representation: Representation::MergedCuboids,
    };
    let before = (
        simulation.world.volume_hash(terrain_id).unwrap(),
        simulation.world.registry().counters(),
        simulation.world.physics().body_count(),
        simulation.journal.cursor(),
        simulation.creatures.as_ref().unwrap().revision(),
    );
    assert!(
        simulation
            .prepare_creature_lifecycle_transaction(
                Tick(1),
                spall_protocol::ControlSeq(1),
                vec![invalid],
                Some(&staged),
                &catalog.view(),
            )
            .is_err()
    );
    assert_eq!(
        (
            simulation.world.volume_hash(terrain_id).unwrap(),
            simulation.world.registry().counters(),
            simulation.world.physics().body_count(),
            simulation.journal.cursor(),
            simulation.creatures.as_ref().unwrap().revision(),
        ),
        before,
        "late actor rejection must leave staged geometry untouched"
    );

    let committed = simulation
        .commit_creature_lifecycle_transaction(
            Tick(1),
            spall_protocol::ControlSeq(1),
            vec![AuthoritativeCreatureIntent::Birth {
                state: Box::new(state.clone()),
                parts: vec![CreatureBirthPartInput {
                    part_id: 1,
                    parent_id: None,
                    local_transform: PartTransform::IDENTITY,
                    volume: source_volume(source_id),
                }],
                representation: Representation::MergedCuboids,
            }],
            Some(&staged),
            &catalog.view(),
        )
        .unwrap();
    assert_eq!(committed.transaction_id.get(), 1);
    assert_eq!(committed.journal_seq.0, 1);
    assert_eq!(simulation.journal.len(), 1);
    assert_eq!(
        simulation.journal.entries()[0].transaction,
        committed.topology
    );
    assert!(committed.topology.ops.len() > 1);
    assert!(matches!(
        committed.topology.ops.last().unwrap(),
        spall_protocol::TopologyOp::CreatureLifecycle { .. }
    ));
    assert_eq!(
        simulation.world.creature_source_owner(source_id),
        Some((state.key, 1))
    );
    assert_eq!(
        simulation.world.registry().counters().1,
        candidate_registry.counters().1
    );
    assert_ne!(simulation.world.volume_hash(terrain_id).unwrap(), before.0);
}

fn source_volume_material(id: spall_core::VolumeId, material: MaterialId) -> Volume {
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    volume
        .apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            material,
        ))
        .unwrap();
    volume
}

struct HierarchyCatalog {
    materials: spall_core::MaterialManifest,
    asset: [spall_protocol::CreatureAssetCatalogEntry<'static>; 1],
    species: [spall_protocol::CreatureSpeciesCatalogEntry<'static>; 1],
    mapping: [spall_protocol::CreatureMaterialMappingEntry<'static>; 2],
}

impl HierarchyCatalog {
    fn view(&self) -> spall_protocol::CreatureValidationCatalog<'_> {
        spall_protocol::CreatureValidationCatalog {
            assets: &self.asset,
            species: &self.species,
            materials: &self.materials,
            material_mapping_version: 1,
            material_mapping_hash: spall_protocol::creature_material_mapping_hash(&self.mapping)
                .unwrap(),
            material_mapping: &self.mapping,
        }
    }

    fn state(&self, volumes: [spall_core::VolumeId; 3]) -> spall_protocol::CreatureStateV1 {
        let mapping_hash = spall_protocol::creature_material_mapping_hash(&self.mapping).unwrap();
        spall_protocol::CreatureStateV1 {
            schema: spall_protocol::CREATURE_STATE_SCHEMA,
            key: spall_core::CreatureKey::new(
                WorldId::from_u128(10),
                spall_core::WorldEpoch::INITIAL,
                spall_core::CreatureId::new(1).unwrap(),
                spall_core::CreatureGeneration::INITIAL,
            )
            .unwrap(),
            state_revision: 1,
            lifecycle: spall_protocol::CreatureLifecycleState::Alive,
            species_key: "deer".into(),
            species_version: 1,
            asset: spall_protocol::CreatureAssetIdentity {
                uuid: [8; 16],
                logical_hash: spall_protocol::Hash32([9; 32]),
            },
            material_manifest_hash: spall_protocol::content_manifest_hash(&self.materials),
            material_mapping_version: 1,
            material_mapping_hash: mapping_hash,
            clip_name: "idle".into(),
            clip_mode: spall_protocol::CreatureClipMode::Playing,
            clip_phase_tick: 0,
            clip_subtick_remainder: 0,
            clip_epoch: 1,
            source_bindings: volumes
                .into_iter()
                .enumerate()
                .map(
                    |(index, source_volume_id)| spall_protocol::CreaturePartBinding {
                        part_id: index as u32 + 1,
                        source_volume_id,
                        source_revision: 1,
                    },
                )
                .collect(),
            aggregate_topology_revision: 2,
            pose_epoch: 1,
            root_translation_m: [10.0, 5.0, 10.0],
            root_rotation_xyzw: [
                0.0,
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
            ],
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            locomotion_tag: 0,
        }
    }
}

#[test]
fn test_only_creature_birth_publishes_source_physics_and_ids_atomically() {
    let mut simulation =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
        simulation.world.materials().clone(),
    );
    let allocated_volume = simulation
        .world
        .registry()
        .clone()
        .allocate_volume()
        .unwrap();
    let source = source_volume(allocated_volume);
    let state = catalog.state(1, allocated_volume.get());

    let counters_before = simulation.world.registry().counters();
    let host_before = simulation.creature_allocator_snapshot().unwrap();
    let physics_before = simulation.world.physics().body_count();
    let foreign_volume = spall_core::VolumeId::new(999).unwrap();
    assert!(
        simulation
            .commit_creature_birth_for_test(
                catalog.state(1, foreign_volume.get()),
                &catalog.view(),
                vec![CreatureBirthPartInput {
                    part_id: 1,
                    parent_id: None,
                    local_transform: PartTransform::IDENTITY,
                    volume: source_volume(foreign_volume),
                }],
                Representation::MergedCuboids,
            )
            .is_err()
    );
    let invalid = simulation.commit_creature_birth_for_test(
        state.clone(),
        &catalog.view(),
        vec![CreatureBirthPartInput {
            part_id: 1,
            parent_id: None,
            local_transform: PartTransform {
                rotation_xyzw: [0.0; 4],
                ..PartTransform::IDENTITY
            },
            volume: source.clone(),
        }],
        Representation::MergedCuboids,
    );
    assert!(invalid.is_err());
    assert_eq!(simulation.world.registry().counters(), counters_before);
    assert_eq!(
        simulation.creature_allocator_snapshot().unwrap(),
        host_before
    );
    assert_eq!(simulation.world.physics().body_count(), physics_before);
    assert!(
        simulation
            .creatures
            .as_ref()
            .unwrap()
            .record(state.key.creature_id)
            .is_none()
    );

    let owner = simulation
        .commit_creature_birth_for_test(
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
    assert_eq!(
        simulation.world.registry().counters().1,
        counters_before.1 + 1
    );
    assert_eq!(
        simulation
            .creature_allocator_snapshot()
            .unwrap()
            .next_creature_id,
        2
    );
    assert_eq!(
        simulation.world.creature_physics_owner(state.key),
        Some(owner)
    );
    assert_eq!(
        simulation.world.creature_source_owner(allocated_volume),
        Some((state.key, 1))
    );
    assert_eq!(
        simulation.world.physics().articulated_collider_count(owner),
        1
    );
    assert!(simulation.world.volume_ref(allocated_volume).is_none());
    assert!(simulation.world.body_by_volume(allocated_volume).is_none());

    let moved_part_pose = PartTransform {
        translation_m: [0.75, 0.0, 0.0],
        ..PartTransform::IDENTITY
    };
    simulation
        .world
        .physics_mut()
        .update_articulated_pose(
            owner,
            ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![(1, moved_part_pose)],
            },
        )
        .unwrap();

    let source_hash = simulation.world.creature_source_hash_for_test();
    let body_count = simulation.world.physics().body_count();
    assert!(
        simulation
            .commit_creature_death_for_test(
                state.key,
                state.state_revision,
                state.aggregate_topology_revision,
                99,
                &catalog.view(),
            )
            .is_err()
    );
    assert_eq!(simulation.world.physics().body_count(), body_count);
    assert_eq!(
        simulation.world.creature_source_hash_for_test(),
        source_hash
    );
    assert_eq!(
        simulation
            .creatures
            .as_ref()
            .unwrap()
            .record(state.key.creature_id)
            .unwrap()
            .lifecycle,
        spall_protocol::CreatureLifecycleState::Alive
    );
    assert!(
        simulation
            .commit_creature_death_for_test(
                state.key,
                state.state_revision,
                state.aggregate_topology_revision,
                1,
                &catalog.view(),
            )
            .is_err()
    );
    simulation.world.step_physics();

    let transfer = simulation
        .commit_creature_death_for_test(
            state.key,
            state.state_revision,
            state.aggregate_topology_revision,
            1,
            &catalog.view(),
        )
        .unwrap();
    assert!(transfer.mass_kg > 0.0);
    assert_eq!(
        simulation.world.creature_source_hash_for_test(),
        source_hash
    );
    let corpse = simulation
        .creatures
        .as_ref()
        .unwrap()
        .record(state.key.creature_id)
        .unwrap();
    assert_eq!(
        corpse.lifecycle,
        spall_protocol::CreatureLifecycleState::Corpse
    );
    assert_eq!(corpse.clip_mode, spall_protocol::CreatureClipMode::Frozen);
    assert_eq!(corpse.state_revision, state.state_revision + 1);
    assert_eq!(
        simulation.world.creature_frozen_poses(state.key).unwrap(),
        &[(1, moved_part_pose)]
    );
    assert!(
        simulation
            .world
            .physics()
            .articulated_owner_snapshot(owner)
            .unwrap()
            .root
            .mass_kg
            > 0.0
    );
}

#[test]
fn death_rejects_replaced_source_content_with_same_max_revision() {
    let mut simulation =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
        simulation.world.materials().clone(),
    );
    let volume_id = simulation
        .world
        .registry()
        .clone()
        .allocate_volume()
        .unwrap();
    let sparse_brick = |solid_local_cell: usize, revision: u64| {
        let mut cells = vec![MaterialId::AIR; 32 * 32 * 32];
        cells[solid_local_cell] = crate::fixtures::STONE;
        spall_voxel::Brick::restored(&cells, spall_core::Revision(revision), true)
    };
    let mut source = Volume::new(volume_id, CellSizeCode::Quarter);
    source
        .insert_brick(spall_core::BrickCoord::new(0, 0, 0), sparse_brick(0, 1))
        .unwrap();
    source
        .insert_brick(spall_core::BrickCoord::new(1, 0, 0), sparse_brick(0, 5))
        .unwrap();
    let mut state = catalog.state(1, volume_id.get());
    state.source_bindings[0].source_revision = 5;
    let owner = simulation
        .commit_creature_birth_for_test(
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
    let captured_stamp = simulation
        .world
        .creature_source_metadata(state.key, 1)
        .unwrap()
        .state_stamp;
    let max_revision = |volume: &Volume| {
        volume
            .resident_brick_coords()
            .into_iter()
            .filter_map(|coord| volume.brick_revision(coord).ok().flatten())
            .map(|revision| revision.0)
            .max()
            .unwrap()
    };
    let before_max_revision =
        max_revision(simulation.world.creature_source_part(state.key, 1).unwrap());
    assert_eq!(before_max_revision, 5);
    let mut changed_source = Volume::new(volume_id, CellSizeCode::Quarter);
    changed_source
        .insert_brick(spall_core::BrickCoord::new(0, 0, 0), sparse_brick(1, 1))
        .unwrap();
    changed_source
        .insert_brick(spall_core::BrickCoord::new(1, 0, 0), sparse_brick(0, 5))
        .unwrap();
    simulation
        .world
        .replace_creature_source(state.key, 1, changed_source)
        .unwrap();
    assert_ne!(
        simulation
            .world
            .creature_source_part(state.key, 1)
            .unwrap()
            .state_stamp(),
        captured_stamp
    );
    assert_eq!(
        max_revision(simulation.world.creature_source_part(state.key, 1).unwrap()),
        before_max_revision,
        "replacement changes low-revision content while the maximum stays 5"
    );
    let before_hash = simulation.world.creature_source_hash_for_test();
    let before_counters = simulation.world.registry().counters();
    let before_allocator = simulation.creature_allocator_snapshot().unwrap();
    let before_body_count = simulation.world.physics().body_count();
    assert!(
        simulation
            .commit_creature_death_for_test(
                state.key,
                state.state_revision,
                state.aggregate_topology_revision,
                0,
                &catalog.view(),
            )
            .is_err()
    );
    assert_eq!(
        simulation.world.creature_source_hash_for_test(),
        before_hash
    );
    assert_eq!(simulation.world.registry().counters(), before_counters);
    assert_eq!(
        simulation.creature_allocator_snapshot().unwrap(),
        before_allocator
    );
    assert_eq!(simulation.world.physics().body_count(), before_body_count);
    assert_eq!(
        simulation.world.creature_physics_owner(state.key),
        Some(owner)
    );
    assert_eq!(
        simulation
            .creatures
            .as_ref()
            .unwrap()
            .record(state.key.creature_id),
        Some(&state)
    );
}

#[test]
fn prepared_death_rejects_source_replacement_before_publication() {
    let mut simulation =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
        simulation.world.materials().clone(),
    );
    let volume_id = simulation
        .world
        .registry()
        .clone()
        .allocate_volume()
        .unwrap();
    let sparse = |cell: usize, revision: u64| {
        let mut cells = vec![MaterialId::AIR; 32 * 32 * 32];
        cells[cell] = crate::fixtures::STONE;
        spall_voxel::Brick::restored(&cells, spall_core::Revision(revision), true)
    };
    let mut source = Volume::new(volume_id, CellSizeCode::Quarter);
    source
        .insert_brick(spall_core::BrickCoord::new(0, 0, 0), sparse(0, 1))
        .unwrap();
    source
        .insert_brick(
            spall_core::BrickCoord::new(1, 0, 0),
            spall_voxel::Brick::uniform(MaterialId::AIR, spall_core::Revision(5)),
        )
        .unwrap();
    let mut state = catalog.state(1, volume_id.get());
    state.source_bindings[0].source_revision = 5;
    simulation
        .commit_creature_birth_for_test(
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
    let owner = simulation.world.creature_physics_owner(state.key).unwrap();
    let prepared = simulation
        .prepare_creature_lifecycle_transaction(
            Tick(1),
            spall_protocol::ControlSeq(1),
            vec![AuthoritativeCreatureIntent::Death {
                key: state.key,
                expected_state_revision: state.state_revision,
                expected_topology_token: state.aggregate_topology_revision,
                expected_pose_token: 0,
            }],
            None,
            &catalog.view(),
        )
        .unwrap();

    let mut replacement = Volume::new(volume_id, CellSizeCode::Quarter);
    replacement
        .insert_brick(spall_core::BrickCoord::new(0, 0, 0), sparse(1, 1))
        .unwrap();
    replacement
        .insert_brick(
            spall_core::BrickCoord::new(1, 0, 0),
            spall_voxel::Brick::uniform(MaterialId::AIR, spall_core::Revision(5)),
        )
        .unwrap();
    simulation
        .world
        .replace_creature_source(state.key, 1, replacement)
        .unwrap();
    let before = (
        simulation.journal.cursor(),
        simulation.world.registry().counters(),
        simulation.world.physics().body_count(),
        simulation.world.creature_source_hash_for_test(),
        simulation.creatures.as_ref().unwrap().revision(),
    );
    assert!(
        simulation
            .validate_prepared_creature_lifecycle(prepared, &catalog.view())
            .is_err()
    );
    assert_eq!(
        (
            simulation.journal.cursor(),
            simulation.world.registry().counters(),
            simulation.world.physics().body_count(),
            simulation.world.creature_source_hash_for_test(),
            simulation.creatures.as_ref().unwrap().revision(),
        ),
        before
    );
    assert_eq!(
        simulation.world.creature_physics_owner(state.key),
        Some(owner)
    );
}

#[test]
fn nested_mixed_material_sources_keep_hierarchy_queries_and_mass_on_transfer() {
    use spall_physics::{ArticulatedPose, PartTransform, Representation};
    use spall_protocol::{
        CreatureAssetCatalogEntry, CreatureAssetIdentity, CreatureMaterialMappingEntry,
        CreatureSpeciesCatalogEntry, content_manifest_hash,
    };

    let mut simulation =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let materials = simulation.world.materials().clone();
    let catalog = HierarchyCatalog {
        materials: materials.clone(),
        asset: [CreatureAssetCatalogEntry {
            uuid: [8; 16],
            logical_hash: spall_protocol::Hash32([9; 32]),
            part_ids: &[1, 2, 3],
            clip_names: &["idle"],
            portable_material_keys: &["fur", "hide"],
        }],
        species: [CreatureSpeciesCatalogEntry {
            key: "deer",
            version: 1,
            enabled: true,
            asset: CreatureAssetIdentity {
                uuid: [8; 16],
                logical_hash: spall_protocol::Hash32([9; 32]),
            },
        }],
        mapping: [
            CreatureMaterialMappingEntry {
                portable_key: "fur",
                material_id: MaterialId(1),
            },
            CreatureMaterialMappingEntry {
                portable_key: "hide",
                material_id: MaterialId(2),
            },
        ],
    };
    assert_eq!(
        content_manifest_hash(&materials),
        content_manifest_hash(simulation.world.materials())
    );
    let mut candidate_registry = simulation.world.registry().clone();
    let volumes = [
        candidate_registry.allocate_volume().unwrap(),
        candidate_registry.allocate_volume().unwrap(),
        candidate_registry.allocate_volume().unwrap(),
    ];
    let state = catalog.state(volumes);
    let q90 = [
        0.0,
        0.0,
        std::f32::consts::FRAC_1_SQRT_2,
        std::f32::consts::FRAC_1_SQRT_2,
    ];
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
    let sources = [
        source_volume_material(volumes[0], MaterialId(1)),
        source_volume_material(volumes[1], MaterialId(2)),
        source_volume_material(volumes[2], MaterialId(1)),
    ];
    let owner = simulation
        .commit_creature_birth_for_test(
            state.clone(),
            &catalog.view(),
            (0..3)
                .map(|index| CreatureBirthPartInput {
                    part_id: index as u32 + 1,
                    parent_id: authored[index].0,
                    local_transform: authored[index].1,
                    volume: sources[index].clone(),
                })
                .collect(),
            Representation::MergedCuboids,
        )
        .unwrap();
    assert_eq!(
        simulation.world.registry().counters().1,
        candidate_registry.counters().1
    );

    let moving = [
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
    ];
    simulation
        .world
        .physics_mut()
        .update_articulated_pose(
            owner,
            ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: 0,
                pose_token: 1,
                parts: moving.to_vec(),
            },
        )
        .unwrap();
    simulation.world.step_physics();

    let root_q = glam::Quat::from_xyzw(q90[0], q90[1], q90[2], q90[3]);
    let root_t = glam::Vec3::new(10.0, 5.0, 10.0);
    let compose = |parent: (glam::Vec3, glam::Quat), local: PartTransform| {
        let q = glam::Quat::from_xyzw(
            local.rotation_xyzw[0],
            local.rotation_xyzw[1],
            local.rotation_xyzw[2],
            local.rotation_xyzw[3],
        );
        (
            parent.0 + parent.1 * glam::Vec3::from_array(local.translation_m),
            parent.1 * q,
        )
    };
    let part1 = (root_t, root_q);
    let part2 = compose(part1, moving[1].1);
    let part3 = compose(part2, moving[2].1);
    let rays: Vec<_> = [part1, part2, part3]
        .into_iter()
        .map(|(translation, rotation)| {
            let center = translation + rotation * glam::Vec3::splat(0.125);
            let origin = center - glam::Vec3::Z;
            (
                [origin.x, origin.y, origin.z],
                [center.x, center.y, center.z],
            )
        })
        .collect();
    let mut baseline = Vec::new();
    for (origin, _) in &rays {
        let hit = simulation
            .world
            .physics_mut()
            .raycast_articulated(owner, *origin, [0.0, 0.0, 1.0], 2.0)
            .unwrap();
        baseline.push((hit.part_id, hit.volume_id, hit.cell));
    }
    assert_eq!(
        baseline.iter().map(|hit| hit.0).collect::<Vec<_>>(),
        [1, 2, 3]
    );

    let source_hash = simulation.world.creature_source_hash_for_test();
    let transfer = simulation
        .commit_creature_death_for_test(
            state.key,
            state.state_revision,
            state.aggregate_topology_revision,
            1,
            &catalog.view(),
        )
        .unwrap();
    let cell_volume = 0.25_f64.powi(3);
    let expected_mass = cell_volume
        * (simulation.world.density(MaterialId(1)) * 2.0 + simulation.world.density(MaterialId(2)));
    assert!((transfer.mass_kg - expected_mass).abs() < 1.0e-5);
    assert_eq!(
        simulation.world.creature_source_hash_for_test(),
        source_hash
    );
    let hierarchy = simulation.world.creature_hierarchy(state.key).unwrap();
    assert_eq!(hierarchy.len(), 3);
    for (index, part) in hierarchy.iter().enumerate() {
        assert_eq!(part.part_id, index as u32 + 1);
        assert_eq!(part.parent_id, authored[index].0);
        assert_eq!(
            part.initial_local_transform.translation_m,
            authored[index].1.translation_m
        );
        assert_eq!(
            part.frozen_local_transform.translation_m,
            moving[index].1.translation_m
        );
        for (actual, expected) in part
            .initial_local_transform
            .rotation_xyzw
            .iter()
            .zip(authored[index].1.rotation_xyzw)
            .chain(
                part.frozen_local_transform
                    .rotation_xyzw
                    .iter()
                    .zip(moving[index].1.rotation_xyzw),
            )
        {
            assert!((actual - expected).abs() < 1.0e-6);
        }
        assert_eq!(
            simulation.world.creature_source_owner(volumes[index]),
            Some((state.key, index as u32 + 1))
        );
        assert_eq!(
            simulation
                .world
                .creature_source_part(state.key, index as u32 + 1)
                .unwrap()
                .id(),
            volumes[index]
        );
    }
    for ((origin, _), expected) in rays.iter().zip(baseline) {
        let hit = simulation
            .world
            .physics_mut()
            .raycast_articulated(owner, *origin, [0.0, 0.0, 1.0], 2.0)
            .unwrap();
        assert_eq!((hit.part_id, hit.volume_id, hit.cell), expected);
    }
    let corpse = simulation
        .creatures
        .as_ref()
        .unwrap()
        .record(state.key.creature_id)
        .unwrap();
    assert_eq!(
        corpse.material_manifest_hash,
        content_manifest_hash(simulation.world.materials())
    );
    assert_eq!(
        spall_protocol::creature_material_mapping_hash(&catalog.mapping).unwrap(),
        corpse.material_mapping_hash
    );
    assert_eq!(
        simulation.world.physics().articulated_collider_count(owner),
        3
    );
}

impl CreatureSchedulingRules for CountingRules {
    fn propose(
        &self,
        context: CreatureScheduleContext,
        _snapshots: &[CreatureScheduleSnapshot],
        _output: &mut CreatureIntentWriter,
    ) -> Result<(), CreatureRuleError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(context);
        Ok(())
    }
}

#[test]
fn simulation_host_starts_empty_and_world_replacement_advances_epoch_without_resetting_counter() {
    let mut simulation =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    simulation
        .initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    let catalog = crate::creatures::tests::FixtureCatalog::new();
    simulation
        .creatures
        .as_mut()
        .unwrap()
        .register_validated(catalog.state(1, 1001), &catalog.view())
        .unwrap();
    let before = simulation.creature_allocator_snapshot().unwrap();
    assert_eq!(before.world_epoch, 1);
    assert_eq!(before.next_creature_id, 2);

    let rules = CountingRules {
        calls: AtomicUsize::new(0),
        contexts: Mutex::new(Vec::new()),
    };
    let report = simulation.tick_with_creature_rules(&rules).unwrap();
    let schedule = report.creature_schedule.unwrap();
    assert_eq!(schedule.snapshot_count, 1);
    assert!(schedule.accepted_intents.is_empty());
    assert_eq!(rules.calls.load(Ordering::SeqCst), 1);
    let context = rules.contexts.lock().unwrap()[0];
    assert_eq!(context.tick, 1);
    assert!(context.terrain_revision > 0);
    assert_eq!(context.water_frame_revision, None);

    let fresh =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    simulation.replace_world(fresh).unwrap();
    let after = simulation.creature_allocator_snapshot().unwrap();
    assert_eq!(after.world_epoch, 2);
    assert_eq!(after.next_creature_id, before.next_creature_id);
    let report = simulation.tick_with_creature_rules(&rules).unwrap();
    assert_eq!(report.creature_schedule.unwrap().snapshot_count, 0);
    assert_eq!(rules.calls.load(Ordering::SeqCst), 1);
}

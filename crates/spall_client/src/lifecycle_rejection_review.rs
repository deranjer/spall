//! Independent rejection checks until complete actor candidates are installed.
use super::*;
use spall_core::{CreatureGeneration, CreatureId, CreatureKey, WorldEpoch, WorldId};
use spall_protocol::*;

fn mixed_transaction() -> TopologyTransaction {
    let materials = spall_sim::fixtures::flat_terrain_setup().materials;
    let asset = CreatureAssetIdentity {
        uuid: [3; 16],
        logical_hash: Hash32([4; 32]),
    };
    let assets = [CreatureAssetCatalogEntry {
        uuid: asset.uuid,
        logical_hash: asset.logical_hash,
        part_ids: &[1],
        clip_names: &["idle"],
        portable_material_keys: &["stone"],
    }];
    let species = [CreatureSpeciesCatalogEntry {
        key: "deer",
        version: 1,
        enabled: true,
        asset,
    }];
    let mapping = [CreatureMaterialMappingEntry {
        portable_key: "stone",
        material_id: MaterialId(1),
    }];
    let mapping_hash = creature_material_mapping_hash(&mapping).unwrap();
    let state = CreatureStateV1 {
        schema: CREATURE_STATE_SCHEMA,
        key: CreatureKey::new(
            WorldId::from_u128(900),
            WorldEpoch::INITIAL,
            CreatureId::new(1).unwrap(),
            CreatureGeneration::INITIAL,
        )
        .unwrap(),
        state_revision: 1,
        lifecycle: CreatureLifecycleState::Alive,
        species_key: "deer".into(),
        species_version: 1,
        asset,
        material_manifest_hash: content_manifest_hash(&materials),
        material_mapping_version: 1,
        material_mapping_hash: mapping_hash,
        clip_name: "idle".into(),
        clip_mode: CreatureClipMode::Playing,
        clip_phase_tick: 0,
        clip_subtick_remainder: 0,
        clip_epoch: 1,
        source_bindings: vec![CreaturePartBinding {
            part_id: 1,
            source_volume_id: VolumeId::new(100).unwrap(),
            source_revision: 1,
        }],
        aggregate_topology_revision: 1,
        pose_epoch: 1,
        root_translation_m: [0.0; 3],
        root_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        linear_velocity_m_s: [0.0; 3],
        angular_velocity_rad_s: [0.0; 3],
        locomotion_tag: 0,
    };
    let local = CreatureLocalPoseV1 {
        translation_m: [0.0; 3],
        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
    };
    let mut cells = vec![0; spall_core::CELLS_PER_BRICK];
    cells[0] = 1;
    let mut part = CreatureSourcePartV1 {
        part_id: 1,
        source_volume_id: VolumeId::new(100).unwrap(),
        source_generation: 1,
        source_revision: 1,
        cell_size_code: CellSizeCode::Quarter.to_u8(),
        bounds: Some([[0; 3], [0; 3]]),
        source_content_hash: Hash32::ZERO,
        parent_id: None,
        initial_local_pose: local,
        frozen_local_pose: local,
        bricks: vec![BaselineBrick {
            coord: [0; 3],
            revision: 1,
            edited: true,
            cells: BaselineCells::Dense(cells),
        }],
    };
    part.source_content_hash = part.canonical_source_content_hash();
    let after = CreatureOwnershipRecordV1 {
        schema: CREATURE_OWNER_SCHEMA,
        state,
        owner_key: 9,
        source_generation: 1,
        next_component_serial: 1,
        parts: vec![part],
        components: vec![CreatureOwnedComponentV1 {
            owner_key: 9,
            serial: None,
            translation_m: [0.0; 3],
            rotation_xyzw: local.rotation_xyzw,
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            source_masks: vec![CreatureSourceMaskBrickV1 {
                volume_id: VolumeId::new(100).unwrap(),
                part_id: 1,
                coord: [0; 3],
                source_revision: 1,
                brick_revision: 1,
                cells: vec![CreatureMaskCellV1 {
                    index: 0,
                    material_id: 1,
                }],
            }],
        }],
    };
    after
        .validate(&CreatureValidationCatalog {
            assets: &assets,
            species: &species,
            materials: &materials,
            material_mapping_version: 1,
            material_mapping_hash: mapping_hash,
            material_mapping: &mapping,
        })
        .unwrap();
    let transaction_id = spall_core::TransactionId::new(77).unwrap();
    let change = CreatureLifecycleChangeV1 {
        schema: CREATURE_CHANGE_SCHEMA,
        transaction_id,
        operation_ordinal: 1,
        expected_before_hash: Hash32::ZERO,
        expected_after_hash: after.canonical_hash(),
        operation: CreatureLifecycleOperationV1::Birth { after },
    };
    TopologyTransaction {
        transaction_id,
        server_tick: spall_core::Tick(999),
        control_seq: ControlSeq(100),
        algorithm_version: 1,
        dependencies: vec![],
        before: vec![],
        after: vec![],
        result_hashes: vec![],
        ops: vec![
            TopologyOp::CellRun {
                volume: VolumeId::new(1).unwrap(),
                start: GlobalCell::new(0, 0, 0),
                len: 1,
                material: MaterialId::AIR,
            },
            TopologyOp::CreatureLifecycle {
                commit: Box::new(CreatureLifecycleCommitV1 {
                    schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
                    change,
                    expected_before_topology_hash: Hash32::ZERO,
                    pre_motion: None,
                    death_witness: None,
                }),
            },
        ],
    }
}

#[test]
fn unsupported_lifecycle_does_not_mutate_replica_geometry_clocks_or_queues() {
    for pending_catalogue in [false, true] {
        let setup = spall_sim::fixtures::flat_terrain_setup();
        let mut replica = ReplicaWorld::from_baseline(setup.terrain, ReplicaConfig::default());
        if pending_catalogue {
            replica.catalogue_pending = Some(TransferId(12));
        }
        let before = (
            replica.world_hash(),
            replica.now_tick,
            replica.control_gate.high_water(),
            replica.terrain_generation,
            replica.applied_tx.clone(),
            replica.catalogue_pending,
        );
        let mut tx = mixed_transaction();
        tx.validate().unwrap();
        assert_eq!(
            decode_control::<TopologyTransaction>(&encode_control(&tx).unwrap()).unwrap(),
            tx
        );
        for sequence in [100, 101] {
            tx.control_seq = ControlSeq(sequence);
            assert!(matches!(
                replica.apply_transaction(&tx),
                ApplyOutcome::Rejected { .. }
            ));
            assert_eq!(
                (
                    replica.world_hash(),
                    replica.now_tick,
                    replica.control_gate.high_water(),
                    replica.terrain_generation,
                    replica.applied_tx.clone(),
                    replica.catalogue_pending
                ),
                before
            );
            assert!(replica.pending_repair_txns.is_empty());
            assert!(replica.pending_bulk_split_txns.is_empty());
            assert!(replica.catalogue_deferred.is_empty());
        }
    }
}

#[test]
fn geometry_only_recovery_rejects_later_lifecycle_op_before_first_cell_run() {
    let mut world = spall_sim::SimWorld::new(spall_sim::fixtures::flat_terrain_setup()).unwrap();
    let before = (
        world.world_hash(),
        world.registry().counters(),
        world.physics().body_count(),
    );
    let tx = mixed_transaction();
    tx.validate().unwrap();
    assert!(world.replay_transaction(&tx, &[], None).is_err());
    assert_eq!(
        (
            world.world_hash(),
            world.registry().counters(),
            world.physics().body_count()
        ),
        before
    );
}

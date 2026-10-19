use super::*;
use crate::creatures::tests::FixtureCatalog;
use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId, WorldId};
use spall_physics::{ArticulatedPose, PartTransform, Representation};
use spall_protocol::*;
use spall_voxel::{EditPlan, Volume};

struct Catalog {
    base: FixtureCatalog,
    assets: [CreatureAssetCatalogEntry<'static>; 1],
}
impl Catalog {
    fn new(sim: &Simulation) -> Self {
        let base = FixtureCatalog::with_materials(sim.world.materials().clone());
        let view = base.view();
        let assets = [CreatureAssetCatalogEntry {
            uuid: view.assets[0].uuid,
            logical_hash: view.assets[0].logical_hash,
            part_ids: &[1, 2, 3],
            clip_names: &["idle"],
            portable_material_keys: &["fur"],
        }];
        Self { base, assets }
    }
    fn view(&self) -> CreatureValidationCatalog<'_> {
        CreatureValidationCatalog {
            assets: &self.assets,
            ..self.base.view()
        }
    }
}
fn simulation(setup: crate::world::WorldSetup) -> Simulation {
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    sim.initialize_creature_host(WorldId::from_u128(10))
        .unwrap();
    sim
}
fn birth(cat: &Catalog, next: u64, creature: u64) -> AuthoritativeCreatureIntent {
    let mut state = cat.base.state(creature, next);
    state.root_translation_m = [10.0, 4.0, 10.0];
    state.source_bindings = (0..3)
        .map(|i| CreaturePartBinding {
            part_id: i + 1,
            source_volume_id: VolumeId::new(next + u64::from(i)).unwrap(),
            source_revision: 1,
        })
        .collect();
    let parts = state
        .source_bindings
        .iter()
        .map(|binding| {
            let volume =
                crate::fixtures::noisy_source_volume(binding.source_volume_id, MaterialId(1));
            CreatureBirthPartInput {
                part_id: binding.part_id,
                parent_id: if binding.part_id == 1 {
                    None
                } else {
                    Some(binding.part_id - 1)
                },
                local_transform: PartTransform {
                    translation_m: [2.0 * (binding.part_id - 1) as f32, 0.0, 0.0],
                    ..PartTransform::IDENTITY
                },
                volume,
            }
        })
        .collect();
    AuthoritativeCreatureIntent::Birth {
        state: Box::new(state),
        parts,
        representation: Representation::MergedCuboids,
    }
}
fn reserved(low: u64) -> CreatureTransactionTransfers {
    CreatureTransactionTransfers {
        geometry: None,
        lifecycle: vec![TransferId(CREATURE_LIFECYCLE_TRANSFER_ID_BIT | low)],
    }
}
fn fingerprint(
    sim: &Simulation,
    cat: &Catalog,
) -> (Hash32, (u64, u64, u64, u64), usize, u64, Hash32) {
    (
        sim.world.geometry_topology_hash(),
        sim.world.registry().counters(),
        sim.world.physics().body_count(),
        sim.journal.cursor(),
        sim.capture_creature_snapshot(&cat.view())
            .unwrap()
            .unwrap()
            .canonical_hash(),
    )
}
fn finish(
    sim: &mut Simulation,
    prepared: PreparedSimulationTransaction,
    cat: &Catalog,
) -> CommittedCreatureLifecycle {
    let validated = sim
        .validate_prepared_creature_lifecycle(prepared, &cat.view())
        .unwrap();
    sim.publish_validated_creature_lifecycle(validated)
}
fn check_handoff(
    sim: &Simulation,
    committed: &CommittedCreatureLifecycle,
    id: u64,
) -> CreatureLifecycleCommitV4 {
    let entry = sim.journal.last().unwrap();
    assert_eq!(entry.transaction, committed.topology);
    assert_eq!(entry.attachments, committed.attachments);
    assert_eq!(entry.seq, committed.journal_seq);
    let bytes = entry
        .attachments
        .as_ref()
        .unwrap()
        .encode_for_transaction(&entry.transaction)
        .unwrap();
    let decoded = decode_for_transaction(&bytes, &entry.transaction).unwrap();
    assert!(decoded.matches_transaction(&committed.topology));
    let Some(ValidatedTransactionAttachmentV1::CreatureLifecycle(payload)) = decoded.get(id) else {
        panic!("missing lifecycle attachment")
    };
    assert_eq!(payload.transaction_id, committed.transaction_id);
    assert_eq!(payload.commit_hash, payload.commit.canonical_hash());
    assert!(entry.geometry_only_bulk().is_none());
    let mut inline = committed.topology.clone();
    let ordinal = payload.operation_ordinal as usize;
    inline.ops[ordinal] = TopologyOp::CreatureLifecycle {
        commit: Box::new(payload.commit.clone()),
    };
    inline.validate().unwrap();
    assert!(matches!(encode_control(&inline), Err(CodecError::Size(_))));
    assert!(encode_control(&committed.topology).is_ok());
    payload.commit.clone()
}

#[test]
fn actual_three_part_birth_motion_and_checked_death_publish_typed_bulk_once_each() {
    let mut sim = simulation(crate::fixtures::flat_terrain_setup());
    let cat = Catalog::new(&sim);
    let next = sim.world.registry().counters().1;
    let intent = birth(&cat, next, 1);
    let AuthoritativeCreatureIntent::Birth { state, .. } = &intent else {
        unreachable!()
    };
    let state = state.clone();
    let initial = fingerprint(&sim, &cat);
    let prepared = sim
        .prepare_creature_lifecycle_transaction_with_transfers(
            Tick(1),
            ControlSeq(1),
            vec![intent],
            None,
            &cat.view(),
            reserved(101),
        )
        .unwrap();
    assert_eq!(fingerprint(&sim, &cat), initial);
    let result = finish(&mut sim, prepared, &cat);
    assert_eq!(sim.journal.len(), 1);
    let commit = check_handoff(&sim, &result, CREATURE_LIFECYCLE_TRANSFER_ID_BIT | 101);
    let CreatureLifecycleOperationV4::Birth { after } = commit.change.operation else {
        unreachable!()
    };
    let captured = sim.capture_creature_snapshot(&cat.view()).unwrap().unwrap();
    assert_eq!(captured.owners.records, vec![after]);
    assert_eq!(sim.world.registry().counters().1, next + 3);
    let owner = sim.world.creature_physics_owner(state.key).unwrap();
    let poses = vec![
        (
            1,
            PartTransform {
                translation_m: [0.25, 0.0, 0.0],
                ..PartTransform::IDENTITY
            },
        ),
        (
            2,
            PartTransform {
                translation_m: [2.0, 0.25, 0.0],
                ..PartTransform::IDENTITY
            },
        ),
        (
            3,
            PartTransform {
                translation_m: [4.0, 0.0, 0.5],
                ..PartTransform::IDENTITY
            },
        ),
    ];
    sim.world
        .physics_mut()
        .update_articulated_pose(
            owner,
            ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: 0,
                pose_token: 1,
                parts: poses,
            },
        )
        .unwrap();
    sim.world.step_physics();
    let measured = sim
        .world
        .physics()
        .articulated_part_motion_snapshot(owner)
        .unwrap();
    assert!(
        measured
            .windows(2)
            .any(|w| w[0].linear_velocity_m_s != w[1].linear_velocity_m_s)
    );
    let prepared = sim
        .prepare_creature_lifecycle_transaction_with_transfers(
            Tick(2),
            ControlSeq(2),
            vec![AuthoritativeCreatureIntent::Death {
                key: state.key,
                expected_state_revision: state.state_revision,
                expected_topology_token: state.aggregate_topology_revision,
                expected_pose_token: 1,
            }],
            None,
            &cat.view(),
            reserved(102),
        )
        .unwrap();
    let before = fingerprint(&sim, &cat);
    let result = finish(&mut sim, prepared, &cat);
    assert_eq!(sim.journal.len(), 2);
    assert_eq!(result.journal_seq.0, 2);
    let commit = check_handoff(&sim, &result, CREATURE_LIFECYCLE_TRANSFER_ID_BIT | 102);
    let motion = commit.pre_motion.as_ref().unwrap();
    for (saved, actual) in motion.part_velocities.iter().zip(&measured) {
        assert_eq!(saved.part_id, actual.part_id);
        assert_eq!(saved.linear_velocity_m_s, actual.linear_velocity_m_s);
        assert_eq!(saved.angular_velocity_rad_s, actual.angular_velocity_rad_s);
    }
    assert_eq!(motion.part_velocities.len(), 3);
    assert_eq!(result.witnesses, vec![commit.death_witness.unwrap()]);
    assert_eq!(sim.world.registry().counters().1, before.1.1);
    let CreatureLifecycleOperationV4::Death { after } = commit.change.operation else {
        unreachable!()
    };
    assert!(after.part_velocities.is_empty());
    assert_eq!(
        sim.capture_creature_snapshot(&cat.view())
            .unwrap()
            .unwrap()
            .owners
            .records,
        vec![after]
    );
}

#[test]
fn missing_invalid_duplicate_and_unused_reserved_ids_never_mutate_live_state() {
    let mut sim = simulation(crate::fixtures::flat_terrain_setup());
    let cat = Catalog::new(&sim);
    let next = sim.world.registry().counters().1;
    let before = fingerprint(&sim, &cat);
    for ids in [
        vec![],
        vec![TransferId(0)],
        vec![TransferId(CREATURE_LIFECYCLE_TRANSFER_ID_BIT)],
        vec![TransferId(SPLIT_BULK_TRANSFER_ID_BIT | 7)],
        vec![TransferId(
            CREATURE_LIFECYCLE_TRANSFER_ID_BIT | WORLD_RESET_TRANSFER_ID_BIT | 7,
        )],
    ] {
        assert!(
            sim.prepare_creature_lifecycle_transaction_with_transfers(
                Tick(1),
                ControlSeq(1),
                vec![birth(&cat, next, 1)],
                None,
                &cat.view(),
                CreatureTransactionTransfers {
                    geometry: None,
                    lifecycle: ids
                }
            )
            .is_err()
        );
        assert_eq!(fingerprint(&sim, &cat), before);
    }
    assert!(
        sim.prepare_creature_lifecycle_transaction_with_transfers(
            Tick(1),
            ControlSeq(1),
            vec![birth(&cat, next, 1), birth(&cat, next + 3, 2)],
            None,
            &cat.view(),
            CreatureTransactionTransfers {
                geometry: None,
                lifecycle: vec![TransferId(CREATURE_LIFECYCLE_TRANSFER_ID_BIT | 7); 2]
            }
        )
        .is_err()
    );
    assert_eq!(fingerprint(&sim, &cat), before);
    // Run out after staging the first of two valid bulk owners: no prefix publishes.
    assert!(
        sim.prepare_creature_lifecycle_transaction_with_transfers(
            Tick(1),
            ControlSeq(1),
            vec![birth(&cat, next, 1), birth(&cat, next + 3, 2)],
            None,
            &cat.view(),
            reserved(7)
        )
        .is_err()
    );
    assert_eq!(fingerprint(&sim, &cat), before);
    let mut extra = reserved(7);
    extra
        .lifecycle
        .push(TransferId(CREATURE_LIFECYCLE_TRANSFER_ID_BIT | 8));
    assert!(
        sim.prepare_creature_lifecycle_transaction_with_transfers(
            Tick(1),
            ControlSeq(1),
            vec![birth(&cat, next, 1)],
            None,
            &cat.view(),
            extra
        )
        .is_err()
    );
    assert_eq!(fingerprint(&sim, &cat), before);
    // A valid but unused reservation does not force a small transaction to bulk.
    let AuthoritativeCreatureIntent::Birth {
        mut state,
        mut parts,
        representation,
    } = birth(&cat, next, 1)
    else {
        unreachable!()
    };
    state.source_bindings.truncate(1);
    parts.truncate(1);
    let mut tiny = Volume::new(
        state.source_bindings[0].source_volume_id,
        CellSizeCode::Quarter,
    );
    tiny.apply_edit(&EditPlan::filled_box(
        tiny.id(),
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(0, 0, 0),
        MaterialId(1),
    ))
    .unwrap();
    parts[0].volume = tiny;
    let small_assets = [CreatureAssetCatalogEntry {
        part_ids: &[1],
        ..cat.assets[0]
    }];
    let small = CreatureValidationCatalog {
        assets: &small_assets,
        ..cat.view()
    };
    let prepared = sim
        .prepare_creature_lifecycle_transaction_with_transfers(
            Tick(1),
            ControlSeq(1),
            vec![AuthoritativeCreatureIntent::Birth {
                state,
                parts,
                representation,
            }],
            None,
            &small,
            reserved(9),
        )
        .unwrap();
    assert!(prepared.attachments.is_none());
    let validated = sim
        .validate_prepared_creature_lifecycle(prepared, &small)
        .unwrap();
    let committed = sim.publish_validated_creature_lifecycle(validated);
    assert!(committed.attachments.is_none());
}

#[test]
fn stale_or_changed_catalog_bulk_candidate_refuses_without_partial_publication() {
    for fault in 0..3 {
        let mut sim = simulation(crate::fixtures::flat_terrain_setup());
        let cat = Catalog::new(&sim);
        let prepared = sim
            .prepare_creature_lifecycle_transaction_with_transfers(
                Tick(1),
                ControlSeq(1),
                vec![birth(&cat, sim.world.registry().counters().1, 1)],
                None,
                &cat.view(),
                reserved(1),
            )
            .unwrap();
        if fault == 0 {
            sim.world.step_physics();
        }
        if fault == 1 {
            sim.world.registry_mut().allocate_volume().unwrap();
        }
        let before = fingerprint(&sim, &cat);
        let mut view = cat.view();
        if fault == 2 {
            view.material_mapping_version += 1;
        }
        assert!(
            sim.validate_prepared_creature_lifecycle(prepared, &view)
                .is_err()
        );
        assert_eq!(fingerprint(&sim, &cat), before);
        assert!(sim.journal.is_empty());
    }
}

#[test]
fn real_bulk_geometry_and_three_part_birth_share_one_complete_set_and_publication() {
    use crate::stage::{StageInput, stage_edit};
    use spall_jobs::{Generation, TopologyEpoch};
    let mut sim = simulation(crate::fixtures::bulk_split_setup());
    let cat = Catalog::new(&sim);
    let terrain = sim.world.terrain_volume_id();
    let half = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            31 * BRUSH_UNIT + half,
            8 * BRUSH_UNIT + half,
            31 * BRUSH_UNIT + half,
        ),
        8 * BRUSH_UNIT,
    )
    .unwrap();
    let intent = EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    );
    let staged = stage_edit(&StageInput::new(
        &intent,
        terrain,
        sim.world.terrain().volume.clone(),
        spall_voxel::EvictedBricks::new(),
        spall_structure::AnchorPlane::at(0),
        Generation::START,
        TopologyEpoch::START,
    ))
    .unwrap();
    let geometry_id = TransferId(SPLIT_BULK_TRANSFER_ID_BIT | 501);
    let mut ids = [geometry_id].into_iter().collect();
    let crate::commit::PrepareGeometryOutcome::Ready(geometry) =
        crate::commit::prepare_geometry_with_transfer_ids(
            &sim.world,
            &sim.journal,
            &staged,
            Tick(1),
            ControlSeq(1),
            &mut ids,
        )
        .unwrap()
    else {
        panic!("stale geometry")
    };
    assert!(
        geometry.attachments.is_some(),
        "full-size geometry must require bulk"
    );
    let next = geometry.registry.counters().1;
    let before = fingerprint(&sim, &cat);
    assert!(
        sim.prepare_creature_lifecycle_transaction_with_transfers(
            Tick(1),
            ControlSeq(1),
            vec![birth(&cat, next, 1)],
            Some(&staged),
            &cat.view(),
            reserved(502)
        )
        .is_err()
    );
    assert_eq!(fingerprint(&sim, &cat), before);
    let mut invalid = birth(&cat, next, 1);
    if let AuthoritativeCreatureIntent::Birth { parts, .. } = &mut invalid {
        parts[1].local_transform.rotation_xyzw = [0.0; 4];
    }
    let transfers = || CreatureTransactionTransfers {
        geometry: Some(geometry_id),
        lifecycle: vec![TransferId(CREATURE_LIFECYCLE_TRANSFER_ID_BIT | 502)],
    };
    assert!(
        sim.prepare_creature_lifecycle_transaction_with_transfers(
            Tick(1),
            ControlSeq(1),
            vec![invalid],
            Some(&staged),
            &cat.view(),
            transfers()
        )
        .is_err()
    );
    assert_eq!(fingerprint(&sim, &cat), before);
    let prepared = sim
        .prepare_creature_lifecycle_transaction_with_transfers(
            Tick(1),
            ControlSeq(1),
            vec![birth(&cat, next, 1)],
            Some(&staged),
            &cat.view(),
            transfers(),
        )
        .unwrap();
    assert_eq!(fingerprint(&sim, &cat), before);
    let expected_geometry = geometry.topology.result_hashes.clone();
    let committed = finish(&mut sim, prepared, &cat);
    assert_eq!(sim.journal.len(), 1);
    assert_eq!(committed.topology.result_hashes, expected_geometry);
    let set = committed.attachments.as_ref().unwrap();
    assert_eq!(set.attachments.len(), 2);
    let commit = check_handoff(&sim, &committed, CREATURE_LIFECYCLE_TRANSFER_ID_BIT | 502);
    assert!(commit.change.operation_ordinal > 0);
    assert_eq!(sim.world.body_count(), 1);
    let bytes = set.encode_for_transaction(&committed.topology).unwrap();
    let decoded = decode_for_transaction(&bytes, &committed.topology).unwrap();
    assert!(matches!(
        decoded.get(geometry_id.0),
        Some(ValidatedTransactionAttachmentV1::Geometry(_))
    ));
    // Even an inline actor plus a sole geometry attachment is a mixed transaction.
    let mut inline_mixed = sim.journal.last().unwrap().clone();
    let actor_ordinal = commit.change.operation_ordinal as usize;
    inline_mixed.transaction.ops[actor_ordinal] = TopologyOp::CreatureLifecycle {
        commit: Box::new(commit),
    };
    inline_mixed
        .attachments
        .as_mut()
        .unwrap()
        .attachments
        .retain(|a| matches!(a, TransactionAttachmentV1::Geometry { .. }));
    assert!(inline_mixed.geometry_only_bulk().is_none());
    assert_ne!(sim.world.geometry_topology_hash(), before.0);
    assert_eq!(
        sim.capture_creature_snapshot(&cat.view())
            .unwrap()
            .unwrap()
            .owners
            .records
            .len(),
        1
    );
}

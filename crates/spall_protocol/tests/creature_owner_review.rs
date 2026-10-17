//! Coordinator-owned portable ownership contract regressions.
use spall_core::{
    CreatureGeneration, CreatureId, CreatureKey, MaterialDef, MaterialFlags, MaterialId,
    MaterialManifest, RenderProps, SimProps, VolumeId, WorldEpoch, WorldId,
};
use spall_protocol::creature_owner::{CreatureDeathWitnessV1, CreatureLifecycleReplayReceiptV1};
use spall_protocol::*;

#[test]
fn exact_partition_rejects_missing_foreign_air_stale_and_overlapping_cells() {
    let f = Fixture::new();
    let original = owner(&f);
    original.validate(&f.catalog()).unwrap();
    for case in 0..8 {
        let mut record = original.clone();
        let mask = &mut record.components[0].source_masks[0];
        match case {
            0 => mask.cells.clear(),
            1 => mask.volume_id = VolumeId::new(999).unwrap(),
            2 => mask.cells[0].index = 1, // Known air in the source.
            3 => mask.source_revision += 1,
            4 => mask.brick_revision += 1,
            5 => mask.cells.push(mask.cells[0]),
            6 => mask.cells[0].material_id = 2,
            7 => {
                record.state.lifecycle = CreatureLifecycleState::Corpse;
                record.state.clip_mode = CreatureClipMode::Frozen;
                record.next_component_serial = 3;
                record.components[0].serial = Some(1);
                let mut overlapping = record.components[0].clone();
                overlapping.serial = Some(2);
                record.components.push(overlapping);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            record.validate(&f.catalog()),
            Err(CreatureOwnershipError::Mask),
            "partition case {case}"
        );
    }
}

#[test]
fn failed_birth_preserves_inputs_and_rejects_collisions_stale_hash_and_exhaustion() {
    let f = Fixture::new();
    let record = owner(&f);
    let change = birth(record.clone());
    let before = change.clone();
    let keys = [record.state.key];
    let owners = [record.owner_key];
    let volumes = [record.parts[0].source_volume_id];
    for case in 0..4 {
        let mut ctx = context(7, 1, 100);
        let mut attempted = change.clone();
        match case {
            0 => ctx.existing_keys = &keys,
            1 => ctx.existing_owner_keys = &owners,
            2 => ctx.existing_source_volumes = &volumes,
            3 => attempted.expected_before_hash = Hash32([7; 32]),
            _ => unreachable!(),
        }
        let saved_context = ctx;
        assert!(reduce_creature_lifecycle_candidate(None, &attempted, ctx, &f.catalog()).is_err());
        assert_eq!(ctx, saved_context);
        assert_eq!(change, before);
    }
    let mut exhausted = record;
    exhausted.state.key.creature_id = CreatureId::new(u64::MAX).unwrap();
    let attempted = birth(exhausted);
    assert_eq!(
        reduce_creature_lifecycle_candidate(
            None,
            &attempted,
            context(7, u64::MAX, 100),
            &f.catalog()
        ),
        Err(CreatureOwnershipError::CounterExhausted)
    );
}

#[test]
fn source_hash_binds_air_resident_revision_edited_bounds_and_cell_size() {
    let f = Fixture::new();
    let original = owner(&f);
    let original_hash = original.parts[0].canonical_source_content_hash();
    for case in 0..5 {
        let mut part = original.parts[0].clone();
        match case {
            0 => part.bricks[0].revision += 1,
            1 => part.bricks[0].edited = false,
            2 => part.bounds = None,
            3 => part.cell_size_code += 1,
            4 => part.bricks.push(BaselineBrick {
                coord: [1, 0, 0],
                revision: 2,
                edited: true,
                cells: BaselineCells::Uniform(0),
            }),
            _ => unreachable!(),
        }
        assert_ne!(
            part.canonical_source_content_hash(),
            original_hash,
            "source metadata case {case}"
        );
    }
}

struct Fixture {
    materials: MaterialManifest,
    assets: [CreatureAssetCatalogEntry<'static>; 2],
    species: [CreatureSpeciesCatalogEntry<'static>; 2],
    mapping: [CreatureMaterialMappingEntry<'static>; 1],
}
fn asset(n: u8) -> CreatureAssetIdentity {
    CreatureAssetIdentity {
        uuid: [n; 16],
        logical_hash: Hash32([n + 1; 32]),
    }
}
impl Fixture {
    fn new() -> Self {
        Self {
            materials: MaterialManifest::validated(vec![MaterialDef {
                id: MaterialId(1),
                name: "flesh".into(),
                render: RenderProps {
                    albedo: [0.5; 3],
                    roughness: 0.5,
                    metalness: 0.0,
                    emissive: [0.0; 3],
                },
                sim: SimProps {
                    density_kg_m3: 1.0,
                    friction: 0.5,
                    restitution: 0.0,
                    hardness: 0.0,
                    bond_strength: 0.0,
                    flags: MaterialFlags(0),
                },
            }])
            .unwrap(),
            assets: [
                CreatureAssetCatalogEntry {
                    uuid: asset(3).uuid,
                    logical_hash: asset(3).logical_hash,
                    part_ids: &[1],
                    clip_names: &["idle"],
                    portable_material_keys: &["flesh"],
                },
                CreatureAssetCatalogEntry {
                    uuid: asset(5).uuid,
                    logical_hash: asset(5).logical_hash,
                    part_ids: &[1],
                    clip_names: &["idle"],
                    portable_material_keys: &["flesh"],
                },
            ],
            species: [
                CreatureSpeciesCatalogEntry {
                    key: "deer",
                    version: 1,
                    enabled: true,
                    asset: asset(3),
                },
                CreatureSpeciesCatalogEntry {
                    key: "trout",
                    version: 1,
                    enabled: true,
                    asset: asset(5),
                },
            ],
            mapping: [CreatureMaterialMappingEntry {
                portable_key: "flesh",
                material_id: MaterialId(1),
            }],
        }
    }
    fn catalog(&self) -> CreatureValidationCatalog<'_> {
        CreatureValidationCatalog {
            assets: &self.assets,
            species: &self.species,
            materials: &self.materials,
            material_mapping_version: 1,
            material_mapping_hash: creature_material_mapping_hash(&self.mapping).unwrap(),
            material_mapping: &self.mapping,
        }
    }
    fn state(&self) -> CreatureStateV1 {
        CreatureStateV1 {
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
            asset: asset(3),
            material_manifest_hash: content_manifest_hash(&self.materials),
            material_mapping_version: 1,
            material_mapping_hash: creature_material_mapping_hash(&self.mapping).unwrap(),
            clip_name: "idle".into(),
            clip_mode: CreatureClipMode::Playing,
            clip_phase_tick: 1,
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
        }
    }
}

fn owner(f: &Fixture) -> CreatureOwnershipRecordV1 {
    let state = f.state();
    let q = [0.0_f32, 0.0, 0.0, 1.0];
    let pose = serde_json::json!({"translation_m": [0.0,0.0,0.0], "rotation_xyzw": q});
    let mut ids = vec![0_u16; 32768];
    ids[0] = 1;
    let raw = serde_json::json!({
        "schema": 1, "state": state, "owner_key": 9, "source_generation": 1,
        "next_component_serial": 1,
        "parts": [{
            "part_id": 1, "source_volume_id": 100, "source_generation": 1,
            "source_revision": 1, "cell_size_code": 1, "bounds": [[0,0,0],[0,0,0]],
            "source_content_hash": vec![0_u8;32], "parent_id": null,
            "initial_local_pose": pose, "frozen_local_pose": pose,
            "bricks": [{"coord": [0,0,0], "revision": 1, "edited": true, "cells": {"Dense": ids}}]
        }],
        "components": [{
            "owner_key": 9, "serial": null, "translation_m": [0.0,0.0,0.0],
            "rotation_xyzw": q, "linear_velocity_m_s": [0.0,0.0,0.0], "angular_velocity_rad_s": [0.0,0.0,0.0],
            "source_masks": [{"volume_id":100,"part_id":1,"coord":[0,0,0],"source_revision":1,"brick_revision":1,
                "cells":[{"index":0,"material_id":1}]}]
        }]
    });
    let mut record: CreatureOwnershipRecordV1 = serde_json::from_value(raw).unwrap();
    record.parts[0].source_content_hash = record.parts[0].canonical_source_content_hash();
    record
}

#[test]
fn portable_owner_roundtrip_and_logical_storage_equivalence() {
    let f = Fixture::new();
    let mut uniform = owner(&f);
    uniform.parts[0].bricks[0].cells = BaselineCells::Uniform(1);
    uniform.components[0].source_masks[0].cells = (0..32768_u16)
        .map(|index| CreatureMaskCellV1 {
            index,
            material_id: 1,
        })
        .collect();
    uniform.parts[0].source_content_hash = uniform.parts[0].canonical_source_content_hash();
    uniform.validate(&f.catalog()).unwrap();
    let mut dense = uniform.clone();
    dense.parts[0].bricks[0].cells = BaselineCells::Dense(vec![1; 32768]);
    dense.parts[0].source_content_hash = dense.parts[0].canonical_source_content_hash();
    dense.validate(&f.catalog()).unwrap();
    assert_eq!(
        uniform.parts[0].source_content_hash,
        dense.parts[0].source_content_hash
    );
    assert_eq!(uniform.canonical_hash(), dense.canonical_hash());
    for record in [uniform, dense] {
        let bytes = record.encode(&f.catalog()).unwrap();
        let decoded = CreatureOwnershipRecordV1::decode(&bytes, &f.catalog()).unwrap();
        assert_eq!(record, decoded);
        assert_eq!(bytes, decoded.encode(&f.catalog()).unwrap());
    }
}

#[test]
fn frozen_local_pose_is_owner_state_not_source_content() {
    let f = Fixture::new();
    let original = owner(&f);
    original.validate(&f.catalog()).unwrap();
    let mut moved = original.clone();
    moved.parts[0].frozen_local_pose.translation_m = [0.25, 0.5, 0.0];
    assert_eq!(
        original.parts[0].canonical_source_content_hash(),
        moved.parts[0].canonical_source_content_hash()
    );
    assert_ne!(original.canonical_hash(), moved.canonical_hash());
}

#[test]
fn intact_component_cannot_disagree_with_actor_frame_or_twist() {
    let f = Fixture::new();
    for case in 0..4 {
        let mut record = owner(&f);
        match case {
            0 => record.components[0].translation_m[0] = 1.0,
            1 => record.components[0].rotation_xyzw = [0.0, 0.0, 1.0, 0.0],
            2 => record.components[0].linear_velocity_m_s[0] = 1.0,
            _ => record.components[0].angular_velocity_rad_s[0] = 1.0,
        }
        assert!(
            record.validate(&f.catalog()).is_err(),
            "contradictory intact component case {case}"
        );
    }
}

#[test]
fn hierarchy_requires_one_root_and_no_cycle() {
    let mut f = Fixture::new();
    f.assets[0].part_ids = &[1, 2];
    let mut record = owner(&f);
    record.state.source_bindings.push(CreaturePartBinding {
        part_id: 2,
        source_volume_id: VolumeId::new(101).unwrap(),
        source_revision: 1,
    });
    let mut child = record.parts[0].clone();
    child.part_id = 2;
    child.source_volume_id = VolumeId::new(101).unwrap();
    child.parent_id = Some(1);
    child.source_content_hash = child.canonical_source_content_hash();
    record.parts.push(child);
    let mut mask = record.components[0].source_masks[0].clone();
    mask.part_id = 2;
    mask.volume_id = VolumeId::new(101).unwrap();
    record.components[0].source_masks.push(mask);
    record.validate(&f.catalog()).unwrap();
    let mut two_roots = record.clone();
    two_roots.parts[1].parent_id = None;
    two_roots.parts[1].source_content_hash = two_roots.parts[1].canonical_source_content_hash();
    assert!(two_roots.validate(&f.catalog()).is_err());
    record.parts[0].parent_id = Some(2);
    record.parts[0].source_content_hash = record.parts[0].canonical_source_content_hash();
    assert!(record.validate(&f.catalog()).is_err());
}

#[test]
fn oversized_nested_part_count_is_rejected_during_decode() {
    let f = Fixture::new();
    let mut malformed = owner(&f);
    let mut part = malformed.parts[0].clone();
    part.bricks[0].cells = BaselineCells::Uniform(0);
    malformed.parts = vec![part; MAX_CREATURE_OWNER_PARTS + 1];
    // Bypass the trusted encoder to exercise the actual hostile byte decoder.
    let raw = postcard::to_stdvec(&malformed).unwrap();
    assert!(raw.len() < MAX_CREATURE_OWNER_RECORD_BYTES);
    assert!(
        matches!(
            CreatureOwnershipRecordV1::decode(&raw, &f.catalog()),
            Err(CreatureOwnershipError::Encoding(_))
        ),
        "a post-allocation validation error is too late"
    );
}

#[test]
fn last_surviving_fragment_keeps_identity_and_retired_serial_high_water() {
    let f = Fixture::new();
    let mut record = owner(&f);
    record.state.lifecycle = CreatureLifecycleState::Corpse;
    record.state.clip_mode = CreatureClipMode::Frozen;
    record.components[0].serial = Some(41);
    record.next_component_serial = 100;
    record.validate(&f.catalog()).unwrap();
    let bytes = record.encode(&f.catalog()).unwrap();
    let decoded = CreatureOwnershipRecordV1::decode(&bytes, &f.catalog()).unwrap();
    assert_eq!(decoded.components[0].serial, Some(41));
    assert_eq!(decoded.next_component_serial, 100);
    let mut invalid = record.clone();
    invalid.next_component_serial = 41;
    assert!(invalid.validate(&f.catalog()).is_err());
    let mut reused = record.clone();
    reused.next_component_serial = 99;
    assert_ne!(record.canonical_hash(), reused.canonical_hash());
}

fn context(
    transaction: u64,
    next_creature: u64,
    next_volume: u64,
) -> CreatureLifecycleContextV1<'static> {
    CreatureLifecycleContextV1 {
        transaction_id: spall_core::TransactionId::new(transaction).unwrap(),
        operation_ordinal: 0,
        replay_receipt: None,
        allocator: spall_core::CreatureAllocatorSnapshotV1 {
            schema: 1,
            world_id: WorldId::from_u128(900),
            world_epoch: 1,
            next_creature_id: next_creature,
        },
        next_volume_id: next_volume,
        death_witness: None,
        existing_keys: &[],
        existing_owner_keys: &[],
        existing_source_volumes: &[],
    }
}
fn birth(record: CreatureOwnershipRecordV1) -> CreatureLifecycleChangeV1 {
    CreatureLifecycleChangeV1 {
        schema: 1,
        transaction_id: spall_core::TransactionId::new(7).unwrap(),
        operation_ordinal: 0,
        expected_before_hash: Hash32::ZERO,
        expected_after_hash: record.canonical_hash(),
        operation: CreatureLifecycleOperationV1::Birth { after: record },
    }
}
fn two_parts(f: &Fixture) -> CreatureOwnershipRecordV1 {
    let mut record = owner(f);
    record.state.source_bindings.push(CreaturePartBinding {
        part_id: 2,
        source_volume_id: VolumeId::new(101).unwrap(),
        source_revision: 1,
    });
    let mut child = record.parts[0].clone();
    child.part_id = 2;
    child.source_volume_id = VolumeId::new(101).unwrap();
    child.parent_id = Some(1);
    child.source_content_hash = child.canonical_source_content_hash();
    record.parts.push(child);
    let mut mask = record.components[0].source_masks[0].clone();
    mask.part_id = 2;
    mask.volume_id = VolumeId::new(101).unwrap();
    record.components[0].source_masks.push(mask);
    record
}

#[test]
fn receipt_binds_outer_transaction_ordinal_and_validated_operation() {
    let mut f = Fixture::new();
    f.assets[0].part_ids = &[1, 2];
    let record = two_parts(&f);
    let change = birth(record);
    let result =
        reduce_creature_lifecycle_candidate(None, &change, context(7, 1, 100), &f.catalog())
            .unwrap();
    assert_eq!(result.next_creature_id, 2);
    assert_eq!(result.next_volume_id, 102);
    let mut replay_context = context(7, 2, 102);
    replay_context.replay_receipt = Some(CreatureLifecycleReplayReceiptV1 {
        transaction_id: change.transaction_id,
        operation_ordinal: 0,
        change_hash: change.canonical_hash(),
    });
    assert_eq!(
        reduce_creature_lifecycle_candidate(
            Some(&result.record),
            &change,
            replay_context,
            &f.catalog()
        )
        .unwrap()
        .outcome,
        CreatureReductionOutcome::Duplicate
    );
    let mut wrong_ordinal = change.clone();
    wrong_ordinal.operation_ordinal = 1;
    let mut wrong_context = replay_context;
    wrong_context.operation_ordinal = 1;
    assert!(
        reduce_creature_lifecycle_candidate(
            Some(&result.record),
            &wrong_ordinal,
            wrong_context,
            &f.catalog()
        )
        .is_err()
    );
    let mut malformed = change.clone();
    let CreatureLifecycleOperationV1::Birth { after } = &mut malformed.operation else {
        unreachable!()
    };
    after.parts.reverse();
    assert_eq!(
        malformed.canonical_hash(),
        change.canonical_hash(),
        "canonical ordering must not be mistaken for input validity"
    );
    assert!(
        reduce_creature_lifecycle_candidate(
            Some(&result.record),
            &malformed,
            replay_context,
            &f.catalog()
        )
        .is_err()
    );
}

#[test]
fn fresh_birth_cannot_invent_source_lineage_or_initial_motion() {
    let f = Fixture::new();
    for case in 0..2 {
        let mut record = owner(&f);
        if case == 0 {
            record.source_generation = 2;
            record.parts[0].source_generation = 2;
            record.parts[0].source_content_hash = record.parts[0].canonical_source_content_hash();
        } else {
            record.state.linear_velocity_m_s = [1.0, 0.0, 0.0];
            record.components[0].linear_velocity_m_s = [1.0, 0.0, 0.0];
        }
        record.validate(&f.catalog()).unwrap();
        assert!(
            reduce_creature_lifecycle_candidate(
                None,
                &birth(record),
                context(7, 1, 100),
                &f.catalog()
            )
            .is_err(),
            "unsupported birth case {case}"
        );
    }
}

#[test]
fn death_requires_witness_and_preserves_definition_and_source_data() {
    let f = Fixture::new();
    let current = owner(&f);
    let mut after = current.clone();
    after.state.lifecycle = CreatureLifecycleState::Corpse;
    after.state.clip_mode = CreatureClipMode::Frozen;
    after.state.state_revision += 1;
    after.state.pose_epoch += 1;
    after.state.clip_epoch += 1;
    after.state.root_translation_m = [2.0, 4.0, 6.0];
    after.state.linear_velocity_m_s = [3.0, 0.0, 0.0];
    after.components[0].translation_m = after.state.root_translation_m;
    after.components[0].linear_velocity_m_s = after.state.linear_velocity_m_s;
    after.parts[0].frozen_local_pose.translation_m = [0.125, 0.0, 0.0];
    after.validate(&f.catalog()).unwrap();
    assert_eq!(
        current.parts[0].source_content_hash,
        after.parts[0].source_content_hash
    );
    let witness = CreatureDeathWitnessV1 {
        key: current.state.key,
        source_generation: 1,
        before_state_revision: 1,
        before_topology_revision: 1,
        before_pose_epoch: 1,
        root_translation_m: after.state.root_translation_m,
        root_rotation_xyzw: after.state.root_rotation_xyzw,
        installed_linear_velocity_m_s: after.state.linear_velocity_m_s,
        installed_angular_velocity_rad_s: after.state.angular_velocity_rad_s,
        frozen_local_poses: vec![(1, after.parts[0].frozen_local_pose)],
    };
    let change_for = |record: CreatureOwnershipRecordV1| CreatureLifecycleChangeV1 {
        schema: 1,
        transaction_id: spall_core::TransactionId::new(8).unwrap(),
        operation_ordinal: 0,
        expected_before_hash: current.canonical_hash(),
        expected_after_hash: record.canonical_hash(),
        operation: CreatureLifecycleOperationV1::Death { after: record },
    };
    let valid = change_for(after.clone());
    assert!(
        reduce_creature_lifecycle_candidate(
            Some(&current),
            &valid,
            context(8, 2, 101),
            &f.catalog()
        )
        .is_err()
    );
    let mut checked_context = context(8, 2, 101);
    checked_context.death_witness = Some(&witness);
    let candidate =
        reduce_creature_lifecycle_candidate(Some(&current), &valid, checked_context, &f.catalog())
            .unwrap();
    assert_eq!(candidate.record, after);
    assert_eq!(
        (candidate.next_creature_id, candidate.next_volume_id),
        (2, 101)
    );
    for case in 0..3 {
        let mut invalid = after.clone();
        match case {
            0 => invalid.state.clip_phase_tick += 1,
            1 => {
                invalid.state.species_key = "trout".into();
                invalid.state.asset = asset(5);
            }
            _ => {
                let BaselineCells::Dense(cells) = &mut invalid.parts[0].bricks[0].cells else {
                    unreachable!()
                };
                cells[0] = 0;
                cells[1] = 1;
                invalid.components[0].source_masks[0].cells[0].index = 1;
                invalid.parts[0].source_content_hash =
                    invalid.parts[0].canonical_source_content_hash();
            }
        }
        invalid.validate(&f.catalog()).unwrap();
        assert!(
            reduce_creature_lifecycle_candidate(
                Some(&current),
                &change_for(invalid),
                checked_context,
                &f.catalog()
            )
            .is_err(),
            "unauthorized death mutation {case}"
        );
    }
    assert_eq!(current.state.lifecycle, CreatureLifecycleState::Alive);
}

#[test]
fn aggregate_brick_limit_rejects_nested_payload_before_record_validation() {
    let mut f = Fixture::new();
    f.assets[0].part_ids = &[1, 2];
    let mut record = two_parts(&f);
    for (part, count) in record.parts.iter_mut().zip([2049_usize, 2048]) {
        let occupied = part.bricks[0].cells.clone();
        part.bounds = None;
        part.bricks = (0..count)
            .map(|index| BaselineBrick {
                coord: [index as i64, 0, 0],
                revision: 1,
                edited: true,
                cells: if index == 0 {
                    occupied.clone()
                } else {
                    BaselineCells::Uniform(0)
                },
            })
            .collect();
        part.source_content_hash = part.canonical_source_content_hash();
    }
    let bytes = postcard::to_stdvec(&record).unwrap();
    assert!(bytes.len() < MAX_CREATURE_OWNER_RECORD_BYTES);
    assert!(
        matches!(
            CreatureOwnershipRecordV1::decode(&bytes, &f.catalog()),
            Err(CreatureOwnershipError::Encoding(_))
        ),
        "per-part bounds must not defer cumulative rejection until after nested allocations"
    );
}

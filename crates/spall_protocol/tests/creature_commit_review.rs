//! Worker-owned lifecycle commit wrapper regressions.
use spall_core::{
    CreatureGeneration, CreatureId, CreatureKey, MaterialDef, MaterialFlags, MaterialId,
    MaterialManifest, RenderProps, SimProps, VolumeId, WorldEpoch, WorldId,
};
use spall_protocol::creature_owner::CreatureDeathWitnessV1;
use spall_protocol::*;

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
#[test]
fn authoritative_motion_overlay_allows_death_without_any_motion_datagram() {
    let f = Fixture::new();
    let client_before = owner(&f);
    let mut authoritative_before = client_before.clone();
    authoritative_before.state.state_revision = 4;
    authoritative_before.state.pose_epoch = 7;
    authoritative_before.state.clip_epoch = 5;
    authoritative_before.state.clip_phase_tick = 9;
    authoritative_before.state.root_translation_m = [1.25, 2.0, 3.0];
    authoritative_before.state.linear_velocity_m_s = [2.5, 0.0, 0.0];
    authoritative_before.components[0].translation_m =
        authoritative_before.state.root_translation_m;
    authoritative_before.components[0].linear_velocity_m_s =
        authoritative_before.state.linear_velocity_m_s;

    let mut after = authoritative_before.clone();
    after.state.lifecycle = CreatureLifecycleState::Corpse;
    after.state.clip_mode = CreatureClipMode::Frozen;
    after.state.state_revision += 1;
    after.state.pose_epoch += 1;
    after.state.clip_epoch += 1;
    after.parts[0].frozen_local_pose.translation_m = [0.125, 0.0, 0.0];
    after.validate(&f.catalog()).unwrap();
    let witness = CreatureDeathWitnessV1 {
        key: authoritative_before.state.key,
        source_generation: authoritative_before.source_generation,
        before_state_revision: authoritative_before.state.state_revision,
        before_topology_revision: authoritative_before.state.aggregate_topology_revision,
        before_pose_epoch: authoritative_before.state.pose_epoch,
        root_translation_m: after.state.root_translation_m,
        root_rotation_xyzw: after.state.root_rotation_xyzw,
        installed_linear_velocity_m_s: after.state.linear_velocity_m_s,
        installed_angular_velocity_rad_s: after.state.angular_velocity_rad_s,
        frozen_local_poses: vec![(1, after.parts[0].frozen_local_pose)],
    };
    let overlay = CreatureMotionOverlayV1 {
        state_revision: authoritative_before.state.state_revision,
        clip_mode: authoritative_before.state.clip_mode,
        clip_phase_tick: authoritative_before.state.clip_phase_tick,
        clip_subtick_remainder: authoritative_before.state.clip_subtick_remainder,
        clip_epoch: authoritative_before.state.clip_epoch,
        pose_epoch: authoritative_before.state.pose_epoch,
        root_translation_m: authoritative_before.state.root_translation_m,
        root_rotation_xyzw: authoritative_before.state.root_rotation_xyzw,
        linear_velocity_m_s: authoritative_before.state.linear_velocity_m_s,
        angular_velocity_rad_s: authoritative_before.state.angular_velocity_rad_s,
        locomotion_tag: authoritative_before.state.locomotion_tag,
        parts: authoritative_before
            .parts
            .iter()
            .map(|part| CreaturePartMotionV1 {
                part_id: part.part_id,
                frozen_local_pose: part.frozen_local_pose,
            })
            .collect(),
        components: authoritative_before
            .components
            .iter()
            .map(|component| CreatureComponentMotionV1 {
                owner_key: component.owner_key,
                serial: component.serial,
                translation_m: component.translation_m,
                rotation_xyzw: component.rotation_xyzw,
                linear_velocity_m_s: component.linear_velocity_m_s,
                angular_velocity_rad_s: component.angular_velocity_rad_s,
            })
            .collect(),
    };
    let change = CreatureLifecycleChangeV1 {
        schema: CREATURE_CHANGE_SCHEMA,
        transaction_id: spall_core::TransactionId::new(8).unwrap(),
        operation_ordinal: 0,
        expected_before_hash: authoritative_before.canonical_hash(),
        expected_after_hash: after.canonical_hash(),
        operation: CreatureLifecycleOperationV1::Death {
            after: after.clone(),
        },
    };
    let commit = CreatureLifecycleCommitV1 {
        schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
        change,
        expected_before_topology_hash: client_before.topology_hash(),
        pre_motion: Some(overlay.clone()),
        death_witness: Some(witness.clone()),
    };
    let applied = commit
        .validate_and_reduce(Some(&client_before), context(8, 2, 101), &f.catalog())
        .unwrap();
    assert_eq!(applied.record, after);
    let duplicate_receipt = CreatureLifecycleCommitReplayReceiptV1 {
        transaction_id: commit.change.transaction_id,
        operation_ordinal: commit.change.operation_ordinal,
        commit_hash: commit.canonical_hash(),
    };
    let duplicate = commit
        .validate_and_reduce_with_receipt(
            Some(&applied.record),
            context(8, 2, 101),
            &f.catalog(),
            Some(duplicate_receipt),
        )
        .unwrap();
    assert_eq!(duplicate.outcome, CreatureReductionOutcome::Duplicate);
    let wrong_receipt = CreatureLifecycleCommitReplayReceiptV1 {
        commit_hash: Hash32([8; 32]),
        ..duplicate_receipt
    };
    assert_eq!(
        commit.validate_and_reduce_with_receipt(
            Some(&applied.record),
            context(8, 2, 101),
            &f.catalog(),
            Some(wrong_receipt)
        ),
        Err(CreatureOwnershipError::DivergentReplay)
    );
    let mut malformed_duplicate = commit.clone();
    malformed_duplicate
        .pre_motion
        .as_mut()
        .unwrap()
        .root_translation_m[0] = f64::NAN;
    let malformed_receipt = CreatureLifecycleCommitReplayReceiptV1 {
        commit_hash: malformed_duplicate.canonical_hash(),
        ..duplicate_receipt
    };
    assert!(
        malformed_duplicate
            .validate_and_reduce_with_receipt(
                Some(&applied.record),
                context(8, 2, 101),
                &f.catalog(),
                Some(malformed_receipt)
            )
            .is_err()
    );

    let mut stale_fence = commit.clone();
    stale_fence.expected_before_topology_hash = Hash32([8; 32]);
    assert!(
        stale_fence
            .validate_and_reduce(Some(&client_before), context(8, 2, 101), &f.catalog())
            .is_err()
    );

    let mut tampered_source = client_before.clone();
    tampered_source.parts[0].source_revision += 1;
    assert!(
        commit
            .validate_and_reduce(Some(&tampered_source), context(8, 2, 101), &f.catalog())
            .is_err()
    );

    let mut rolled_back = commit.clone();
    rolled_back.pre_motion.as_mut().unwrap().pose_epoch = client_before.state.pose_epoch - 1;
    assert!(
        rolled_back
            .validate_and_reduce(Some(&client_before), context(8, 2, 101), &f.catalog())
            .is_err()
    );

    let mut duplicate_keys = commit.clone();
    duplicate_keys
        .pre_motion
        .as_mut()
        .unwrap()
        .parts
        .push(overlay.parts[0]);
    assert!(
        duplicate_keys
            .validate_and_reduce(Some(&client_before), context(8, 2, 101), &f.catalog())
            .is_err()
    );

    let mut bad_witness = commit;
    bad_witness
        .death_witness
        .as_mut()
        .unwrap()
        .before_state_revision += 1;
    assert!(
        bad_witness
            .validate_and_reduce(Some(&client_before), context(8, 2, 101), &f.catalog())
            .is_err()
    );
}

#[test]
fn birth_rejects_motion_overlay_and_death_witness() {
    let f = Fixture::new();
    let after = owner(&f);
    let mut change = birth(after.clone());
    change.transaction_id = spall_core::TransactionId::new(7).unwrap();
    let commit = CreatureLifecycleCommitV1 {
        schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
        change,
        expected_before_topology_hash: Hash32::ZERO,
        pre_motion: Some(CreatureMotionOverlayV1 {
            state_revision: 1,
            clip_mode: CreatureClipMode::Playing,
            clip_phase_tick: 0,
            clip_subtick_remainder: 0,
            clip_epoch: 1,
            pose_epoch: 1,
            root_translation_m: [0.0; 3],
            root_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            locomotion_tag: 0,
            parts: vec![],
            components: vec![],
        }),
        death_witness: None,
    };
    assert!(
        commit
            .validate_and_reduce(None, context(7, 1, 100), &f.catalog())
            .is_err()
    );
}

#[test]
fn birth_duplicate_uses_outer_receipt_and_rejects_divergent_identity() {
    let f = Fixture::new();
    let after = owner(&f);
    let commit = CreatureLifecycleCommitV1 {
        schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
        change: birth(after.clone()),
        expected_before_topology_hash: Hash32::ZERO,
        pre_motion: None,
        death_witness: None,
    };
    let applied = commit
        .validate_and_reduce(None, context(7, 1, 100), &f.catalog())
        .unwrap();
    let receipt = CreatureLifecycleCommitReplayReceiptV1 {
        transaction_id: commit.change.transaction_id,
        operation_ordinal: commit.change.operation_ordinal,
        commit_hash: commit.canonical_hash(),
    };
    let duplicate = commit
        .validate_and_reduce_with_receipt(
            Some(&applied.record),
            context(7, 2, 101),
            &f.catalog(),
            Some(receipt),
        )
        .unwrap();
    assert_eq!(duplicate.outcome, CreatureReductionOutcome::Duplicate);
    let wrong = CreatureLifecycleCommitReplayReceiptV1 {
        commit_hash: Hash32([9; 32]),
        ..receipt
    };
    assert_eq!(
        commit.validate_and_reduce_with_receipt(
            Some(&applied.record),
            context(7, 2, 101),
            &f.catalog(),
            Some(wrong)
        ),
        Err(CreatureOwnershipError::DivergentReplay)
    );
}

#[test]
fn outer_decode_bounds_overlay_and_witness_lists_before_accepting_items() {
    let f = Fixture::new();
    let after = owner(&f);
    let base = CreatureLifecycleCommitV1 {
        schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
        change: birth(after),
        expected_before_topology_hash: Hash32::ZERO,
        pre_motion: None,
        death_witness: None,
    };
    let mut raw = serde_json::to_value(&base).unwrap();
    let pose =
        serde_json::json!({"translation_m": [0.0,0.0,0.0], "rotation_xyzw": [0.0,0.0,0.0,1.0]});
    raw["pre_motion"] = serde_json::json!({
        "state_revision": 1, "clip_mode": "Playing", "clip_phase_tick": 0,
        "clip_subtick_remainder": 0, "clip_epoch": 1, "pose_epoch": 1,
        "root_translation_m": [0.0,0.0,0.0], "root_rotation_xyzw": [0.0,0.0,0.0,1.0],
        "linear_velocity_m_s": [0.0,0.0,0.0], "angular_velocity_rad_s": [0.0,0.0,0.0],
        "locomotion_tag": 0,
        "parts": vec![serde_json::json!({"part_id":1,"frozen_local_pose":pose}); MAX_CREATURE_OWNER_PARTS + 1],
        "components": []
    });
    assert!(serde_json::from_value::<CreatureLifecycleCommitV1>(raw.clone()).is_err());
    raw["pre_motion"] = serde_json::Value::Null;
    raw["death_witness"] = serde_json::json!({
        "key": owner(&f).state.key, "source_generation": 1,
        "before_state_revision": 1, "before_topology_revision": 1, "before_pose_epoch": 1,
        "root_translation_m": [0.0,0.0,0.0], "root_rotation_xyzw": [0.0,0.0,0.0,1.0],
        "installed_linear_velocity_m_s": [0.0,0.0,0.0], "installed_angular_velocity_rad_s": [0.0,0.0,0.0],
        "frozen_local_poses": vec![serde_json::json!([1, pose]); MAX_CREATURE_OWNER_PARTS + 1]
    });
    assert!(serde_json::from_value::<CreatureLifecycleCommitV1>(raw).is_err());
}

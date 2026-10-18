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
fn population_decode_roundtrips_distinct_sources_and_still_requires_semantic_validation() {
    let f = Fixture::new();
    let first = owner(&f);
    let mut second = first.clone();
    second.state.key.creature_id = CreatureId::new(2).unwrap();
    second.owner_key = 10;
    let volume = VolumeId::new(101).unwrap();
    second.state.source_bindings[0].source_volume_id = volume;
    second.parts[0].source_volume_id = volume;
    second.parts[0].source_content_hash = second.parts[0].canonical_source_content_hash();
    second.components[0].owner_key = 10;
    second.components[0].source_masks[0].volume_id = volume;
    let set = CreatureOwnershipSetV1 {
        schema: CREATURE_OWNER_SCHEMA,
        records: vec![first, second],
    };
    set.validate(&f.catalog()).unwrap();
    let bytes = postcard::to_allocvec(&set).unwrap();
    let decoded = CreatureOwnershipSetV1::decode(&bytes, &f.catalog()).unwrap();
    assert_eq!(decoded, set);
    assert_eq!(decoded.canonical_hash(), set.canonical_hash());
    decoded.validate(&f.catalog()).unwrap();
    assert_eq!(postcard::to_allocvec(&decoded).unwrap(), bytes);
    let mut trailing = bytes;
    trailing.push(0);
    assert!(CreatureOwnershipSetV1::decode(&trailing, &f.catalog()).is_err());
    let json = serde_json::to_vec(&set).unwrap();
    assert_eq!(
        serde_json::from_slice::<CreatureOwnershipSetV1>(&json).unwrap(),
        set
    );

    // Bounded decoding cannot substitute for catalog, canonical order or
    // identity validation at the consumer's publication boundary.
    let mut duplicate = set;
    duplicate.records[1] = duplicate.records[0].clone();
    let bytes = postcard::to_allocvec(&duplicate).unwrap();
    let decoded: CreatureOwnershipSetV1 = postcard::from_bytes(&bytes).unwrap();
    assert!(decoded.validate(&f.catalog()).is_err());
    assert!(CreatureOwnershipSetV1::decode(&bytes, &f.catalog()).is_err());

    let empty = CreatureOwnershipSetV1::empty();
    assert_eq!(
        postcard::from_bytes::<CreatureOwnershipSetV1>(&postcard::to_allocvec(&empty).unwrap())
            .unwrap(),
        empty
    );
}

fn compact_cost_record(f: &Fixture, bricks: usize, material: u16) -> CreatureOwnershipRecordV1 {
    let mut record = owner(f);
    record.parts[0].bricks = (0..bricks)
        .map(|x| BaselineBrick {
            coord: [x as i64, 0, 0],
            revision: 1,
            edited: true,
            cells: BaselineCells::Uniform(material),
        })
        .collect();
    // These deliberately incomplete masks test decode cost admission, not a
    // valid population. Full semantic validation remains mandatory afterward.
    record.components.clear();
    record
}

#[test]
fn population_decode_charges_compact_uniform_solids_across_records() {
    let f = Fixture::new();
    let full = compact_cost_record(&f, 30, 1); // 983,040 solids per record.
    let tail = compact_cost_record(&f, 2, 1);
    let mut set = CreatureOwnershipSetV1 {
        schema: CREATURE_OWNER_SCHEMA,
        records: vec![full.clone(), full.clone(), full.clone(), full, tail],
    };
    for record in &set.records {
        postcard::from_bytes::<CreatureOwnershipRecordV1>(&postcard::to_allocvec(record).unwrap())
            .unwrap();
    }
    let bytes = postcard::to_allocvec(&set).unwrap();
    assert!(
        bytes.len() < 16 * 1024,
        "compact wire must not hide expansion cost"
    );
    postcard::from_bytes::<CreatureOwnershipSetV1>(&bytes).unwrap(); // 3,997,696 solids.
    set.records[4] = compact_cost_record(&f, 3, 1); // 4,030,464 solids.
    let bytes = postcard::to_allocvec(&set).unwrap();
    assert!(postcard::from_bytes::<CreatureOwnershipSetV1>(&bytes).is_err());

    // A set's larger allowance must not relax the one-owner solid ceiling.
    set.records = vec![compact_cost_record(&f, 31, 1)];
    assert!(
        postcard::from_bytes::<CreatureOwnershipSetV1>(&postcard::to_allocvec(&set).unwrap())
            .is_err()
    );
}

#[test]
fn population_decode_limits_air_brick_metadata_across_records() {
    let f = Fixture::new();
    let mut set = CreatureOwnershipSetV1 {
        schema: CREATURE_OWNER_SCHEMA,
        records: vec![
            compact_cost_record(&f, 2048, 0),
            compact_cost_record(&f, 2048, 0),
        ],
    };
    postcard::from_bytes::<CreatureOwnershipSetV1>(&postcard::to_allocvec(&set).unwrap()).unwrap();
    set.records[1] = compact_cost_record(&f, 2049, 0);
    for record in &set.records {
        postcard::from_bytes::<CreatureOwnershipRecordV1>(&postcard::to_allocvec(record).unwrap())
            .unwrap();
    }
    assert!(
        postcard::from_bytes::<CreatureOwnershipSetV1>(&postcard::to_allocvec(&set).unwrap())
            .is_err()
    );
}

#[test]
fn population_decode_rejects_unknown_map_keys_before_reading_their_values() {
    let f = Fixture::new();
    let set_prefix = "{\"schema\":1,\"records\":[],\"unexpected\":";
    let error = serde_json::from_str::<CreatureOwnershipSetV1>(set_prefix).unwrap_err();
    assert!(error.to_string().contains("unknown"), "{error}");

    let mut record_prefix = serde_json::to_string(&owner(&f)).unwrap();
    assert_eq!(record_prefix.pop(), Some('}'));
    record_prefix.push_str(",\"unexpected\":");
    let set_prefix = format!("{{\"schema\":1,\"records\":[{record_prefix}");
    let error = serde_json::from_str::<CreatureOwnershipSetV1>(&set_prefix).unwrap_err();
    assert!(error.to_string().contains("unknown"), "{error}");
}

#[test]
fn population_state_strings_enforce_byte_limits_before_semantic_validation() {
    let f = Fixture::new();
    let mut record = owner(&f);
    record.state.species_key = "s".repeat(MAX_CREATURE_SPECIES_KEY_BYTES);
    record.state.clip_name = format!("{}a", "é".repeat(127));
    assert_eq!(record.state.clip_name.len(), MAX_CREATURE_CLIP_NAME_BYTES);
    let mut set = CreatureOwnershipSetV1 {
        schema: CREATURE_OWNER_SCHEMA,
        records: vec![record],
    };
    postcard::from_bytes::<CreatureOwnershipSetV1>(&postcard::to_allocvec(&set).unwrap()).unwrap();
    for species in [true, false] {
        let mut oversized = set.clone();
        if species {
            oversized.records[0].state.species_key.push('s');
        } else {
            oversized.records[0].state.clip_name = "é".repeat(128);
        }
        let bytes = postcard::to_allocvec(&oversized).unwrap();
        assert!(postcard::from_bytes::<CreatureOwnershipSetV1>(&bytes).is_err());
        let json = serde_json::to_vec(&oversized).unwrap();
        assert!(serde_json::from_slice::<CreatureOwnershipSetV1>(&json).is_err());
    }
    let mut prefix = serde_json::to_string(&set.records.remove(0).state).unwrap();
    assert_eq!(prefix.pop(), Some('}'));
    prefix.push_str(",\"unexpected\":");
    let error = serde_json::from_str::<CreatureStateV1>(&prefix).unwrap_err();
    assert!(error.to_string().contains("unknown"), "{error}");
}

#[test]
fn population_nested_fixed_metadata_rejects_unknown_map_fields() {
    let f = Fixture::new();
    let original = serde_json::to_value(owner(&f)).unwrap();
    for path in [
        "/state/key",
        "/state/asset",
        "/state/source_bindings/0",
        "/parts/0/initial_local_pose",
        "/parts/0/frozen_local_pose",
        "/components/0/source_masks/0/cells/0",
    ] {
        let mut record = original.clone();
        record
            .pointer_mut(path)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unknown_extension".into(), serde_json::json!([1, 2, 3]));
        let value = serde_json::json!({"schema": CREATURE_OWNER_SCHEMA, "records": [record]});
        let Err(error) = serde_json::from_value::<CreatureOwnershipSetV1>(value) else {
            panic!("unknown map metadata was accepted at {path}");
        };
        assert!(error.to_string().contains("unknown"), "{path}: {error}");
    }
}

#[test]
fn population_decode_refuses_excess_source_binding_before_reading_its_body() {
    let f = Fixture::new();
    let mut record = serde_json::to_value(owner(&f)).unwrap();
    let binding = record["state"]["source_bindings"][0].clone();
    let mut bindings = vec![binding; MAX_CREATURE_SOURCE_PARTS];
    // This would fail with a type error if the over-limit item's body were read.
    bindings.push(serde_json::json!("body must not be decoded"));
    record["state"]["source_bindings"] = serde_json::Value::Array(bindings);
    let value = serde_json::json!({"schema": CREATURE_OWNER_SCHEMA, "records": [record]});
    let error = serde_json::from_str::<CreatureOwnershipSetV1>(&value.to_string()).unwrap_err();
    assert!(
        error.to_string().contains("source binding limit"),
        "{error}"
    );
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

fn motion_overlay_for_review(record: &CreatureOwnershipRecordV1) -> CreatureMotionOverlayV1 {
    CreatureMotionOverlayV1 {
        state_revision: record.state.state_revision,
        clip_mode: record.state.clip_mode,
        clip_phase_tick: record.state.clip_phase_tick,
        clip_subtick_remainder: record.state.clip_subtick_remainder,
        clip_epoch: record.state.clip_epoch,
        pose_epoch: record.state.pose_epoch,
        root_translation_m: record.state.root_translation_m,
        root_rotation_xyzw: record.state.root_rotation_xyzw,
        linear_velocity_m_s: record.state.linear_velocity_m_s,
        angular_velocity_rad_s: record.state.angular_velocity_rad_s,
        locomotion_tag: record.state.locomotion_tag,
        parts: record
            .parts
            .iter()
            .map(|p| CreaturePartMotionV1 {
                part_id: p.part_id,
                frozen_local_pose: p.frozen_local_pose,
            })
            .collect(),
        components: record
            .components
            .iter()
            .map(|c| CreatureComponentMotionV1 {
                owner_key: c.owner_key,
                serial: c.serial,
                translation_m: c.translation_m,
                rotation_xyzw: c.rotation_xyzw,
                linear_velocity_m_s: c.linear_velocity_m_s,
                angular_velocity_rad_s: c.angular_velocity_rad_s,
            })
            .collect(),
    }
}

fn moved_death_for_review(f: &Fixture) -> (CreatureOwnershipRecordV1, CreatureLifecycleCommitV1) {
    let mut before = owner(f);
    // A client may have a large phase in an old playback epoch.
    before.state.clip_phase_tick = 100;
    let mut moved = before.clone();
    moved.state.state_revision = 4;
    moved.state.pose_epoch = 7;
    moved.state.clip_epoch = 2;
    moved.state.clip_phase_tick = 3;
    moved.state.root_translation_m = [3.0, 4.0, 5.0];
    moved.state.linear_velocity_m_s = [1.0, 0.0, 0.0];
    moved.parts[0].frozen_local_pose.translation_m[0] = 0.125;
    moved.components[0].translation_m = moved.state.root_translation_m;
    moved.components[0].linear_velocity_m_s = moved.state.linear_velocity_m_s;
    moved.validate(&f.catalog()).unwrap();
    let mut after = moved.clone();
    after.state.lifecycle = CreatureLifecycleState::Corpse;
    after.state.clip_mode = CreatureClipMode::Frozen;
    after.state.state_revision += 1;
    after.state.pose_epoch += 1;
    after.state.clip_epoch += 1;
    after.state.root_translation_m[0] += 1.0;
    after.state.linear_velocity_m_s[0] = 2.0;
    after.components[0].translation_m = after.state.root_translation_m;
    after.components[0].linear_velocity_m_s = after.state.linear_velocity_m_s;
    after.validate(&f.catalog()).unwrap();
    let witness = CreatureDeathWitnessV1 {
        key: moved.state.key,
        source_generation: moved.source_generation,
        before_state_revision: moved.state.state_revision,
        before_topology_revision: moved.state.aggregate_topology_revision,
        before_pose_epoch: moved.state.pose_epoch,
        root_translation_m: after.state.root_translation_m,
        root_rotation_xyzw: after.state.root_rotation_xyzw,
        installed_linear_velocity_m_s: after.state.linear_velocity_m_s,
        installed_angular_velocity_rad_s: after.state.angular_velocity_rad_s,
        frozen_local_poses: after
            .parts
            .iter()
            .map(|p| (p.part_id, p.frozen_local_pose))
            .collect(),
    };
    let commit = CreatureLifecycleCommitV1 {
        schema: 1,
        change: CreatureLifecycleChangeV1 {
            schema: 1,
            transaction_id: spall_core::TransactionId::new(8).unwrap(),
            operation_ordinal: 0,
            expected_before_hash: moved.canonical_hash(),
            expected_after_hash: after.canonical_hash(),
            operation: CreatureLifecycleOperationV1::Death { after },
        },
        expected_before_topology_hash: before.topology_hash(),
        pre_motion: Some(motion_overlay_for_review(&moved)),
        death_witness: Some(witness),
    };
    (before, commit)
}

#[test]
fn coordinator_motion_refresh_accepts_epoch_reset_but_preserves_source_fence() {
    let f = Fixture::new();
    let (before, commit) = moved_death_for_review(&f);
    let result = commit
        .validate_and_reduce(Some(&before), context(8, 2, 101), &f.catalog())
        .unwrap();
    assert_eq!(
        result.record.canonical_hash(),
        commit.change.expected_after_hash
    );
    assert_eq!(result.record.parts[0].bricks, before.parts[0].bricks);
    assert_eq!(
        result.record.parts[0].source_volume_id,
        before.parts[0].source_volume_id
    );
    assert_ne!(
        result.record.topology_hash(),
        before.topology_hash(),
        "lifecycle is topology"
    );
    for case in 0..4 {
        let mut stale = before.clone();
        match case {
            0 => stale.parts[0].initial_local_pose.translation_m[0] = 0.25,
            1 => {
                stale.source_generation += 1;
                stale.parts[0].source_generation += 1;
                stale.parts[0].source_content_hash = stale.parts[0].canonical_source_content_hash();
            }
            2 => stale.state.pose_epoch = 8, // Snapshot is already newer than pre-death motion.
            3 => stale.state.clip_epoch = 3,
            _ => unreachable!(),
        }
        stale.validate(&f.catalog()).unwrap();
        let saved = stale.clone();
        assert!(
            commit
                .validate_and_reduce(Some(&stale), context(8, 2, 101), &f.catalog())
                .is_err(),
            "stale case {case}"
        );
        assert_eq!(stale, saved);
    }
}

#[test]
fn coordinator_outer_receipt_cannot_authorize_changed_death_evidence() {
    use spall_protocol::creature_owner::CreatureLifecycleCommitReplayReceiptV1;
    let f = Fixture::new();
    let (before, commit) = moved_death_for_review(&f);
    let applied = commit
        .validate_and_reduce(Some(&before), context(8, 2, 101), &f.catalog())
        .unwrap();
    let receipt = CreatureLifecycleCommitReplayReceiptV1 {
        transaction_id: commit.change.transaction_id,
        operation_ordinal: 0,
        commit_hash: commit.canonical_hash(),
    };
    let duplicate = commit
        .validate_and_reduce_with_receipt(
            Some(&applied.record),
            context(8, 2, 101),
            &f.catalog(),
            Some(receipt),
        )
        .unwrap();
    assert_eq!(duplicate.outcome, CreatureReductionOutcome::Duplicate);
    for case in 0..3 {
        let mut changed = commit.clone();
        match case {
            0 => {
                changed
                    .death_witness
                    .as_mut()
                    .unwrap()
                    .installed_linear_velocity_m_s[0] += 1.0
            }
            1 => {
                changed.pre_motion.as_mut().unwrap().parts[0]
                    .frozen_local_pose
                    .translation_m[0] += 0.25
            }
            2 => changed.change.operation_ordinal = 1,
            _ => unreachable!(),
        }
        assert_ne!(changed.canonical_hash(), receipt.commit_hash);
        assert!(
            changed
                .validate_and_reduce_with_receipt(
                    Some(&applied.record),
                    context(8, 2, 101),
                    &f.catalog(),
                    Some(receipt)
                )
                .is_err(),
            "receipt case {case}"
        );
    }
}

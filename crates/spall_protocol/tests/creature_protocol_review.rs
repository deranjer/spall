//! Independent public DTO reference, identity and decode-boundary review.
use spall_core::{
    CreatureGeneration, CreatureId, CreatureKey, MaterialDef, MaterialFlags, MaterialId,
    MaterialManifest, RenderProps, SimProps, VolumeId, WorldEpoch, WorldId,
};
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
            schema: WIRE_SCHEMA_VERSION,
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

#[test]
fn known_species_cannot_select_another_known_species_asset() {
    let mut fixture = Fixture::new();
    let mut state = fixture.state();
    let bytes = state.encode(&fixture.catalog()).unwrap();
    assert_eq!(
        CreatureStateV1::decode(&bytes, &fixture.catalog()).unwrap(),
        state
    );
    assert_eq!(
        CreatureStateV1::decode(&bytes, &fixture.catalog())
            .unwrap()
            .encode(&fixture.catalog())
            .unwrap(),
        bytes
    );
    state.asset = asset(5);
    assert_eq!(
        state.validate(&fixture.catalog()),
        Err(CreatureStateError::Reference)
    );
    state.species_key = "trout".into();
    assert!(state.validate(&fixture.catalog()).is_ok());
    fixture.species[1].key = "deer";
    state.species_key = "deer".into();
    assert!(
        state.validate(&fixture.catalog()).is_err(),
        "conflicting species definitions accepted"
    );
    state.asset = asset(3);
    assert!(state.validate(&fixture.catalog()).is_err());
}

#[test]
fn hostile_vector_count_is_rejected_by_bounded_deserialization() {
    let fixture = Fixture::new();
    let mut state = fixture.state();
    state.source_bindings = vec![state.source_bindings[0]; 5000];
    let bytes = postcard::to_stdvec(&state).unwrap();
    assert!(bytes.len() < MAX_CREATURE_STATE_BYTES);
    // Distinguish the early count visitor from later duplicate/reference checks.
    let error = CreatureStateV1::decode(&bytes, &fixture.catalog()).unwrap_err();
    assert!(
        matches!(error, CreatureStateError::Encoding(_)),
        "late validation instead of early bounded decode: {error:?}"
    );
}

#[test]
fn frozen_corpse_and_generation_contract_are_enforced() {
    let fixture = Fixture::new();
    let mut state = fixture.state();
    state.lifecycle = CreatureLifecycleState::Corpse;
    assert!(
        state.validate(&fixture.catalog()).is_err(),
        "corpse animation was accepted"
    );
    state.clip_mode = CreatureClipMode::Frozen;
    assert!(state.validate(&fixture.catalog()).is_ok());
    state.key.generation = CreatureGeneration::new(2).unwrap();
    assert!(
        state.validate(&fixture.catalog()).is_err(),
        "unsupported V1 reincarnation accepted"
    );
}

#[test]
fn public_key_constructor_rejects_deserialized_zero_epoch() {
    let zero: WorldEpoch = serde_json::from_str("0").unwrap();
    assert!(
        CreatureKey::new(
            WorldId::from_u128(900),
            zero,
            CreatureId::new(1).unwrap(),
            CreatureGeneration::INITIAL
        )
        .is_err()
    );
}

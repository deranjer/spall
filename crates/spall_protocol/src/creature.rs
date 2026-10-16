//! Bounded standalone creature state DTOs for the reviewed lifecycle seam.
//!
//! These values are versioned and canonical-round-trippable, but are not a
//! wire record, checkpoint section, journal event, or actor replication channel.
use serde::{Deserialize, Serialize};
use spall_core::{CreatureKey, MaterialId, MaterialManifest, VolumeId};
use thiserror::Error;

use crate::canonical::{CanonicalWriter, Hash32, content_manifest_hash};

pub const CREATURE_STATE_SCHEMA: u16 = 1;
pub const MAX_CREATURE_SPECIES_KEY_BYTES: usize = 63;
pub const MAX_CREATURE_CLIP_NAME_BYTES: usize = 255;
pub const MAX_CREATURE_SOURCE_PARTS: usize = 256;
pub const MAX_CREATURE_ASSET_PARTS: usize = 1024;
pub const MAX_CREATURE_ASSET_CLIPS: usize = 256;
pub const MAX_CREATURE_ASSET_MATERIAL_KEYS: usize = 256;
pub const MAX_CREATURE_CATALOG_ASSETS: usize = 256;
pub const MAX_CREATURE_CATALOG_SPECIES: usize = 256;
pub const MAX_CREATURE_STATE_BYTES: usize = 64 * 1024;
pub const CREATURE_MATERIAL_MAPPING_FORMAT: u16 = 1;
pub const MAX_CREATURE_POSITION_M: f64 = 1.0e9;
pub const MAX_CREATURE_LINEAR_VELOCITY_M_S: f32 = 1.0e4;
pub const MAX_CREATURE_ANGULAR_VELOCITY_RAD_S: f32 = 1.0e4;
const QUATERNION_NORM_TOLERANCE: f32 = 1.0e-4;

/// Exact content identity of a parsed SPVX source asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatureAssetIdentity {
    pub uuid: [u8; 16],
    pub logical_hash: Hash32,
}

/// One source part's logical owner binding. This is metadata only; it does not
/// duplicate or serialize per-cell topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreaturePartBinding {
    pub part_id: u32,
    pub source_volume_id: VolumeId,
    pub source_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum CreatureLifecycleState {
    Alive = 1,
    Corpse = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum CreatureClipMode {
    Playing = 1,
    Frozen = 2,
}

/// Versioned bounded actor state. Runtime ECS, physics, and asset handles are
/// intentionally absent. Source bindings are sorted metadata, not a topology
/// store. Durable and wire acceptance remain separate work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatureStateV1 {
    pub schema: u16,
    pub key: CreatureKey,
    pub state_revision: u64,
    pub lifecycle: CreatureLifecycleState,
    pub species_key: String,
    pub species_version: u32,
    pub asset: CreatureAssetIdentity,
    pub material_manifest_hash: Hash32,
    pub material_mapping_version: u32,
    pub material_mapping_hash: Hash32,
    pub clip_name: String,
    pub clip_mode: CreatureClipMode,
    pub clip_phase_tick: u64,
    pub clip_subtick_remainder: u32,
    pub clip_epoch: u64,
    pub source_bindings: Vec<CreaturePartBinding>,
    pub aggregate_topology_revision: u64,
    pub pose_epoch: u64,
    pub root_translation_m: [f64; 3],
    pub root_rotation_xyzw: [f32; 4],
    pub linear_velocity_m_s: [f32; 3],
    pub angular_velocity_rad_s: [f32; 3],
    pub locomotion_tag: u8,
}

/// Read-only resolver inputs supplied by the game/asset owner. The protocol
/// crate owns neither these assets nor a competing species/material catalogue.
#[derive(Debug, Clone, Copy)]
pub struct CreatureAssetCatalogEntry<'a> {
    pub uuid: [u8; 16],
    pub logical_hash: Hash32,
    pub part_ids: &'a [u32],
    pub clip_names: &'a [&'a str],
    pub portable_material_keys: &'a [&'a str],
}

#[derive(Debug, Clone, Copy)]
pub struct CreatureSpeciesCatalogEntry<'a> {
    pub key: &'a str,
    pub version: u32,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct CreatureMaterialMappingEntry<'a> {
    pub portable_key: &'a str,
    pub material_id: MaterialId,
}

/// Immutable catalog references against which untrusted actor DTO references
/// are resolved. All mappings are checked against the validated world manifest.
pub struct CreatureValidationCatalog<'a> {
    pub assets: &'a [CreatureAssetCatalogEntry<'a>],
    pub species: &'a [CreatureSpeciesCatalogEntry<'a>],
    pub materials: &'a MaterialManifest,
    pub material_mapping_version: u32,
    pub material_mapping_hash: Hash32,
    pub material_mapping: &'a [CreatureMaterialMappingEntry<'a>],
}

/// Canonical hash for the game-owned portable-key to manifest-ID map. The
/// ordering and fixed-width encoding follow the existing canonical writer.
pub fn creature_material_mapping_hash(
    mapping: &[CreatureMaterialMappingEntry<'_>],
) -> Result<Hash32, CreatureStateError> {
    if mapping.is_empty() || mapping.len() > spall_core::MAX_MATERIALS {
        return Err(CreatureStateError::Limit);
    }
    let mut sorted = mapping.iter().collect::<Vec<_>>();
    sorted.sort_unstable_by(|left, right| {
        left.portable_key
            .as_bytes()
            .cmp(right.portable_key.as_bytes())
    });
    if sorted
        .windows(2)
        .any(|pair| pair[0].portable_key == pair[1].portable_key)
        || sorted.iter().any(|entry| {
            entry.portable_key.is_empty()
                || entry.portable_key.len() > 255
                || !entry.portable_key.is_ascii()
        })
    {
        return Err(CreatureStateError::Reference);
    }
    let mut writer = CanonicalWriter::with_domain(b"spall.creature-material-mapping.v1");
    writer
        .u16(CREATURE_MATERIAL_MAPPING_FORMAT)
        .seq(sorted.len());
    for entry in sorted {
        writer
            .blob(entry.portable_key.as_bytes())
            .u16(entry.material_id.0);
    }
    Ok(writer.hash())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CreatureStateError {
    #[error("unsupported creature state schema")]
    Schema,
    #[error("creature DTO exceeds its byte, count, or numeric bound")]
    Limit,
    #[error("creature identity, revision, or source binding is invalid")]
    Identity,
    #[error("creature species, asset, clip, or material reference is unresolved")]
    Reference,
    #[error("creature quaternion is not normalized")]
    Quaternion,
    #[error("creature DTO is malformed: {0}")]
    Encoding(String),
}

impl CreatureStateV1 {
    pub fn validate(
        &self,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<(), CreatureStateError> {
        if self.schema != CREATURE_STATE_SCHEMA {
            return Err(CreatureStateError::Schema);
        }
        self.key
            .validate()
            .map_err(|_| CreatureStateError::Identity)?;
        if self.state_revision == 0
            || self.species_version == 0
            || self.material_mapping_version == 0
            || self.clip_epoch == 0
            || self.aggregate_topology_revision == 0
            || self.pose_epoch == 0
            || self.locomotion_tag > 31
            || self.species_key.is_empty()
            || self.species_key.len() > MAX_CREATURE_SPECIES_KEY_BYTES
            || !self.species_key.bytes().all(is_species_key_byte)
            || self.clip_name.is_empty()
            || self.clip_name.len() > MAX_CREATURE_CLIP_NAME_BYTES
            || self.clip_name.chars().any(char::is_control)
            || self.clip_subtick_remainder != 0
            || self.source_bindings.is_empty()
            || self.source_bindings.len() > MAX_CREATURE_SOURCE_PARTS
        {
            return Err(CreatureStateError::Limit);
        }
        if self
            .source_bindings
            .windows(2)
            .any(|pair| pair[0].part_id >= pair[1].part_id)
            || self
                .source_bindings
                .iter()
                .any(|binding| binding.part_id == 0 || binding.source_revision == 0)
        {
            return Err(CreatureStateError::Identity);
        }
        let mut volume_ids = std::collections::BTreeSet::new();
        if self
            .source_bindings
            .iter()
            .any(|binding| !volume_ids.insert(binding.source_volume_id))
        {
            return Err(CreatureStateError::Identity);
        }
        if self
            .root_translation_m
            .iter()
            .any(|value| !value.is_finite() || value.abs() > MAX_CREATURE_POSITION_M)
            || self
                .linear_velocity_m_s
                .iter()
                .any(|value| !value.is_finite() || value.abs() > MAX_CREATURE_LINEAR_VELOCITY_M_S)
            || self.angular_velocity_rad_s.iter().any(|value| {
                !value.is_finite() || value.abs() > MAX_CREATURE_ANGULAR_VELOCITY_RAD_S
            })
        {
            return Err(CreatureStateError::Limit);
        }
        let [x, y, z, w] = self.root_rotation_xyzw;
        if !self
            .root_rotation_xyzw
            .iter()
            .all(|value| value.is_finite())
        {
            return Err(CreatureStateError::Quaternion);
        }
        let norm = x.mul_add(x, y.mul_add(y, z.mul_add(z, w * w))).sqrt();
        if !norm.is_finite() || (norm - 1.0).abs() > QUATERNION_NORM_TOLERANCE {
            return Err(CreatureStateError::Quaternion);
        }

        if catalog.assets.is_empty()
            || catalog.assets.len() > MAX_CREATURE_CATALOG_ASSETS
            || catalog.species.is_empty()
            || catalog.species.len() > MAX_CREATURE_CATALOG_SPECIES
            || catalog.material_mapping.is_empty()
            || catalog.material_mapping.len() > spall_core::MAX_MATERIALS
            || catalog.material_mapping_version != self.material_mapping_version
            || catalog.material_mapping_hash != self.material_mapping_hash
            || creature_material_mapping_hash(catalog.material_mapping)?
                != self.material_mapping_hash
            || content_manifest_hash(catalog.materials) != self.material_manifest_hash
        {
            return Err(CreatureStateError::Reference);
        }
        let species_matches = catalog
            .species
            .iter()
            .filter(|entry| {
                entry.key == self.species_key
                    && entry.version == self.species_version
                    && entry.enabled
            })
            .count();
        if species_matches != 1 {
            return Err(CreatureStateError::Reference);
        }
        let mut asset_matches = catalog.assets.iter().filter(|entry| {
            entry.uuid == self.asset.uuid && entry.logical_hash == self.asset.logical_hash
        });
        let Some(asset) = asset_matches.next() else {
            return Err(CreatureStateError::Reference);
        };
        if asset_matches.next().is_some() {
            return Err(CreatureStateError::Reference);
        }
        if asset.uuid == [0; 16]
            || asset.part_ids.is_empty()
            || asset.part_ids.len() > MAX_CREATURE_ASSET_PARTS
            || asset.clip_names.len() > MAX_CREATURE_ASSET_CLIPS
            || asset.portable_material_keys.is_empty()
            || asset.portable_material_keys.len() > MAX_CREATURE_ASSET_MATERIAL_KEYS
            || asset
                .clip_names
                .iter()
                .filter(|clip| **clip == self.clip_name)
                .count()
                != 1
        {
            return Err(CreatureStateError::Reference);
        }
        if asset.part_ids.contains(&0)
            || asset
                .part_ids
                .iter()
                .enumerate()
                .any(|(index, id)| asset.part_ids[..index].contains(id))
            || self.source_bindings.len() != asset.part_ids.len()
            || self
                .source_bindings
                .iter()
                .any(|binding| !asset.part_ids.contains(&binding.part_id))
        {
            return Err(CreatureStateError::Reference);
        }
        let mut mapping = std::collections::BTreeMap::new();
        for entry in catalog.material_mapping {
            if entry.portable_key.is_empty()
                || entry.portable_key.len() > 255
                || !entry.portable_key.is_ascii()
                || !catalog.materials.contains(entry.material_id)
                || mapping
                    .insert(entry.portable_key, entry.material_id)
                    .is_some()
            {
                return Err(CreatureStateError::Reference);
            }
        }
        let mut portable_keys = std::collections::BTreeSet::new();
        if asset.portable_material_keys.iter().any(|key| {
            key.is_empty()
                || key.len() > 255
                || !key.is_ascii()
                || !portable_keys.insert(*key)
                || !mapping.contains_key(key)
        }) {
            return Err(CreatureStateError::Reference);
        }
        Ok(())
    }

    pub fn encode(
        &self,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<Vec<u8>, CreatureStateError> {
        self.validate(catalog)?;
        let bytes = postcard::to_stdvec(self)
            .map_err(|error| CreatureStateError::Encoding(error.to_string()))?;
        if bytes.len() > MAX_CREATURE_STATE_BYTES {
            return Err(CreatureStateError::Limit);
        }
        Ok(bytes)
    }

    pub fn decode(
        bytes: &[u8],
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<Self, CreatureStateError> {
        if bytes.len() > MAX_CREATURE_STATE_BYTES {
            return Err(CreatureStateError::Limit);
        }
        let (record, rest): (Self, &[u8]) = postcard::take_from_bytes(bytes)
            .map_err(|error| CreatureStateError::Encoding(error.to_string()))?;
        if !rest.is_empty() {
            return Err(CreatureStateError::Encoding("trailing bytes".into()));
        }
        record.validate(catalog)?;
        Ok(record)
    }
}

fn is_species_key_byte(value: u8) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, b'_' | b'-' | b'.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{
        CreatureGeneration, CreatureId, CreatureKey, MaterialDef, MaterialFlags, RenderProps,
        SimProps, WorldEpoch, WorldId,
    };

    fn material_manifest() -> MaterialManifest {
        MaterialManifest::validated(vec![MaterialDef {
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
        .unwrap()
    }

    fn catalog<'a>(
        materials: &'a MaterialManifest,
        asset: &'a [CreatureAssetCatalogEntry<'a>],
        species: &'a [CreatureSpeciesCatalogEntry<'a>],
        mapping: &'a [CreatureMaterialMappingEntry<'a>],
        mapping_hash: Hash32,
    ) -> CreatureValidationCatalog<'a> {
        CreatureValidationCatalog {
            assets: asset,
            species,
            materials,
            material_mapping_version: 1,
            material_mapping_hash: mapping_hash,
            material_mapping: mapping,
        }
    }

    fn valid_state() -> CreatureStateV1 {
        CreatureStateV1 {
            schema: CREATURE_STATE_SCHEMA,
            key: CreatureKey::new(
                WorldId::from_u128(17),
                WorldEpoch::INITIAL,
                CreatureId::new(1).unwrap(),
                CreatureGeneration::INITIAL,
            )
            .unwrap(),
            state_revision: 1,
            lifecycle: CreatureLifecycleState::Alive,
            species_key: "deer".into(),
            species_version: 1,
            asset: CreatureAssetIdentity {
                uuid: [3; 16],
                logical_hash: Hash32([4; 32]),
            },
            material_manifest_hash: Hash32([0; 32]),
            material_mapping_version: 1,
            material_mapping_hash: Hash32::ZERO,
            clip_name: "walk".into(),
            clip_mode: CreatureClipMode::Playing,
            clip_phase_tick: 7,
            clip_subtick_remainder: 0,
            clip_epoch: 1,
            source_bindings: vec![CreaturePartBinding {
                part_id: 1,
                source_volume_id: VolumeId::new(88).unwrap(),
                source_revision: 1,
            }],
            aggregate_topology_revision: 1,
            pose_epoch: 1,
            root_translation_m: [1.0, 2.0, 3.0],
            root_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            locomotion_tag: 0,
        }
    }

    #[test]
    fn standalone_state_canonical_round_trip_resolves_exact_catalog_references() {
        let materials = material_manifest();
        let asset = [CreatureAssetCatalogEntry {
            uuid: [3; 16],
            logical_hash: Hash32([4; 32]),
            part_ids: &[1],
            clip_names: &["walk"],
            portable_material_keys: &["flesh"],
        }];
        let species = [CreatureSpeciesCatalogEntry {
            key: "deer",
            version: 1,
            enabled: true,
        }];
        let mapping = [CreatureMaterialMappingEntry {
            portable_key: "flesh",
            material_id: MaterialId(1),
        }];
        let mapping_hash = creature_material_mapping_hash(&mapping).unwrap();
        let catalog = catalog(&materials, &asset, &species, &mapping, mapping_hash);
        let mut state = valid_state();
        state.material_manifest_hash = content_manifest_hash(&materials);
        state.material_mapping_hash = mapping_hash;
        let bytes = state.encode(&catalog).unwrap();
        assert!(bytes.len() <= MAX_CREATURE_STATE_BYTES);
        assert_eq!(CreatureStateV1::decode(&bytes, &catalog).unwrap(), state);
        assert_eq!(state.encode(&catalog).unwrap(), bytes);
    }

    #[test]
    fn state_rejects_duplicate_foreign_or_unknown_bindings_before_use() {
        let materials = material_manifest();
        let asset = [CreatureAssetCatalogEntry {
            uuid: [3; 16],
            logical_hash: Hash32([4; 32]),
            part_ids: &[1, 2],
            clip_names: &["walk"],
            portable_material_keys: &["flesh"],
        }];
        let species = [CreatureSpeciesCatalogEntry {
            key: "deer",
            version: 1,
            enabled: true,
        }];
        let mapping = [CreatureMaterialMappingEntry {
            portable_key: "flesh",
            material_id: MaterialId(1),
        }];
        let mapping_hash = creature_material_mapping_hash(&mapping).unwrap();
        let catalog = catalog(&materials, &asset, &species, &mapping, mapping_hash);
        let mut state = valid_state();
        state.material_manifest_hash = content_manifest_hash(&materials);
        state.material_mapping_hash = mapping_hash;
        state.source_bindings.push(CreaturePartBinding {
            part_id: 1,
            source_volume_id: VolumeId::new(89).unwrap(),
            source_revision: 1,
        });
        assert_eq!(state.validate(&catalog), Err(CreatureStateError::Identity));
        state.source_bindings[1].part_id = 2;
        state.source_bindings[1].source_volume_id = VolumeId::new(88).unwrap();
        assert_eq!(state.validate(&catalog), Err(CreatureStateError::Identity));
        state.source_bindings[1].source_volume_id = VolumeId::new(89).unwrap();
        state.source_bindings[1].part_id = 9;
        assert_eq!(state.validate(&catalog), Err(CreatureStateError::Reference));
    }

    #[test]
    fn state_rejects_hash_clip_species_numeric_and_byte_limit_mismatches() {
        let materials = material_manifest();
        let asset = [CreatureAssetCatalogEntry {
            uuid: [3; 16],
            logical_hash: Hash32([4; 32]),
            part_ids: &[1],
            clip_names: &["walk"],
            portable_material_keys: &["flesh"],
        }];
        let species = [CreatureSpeciesCatalogEntry {
            key: "deer",
            version: 1,
            enabled: true,
        }];
        let mapping = [CreatureMaterialMappingEntry {
            portable_key: "flesh",
            material_id: MaterialId(1),
        }];
        let mapping_hash = creature_material_mapping_hash(&mapping).unwrap();
        let catalog = catalog(&materials, &asset, &species, &mapping, mapping_hash);
        let mut state = valid_state();
        state.material_manifest_hash = content_manifest_hash(&materials);
        state.material_mapping_hash = mapping_hash;
        let mut bad = state.clone();
        bad.asset.logical_hash = Hash32([9; 32]);
        assert_eq!(bad.validate(&catalog), Err(CreatureStateError::Reference));
        bad = state.clone();
        bad.clip_name = "missing".into();
        assert_eq!(bad.validate(&catalog), Err(CreatureStateError::Reference));
        bad = state.clone();
        bad.species_key = "unknown".into();
        assert_eq!(bad.validate(&catalog), Err(CreatureStateError::Reference));
        bad = state.clone();
        bad.root_translation_m[0] = f64::INFINITY;
        assert_eq!(bad.validate(&catalog), Err(CreatureStateError::Limit));
        assert_eq!(
            CreatureStateV1::decode(&vec![0; MAX_CREATURE_STATE_BYTES + 1], &catalog),
            Err(CreatureStateError::Limit)
        );
    }
}

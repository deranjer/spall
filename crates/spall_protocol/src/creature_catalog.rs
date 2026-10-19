//! Owned, game-supplied creature catalog.
//!
//! [`CreatureValidationCatalog`] borrows its slices, which suits one-shot
//! validation but not a long-lived replica or server that must keep the catalog
//! between transactions. The game builds one [`OwnedCreatureCatalog`] at startup
//! and both peers lend a borrowed view per call. The protocol crate still owns no
//! species or assets; it only stores what the game gave it.

use spall_core::{MaterialId, MaterialManifest};

use crate::{
    CreatureAssetCatalogEntry, CreatureAssetIdentity, CreatureMaterialMappingEntry,
    CreatureSpeciesCatalogEntry, CreatureStateError, CreatureValidationCatalog, Hash32,
    creature_material_mapping_hash,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedCreatureAsset {
    pub uuid: [u8; 16],
    pub logical_hash: Hash32,
    pub part_ids: Vec<u32>,
    pub clip_names: Vec<String>,
    pub portable_material_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedCreatureSpecies {
    pub key: String,
    pub version: u32,
    pub enabled: bool,
    pub asset: CreatureAssetIdentity,
}

#[derive(Debug, Clone)]
pub struct OwnedCreatureCatalog {
    pub assets: Vec<OwnedCreatureAsset>,
    pub species: Vec<OwnedCreatureSpecies>,
    pub materials: MaterialManifest,
    pub material_mapping_version: u32,
    pub material_mapping: Vec<(String, MaterialId)>,
}

impl OwnedCreatureCatalog {
    /// Lends a borrowed validation view for the duration of `f`.
    pub fn with_view<R>(
        &self,
        f: impl FnOnce(&CreatureValidationCatalog<'_>) -> R,
    ) -> Result<R, CreatureStateError> {
        let clip_names: Vec<Vec<&str>> = self
            .assets
            .iter()
            .map(|asset| asset.clip_names.iter().map(String::as_str).collect())
            .collect();
        let material_keys: Vec<Vec<&str>> = self
            .assets
            .iter()
            .map(|asset| {
                asset
                    .portable_material_keys
                    .iter()
                    .map(String::as_str)
                    .collect()
            })
            .collect();
        let assets: Vec<CreatureAssetCatalogEntry<'_>> = self
            .assets
            .iter()
            .enumerate()
            .map(|(index, asset)| CreatureAssetCatalogEntry {
                uuid: asset.uuid,
                logical_hash: asset.logical_hash,
                part_ids: &asset.part_ids,
                clip_names: &clip_names[index],
                portable_material_keys: &material_keys[index],
            })
            .collect();
        let species: Vec<CreatureSpeciesCatalogEntry<'_>> = self
            .species
            .iter()
            .map(|species| CreatureSpeciesCatalogEntry {
                key: &species.key,
                version: species.version,
                enabled: species.enabled,
                asset: species.asset,
            })
            .collect();
        let mapping: Vec<CreatureMaterialMappingEntry<'_>> = self
            .material_mapping
            .iter()
            .map(|(key, id)| CreatureMaterialMappingEntry {
                portable_key: key,
                material_id: *id,
            })
            .collect();
        let view = CreatureValidationCatalog {
            assets: &assets,
            species: &species,
            materials: &self.materials,
            material_mapping_version: self.material_mapping_version,
            material_mapping_hash: creature_material_mapping_hash(&mapping)?,
            material_mapping: &mapping,
        };
        Ok(f(&view))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{MaterialDef, MaterialFlags, RenderProps, SimProps};

    #[test]
    fn view_borrows_exactly_the_owned_entries() {
        let catalog = OwnedCreatureCatalog {
            assets: vec![OwnedCreatureAsset {
                uuid: [1; 16],
                logical_hash: Hash32([2; 32]),
                part_ids: vec![1, 2],
                clip_names: vec!["idle".into(), "walk".into()],
                portable_material_keys: vec!["fur".into()],
            }],
            species: vec![OwnedCreatureSpecies {
                key: "deer".into(),
                version: 1,
                enabled: true,
                asset: CreatureAssetIdentity {
                    uuid: [1; 16],
                    logical_hash: Hash32([2; 32]),
                },
            }],
            materials: MaterialManifest::validated(vec![MaterialDef {
                id: MaterialId(1),
                name: "fur".into(),
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
            material_mapping_version: 1,
            material_mapping: vec![("fur".into(), MaterialId(1))],
        };
        catalog
            .with_view(|view| {
                assert_eq!(view.assets[0].part_ids, &[1, 2]);
                assert_eq!(view.assets[0].clip_names, &["idle", "walk"]);
                assert_eq!(view.species[0].key, "deer");
                assert_eq!(view.material_mapping[0].portable_key, "fur");
            })
            .unwrap();
    }
}

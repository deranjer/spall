//! Material identity, per-material properties, and the world material manifest.
//!
//! `MaterialId` is a `u16` with `0` reserved for air. Numeric ids are fixed in
//! the world manifest and are never assigned by file-enumeration order. A
//! manifest is validated once at load / handshake; unknown ids and
//! out-of-range fields are hard errors, not silently clamped.

use serde::{Deserialize, Serialize};

/// A material id. `MaterialId::AIR` (`0`) is always empty space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MaterialId(pub u16);

impl MaterialId {
    /// Reserved empty-space id.
    pub const AIR: Self = Self(0);

    #[inline]
    pub const fn is_air(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }
}

/// Collision / opacity / structural bits for a material. Hand-rolled `u32`
/// flags to avoid a dependency; unknown bits are rejected by the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialFlags(pub u32);

impl MaterialFlags {
    pub const NONE: Self = Self(0);
    /// Blocks light for the renderer / lighting occupancy.
    pub const OPAQUE: Self = Self(1 << 0);
    /// Participates in physics collision.
    pub const COLLIDES: Self = Self(1 << 1);
    /// Participates in the structural support graph.
    pub const STRUCTURAL: Self = Self(1 << 2);

    const ALL_BITS: u32 = 0b111;

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    const fn has_unknown_bits(self) -> bool {
        self.0 & !Self::ALL_BITS != 0
    }
}

/// Fields consumed only by the renderer. Colours are linear (converted from
/// sRGB at import), not gamma-encoded.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RenderProps {
    /// Linear RGB reflectance, each channel `0..=1`.
    pub albedo: [f32; 3],
    /// Perceptual roughness, `0..=1`.
    pub roughness: f32,
    /// Metalness, `0..=1`.
    pub metalness: f32,
    /// Emitted radiance in linear RGB, each channel `>= 0`.
    pub emissive: [f32; 3],
}

/// Fields consumed by simulation: physics and the structural model.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SimProps {
    /// Density in kg/m³, `> 0` for any non-air material.
    pub density_kg_m3: f32,
    /// Coulomb friction coefficient, `>= 0`.
    pub friction: f32,
    /// Restitution, `0..=1`.
    pub restitution: f32,
    /// Abstract hardness score used by tools, `>= 0`.
    pub hardness: f32,
    /// Abstract bond strength used by the structural model, `>= 0`.
    pub bond_strength: f32,
    pub flags: MaterialFlags,
}

/// One material entry: a stable string name bound to a fixed numeric id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterialDef {
    pub id: MaterialId,
    pub name: String,
    pub render: RenderProps,
    pub sim: SimProps,
}

/// Why a manifest failed validation. Every variant is actionable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest is empty")]
    Empty,
    #[error("manifest has {count} entries; limit is {limit}")]
    TooManyEntries { count: usize, limit: usize },
    #[error("duplicate material id {0}")]
    DuplicateId(u16),
    #[error("duplicate material name {0:?}")]
    DuplicateName(String),
    #[error("material id {0} has an empty name")]
    EmptyName(u16),
    #[error("id 0 is reserved for air but is named {0:?}")]
    AirMisnamed(String),
    #[error("material {name:?} (id {id}) field {field} is out of range or not finite")]
    FieldOutOfRange {
        id: u16,
        name: String,
        field: &'static str,
    },
    #[error("material {name:?} (id {id}) sets unknown flag bits")]
    UnknownFlags { id: u16, name: String },
    #[error("entries are not sorted by ascending id (id {0} follows a larger id)")]
    NotSorted(u16),
}

/// Upper bound on manifest size. Keeps handshake parsing bounded.
pub const MAX_MATERIALS: usize = 4096;

/// A validated set of materials for one world. Construct with
/// [`MaterialManifest::validated`]; the constructor is the only way in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterialManifest {
    entries: Vec<MaterialDef>,
}

impl MaterialManifest {
    /// Validates `entries` and takes ownership. Requires: non-empty, at most
    /// [`MAX_MATERIALS`], ascending unique ids, unique non-empty names, id `0`
    /// (if present) named `"air"`, and all numeric fields finite and in range.
    pub fn validated(entries: Vec<MaterialDef>) -> Result<Self, ManifestError> {
        if entries.is_empty() {
            return Err(ManifestError::Empty);
        }
        if entries.len() > MAX_MATERIALS {
            return Err(ManifestError::TooManyEntries {
                count: entries.len(),
                limit: MAX_MATERIALS,
            });
        }

        let mut last_id: Option<u16> = None;
        let mut seen_names: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for def in &entries {
            let id = def.id.0;

            if let Some(prev) = last_id {
                if id == prev {
                    return Err(ManifestError::DuplicateId(id));
                }
                if id < prev {
                    return Err(ManifestError::NotSorted(id));
                }
            }
            last_id = Some(id);

            if def.name.is_empty() {
                return Err(ManifestError::EmptyName(id));
            }
            if id == 0 && def.name != "air" {
                return Err(ManifestError::AirMisnamed(def.name.clone()));
            }
            if !seen_names.insert(def.name.as_str()) {
                return Err(ManifestError::DuplicateName(def.name.clone()));
            }

            validate_fields(def)?;
        }

        Ok(Self { entries })
    }

    /// Entries in canonical order (ascending id).
    pub fn entries(&self) -> &[MaterialDef] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Looks up a material by id. `None` for any id not in the manifest —
    /// callers must treat that as an error, never as air.
    pub fn get(&self, id: MaterialId) -> Option<&MaterialDef> {
        self.entries
            .binary_search_by_key(&id.0, |def| def.id.0)
            .ok()
            .map(|index| &self.entries[index])
    }

    /// True if `id` is defined by this manifest.
    pub fn contains(&self, id: MaterialId) -> bool {
        self.get(id).is_some()
    }

    /// Deterministic little-endian serialization of the manifest, used as the
    /// pre-image for the content manifest hash. Field order and encoding are
    /// fixed here; `spall_protocol` hashes these bytes.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.entries.len() * 64);
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for def in &self.entries {
            out.extend_from_slice(&def.id.0.to_le_bytes());
            let name = def.name.as_bytes();
            out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out.extend_from_slice(name);
            for channel in def.render.albedo {
                out.extend_from_slice(&canonical_f32(channel).to_le_bytes());
            }
            out.extend_from_slice(&canonical_f32(def.render.roughness).to_le_bytes());
            out.extend_from_slice(&canonical_f32(def.render.metalness).to_le_bytes());
            for channel in def.render.emissive {
                out.extend_from_slice(&canonical_f32(channel).to_le_bytes());
            }
            out.extend_from_slice(&canonical_f32(def.sim.density_kg_m3).to_le_bytes());
            out.extend_from_slice(&canonical_f32(def.sim.friction).to_le_bytes());
            out.extend_from_slice(&canonical_f32(def.sim.restitution).to_le_bytes());
            out.extend_from_slice(&canonical_f32(def.sim.hardness).to_le_bytes());
            out.extend_from_slice(&canonical_f32(def.sim.bond_strength).to_le_bytes());
            out.extend_from_slice(&def.sim.flags.0.to_le_bytes());
        }
        out
    }
}

/// Normalizes a finite `f32` so `-0.0` and `0.0` encode identically. Non-finite
/// values never reach this: the manifest is validated first.
#[inline]
fn canonical_f32(value: f32) -> f32 {
    if value == 0.0 { 0.0 } else { value }
}

fn validate_fields(def: &MaterialDef) -> Result<(), ManifestError> {
    let id = def.id.0;
    let name = || def.name.clone();
    let bad = |field: &'static str| ManifestError::FieldOutOfRange {
        id,
        name: name(),
        field,
    };

    let unit = |v: f32| v.is_finite() && (0.0..=1.0).contains(&v);
    let nonneg = |v: f32| v.is_finite() && v >= 0.0;

    for (channel, field) in def
        .render
        .albedo
        .iter()
        .zip(["albedo.r", "albedo.g", "albedo.b"])
    {
        if !unit(*channel) {
            return Err(bad(field));
        }
    }
    if !unit(def.render.roughness) {
        return Err(bad("roughness"));
    }
    if !unit(def.render.metalness) {
        return Err(bad("metalness"));
    }
    for (channel, field) in
        def.render
            .emissive
            .iter()
            .zip(["emissive.r", "emissive.g", "emissive.b"])
    {
        if !nonneg(*channel) {
            return Err(bad(field));
        }
    }

    if !nonneg(def.sim.friction) {
        return Err(bad("friction"));
    }
    if !unit(def.sim.restitution) {
        return Err(bad("restitution"));
    }
    if !nonneg(def.sim.hardness) {
        return Err(bad("hardness"));
    }
    if !nonneg(def.sim.bond_strength) {
        return Err(bad("bond_strength"));
    }

    // Air has no density; every other material must have positive density.
    if def.id.is_air() {
        if def.sim.density_kg_m3 != 0.0 {
            return Err(bad("density_kg_m3"));
        }
    } else if !(def.sim.density_kg_m3.is_finite() && def.sim.density_kg_m3 > 0.0) {
        return Err(bad("density_kg_m3"));
    }

    if def.sim.flags.has_unknown_bits() {
        return Err(ManifestError::UnknownFlags { id, name: name() });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn air() -> MaterialDef {
        MaterialDef {
            id: MaterialId::AIR,
            name: "air".into(),
            render: RenderProps {
                albedo: [0.0, 0.0, 0.0],
                roughness: 1.0,
                metalness: 0.0,
                emissive: [0.0, 0.0, 0.0],
            },
            sim: SimProps {
                density_kg_m3: 0.0,
                friction: 0.0,
                restitution: 0.0,
                hardness: 0.0,
                bond_strength: 0.0,
                flags: MaterialFlags::NONE,
            },
        }
    }

    fn stone(id: u16, name: &str) -> MaterialDef {
        MaterialDef {
            id: MaterialId(id),
            name: name.into(),
            render: RenderProps {
                albedo: [0.5, 0.5, 0.52],
                roughness: 0.9,
                metalness: 0.0,
                emissive: [0.0, 0.0, 0.0],
            },
            sim: SimProps {
                density_kg_m3: 2600.0,
                friction: 0.8,
                restitution: 0.1,
                hardness: 4.0,
                bond_strength: 12.0,
                flags: MaterialFlags(
                    MaterialFlags::OPAQUE.0
                        | MaterialFlags::COLLIDES.0
                        | MaterialFlags::STRUCTURAL.0,
                ),
            },
        }
    }

    #[test]
    fn valid_manifest_round_trips_and_looks_up_by_id() {
        let manifest =
            MaterialManifest::validated(vec![air(), stone(1, "stone"), stone(7, "granite")])
                .unwrap();
        assert_eq!(manifest.len(), 3);
        assert_eq!(manifest.get(MaterialId(7)).unwrap().name, "granite");
        assert!(manifest.contains(MaterialId(1)));
        assert!(!manifest.contains(MaterialId(2)));
    }

    #[test]
    fn unknown_material_id_is_none_not_air() {
        let manifest = MaterialManifest::validated(vec![air(), stone(1, "stone")]).unwrap();
        assert!(manifest.get(MaterialId(999)).is_none());
    }

    #[test]
    fn rejects_duplicate_id_name_and_unsorted() {
        assert_eq!(
            MaterialManifest::validated(vec![air(), stone(1, "a"), stone(1, "b")]),
            Err(ManifestError::DuplicateId(1))
        );
        assert_eq!(
            MaterialManifest::validated(vec![air(), stone(1, "dup"), stone(2, "dup")]),
            Err(ManifestError::DuplicateName("dup".into()))
        );
        assert_eq!(
            MaterialManifest::validated(vec![air(), stone(5, "a"), stone(2, "b")]),
            Err(ManifestError::NotSorted(2))
        );
    }

    #[test]
    fn rejects_out_of_range_and_non_finite_fields() {
        let mut bad = stone(1, "bad");
        bad.render.roughness = 1.5;
        assert!(matches!(
            MaterialManifest::validated(vec![air(), bad]),
            Err(ManifestError::FieldOutOfRange {
                field: "roughness",
                ..
            })
        ));

        let mut nan = stone(1, "nan");
        nan.sim.density_kg_m3 = f32::NAN;
        assert!(matches!(
            MaterialManifest::validated(vec![air(), nan]),
            Err(ManifestError::FieldOutOfRange {
                field: "density_kg_m3",
                ..
            })
        ));

        let mut air_with_mass = air();
        air_with_mass.sim.density_kg_m3 = 1.0;
        assert!(matches!(
            MaterialManifest::validated(vec![air_with_mass]),
            Err(ManifestError::FieldOutOfRange {
                field: "density_kg_m3",
                ..
            })
        ));
    }

    #[test]
    fn rejects_unknown_flag_bits() {
        let mut weird = stone(1, "weird");
        weird.sim.flags = MaterialFlags(0x8000_0000);
        assert!(matches!(
            MaterialManifest::validated(vec![air(), weird]),
            Err(ManifestError::UnknownFlags { .. })
        ));
    }

    #[test]
    fn canonical_bytes_are_stable_and_normalize_negative_zero() {
        let a = MaterialManifest::validated(vec![air(), stone(1, "stone")]).unwrap();
        let mut b_air = air();
        b_air.render.albedo = [-0.0, -0.0, -0.0];
        let b = MaterialManifest::validated(vec![b_air, stone(1, "stone")]).unwrap();
        assert_eq!(a.canonical_bytes(), b.canonical_bytes());
        // First four bytes are the LE entry count.
        assert_eq!(&a.canonical_bytes()[..4], &2u32.to_le_bytes());
    }
}

//! Appearance variants: authored tint that survives destruction, replication and
//! reload.
//!
//! Runtime cells carry a `MaterialId` and nothing else, and that id is what the
//! whole engine already replicates, persists, splits into bodies and hashes. So
//! an authored tint is kept by making it a *material*: for each portable
//! material key (grass, foliage, wood, ...) the game manifest carries a small
//! palette of variants that share the base material's simulation properties and
//! differ only in colour. Importing an editor scene maps each tinted cell to the
//! nearest variant of its material; an untinted cell keeps the base material.
//!
//! Variant identity is **explicit and append-only**. `appearance_v3.rs` freezes
//! the variants of the first variant manifest (ids 100..=136, names, colours);
//! `appearance_extensions.rs`, written by
//! `cargo run -p spall_editor --example appearance_palette`, holds later ones
//! under ids from [`EXTENSION_ID_BASE`], and the generator never renumbers or
//! recolours a row it finds there. Extending the palette is therefore an
//! additive manifest extension (`MaterialManifest::superseding_extension`):
//! every saved world keeps its meaning and colours. Recolouring or remapping an
//! existing variant is not an extension; it needs a new versioned manifest
//! migration (`superseding_appearance` for colour-only changes).
//!
//! Game rules key on base materials (a wood variant still drops wood):
//! [`base_material`] and [`merge_variants`] fold variants back.

use std::collections::BTreeMap;

use spall_core::{MaterialDef, MaterialId, MaterialManifest};

use crate::appearance_extensions::EXTENSIONS;
use crate::appearance_v3::V3;
use crate::editor_scene::material_mapping;

/// The first variant's id (the frozen v3 variants are `100..=136`).
pub const VARIANT_ID_BASE: u16 = 100;
/// The first id the generator may allocate for a later variant. Everything
/// below is reserved for hand-assigned materials and the frozen v3 variants;
/// the lamp is 200, so an extension can never collide with it.
pub const EXTENSION_ID_BASE: u16 = 256;

/// One variant row: `(id, portable material key, name, authored sRGB tint)`.
pub type VariantRow = (u16, &'static str, &'static str, [u8; 3]);

/// One appearance variant of a base material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub id: MaterialId,
    pub base: MaterialId,
    pub name: String,
    /// The authored display colour, sRGB.
    pub srgb: [u8; 3],
}

/// The variants of `rows`. Keys with no game material are skipped.
pub fn variants_of(rows: &[VariantRow]) -> Vec<Variant> {
    let mapping = material_mapping();
    rows.iter()
        .filter_map(|(id, key, name, srgb)| {
            Some(Variant {
                id: MaterialId(*id),
                base: *mapping.get(*key)?,
                name: (*name).to_owned(),
                srgb: *srgb,
            })
        })
        .collect()
}

/// The frozen variants of the v3 manifest, exactly as shipped.
pub fn frozen_v3_variants() -> Vec<Variant> {
    variants_of(V3)
}

/// The variants added after v3 (generated, append-only).
pub fn extension_variants() -> Vec<Variant> {
    variants_of(EXTENSIONS)
}

/// Every variant: the frozen v3 set, then the extensions.
pub fn variants() -> Vec<Variant> {
    let mut all = frozen_v3_variants();
    all.extend(extension_variants());
    all
}

/// Weighted sRGB distance ("redmean"): cheap and close to perceptual.
fn distance(a: [u8; 3], b: [u8; 3]) -> f64 {
    let r = (f64::from(a[0]) + f64::from(b[0])) / 2.0;
    let d = |c: usize| f64::from(a[c]) - f64::from(b[c]);
    ((2.0 + r / 256.0) * d(0).powi(2)
        + 4.0 * d(1).powi(2)
        + (2.0 + (255.0 - r) / 256.0) * d(2).powi(2))
    .sqrt()
}

/// The material an imported cell gets: the nearest variant of `base` to `tint`,
/// or `base` itself for an untinted cell or a material with no variants.
pub fn resolve(base: MaterialId, tint: Option<[u8; 3]>) -> MaterialId {
    let Some(tint) = tint else {
        return base;
    };
    variants()
        .into_iter()
        .filter(|variant| variant.base == base)
        .min_by(|a, b| distance(a.srgb, tint).total_cmp(&distance(b.srgb, tint)))
        .map_or(base, |variant| variant.id)
}

/// A resolver for many cells (builds the variant list once).
pub struct Resolver {
    by_base: BTreeMap<MaterialId, Vec<Variant>>,
}

impl Default for Resolver {
    fn default() -> Self {
        let mut by_base: BTreeMap<MaterialId, Vec<Variant>> = BTreeMap::new();
        for variant in variants() {
            by_base.entry(variant.base).or_default().push(variant);
        }
        Self { by_base }
    }
}

impl Resolver {
    /// As [`resolve`], without rebuilding the table per cell.
    pub fn resolve(&self, base: MaterialId, tint: Option<[u8; 3]>) -> MaterialId {
        let Some(tint) = tint else {
            return base;
        };
        self.by_base
            .get(&base)
            .and_then(|list| {
                list.iter()
                    .min_by(|a, b| distance(a.srgb, tint).total_cmp(&distance(b.srgb, tint)))
            })
            .map_or(base, |variant| variant.id)
    }

    /// The variant's authored colour, if `id` is a variant.
    pub fn srgb_of(&self, id: MaterialId) -> Option<[u8; 3]> {
        self.by_base
            .values()
            .flatten()
            .find(|variant| variant.id == id)
            .map(|variant| variant.srgb)
    }
}

/// `id`'s base material: itself unless it is a variant.
pub fn base_material(id: MaterialId) -> MaterialId {
    if id.0 < VARIANT_ID_BASE {
        return id;
    }
    variants()
        .into_iter()
        .find(|variant| variant.id == id)
        .map_or(id, |variant| variant.base)
}

/// Removed-cell tallies with every variant counted under its base material, for
/// game rules that key on base materials (drops, crafting).
pub fn merge_variants(removed: &BTreeMap<MaterialId, u64>) -> BTreeMap<MaterialId, u64> {
    let mut merged = BTreeMap::new();
    for (material, count) in removed {
        *merged.entry(base_material(*material)).or_default() += count;
    }
    merged
}

fn srgb_to_linear(channel: u8) -> f32 {
    let c = f32::from(channel) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The manifest entries for `variants`: each base material's definition (so
/// simulation properties are identical) with the authored colour as albedo.
pub fn variant_entries_of(
    variants: Vec<Variant>,
    base_manifest: &MaterialManifest,
) -> Vec<MaterialDef> {
    variants
        .into_iter()
        .filter_map(|variant| {
            let mut def = base_manifest.get(variant.base)?.clone();
            def.id = variant.id;
            def.name = variant.name;
            def.render.albedo = variant.srgb.map(srgb_to_linear);
            Some(def)
        })
        .collect()
}

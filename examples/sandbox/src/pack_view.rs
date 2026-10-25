//! What the sandbox's items look like laid out on the pack's tarp (Tab):
//! names and painted icons (`assets/items/*.png`, drawn by `make_items.py`),
//! the blueprint book's (B) patterns for the game's recipes, and the models of
//! items lying in the world (`fixtures/items/assets/*.spvox`, ENG-254).

use std::collections::{BTreeMap, HashMap};

use spall_client::window::{Blueprint, BlueprintSlot, BlueprintView, ItemVoxel, PackView};
use spall_core::MaterialId;

use crate::game::{ItemId, RECIPE_CATALOG_VERSION, items, recipe_ids};

#[derive(Debug)]
pub struct SandboxPackView {
    blueprints: Vec<Blueprint>,
    models: ItemModels,
}

impl SandboxPackView {
    pub fn new() -> Self {
        let slot = |item: crate::game::ItemId, x: f32, y: f32, size: f32| BlueprintSlot {
            item: item.0,
            at: [x, y],
            size,
        };
        // A ring of `count` slots round the cloth's centre.
        let ring = |item, count: usize, radius: f32, size: f32, turn: f32| {
            (0..count)
                .map(|i| {
                    let a = (i as f32 + turn) / count as f32 * std::f32::consts::TAU;
                    slot(item, 0.5 + radius * a.cos(), 0.5 + radius * a.sin(), size)
                })
                .collect::<Vec<_>>()
        };
        let mut fire_pit = ring(items::STONE_CHUNK, 6, 0.34, 0.17, 0.0);
        fire_pit.extend(ring(items::STICK, 6, 0.1, 0.15, 0.5));
        Self {
            models: ItemModels::load(),
            blueprints: vec![
                // From the side: a log laid on the ground, sawn into planks.
                Blueprint {
                    recipe_id: recipe_ids::SAW_PLANKS.0,
                    output: (items::WOOD_PLANK.0, 4),
                    view: BlueprintView::Side,
                    slots: vec![slot(items::WOOD_LOG, 0.5, 0.6, 0.46)],
                },
                // From the side: two stones stacked on two, shaped into a block.
                Blueprint {
                    recipe_id: recipe_ids::SHAPE_STONE.0,
                    output: (items::STONE_BLOCK.0, 1),
                    view: BlueprintView::Side,
                    slots: vec![
                        slot(items::STONE_CHUNK, 0.39, 0.64, 0.22),
                        slot(items::STONE_CHUNK, 0.61, 0.64, 0.22),
                        slot(items::STONE_CHUNK, 0.39, 0.44, 0.22),
                        slot(items::STONE_CHUNK, 0.61, 0.44, 0.22),
                    ],
                },
                // From above: a log laid out to split into sticks.
                Blueprint {
                    recipe_id: recipe_ids::SPLIT_STICKS.0,
                    output: (items::STICK.0, 6),
                    view: BlueprintView::Overhead,
                    slots: vec![slot(items::WOOD_LOG, 0.5, 0.5, 0.42)],
                },
                // From above: six stones in a ring round six sticks.
                Blueprint {
                    recipe_id: recipe_ids::BUILD_FIRE_PIT.0,
                    output: (items::FIRE_PIT.0, 1),
                    view: BlueprintView::Overhead,
                    slots: fire_pit,
                },
            ],
        }
    }
}

impl Default for SandboxPackView {
    fn default() -> Self {
        Self::new()
    }
}

impl PackView for SandboxPackView {
    fn catalog_version(&self) -> u32 {
        RECIPE_CATALOG_VERSION
    }

    fn item_name(&self, item_id: u16) -> Option<&str> {
        Some(match crate::game::ItemId(item_id) {
            items::WOOD_LOG => "Log",
            items::WOOD_PLANK => "Plank",
            items::STONE_CHUNK => "Stone",
            items::STONE_BLOCK => "Stone block",
            items::STICK => "Stick",
            items::FIRE_PIT => "Fire pit",
            items::SHOVEL => "Shovel",
            items::AXE => "Axe",
            items::COMPASS => "Compass",
            items::NOTE => "Note",
            _ => return None,
        })
    }

    fn item_icon_png(&self, item_id: u16) -> Option<&'static [u8]> {
        Some(match crate::game::ItemId(item_id) {
            items::WOOD_LOG => include_bytes!("../assets/items/log.png"),
            items::WOOD_PLANK => include_bytes!("../assets/items/plank.png"),
            items::STONE_CHUNK => include_bytes!("../assets/items/stone.png"),
            items::STONE_BLOCK => include_bytes!("../assets/items/stone_block.png"),
            items::STICK => include_bytes!("../assets/items/stick.png"),
            items::FIRE_PIT => include_bytes!("../assets/items/fire_pit.png"),
            _ => return None,
        })
    }

    fn blueprints(&self) -> &[Blueprint] {
        &self.blueprints
    }

    fn item_model(&self, item_id: u16) -> &[ItemVoxel] {
        self.models.models.get(&item_id).map_or(&[], Vec::as_slice)
    }

    fn item_tints(&self) -> Vec<(MaterialId, [u8; 3])> {
        self.models.tints.clone()
    }

    fn item_tool(&self, item_id: u16) -> Option<u16> {
        match crate::game::ItemId(item_id) {
            items::SHOVEL => Some(crate::game::tool_ids::SHOVEL),
            _ => None,
        }
    }
}

/// The SPVOX model of each kind that can lie in the world.
const ITEM_ASSETS: [(ItemId, &[u8]); 7] = [
    (
        items::SHOVEL,
        include_bytes!("../../../fixtures/items/assets/shovel.spvox"),
    ),
    (
        items::AXE,
        include_bytes!("../../../fixtures/items/assets/axe.spvox"),
    ),
    (
        items::COMPASS,
        include_bytes!("../../../fixtures/items/assets/compass.spvox"),
    ),
    (
        items::NOTE,
        include_bytes!("../../../fixtures/items/assets/paper_note.spvox"),
    ),
    (
        items::STICK,
        include_bytes!("../../../fixtures/items/assets/stick.spvox"),
    ),
    (
        items::WOOD_LOG,
        include_bytes!("../../../fixtures/items/assets/log.spvox"),
    ),
    (
        items::STONE_CHUNK,
        include_bytes!("../../../fixtures/items/assets/stone.spvox"),
    ),
];
const COMPASS_NEEDLE: &[u8] = include_bytes!("../../../fixtures/items/assets/compass_needle.spvox");
/// Where the needle's origin sits in the compass, in cells
/// (`fixtures/items/README.md`).
const NEEDLE_OFFSET_CELLS: [i32; 3] = [3, 2, 1];
/// Edge of a cell-size-code-1 asset cell.
const ITEM_CELL_M: f32 = 0.0625;

/// Drawn size relative to the asset. 6.25 cm is the finest cell, so the
/// compass (a 44 cm card) and the note (25 x 31 cm) come out larger than
/// life; these bring them to about 11 cm and 15 x 19 cm.
fn display_scale(item: ItemId) -> f32 {
    match item {
        items::COMPASS => 0.25,
        items::NOTE => 0.6,
        _ => 1.0,
    }
}

/// World item models, decoded once when the view is built.
#[derive(Debug, Default)]
struct ItemModels {
    models: HashMap<u16, Vec<ItemVoxel>>,
    /// `(material, sRGB tint)` pairs that [`ItemVoxel::tint`] indexes.
    tints: Vec<(MaterialId, [u8; 3])>,
    /// Kinds whose asset failed and draw [`placeholder_model`] instead.
    fallbacks: Vec<u16>,
}

impl ItemModels {
    fn load() -> Self {
        let mapping = crate::editor_scene::material_mapping();
        let mut out = Self::default();
        for (item, bytes) in ITEM_ASSETS {
            let mut sources = vec![(bytes, [0; 3])];
            if item == items::COMPASS {
                sources.push((COMPASS_NEEDLE, NEEDLE_OFFSET_CELLS));
            }
            let model = match out.build(&sources, display_scale(item), &mapping) {
                Ok(model) => model,
                Err(error) => {
                    tracing::error!(
                        item = item.0,
                        %error,
                        "world item model failed to load; drawing the placeholder"
                    );
                    out.fallbacks.push(item.0);
                    placeholder_model(item).to_vec()
                }
            };
            out.models.insert(item.0, model);
        }
        out
    }

    /// One voxel per asset run, the sources merged in cells (each moved by
    /// its cell offset), laid on the largest face (the thinnest axis turned
    /// up), centred across and resting on `y = 0`, scaled by `scale`.
    fn build(
        &mut self,
        sources: &[(&[u8], [i32; 3])],
        scale: f32,
        mapping: &BTreeMap<String, MaterialId>,
    ) -> Result<Vec<ItemVoxel>, String> {
        let mut boxes = Vec::new();
        for (bytes, offset) in sources {
            let asset = spall_asset::decode(bytes).map_err(|e| e.to_string())?;
            if asset.cell_size_code != 1 {
                return Err(format!("{}: expected 6.25 cm cells", asset.name));
            }
            if asset.parts.len() != 1 {
                return Err(format!(
                    "{}: articulated item models are unsupported",
                    asset.name
                ));
            }
            for run in &asset.runs {
                let key = usize::from(run.material_slot)
                    .checked_sub(1)
                    .and_then(|slot| asset.material_keys.get(slot))
                    .ok_or_else(|| format!("{}: bad material slot", asset.name))?;
                let material = *mapping
                    .get(key)
                    .ok_or_else(|| format!("{}: unmapped material {key}", asset.name))?;
                let tint = match usize::from(run.tint_slot) {
                    0 => None,
                    slot => {
                        let srgb = *asset
                            .tints_srgb
                            .get(slot - 1)
                            .ok_or_else(|| format!("{}: bad tint slot", asset.name))?;
                        Some(self.tint_index(material, srgb)?)
                    }
                };
                let start = [0, 1, 2].map(|i| run.start[i] + offset[i]);
                boxes.push((start, run.length_x as i32, material, tint));
            }
        }
        if boxes.is_empty() {
            return Err("no cells".into());
        }
        let (mut lo, mut hi) = ([i32::MAX; 3], [i32::MIN; 3]);
        for (start, length, ..) in &boxes {
            let end = [start[0] + length, start[1] + 1, start[2] + 1];
            for i in 0..3 {
                lo[i] = lo[i].min(start[i]);
                hi[i] = hi[i].max(end[i]);
            }
        }
        let extent = [0, 1, 2].map(|i| hi[i] - lo[i]);
        // Turn the thinnest axis up (Y wins ties) by a quarter turn.
        let up = if extent[1] <= extent[0] && extent[1] <= extent[2] {
            1
        } else if extent[0] <= extent[2] {
            0
        } else {
            2
        };
        let turn = |v: [f32; 3]| match up {
            0 => [-v[1], v[0], v[2]], // about Z: +X goes up
            2 => [v[0], v[2], -v[1]], // about X: +Z goes up
            _ => v,
        };
        let cell = ITEM_CELL_M * scale;
        let lift = extent[up] as f32 * 0.5;
        Ok(boxes
            .into_iter()
            .map(|(start, length, material, tint)| {
                let size = [length, 1, 1];
                let center = turn([0, 1, 2].map(|i| {
                    (start[i] - lo[i]) as f32 + size[i] as f32 * 0.5 - extent[i] as f32 * 0.5
                }));
                let size = turn(size.map(|s| s as f32));
                ItemVoxel {
                    center: [
                        center[0] * cell,
                        (center[1] + lift) * cell,
                        center[2] * cell,
                    ],
                    size: size.map(|s| s.abs() * cell),
                    material: u32::from(material.0),
                    tint,
                }
            })
            .collect())
    }

    fn tint_index(&mut self, material: MaterialId, srgb: [u8; 3]) -> Result<u16, String> {
        let pair = (material, srgb);
        let index = match self.tints.iter().position(|t| *t == pair) {
            Some(index) => index,
            None => {
                self.tints.push(pair);
                self.tints.len() - 1
            }
        };
        u16::try_from(index).map_err(|_| "too many item tints".to_string())
    }
}

/// A few coloured cubes per world item kind, lying flat with the origin on the
/// ground: drawn only when that kind's SPVOX model fails to load.
pub fn placeholder_model(item: ItemId) -> &'static [ItemVoxel] {
    use crate::game::materials::{DEEP_STONE, LAMP, SNOW, STONE, WOOD};
    const fn v(center: [f32; 3], size: [f32; 3], material: spall_core::MaterialId) -> ItemVoxel {
        ItemVoxel {
            center,
            size,
            material: material.0 as u32,
            tint: None,
        }
    }
    const CLAY: spall_core::MaterialId = crate::game::materials::CLAY;
    const SHOVEL: &[ItemVoxel] = &[
        v([0.0, 0.03, 0.0], [0.8, 0.05, 0.05], WOOD),
        v([-0.42, 0.03, 0.0], [0.05, 0.05, 0.14], WOOD),
        v([0.51, 0.02, 0.0], [0.22, 0.03, 0.2], DEEP_STONE),
    ];
    const AXE: &[ItemVoxel] = &[
        v([0.0, 0.03, 0.0], [0.6, 0.05, 0.05], WOOD),
        v([0.27, 0.04, 0.05], [0.1, 0.07, 0.18], DEEP_STONE),
    ];
    const COMPASS: &[ItemVoxel] = &[
        v([0.0, 0.02, 0.0], [0.12, 0.04, 0.12], CLAY),
        v([0.0, 0.045, 0.0], [0.09, 0.01, 0.09], SNOW),
        v([0.0, 0.052, 0.015], [0.015, 0.006, 0.05], LAMP),
    ];
    const NOTE: &[ItemVoxel] = &[v([0.0, 0.005, 0.0], [0.2, 0.01, 0.28], SNOW)];
    const STICK: &[ItemVoxel] = &[v([0.0, 0.02, 0.0], [0.5, 0.04, 0.04], WOOD)];
    const LOG: &[ItemVoxel] = &[v([0.0, 0.12, 0.0], [0.9, 0.24, 0.24], WOOD)];
    const ROCK: &[ItemVoxel] = &[v([0.0, 0.08, 0.0], [0.2, 0.16, 0.18], STONE)];
    match item {
        items::SHOVEL => SHOVEL,
        items::AXE => AXE,
        items::COMPASS => COMPASS,
        items::NOTE => NOTE,
        items::STICK => STICK,
        items::WOOD_LOG => LOG,
        items::STONE_CHUNK => ROCK,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every item the game can award or craft has a name and an icon that
    /// decodes.
    #[test]
    fn every_item_has_a_name_and_a_decodable_icon() {
        let view = SandboxPackView::new();
        for item in [
            items::WOOD_LOG,
            items::WOOD_PLANK,
            items::STONE_CHUNK,
            items::STONE_BLOCK,
            items::STICK,
            items::FIRE_PIT,
        ] {
            assert!(view.item_name(item.0).is_some(), "{item:?}");
            let png = view.item_icon_png(item.0).expect("icon");
            let icon = image::load_from_memory(png).expect("decodes");
            assert_eq!((icon.width(), icon.height()), (160, 160), "{item:?}");
        }
        assert_eq!(view.item_name(999), None);
        assert!(view.item_icon_png(999).is_none());
    }

    /// Every kind that can lie in the world has a name for the E prompt and a
    /// model to draw.
    #[test]
    fn every_world_item_kind_has_a_name_and_a_model() {
        let view = SandboxPackView::new();
        for key in ["shovel", "axe", "compass", "note", "stick", "log", "stone"] {
            let item = crate::world_items::item_by_key(key).expect(key);
            assert!(view.item_name(item.0).is_some(), "{key}");
            assert!(!view.item_model(item.0).is_empty(), "{key}");
        }
    }

    /// Every pickup kind draws its real SPVOX model (needle and all for the
    /// compass), hand-sized and lying flat on the ground, not the placeholder.
    #[test]
    fn every_pickup_kind_draws_its_real_asset_model() {
        let view = SandboxPackView::new();
        assert!(
            view.models.fallbacks.is_empty(),
            "{:?}",
            view.models.fallbacks
        );
        let tints = view.item_tints().len();
        assert!(tints > 0);
        for key in ["shovel", "axe", "compass", "note", "stick", "log", "stone"] {
            let item = crate::world_items::item_by_key(key).expect(key);
            let model = view.item_model(item.0);
            assert!(!model.is_empty(), "{key}");
            assert_ne!(model, placeholder_model(item), "{key}");
            let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
            for voxel in model {
                assert!(voxel.tint.is_none_or(|t| usize::from(t) < tints), "{key}");
                for i in 0..3 {
                    lo[i] = lo[i].min(voxel.center[i] - voxel.size[i] * 0.5);
                    hi[i] = hi[i].max(voxel.center[i] + voxel.size[i] * 0.5);
                }
            }
            assert!(lo[1].abs() < 1e-4, "{key} rests on the ground: {lo:?}");
            let extents: Vec<f32> = (0..3).map(|i| hi[i] - lo[i]).collect();
            let longest = extents.iter().copied().fold(0.0, f32::max);
            let thinnest = extents.iter().copied().fold(f32::MAX, f32::min);
            assert!((extents[1] - thinnest).abs() < 1e-4, "{key} lies flat");
            let limit = match item {
                items::COMPASS => 0.15,
                items::NOTE => 0.25,
                _ => 1.1,
            };
            assert!(longest <= limit, "{key} is {longest} m long");
        }
        // The compass carries the needle composed on top of the card.
        let card = spall_asset::decode(ITEM_ASSETS[2].1).unwrap().runs.len();
        let needle = spall_asset::decode(COMPASS_NEEDLE).unwrap().runs.len();
        assert_eq!(view.item_model(items::COMPASS.0).len(), card + needle);
    }

    /// An asset that fails to decode is an error the loader reports (and
    /// falls back on), never a silently empty model.
    #[test]
    fn a_corrupt_item_asset_is_an_error() {
        let mapping = crate::editor_scene::material_mapping();
        let mut models = ItemModels::default();
        assert!(
            models
                .build(&[(b"not spvox", [0; 3])], 1.0, &mapping)
                .is_err()
        );
    }

    /// Each blueprint's pattern uses exactly its server recipe's ingredients
    /// and promises exactly its output, so a correct arrangement crafts.
    #[test]
    fn every_blueprint_matches_its_server_recipe() {
        let view = SandboxPackView::new();
        let catalog = crate::game::recipe_catalog();
        assert_eq!(
            view.blueprints().len(),
            catalog.len(),
            "one blueprint per recipe"
        );
        for blueprint in view.blueprints() {
            let recipe = catalog
                .get(crate::game::RecipeId(blueprint.recipe_id))
                .expect("a known recipe");
            let mut needs = blueprint.needs();
            needs.sort();
            let mut ingredients: Vec<_> = recipe
                .ingredients
                .iter()
                .map(|stack| (stack.item.0, stack.count))
                .collect();
            ingredients.sort();
            assert_eq!(needs, ingredients, "recipe {}", blueprint.recipe_id);
            let outputs: Vec<_> = recipe
                .outputs
                .iter()
                .map(|stack| (stack.item.0, stack.count))
                .collect();
            assert_eq!(outputs, vec![blueprint.output]);
            for slot in &blueprint.slots {
                let half = slot.size * 0.5;
                assert!(
                    slot.at.iter().all(|&c| c - half >= 0.0 && c + half <= 1.0),
                    "recipe {}: slot {slot:?} off the cloth",
                    blueprint.recipe_id
                );
            }
        }
    }
}

//! What the sandbox's items look like laid out on the pack's tarp (Tab):
//! names and painted icons (`assets/items/*.png`, drawn by `make_items.py`),
//! and the blueprint book's (B) patterns for the game's recipes.

use spall_client::window::{Blueprint, BlueprintSlot, BlueprintView, PackView};

use crate::game::{RECIPE_CATALOG_VERSION, items, recipe_ids};

#[derive(Debug)]
pub struct SandboxPackView {
    blueprints: Vec<Blueprint>,
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

//! What the sandbox's items look like laid out on the pack's tarp (Tab):
//! names and painted icons (`assets/items/*.png`, drawn by `make_items.py`),
//! and the blueprint book's (B) patterns for the game's recipes.

use spall_client::window::{Blueprint, PackView};

use crate::game::{RECIPE_CATALOG_VERSION, items, recipe_ids};

#[derive(Debug)]
pub struct SandboxPackView {
    blueprints: Vec<Blueprint>,
}

impl SandboxPackView {
    pub fn new() -> Self {
        let log = Some(items::WOOD_LOG.0);
        let stone = Some(items::STONE_CHUNK.0);
        Self {
            blueprints: vec![
                // Saw a log into planks: one log on the cloth.
                Blueprint {
                    recipe_id: recipe_ids::SAW_PLANKS.0,
                    output: (items::WOOD_PLANK.0, 4),
                    pattern: vec![vec![log]],
                },
                // Shape a block: four stones in a square.
                Blueprint {
                    recipe_id: recipe_ids::SHAPE_STONE.0,
                    output: (items::STONE_BLOCK.0, 1),
                    pattern: vec![vec![stone, stone], vec![stone, stone]],
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
            _ => return None,
        })
    }

    fn item_icon_png(&self, item_id: u16) -> Option<&'static [u8]> {
        Some(match crate::game::ItemId(item_id) {
            items::WOOD_LOG => include_bytes!("../assets/items/log.png"),
            items::WOOD_PLANK => include_bytes!("../assets/items/plank.png"),
            items::STONE_CHUNK => include_bytes!("../assets/items/stone.png"),
            items::STONE_BLOCK => include_bytes!("../assets/items/stone_block.png"),
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
            let (w, h) = blueprint.size();
            assert!(w <= 3 && h <= 3, "fits the cloth");
        }
    }
}

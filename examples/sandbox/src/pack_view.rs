//! What the sandbox's items look like laid out on the pack's tarp (Tab):
//! names and painted icons (`assets/items/*.png`, drawn by `make_items.py`).

use spall_client::window::PackView;

use crate::game::{RECIPE_CATALOG_VERSION, items};

#[derive(Debug, Default)]
pub struct SandboxPackView;

impl SandboxPackView {
    pub fn new() -> Self {
        Self
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
}

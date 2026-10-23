//! What the sandbox's items look like laid out on the pack's tarp (Tab):
//! names and small cube models built from manifest materials.

use spall_client::window::PackView;
use spall_render::CubeInstance;

use crate::game::{RECIPE_CATALOG_VERSION, items, materials};

#[derive(Debug)]
pub struct SandboxPackView {
    log: Vec<CubeInstance>,
    plank: Vec<CubeInstance>,
    stone_chunk: Vec<CubeInstance>,
    stone_block: Vec<CubeInstance>,
}

/// A cube resting on `y = 0` at `(x, z)`, raised by `lift`.
fn cube(material: spall_core::MaterialId, at: [f32; 3], size: [f32; 3]) -> CubeInstance {
    CubeInstance::new(
        [at[0], at[1] + size[1] * 0.5, at[2]],
        u32::from(material.0),
        size,
        CubeInstance::IDENTITY_ROTATION,
    )
}

/// The same, turned about Y by `yaw` radians.
fn turned(mut cube: CubeInstance, yaw: f32) -> CubeInstance {
    cube.rotation = glam::Quat::from_rotation_y(yaw).to_array();
    cube
}

impl SandboxPackView {
    pub fn new() -> Self {
        let bark = materials::DIRT;
        let wood = materials::WOOD;
        let stone = materials::STONE;
        Self {
            // A short log lying across the tarp: two crossed boxes of bark
            // round it off, with pale cut ends.
            log: vec![
                cube(bark, [0.0, 0.01, 0.0], [0.26, 0.08, 0.11]),
                cube(bark, [0.0, 0.0, 0.0], [0.26, 0.11, 0.08]),
                cube(wood, [-0.131, 0.012, 0.0], [0.004, 0.085, 0.085]),
                cube(wood, [0.131, 0.012, 0.0], [0.004, 0.085, 0.085]),
            ],
            // A flat sawn board.
            plank: vec![turned(
                cube(wood, [0.0, 0.0, 0.0], [0.26, 0.02, 0.07]),
                0.08,
            )],
            // A rough lump: a few uneven boxes, turned against each other.
            stone_chunk: vec![
                turned(cube(stone, [0.0, 0.0, 0.0], [0.08, 0.05, 0.07]), 0.3),
                turned(cube(stone, [0.015, 0.03, -0.01], [0.05, 0.035, 0.05]), -0.5),
                turned(
                    cube(materials::GRAVEL, [-0.03, 0.0, 0.02], [0.035, 0.03, 0.03]),
                    0.9,
                ),
            ],
            // A squared block with a darker top.
            stone_block: vec![
                cube(stone, [0.0, 0.0, 0.0], [0.12, 0.08, 0.12]),
                cube(materials::DEEP_STONE, [0.0, 0.08, 0.0], [0.11, 0.004, 0.11]),
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

    fn item_model(&self, item_id: u16) -> &[CubeInstance] {
        match crate::game::ItemId(item_id) {
            items::WOOD_LOG => &self.log,
            items::WOOD_PLANK => &self.plank,
            items::STONE_CHUNK => &self.stone_chunk,
            items::STONE_BLOCK => &self.stone_block,
            _ => &[],
        }
    }

    fn tarp_material(&self) -> u32 {
        u32::from(materials::SAND.0)
    }

    fn trim_materials(&self) -> [u32; 2] {
        [u32::from(materials::CLAY.0), u32::from(materials::STONE.0)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every item the game can award or craft has a name and a model in
    /// manifest materials resting on the tarp.
    #[test]
    fn every_item_has_a_name_and_a_grounded_model() {
        let view = SandboxPackView::new();
        let manifest = crate::game::manifest();
        for item in [
            items::WOOD_LOG,
            items::WOOD_PLANK,
            items::STONE_CHUNK,
            items::STONE_BLOCK,
        ] {
            assert!(view.item_name(item.0).is_some(), "{item:?}");
            let model = view.item_model(item.0);
            assert!(!model.is_empty(), "{item:?}");
            for cube in model {
                assert!(
                    manifest
                        .entries()
                        .iter()
                        .any(|def| u32::from(def.id.0) == cube.material),
                    "{item:?} uses material {} outside the manifest",
                    cube.material
                );
                assert!(
                    cube.offset[1] - cube.size[1] * 0.5 >= -1e-6,
                    "{item:?} sinks"
                );
            }
        }
        assert_eq!(view.item_name(999), None);
        assert!(view.item_model(999).is_empty());
    }
}

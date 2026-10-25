//! The sandbox's tool rules (ENG-259, ENG-260): which player may swing which
//! tool, what each tool does to what it strikes, and what a dig turns up.
//!
//! Players start empty-handed. The hammer (the catalog's `DIG` tool) is an
//! admin tool: the server accepts its swings only when it grants admin
//! commands. The shovel works only from a pack that holds one: it clears thin
//! snow off the surface, digs snow and soil fast, struggles through stone, and
//! now and then turns up a stone.

use spall_core::MaterialId;
use spall_server::{SwingEffect, ToolCut, ToolEffects, ToolRules, ToolStrike, ToolUse};

use crate::appearance::base_material;
use crate::game::{ItemId, items, materials, tool_ids};
use crate::world_items::PackStore;

/// Refusal for a hammer swing from a non-admin.
pub const HAMMER_IS_ADMIN_ONLY: &str = "the hammer is an admin tool";
/// Refusal for a shovel swing without a shovel in the pack.
pub const NO_SHOVEL: &str = "you have no shovel";
/// Refusal for a shovel swing at something it cannot dig.
pub const SHOVEL_CANNOT_DIG: &str = "the shovel cannot dig that";

/// Cut radius of a shovel stroke into snow or soil: 33 cells, about a
/// quarter of a 1 m x 1 m x 0.5 m hole.
pub const SHOVEL_SOFT_RADIUS_CELLS: i64 = 2;
/// Cut radius of a shovel stroke into stone: one cell. It struggles.
pub const SHOVEL_STONE_RADIUS_CELLS: i64 = 0;
/// Thin snow deeper than this on the struck face is shovelled off first.
pub const SHOVEL_SNOW_M: f64 = 0.02;
/// How far round the struck point one stroke clears the thin snow.
pub const SHOVEL_SNOW_RADIUS_M: f64 = 0.75;
/// One stroke in this many into soil turns up a stone.
pub const SOIL_STONE_ONE_IN: u64 = 8;
/// One stroke in this many into stone breaks a stone loose.
pub const ROCK_STONE_ONE_IN: u64 = 2;

/// How a shovel meets a material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dig {
    /// Snow: fast, nothing in it.
    Snow,
    /// Soil (dirt, grass, sand, mud, moss, gravel, clay): fast, the odd stone.
    Soil,
    /// Stone of any kind: slow, and it breaks stones loose.
    Stone,
}

/// How the shovel meets `material` (appearance variants count as their
/// base), or `None` where it cannot dig (wood, bedrock, built things).
pub fn shovel_dig(material: MaterialId) -> Option<Dig> {
    const GRASS: MaterialId = MaterialId(10);
    const SANDSTONE: MaterialId = MaterialId(13);
    const SLATE: MaterialId = MaterialId(14);
    match base_material(material) {
        materials::SNOW => Some(Dig::Snow),
        materials::DIRT
        | GRASS
        | materials::SAND
        | materials::MUD
        | materials::MOSS
        | materials::GRAVEL
        | materials::CLAY => Some(Dig::Soil),
        materials::STONE | materials::DEEP_STONE | SANDSTONE | SLATE => Some(Dig::Stone),
        _ => None,
    }
}

/// A fair, repeatable roll for one stroke: the same request always rolls the
/// same, so a retried request cannot reroll its luck.
fn roll(cut: &ToolCut<'_>, one_in: u64) -> bool {
    let mut x = cut.request_id.0 ^ (cut.session.raw() << 32) ^ 0x9e37_79b9_7f4a_7c15;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (x ^ (x >> 31)).is_multiple_of(one_in)
}

/// The server-side rules for the sandbox's tools.
pub struct SandboxToolRules {
    pack: PackStore,
}

impl SandboxToolRules {
    pub fn new(pack: PackStore) -> Self {
        Self { pack }
    }

    fn require(
        &self,
        tool_use: &ToolUse,
        item: ItemId,
        refusal: &'static str,
    ) -> Result<(), &'static str> {
        if self.pack.holds(tool_use.session, tool_use.player, item) {
            Ok(())
        } else {
            Err(refusal)
        }
    }

    /// Puts each drop in the pack; what does not fit lies at the dig.
    fn award(&self, cut: &ToolCut<'_>, drops: &[ItemId]) -> ToolEffects {
        let mut effects = ToolEffects::default();
        for &item in drops {
            if self.pack.put(cut.session, cut.player, item).is_err() {
                effects.world_items.push(item.0);
            }
        }
        effects
    }
}

impl ToolRules for SandboxToolRules {
    fn allow(&mut self, tool_use: &ToolUse) -> Result<(), &'static str> {
        match tool_use.tool {
            tool_ids::DIG if !tool_use.admin => Err(HAMMER_IS_ADMIN_ONLY),
            tool_ids::SHOVEL => self.require(tool_use, items::SHOVEL, NO_SHOVEL),
            _ => Ok(()),
        }
    }

    fn swing(&mut self, strike: &ToolStrike) -> Result<SwingEffect, &'static str> {
        if strike.tool != tool_ids::SHOVEL {
            return Ok(SwingEffect::Cut { radius_cells: None });
        }
        if strike.snow_depth_m > SHOVEL_SNOW_M {
            return Ok(SwingEffect::ClearSnow {
                radius_m: SHOVEL_SNOW_RADIUS_M,
            });
        }
        let radius = match shovel_dig(strike.material).ok_or(SHOVEL_CANNOT_DIG)? {
            Dig::Snow | Dig::Soil => SHOVEL_SOFT_RADIUS_CELLS,
            Dig::Stone => SHOVEL_STONE_RADIUS_CELLS,
        };
        Ok(SwingEffect::Cut {
            radius_cells: Some(radius),
        })
    }

    fn committed(&mut self, cut: &ToolCut<'_>) -> ToolEffects {
        if cut.tool != tool_ids::SHOVEL {
            return ToolEffects::default();
        }
        let dug = |dig| {
            cut.removed
                .iter()
                .any(|(material, count)| *count > 0 && shovel_dig(*material) == Some(dig))
        };
        let stone = (dug(Dig::Stone) && roll(cut, ROCK_STONE_ONE_IN))
            || (dug(Dig::Soil) && roll(cut, SOIL_STONE_ONE_IN));
        if stone {
            self.award(cut, &[items::STONE_CHUNK])
        } else {
            ToolEffects::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::PlayerInventories;
    use crate::world_items::{PACK_SLOTS, put_in_pack};
    use spall_protocol::{RequestId, SessionId, SlotId};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    fn session() -> SessionId {
        SessionId::from_parts(SlotId(0), 1)
    }

    fn rules() -> (SandboxToolRules, Arc<Mutex<PlayerInventories>>) {
        let inventories = Arc::new(Mutex::new(PlayerInventories::default()));
        (
            SandboxToolRules::new(PackStore::InMemory(inventories.clone())),
            inventories,
        )
    }

    fn tool_use(tool: u16, admin: bool) -> ToolUse {
        ToolUse {
            session: session(),
            player: None,
            tool,
            admin,
        }
    }

    fn strike(material: MaterialId, snow_depth_m: f64) -> ToolStrike {
        ToolStrike {
            tool: tool_ids::SHOVEL,
            material,
            on_body: false,
            snow_depth_m,
        }
    }

    #[test]
    fn only_an_admin_may_swing_the_hammer() {
        let (mut rules, _) = rules();
        assert_eq!(
            rules.allow(&tool_use(tool_ids::DIG, false)),
            Err(HAMMER_IS_ADMIN_ONLY)
        );
        assert_eq!(rules.allow(&tool_use(tool_ids::DIG, true)), Ok(()));
        // The torch is not a gated tool.
        assert_eq!(rules.allow(&tool_use(tool_ids::PLACE_LAMP, false)), Ok(()));
    }

    #[test]
    fn the_shovel_works_only_from_a_pack_that_holds_one() {
        let (mut rules, inventories) = rules();
        assert_eq!(
            rules.allow(&tool_use(tool_ids::SHOVEL, true)),
            Err(NO_SHOVEL),
            "admin or not, a shovel needs a shovel"
        );
        inventories
            .lock()
            .unwrap()
            .ensure(0)
            .grant(items::SHOVEL, 1)
            .unwrap();
        assert_eq!(rules.allow(&tool_use(tool_ids::SHOVEL, false)), Ok(()));
    }

    #[test]
    fn the_shovel_digs_snow_and_soil_fast_and_struggles_through_stone() {
        let (mut rules, _) = rules();
        let radius = |rules: &mut SandboxToolRules, material| rules.swing(&strike(material, 0.0));
        for soft in [
            materials::SNOW,
            materials::DIRT,
            MaterialId(10),
            materials::GRAVEL,
        ] {
            assert_eq!(
                radius(&mut rules, soft),
                Ok(SwingEffect::Cut {
                    radius_cells: Some(SHOVEL_SOFT_RADIUS_CELLS)
                })
            );
        }
        for hard in [materials::STONE, materials::DEEP_STONE] {
            assert_eq!(
                radius(&mut rules, hard),
                Ok(SwingEffect::Cut {
                    radius_cells: Some(SHOVEL_STONE_RADIUS_CELLS)
                })
            );
        }
        for refused in [materials::WOOD, materials::BEDROCK, materials::LADDER] {
            assert_eq!(radius(&mut rules, refused), Err(SHOVEL_CANNOT_DIG));
        }
        // Snow lying on the ground comes off before the ground is dug.
        assert_eq!(
            rules.swing(&strike(materials::DIRT, 0.1)),
            Ok(SwingEffect::ClearSnow {
                radius_m: SHOVEL_SNOW_RADIUS_M
            })
        );
        // The hammer is not the shovel: its radius stays the requested one.
        assert_eq!(
            rules.swing(&ToolStrike {
                tool: tool_ids::DIG,
                ..strike(materials::WOOD, 0.3)
            }),
            Ok(SwingEffect::Cut { radius_cells: None })
        );
    }

    /// Stones from `strokes` committed shovel strokes into `material`: how
    /// many went to the pack and how many were left at the dig.
    fn dig(rules: &mut SandboxToolRules, material: MaterialId, strokes: u64) -> (u32, usize) {
        let removed = BTreeMap::from([(material, 33)]);
        let mut left = 0;
        for id in 0..strokes {
            let effects = rules.committed(&ToolCut {
                session: session(),
                player: None,
                tool: tool_ids::SHOVEL,
                request_id: RequestId(1_000 + id),
                removed: &removed,
                on_body: false,
            });
            assert!(
                effects
                    .world_items
                    .iter()
                    .all(|&k| k == items::STONE_CHUNK.0)
            );
            left += effects.world_items.len();
        }
        let in_pack = match &rules.pack {
            PackStore::InMemory(inventories) => inventories
                .lock()
                .unwrap()
                .get(0)
                .map_or(0, |inventory| inventory.count(items::STONE_CHUNK)),
            PackStore::Durable(_) => unreachable!(),
        };
        (in_pack, left)
    }

    #[test]
    fn digging_turns_up_the_odd_stone_from_soil_and_more_from_stone() {
        let (mut soil, _) = rules();
        let (from_soil, _) = dig(&mut soil, materials::DIRT, 400);
        let (mut rock, _) = rules();
        let (from_stone, _) = dig(&mut rock, materials::STONE, 400);
        let (mut snow, _) = rules();
        assert_eq!(
            dig(&mut snow, materials::SNOW, 400),
            (0, 0),
            "snow holds no stones"
        );
        // About 1 in 8 and 1 in 2 of 400 strokes.
        assert!((25..=75).contains(&from_soil), "{from_soil}");
        assert!((160..=240).contains(&from_stone), "{from_stone}");
        // The same strokes again roll the same: no rerolling by retrying.
        let (mut again, _) = rules();
        assert_eq!(dig(&mut again, materials::DIRT, 400).0, from_soil);
    }

    #[test]
    fn a_full_pack_leaves_the_stones_at_the_dig() {
        let (mut rules, inventories) = rules();
        {
            let mut inventories = inventories.lock().unwrap();
            let inventory = inventories.ensure(0);
            for id in 100..100 + PACK_SLOTS as u16 {
                put_in_pack(inventory, ItemId(id)).unwrap();
            }
        }
        let (in_pack, left) = dig(&mut rules, materials::STONE, 40);
        assert_eq!(in_pack, 0);
        assert!(left > 0, "every stone lies at the dig");
    }
}

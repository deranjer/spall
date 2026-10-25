//! The sandbox's world items (ENG-257): which item kinds can lie in the world,
//! the pack they are picked up into, and the scene file that places them.
//!
//! The engine owns the items themselves (stable id, kind, pose, replication);
//! the kind is the game's [`ItemId`], the same id the pack shows.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use spall_server::{InitialWorldItem, ItemPickup};

use crate::game::{Inventory, ItemId, PlayerInventories, items};
use crate::progression_store::ProgressionStore;

/// Distinct kinds the pack holds: the tarp's 4 x 3 slots. Picking up a kind
/// already in the pack only grows its stack.
pub const PACK_SLOTS: usize = 12;

/// Refusal shown when the pack has no free slot.
pub const PACK_FULL: &str = "your pack is full";

/// Item kinds that can lie in the world, by their scene-file key.
pub fn item_by_key(key: &str) -> Option<ItemId> {
    Some(match key {
        "shovel" => items::SHOVEL,
        "axe" => items::AXE,
        "compass" => items::COMPASS,
        "note" => items::NOTE,
        "stick" => items::STICK,
        "log" => items::WOOD_LOG,
        "stone" => items::STONE_CHUNK,
        _ => return None,
    })
}

/// Adds one `item` to the pack, or refuses when every slot holds another kind.
pub fn put_in_pack(inventory: &mut Inventory, item: ItemId) -> Result<(), &'static str> {
    if inventory.count(item) == 0 && inventory.stacks().count() >= PACK_SLOTS {
        return Err(PACK_FULL);
    }
    inventory
        .grant(item, 1)
        .map_err(|_| "that item cannot go in the pack")
}

/// Pickup into the server's in-memory inventories (no progression database):
/// keyed by account when the session has one, else by connection slot, the
/// same as the progression handler.
pub fn in_memory_pickup(inventories: Arc<Mutex<PlayerInventories>>) -> ItemPickup {
    ItemPickup::new(move |session, player_id, item| {
        let mut inventories = inventories.lock().unwrap_or_else(|e| e.into_inner());
        let inventory = match player_id {
            Some(player_id) => inventories.ensure_player(player_id),
            None => inventories.ensure(session.slot().0),
        };
        put_in_pack(inventory, ItemId(item.kind))
    })
}

/// Pickup into the durable progression database. Only authenticated accounts
/// have a durable pack.
pub fn durable_pickup(store: Arc<Mutex<ProgressionStore>>) -> ItemPickup {
    ItemPickup::new(move |_session, player_id, item| {
        let player_id = player_id.ok_or("no account to keep the item")?;
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        match store.pick_up_item(player_id, ItemId(item.kind)) {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::error!(
                    ?player_id,
                    item = item.item_id,
                    ?error,
                    "could not save pickup"
                );
                Err("the pack could not be saved")
            }
        }
    })
}

#[derive(Debug, Deserialize)]
struct Placement {
    kind: String,
    translation: [f64; 3],
    #[serde(default)]
    yaw_degrees: f32,
}

/// Reads `<project>/items.ron`, the world items a new world from that editor
/// project starts with. A project without the file has none.
pub fn load_placements(project: &Path) -> Result<Vec<InitialWorldItem>, String> {
    let path = project.join("items.ron");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let placements: Vec<Placement> =
        ron::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    placements
        .into_iter()
        .map(|p| {
            let kind = item_by_key(&p.kind)
                .ok_or_else(|| format!("{}: unknown item `{}`", path.display(), p.kind))?;
            if !p.translation.iter().all(|v| v.is_finite()) || !p.yaw_degrees.is_finite() {
                return Err(format!(
                    "{}: `{}` pose is not finite",
                    path.display(),
                    p.kind
                ));
            }
            Ok(InitialWorldItem {
                kind: kind.0,
                translation_m: p.translation,
                yaw_rad: p.yaw_degrees.to_radians(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_pack_refuses_a_new_kind_but_stacks_a_held_one() {
        let mut inventory = Inventory::default();
        for id in 100..100 + PACK_SLOTS as u16 {
            put_in_pack(&mut inventory, ItemId(id)).unwrap();
        }
        let before = inventory.clone();
        assert_eq!(put_in_pack(&mut inventory, items::NOTE), Err(PACK_FULL));
        assert_eq!(inventory, before, "a refusal changes nothing");
        put_in_pack(&mut inventory, ItemId(100)).unwrap();
        assert_eq!(inventory.count(ItemId(100)), 2);
    }
}

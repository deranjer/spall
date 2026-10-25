//! Replicated world items (ENG-257): the server's spawns and removals, and the
//! join list after each baseline (a new baseline forgets the old items).
use super::*;
use spall_protocol::{WorldItem, WorldItemUpdate};

impl ReplicaWorld {
    /// Applies one update. A repeated spawn just replaces the item, and a
    /// removal of an unknown id is ignored, so a join list overlapping an
    /// in-flight update converges.
    pub fn apply_world_items(&mut self, update: &WorldItemUpdate) {
        for item in &update.spawned {
            self.world_items.insert(item.item_id, *item);
        }
        for id in &update.removed {
            self.world_items.remove(id);
        }
    }

    /// The items lying in the world, in no particular order.
    pub fn world_items(&self) -> impl Iterator<Item = &WorldItem> {
        self.world_items.values()
    }
}

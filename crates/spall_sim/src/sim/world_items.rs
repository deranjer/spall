//! World item methods (ENG-257). The owning thread calls these between ticks.

use super::*;
use crate::world_items::WorldItemError;
use spall_protocol::{WorldItem, WorldItemUpdate};

impl Simulation {
    /// Places a new item and queues its spawn for replication. The id is the
    /// next unused one in this world.
    pub fn spawn_world_item(
        &mut self,
        kind: u16,
        translation_m: [f64; 3],
        yaw_rad: f32,
    ) -> Result<WorldItem, WorldItemError> {
        self.world_items.spawn(kind, translation_m, yaw_rad, None)
    }

    /// Sets a fire down (ENG-266): an item of `kind` with `fuel_s` seconds of
    /// fuel, lit if that is above zero, queued for replication.
    pub fn place_fire(
        &mut self,
        kind: u16,
        translation_m: [f64; 3],
        yaw_rad: f32,
        fuel_s: f32,
    ) -> Result<WorldItem, WorldItemError> {
        self.world_items
            .spawn(kind, translation_m, yaw_rad, Some(fuel_s))
    }

    /// Adds fuel to a fire, relighting it if it was out. `None` if the item
    /// is gone or is not a fire, or `fuel_s` is not positive.
    pub fn add_fire_fuel(&mut self, item_id: u64, fuel_s: f32) -> Option<WorldItem> {
        self.world_items.add_fuel(item_id, fuel_s)
    }

    /// One tick of every lit fire: a second of fuel per second, or
    /// [`crate::world_items::BLIZZARD_BURN_RATE`] for a fire in the open
    /// during a blizzard.
    pub(super) fn burn_fires(&mut self) {
        let blizzard = self.weather.kind == spall_protocol::WeatherKind::Blizzard;
        let volume = &self.world.terrain().volume;
        self.world_items.burn(TICK_DT_S, |fire| {
            if blizzard && !crate::world_items::sheltered(volume, fire) {
                crate::world_items::BLIZZARD_BURN_RATE
            } else {
                1.0
            }
        });
    }

    /// Removes an item and queues its removal. `None` if it is already gone,
    /// so of two players reaching for one item only the first gets it.
    pub fn remove_world_item(&mut self, item_id: u64) -> Option<WorldItem> {
        self.world_items.remove(item_id)
    }

    /// Every item, in no particular order.
    pub fn world_items(&self) -> impl Iterator<Item = &WorldItem> {
        self.world_items.iter()
    }

    /// The nearest item within `reach_m` of `eye` that terrain does not hide.
    pub fn nearest_world_item(&self, eye: [f64; 3], reach_m: f64) -> Option<WorldItem> {
        crate::world_items::nearest_visible_item(
            &self.world.terrain().volume,
            eye,
            reach_m,
            self.world_items.iter(),
        )
        .copied()
    }

    /// The spawns and removals since the last call, packed for the wire.
    pub fn take_world_item_changes(&mut self) -> Vec<WorldItemUpdate> {
        self.world_items.take_changes()
    }

    /// The id counter and every item, for a checkpoint.
    pub fn world_item_state(&self) -> (u64, Vec<WorldItem>) {
        let mut items: Vec<_> = self.world_items.iter().copied().collect();
        items.sort_unstable_by_key(|item| item.item_id);
        (self.world_items.next_id(), items)
    }

    /// Installs recovered items; nothing is queued for replication.
    pub fn restore_world_items(
        &mut self,
        next_id: u64,
        items: Vec<WorldItem>,
    ) -> Result<(), WorldItemError> {
        self.world_items.restore(next_id, items)
    }
}

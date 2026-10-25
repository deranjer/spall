//! The sandbox's fires (ENG-266): a crafted fire pit set down from the pack
//! burns, and sticks or logs from the pack feed it. The engine owns the fire
//! itself (a world item with fuel: burning, heat, light, replication); this
//! is the game's side, what goes in and how much it burns.
//!
//! Tuning, to be settled in playtest: every fuel number is here.

use std::sync::{Arc, Mutex};

use spall_protocol::SessionId;
use spall_protocol::{PlayerId, WorldItem};
use spall_server::{FireRules, Fires};

use crate::game::{Inventory, ItemId, PlayerInventories, items};
use crate::progression_store::ProgressionStore;

/// Fuel a fire pit is set down with: lit, about one stick's worth.
pub const START_FUEL_S: f32 = 180.0;
/// Fuel one stick adds: about 3 minutes.
pub const STICK_FUEL_S: f32 = 180.0;
/// Fuel one log adds: about 10 minutes.
pub const LOG_FUEL_S: f32 = 600.0;
/// A fire holding this much fuel or more takes no more.
pub const FULL_FUEL_S: f32 = 1800.0;

/// Takes one fire pit `kind` from the pack; the fire's starting fuel.
pub fn place_from_pack(inventory: &mut Inventory, kind: u16) -> Result<f32, &'static str> {
    if ItemId(kind) != items::FIRE_PIT {
        return Err("that cannot be set down");
    }
    if !inventory.take(items::FIRE_PIT) {
        return Err("no fire pit in your pack");
    }
    Ok(START_FUEL_S)
}

/// Takes one log, else one stick, from the pack for `fire`; the fuel it adds.
pub fn feed_from_pack(inventory: &mut Inventory, fire: &WorldItem) -> Result<f32, &'static str> {
    if fire.fire_fuel_s.unwrap_or(0.0) >= FULL_FUEL_S {
        return Err("the fire is burning high already");
    }
    for (fuel, seconds) in [(items::WOOD_LOG, LOG_FUEL_S), (items::STICK, STICK_FUEL_S)] {
        if inventory.take(fuel) {
            return Ok(seconds);
        }
    }
    Err("no sticks or logs in your pack")
}

/// Fires fed from the server's in-memory inventories (no progression
/// database), keyed like pickup: by account, else by connection slot.
pub fn in_memory_fires(inventories: Arc<Mutex<PlayerInventories>>) -> Fires {
    struct InMemory(Arc<Mutex<PlayerInventories>>);
    impl InMemory {
        fn with<T>(
            &self,
            session: SessionId,
            player: Option<PlayerId>,
            f: impl FnOnce(&mut Inventory) -> T,
        ) -> T {
            let mut inventories = self.0.lock().unwrap_or_else(|e| e.into_inner());
            f(match player {
                Some(player) => inventories.ensure_player(player),
                None => inventories.ensure(session.slot().0),
            })
        }
    }
    impl FireRules for InMemory {
        fn place(
            &mut self,
            session: SessionId,
            player: Option<PlayerId>,
            kind: u16,
        ) -> Result<f32, &'static str> {
            self.with(session, player, |pack| place_from_pack(pack, kind))
        }
        fn refuel(
            &mut self,
            session: SessionId,
            player: Option<PlayerId>,
            fire: &WorldItem,
        ) -> Result<f32, &'static str> {
            self.with(session, player, |pack| feed_from_pack(pack, fire))
        }
    }
    Fires::new(InMemory(inventories))
}

/// Fires fed from the durable progression database. Only authenticated
/// accounts have a durable pack.
pub fn durable_fires(store: Arc<Mutex<ProgressionStore>>) -> Fires {
    struct Durable(Arc<Mutex<ProgressionStore>>);
    impl Durable {
        fn with(
            &self,
            player: Option<PlayerId>,
            f: impl FnOnce(&mut Inventory) -> Result<f32, &'static str> + Send + 'static,
        ) -> Result<f32, &'static str> {
            let player = player.ok_or("no account to keep a pack")?;
            let store = self.0.lock().unwrap_or_else(|e| e.into_inner());
            store.use_pack(player, Box::new(f)).unwrap_or_else(|error| {
                tracing::error!(?player, ?error, "could not save the pack for a fire");
                Err("the pack could not be saved")
            })
        }
    }
    impl FireRules for Durable {
        fn place(
            &mut self,
            _: SessionId,
            player: Option<PlayerId>,
            kind: u16,
        ) -> Result<f32, &'static str> {
            self.with(player, move |pack| place_from_pack(pack, kind))
        }
        fn refuel(
            &mut self,
            _: SessionId,
            player: Option<PlayerId>,
            fire: &WorldItem,
        ) -> Result<f32, &'static str> {
            let fire = *fire;
            self.with(player, move |pack| feed_from_pack(pack, &fire))
        }
    }
    Fires::new(Durable(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fire(fuel: f32) -> WorldItem {
        WorldItem {
            item_id: 1,
            kind: items::FIRE_PIT.0,
            translation_m: [0.0; 3],
            yaw_rad: 0.0,
            fire_fuel_s: Some(fuel),
        }
    }

    #[test]
    fn a_pit_is_taken_from_the_pack_once_and_only_a_pit() {
        let mut pack = Inventory::default();
        pack.grant(items::FIRE_PIT, 1).unwrap();
        pack.grant(items::STICK, 1).unwrap();
        assert!(place_from_pack(&mut pack, items::STICK.0).is_err());
        assert_eq!(
            place_from_pack(&mut pack, items::FIRE_PIT.0),
            Ok(START_FUEL_S)
        );
        assert_eq!(pack.count(items::FIRE_PIT), 0);
        assert!(place_from_pack(&mut pack, items::FIRE_PIT.0).is_err());
        assert_eq!(pack.count(items::STICK), 1, "a refusal takes nothing");
    }

    #[test]
    fn logs_feed_first_then_sticks_and_a_high_fire_takes_nothing() {
        let mut pack = Inventory::default();
        pack.grant(items::STICK, 1).unwrap();
        pack.grant(items::WOOD_LOG, 1).unwrap();
        assert_eq!(feed_from_pack(&mut pack, &fire(0.0)), Ok(LOG_FUEL_S));
        assert_eq!(feed_from_pack(&mut pack, &fire(0.0)), Ok(STICK_FUEL_S));
        assert!(feed_from_pack(&mut pack, &fire(0.0)).is_err(), "empty");
        pack.grant(items::STICK, 1).unwrap();
        assert!(feed_from_pack(&mut pack, &fire(FULL_FUEL_S)).is_err());
        assert_eq!(pack.count(items::STICK), 1, "a refusal takes nothing");
    }
}

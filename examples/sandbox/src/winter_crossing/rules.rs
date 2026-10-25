//! The Winter Crossing's server rules (ENG-251), run after every tick: greet
//! each player with the objective, answer a death by restoring the cabin's
//! items and telling that player, and declare the win for everyone the first
//! time any player stands on the tower's beacon platform (co-op).

use std::collections::{HashMap, HashSet};

use spall_protocol::{GameEvent, SessionId, session_player_entity};
use spall_server::{Audience, InitialWorldItem, TickRules};
use spall_sim::Simulation;

use super::layout::{Layout, WinZone};

/// The scenario's [`GameEvent`] kinds.
pub mod events {
    /// The objective, sent to each player once on joining. `value` is the
    /// tower's heading from the cabin in tenths of a degree.
    pub const OBJECTIVE: u16 = 1;
    /// The receiving player died and woke in the cabin.
    pub const DIED: u16 = 2;
    /// A player reached the beacon: everyone has won.
    pub const WON: u16 = 3;
}

/// A cabin item counts as still there when one of its kind lies this close
/// to its spot, metres.
pub const ITEM_STILL_THERE_M: f64 = 1.0;

/// Re-spawns each of `items` that no longer lies at its spot (nothing of its
/// kind within [`ITEM_STILL_THERE_M`]). Returns how many were spawned.
pub fn restore_items(sim: &mut Simulation, items: &[InitialWorldItem]) -> usize {
    let mut restored = 0;
    for item in items {
        let there = sim.world_items().any(|lying| {
            lying.kind == item.kind
                && (0..3)
                    .map(|i| (lying.translation_m[i] - item.translation_m[i]).powi(2))
                    .sum::<f64>()
                    <= ITEM_STILL_THERE_M * ITEM_STILL_THERE_M
        });
        if there {
            continue;
        }
        match sim.spawn_world_item(item.kind, item.translation_m, item.yaw_rad) {
            Ok(_) => restored += 1,
            Err(error) => tracing::error!(?item, ?error, "could not restore a cabin item"),
        }
    }
    restored
}

/// The scenario's [`TickRules`].
#[derive(Debug)]
pub struct ScenarioRules {
    win_zone: WinZone,
    cabin_items: Vec<InitialWorldItem>,
    heading_tenths: i32,
    greeted: HashSet<u64>,
    /// Deaths last seen per player entity.
    deaths: HashMap<u64, u32>,
    won: bool,
}

impl ScenarioRules {
    pub fn new(layout: &Layout) -> Self {
        Self {
            win_zone: layout.win_zone,
            cabin_items: layout.cabin_items.clone(),
            heading_tenths: (layout.tower_heading_deg * 10.0).round() as i32,
            greeted: HashSet::new(),
            deaths: HashMap::new(),
            won: false,
        }
    }
}

impl TickRules for ScenarioRules {
    fn after_tick(
        &mut self,
        sim: &mut Simulation,
        live: &[SessionId],
    ) -> Vec<(Audience, GameEvent)> {
        let event = |kind, value| GameEvent { kind, value };
        let mut out = Vec::new();
        let mut anyone_died = false;
        for &session in live {
            let entity = session_player_entity(session);
            let Some(player) = sim.world().player(entity) else {
                continue;
            };
            let to = Audience::Session(session);
            if self.greeted.insert(session.raw()) {
                // A new session may reuse a slot's capsule: count from now.
                self.deaths.insert(entity.get(), player.deaths);
                out.push((to, event(events::OBJECTIVE, self.heading_tenths)));
                if self.won {
                    out.push((to, event(events::WON, 0)));
                }
            }
            let seen = self.deaths.entry(entity.get()).or_insert(player.deaths);
            if player.deaths > *seen {
                *seen = player.deaths;
                anyone_died = true;
                tracing::info!(session = session.raw(), "winter crossing: player died");
                out.push((to, event(events::DIED, 0)));
            }
            if !self.won && self.win_zone.contains(player.state.position_m) {
                self.won = true;
                tracing::info!(session = session.raw(), "winter crossing: beacon reached");
                out.push((Audience::All, event(events::WON, 0)));
            }
        }
        if anyone_died {
            let restored = restore_items(sim, &self.cabin_items);
            tracing::info!(restored, "winter crossing: cabin items restored");
        }
        out
    }
}

//! What the Winter Crossing's events say on the HUD (ENG-251): the objective
//! line, the death screen and the win screen. It also keeps the tower heading
//! the objective event carries, for the cabin note.

use std::sync::atomic::{AtomicI32, Ordering};

use spall_client::window::{Banner, GameHud, HudChange};
use spall_protocol::GameEvent;

use super::rules::events;

pub const OBJECTIVE: &str = "Reach the beacon tower to the north";
/// How long the death screen shows, seconds.
const DEATH_SCREEN_S: f32 = 6.0;
/// How long the win screen shows, seconds.
const WIN_SCREEN_S: f32 = 20.0;

/// No heading has arrived yet.
const NO_HEADING: i32 = i32::MIN;

#[derive(Debug)]
pub struct ScenarioHud {
    heading_tenths: AtomicI32,
}

impl Default for ScenarioHud {
    fn default() -> Self {
        Self {
            heading_tenths: AtomicI32::new(NO_HEADING),
        }
    }
}

impl ScenarioHud {
    /// The tower's heading from the cabin, degrees, once the server has sent
    /// the objective; `None` outside the scenario.
    pub fn tower_heading_deg(&self) -> Option<f32> {
        match self.heading_tenths.load(Ordering::Relaxed) {
            NO_HEADING => None,
            tenths => Some(tenths as f32 / 10.0),
        }
    }
}

impl GameHud for ScenarioHud {
    fn on_event(&self, event: GameEvent) -> HudChange {
        match event.kind {
            events::OBJECTIVE => {
                self.heading_tenths.store(event.value, Ordering::Relaxed);
                HudChange::Objective(OBJECTIVE.into())
            }
            events::DIED => HudChange::Banner(Banner {
                title: "You died".into(),
                detail: "You wake in the cabin. Its supplies are back.".into(),
                seconds: Some(DEATH_SCREEN_S),
            }),
            events::WON => HudChange::Banner(Banner {
                title: "The beacon is reached".into(),
                detail: "Your party made the crossing. You win!".into(),
                seconds: Some(WIN_SCREEN_S),
            }),
            _ => HudChange::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_objective_carries_the_heading_and_each_event_has_its_screen() {
        let hud = ScenarioHud::default();
        assert_eq!(hud.tower_heading_deg(), None);
        let change = hud.on_event(GameEvent {
            kind: events::OBJECTIVE,
            value: 3575,
        });
        assert_eq!(change, HudChange::Objective(OBJECTIVE.into()));
        assert_eq!(hud.tower_heading_deg(), Some(357.5));
        for kind in [events::DIED, events::WON] {
            assert!(matches!(
                hud.on_event(GameEvent { kind, value: 0 }),
                HudChange::Banner(_)
            ));
        }
        assert_eq!(
            hud.on_event(GameEvent { kind: 99, value: 0 }),
            HudChange::None
        );
    }
}

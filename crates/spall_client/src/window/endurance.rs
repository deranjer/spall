//! Per-frame local-player endurance: drives the HUD bar and the breathing voice
//! from the predicted character state.

use super::hud::endurance_view;
use super::*;
use crate::breathing::select_state;

impl InteractiveApp {
    pub(super) fn update_endurance(&mut self) {
        let state = if self.fly_eye.is_some() {
            None
        } else {
            self.session
                .view
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|view| view.predicted)
        };
        let Some(state) = state else {
            self.endurance_view = None;
            return;
        };
        self.endurance_view = endurance_view(state.endurance, state.exhausted);
        // Running faster than a walk is sprinting (jump arcs keep takeoff speed too).
        let speed = state.velocity_m_s[0].hypot(state.velocity_m_s[2]);
        let sprinting = speed > spall_physics::character::tuning::WALK_SPEED_M_S + 0.5;
        let breathing = select_state(state.endurance, state.exhausted, sprinting);
        if breathing != self.breathing_state {
            if self.breathing_log {
                eprintln!(
                    "spall-breathing: {:?} -> {breathing:?} (endurance {:.2}, exhausted {}, speed {speed:.1} m/s)",
                    self.breathing_state, state.endurance, state.exhausted
                );
            }
            self.breathing_state = breathing;
        }
        if self.breathing_log && self.breathing_tick.elapsed() >= Duration::from_secs(1) {
            self.breathing_tick = Instant::now();
            eprintln!(
                "spall-breathing: endurance {:.2} exhausted {} speed {speed:.1} m/s",
                state.endurance, state.exhausted
            );
        }
        if let Some(audio) = &self.breathing {
            audio.set_state(breathing);
        }
    }
}

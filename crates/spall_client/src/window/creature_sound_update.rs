//! Per-frame creature sounds: reads each creature's presented tag and place
//! from the replica and plays the game's cues, panned, through the ambience
//! audio worker.

use super::*;
use crate::creature_sounds::HeardCreature;

impl InteractiveApp {
    pub(super) fn update_creature_sounds(&mut self, now: Instant) {
        if self.biome_audio.creatures.cues.is_empty() {
            return;
        }
        let Some(audio) = &self.ambience else {
            return;
        };
        // Heard from the flying camera while flying, else from the player.
        let listener = match self.fly_eye {
            Some(eye) => Some([f64::from(eye.x), f64::from(eye.y), f64::from(eye.z)]),
            None => self
                .session
                .view
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|view| view.predicted.position_m),
        };
        let Some(listener) = listener else {
            return;
        };
        // A frame that finds the replica busy hears nothing new; the next one
        // sees the same tag changes.
        let Some(replica) = self.session.replica.get().and_then(|r| r.try_lock().ok()) else {
            return;
        };
        let creatures = replica.creatures();
        let heard: Vec<HeardCreature> = creatures
            .records()
            .map(|record| {
                let shown = creatures.presentation(record, now);
                HeardCreature {
                    id: record.creature_id,
                    species: &record.species,
                    tag: shown.locomotion_tag,
                    at_m: shown.translation_m,
                }
            })
            .collect();
        let right = [f64::from(self.yaw.cos()), f64::from(self.yaw.sin())];
        let events =
            self.creature_sounds
                .update(&self.biome_audio.creatures, now, listener, right, &heard);
        for event in events {
            // A full worker queue drops the sound; the next one plays.
            let _ = audio.play_panned(event.path, event.gain, event.pan, 1.0);
        }
    }
}

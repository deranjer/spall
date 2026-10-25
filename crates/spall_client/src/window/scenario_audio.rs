//! Scenario sounds in the window: fires, ice, tools, ladders, doors, items and
//! the player's body, played through a second ambience audio worker. The
//! decisions live in [`crate::scenario_sounds`]; this reads the client's state
//! each frame and plays what they ask for.

use super::*;
use crate::scenario_sounds::{BodyTracker, Dice, FireTracker, SoundEvent, SoundKind};
use footstep_update::ground_below;

/// How long after a break a plunge into water gasps.
const GASP_AFTER_BREAK: Duration = Duration::from_secs(10);
/// Wetness (of 255) above which entering water gasps.
const GASP_WETNESS: u8 = 64;
/// How long after an axe swing a body split off the terrain counts as the
/// tree coming down.
const FALL_AFTER_SWING: Duration = Duration::from_secs(4);
/// Seconds between creaks underfoot, and the horizontal speed they need.
const CREAK_GAP_S: (f32, f32) = (2.5, 6.0);
const CREAK_MIN_SPEED: f32 = 0.6;
/// Vertical speed (m/s) above which a ladder climb makes noise, and below
/// which it is a slide; seconds between climb sounds, and between slide ones.
const CLIMB_SPEED: f32 = 0.4;
const SLIDE_SPEED: f32 = -1.5;
const CLIMB_GAP: Duration = Duration::from_millis(3000);
const SLIDE_GAP: Duration = Duration::from_millis(1100);
/// Fewest milliseconds between mix updates.
const MIX_GAP: Duration = Duration::from_millis(200);

pub(super) struct ScenarioAudio {
    audio: Option<AmbienceAudio>,
    dice: Dice,
    fires: FireTracker,
    body: BodyTracker,
    /// Loop layers last sent, volumes in twentieths.
    layers: Vec<(PathBuf, u8)>,
    layers_at: Option<Instant>,
    swimming: bool,
    broke_at: Option<Instant>,
    creak_at: Option<Instant>,
    /// The last tool swing and where it landed.
    swung: Option<(Instant, [f64; 3])>,
    ladder_at: Option<Instant>,
    note_open: bool,
    /// Set by the input handler, which cannot borrow the whole app, and played
    /// by the next update.
    pub(super) pending_interact: Option<String>,
    pub(super) pending_swing: Option<(u16, Vec3, Vec3, Instant)>,
}

impl ScenarioAudio {
    /// Starts the worker when the game has scenario sounds.
    pub(super) fn start(wanted: bool) -> Self {
        Self {
            audio: wanted.then(|| AmbienceAudio::start(0.9)).flatten(),
            dice: Dice::new(std::process::id().into()),
            fires: FireTracker::default(),
            body: BodyTracker::default(),
            layers: Vec::new(),
            layers_at: None,
            swimming: false,
            broke_at: None,
            creak_at: None,
            swung: None,
            ladder_at: None,
            note_open: false,
            pending_interact: None,
            pending_swing: None,
        }
    }

    fn play(&self, event: Option<SoundEvent>) {
        if let (Some(audio), Some(event)) = (&self.audio, event) {
            // A full worker queue drops the sound; the next one plays.
            let _ = audio.play_panned(event.path, event.gain, event.pan, event.rate);
        }
    }
}

impl InteractiveApp {
    /// A click in the Esc menu or the pack.
    pub(super) fn scenario_ui_click(&mut self) {
        let event = self
            .biome_audio
            .scenario
            .ui_click
            .play(&mut self.scenario.dice);
        self.scenario.play(event);
    }

    /// E was pressed on `prompt` (see `interaction_prompt`): a door or a pickup.
    fn scenario_interact(&mut self, prompt: &str) {
        let sounds = &self.biome_audio.scenario;
        let dice = &mut self.scenario.dice;
        let action = prompt.trim_start_matches('E').trim();
        let events = if action.starts_with("Open") {
            vec![sounds.door_latch.play(dice), sounds.door_open.play(dice)]
        } else if action.starts_with("Close") {
            vec![sounds.door_close.play(dice)]
        } else if action.starts_with("Pick up") {
            vec![sounds.pickup_clip(prompt).and_then(|clip| clip.play(dice))]
        } else {
            Vec::new()
        };
        for event in events {
            self.scenario.play(event);
        }
    }

    /// A swing of `tool` from `eye` along `dir`: the sound depends on what it
    /// strikes.
    fn scenario_tool_swing(&mut self, tool: u16, eye: Vec3, dir: Vec3, now: Instant) {
        let Some(replica) = self.session.replica.get().and_then(|r| r.try_lock().ok()) else {
            return;
        };
        let Some((cell, material)) = replica
            .terrain_volume()
            .and_then(|volume| cast_hammer_ray(volume, eye, dir))
        else {
            return;
        };
        drop(replica);
        let sounds = &self.biome_audio.scenario;
        let event = sounds
            .dig_clip(tool, material)
            .and_then(|clip| clip.play(&mut self.scenario.dice));
        let cell_m = f64::from(CELL_M);
        let at = [cell.x, cell.y, cell.z].map(|c| (c as f64 + 0.5) * cell_m);
        self.scenario.swung = event.is_some().then_some((now, at));
        self.scenario.play(event);
    }

    pub(super) fn update_scenario_audio(&mut self, now: Instant) {
        if self.scenario.audio.is_none() {
            return;
        }
        if let Some(prompt) = self.scenario.pending_interact.take() {
            self.scenario_interact(&prompt);
        }
        if let Some((tool, eye, dir, at)) = self.scenario.pending_swing.take() {
            self.scenario_tool_swing(tool, eye, dir, at);
        }
        let (view, water) = (
            *self.session.view.lock().unwrap_or_else(|e| e.into_inner()),
            self.session
                .column_surface
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .query(),
        );
        let listener = match self.fly_eye {
            Some(eye) => Some([f64::from(eye.x), f64::from(eye.y), f64::from(eye.z)]),
            None => view.map(|view| view.predicted.position_m),
        };
        let Some(listener) = listener else {
            return;
        };
        let right = [f64::from(self.yaw.cos()), f64::from(self.yaw.sin())];
        let sounds = &self.biome_audio.scenario;
        let scenario = &mut self.scenario;

        let fires = scenario.fires.update(
            sounds,
            &mut scenario.dice,
            listener,
            right,
            &self.last_fires,
        );
        for event in fires.events {
            scenario.play(Some(event));
        }

        // Ice: cracks and breaks arrive as ordinary terrain edits.
        let mut on_ice = false;
        let mut tree_fell = None;
        if let Some(mut replica) = self.session.replica.get().and_then(|r| r.try_lock().ok()) {
            let edits = replica.take_terrain_edits();
            let heard = crate::scenario_sounds::edit_sounds(
                sounds,
                &mut scenario.dice,
                listener,
                right,
                &edits.edits,
            );
            if heard.broke {
                scenario.broke_at = Some(now);
            }
            for event in heard.events {
                scenario.play(Some(event));
            }
            if edits.splits > 0
                && let Some((at, place)) = scenario.swung
                && now.saturating_duration_since(at) <= FALL_AFTER_SWING
            {
                tree_fell = Some(place);
            }
            if self.fly_eye.is_none()
                && let Some(view) = &view
                && view.predicted.grounded
                && let Some(volume) = replica.terrain_volume()
            {
                on_ice = ground_below(volume, view.predicted.position_m)
                    .is_some_and(|(_, material)| sounds.ice_materials.contains(&material));
            }
        }
        if let Some(place) = tree_fell {
            scenario.swung = None;
            let event = sounds
                .tree_fall
                .play_at(&mut scenario.dice, listener, right, place);
            scenario.play(event);
        }

        if let Some(view) = &view
            && self.fly_eye.is_none()
        {
            let [vx, vy, vz] = view.predicted.velocity_m_s;
            // Creaking ice underfoot while walking on it.
            if on_ice && vx.hypot(vz) > CREAK_MIN_SPEED {
                let due = scenario.creak_at.get_or_insert(now);
                if now >= *due {
                    let (low, high) = CREAK_GAP_S;
                    let gap = low + (high - low) * scenario.dice.unit();
                    scenario.creak_at = Some(now + Duration::from_secs_f32(gap));
                    let event = sounds.ice_creak.play(&mut scenario.dice);
                    scenario.play(event);
                }
            } else {
                scenario.creak_at = None;
            }
            // Ladders: a climb sound while going up, a quick one sliding down.
            let gap_over = scenario.ladder_at.is_none_or(|at| now >= at);
            if view.predicted.climbing && gap_over && !(SLIDE_SPEED..=CLIMB_SPEED).contains(&vy) {
                let (clip, gap) = if vy < SLIDE_SPEED {
                    (&sounds.ladder_slide, SLIDE_GAP)
                } else {
                    (&sounds.ladder_climb, CLIMB_GAP)
                };
                scenario.ladder_at = Some(now + gap);
                let event = clip.play(&mut scenario.dice);
                scenario.play(event);
            }
            // Into the water: a splash, and a gasp if already wet or just
            // through the ice.
            let swimming = player_swims(&water, view.predicted.position_m);
            if swimming && !scenario.swimming {
                let wet = self
                    .player_health
                    .is_some_and(|health| health.wetness_q8 > GASP_WETNESS);
                let broke = scenario
                    .broke_at
                    .is_some_and(|at| now.saturating_duration_since(at) <= GASP_AFTER_BREAK);
                let splash = sounds.splash.play(&mut scenario.dice);
                scenario.play(splash);
                if wet || broke {
                    let gasp = sounds.gasp.play(&mut scenario.dice);
                    scenario.play(gasp);
                }
            }
            scenario.swimming = swimming;
        }

        // Reading a note unfolds it.
        let reading = self.reading_note.is_some();
        if reading && !scenario.note_open {
            let event = sounds.note.play(&mut scenario.dice);
            scenario.play(event);
        }
        scenario.note_open = reading;

        // The body: hurt and death sounds, and shivering and heartbeat beds.
        let body = scenario.body.update(self.player_health, now);
        for kind in body.events {
            let clip = match kind {
                SoundKind::Hurt => &sounds.hurt,
                SoundKind::Death => &sounds.death,
            };
            let event = clip.play(&mut scenario.dice);
            scenario.play(event);
        }

        // Loop beds, one layer each: fire, underwater, shivering, heartbeat.
        if scenario
            .layers_at
            .is_some_and(|at| now.saturating_duration_since(at) < MIX_GAP)
        {
            return;
        }
        let mut mix: Vec<(PathBuf, f32)> = Vec::new();
        let mut bed = |clip: &crate::scenario_sounds::Clip, level: f32| {
            if let Some(path) = clip.files.first()
                && level > 0.02
            {
                mix.push((path.clone(), level.clamp(0.0, 1.0)));
            }
        };
        bed(&sounds.fire_loop, fires.bed);
        bed(
            &sounds.underwater_loop,
            sounds.underwater_loop.gain * self.weather_submerge,
        );
        bed(&sounds.shiver_loop, sounds.shiver_loop.gain * body.shiver);
        if let Some((fast, level)) = body.heartbeat {
            let clip = if fast {
                &sounds.heartbeat_fast
            } else {
                &sounds.heartbeat_slow
            };
            bed(clip, clip.gain * level);
        }
        let quantized: Vec<(PathBuf, u8)> = mix
            .iter()
            .map(|(path, level)| (path.clone(), (level * 20.0).round() as u8))
            .collect();
        if quantized != scenario.layers
            && let Some(audio) = &scenario.audio
            && audio.set_mix(mix).is_ok()
        {
            scenario.layers = quantized;
            scenario.layers_at = Some(now);
        }
    }
}

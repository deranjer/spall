//! Sounds for a survival scenario: fire, ice, tools, items, doors, ladders and
//! the player's body. The game supplies a [`ScenarioSounds`] table; this
//! module decides, from what the client can see (the replicated fires, the
//! server's health record, terrain edits), which sound plays and how loud.
//! Playback goes through the ambience audio worker, as creature sounds do.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spall_core::MaterialId;
use spall_protocol::{PlayerHealth, WorldItem};

use crate::creature_sounds::heard;
use crate::replica::TerrainEdit;

/// A sound with a few recordings, one picked at random each time.
#[derive(Debug, Clone)]
pub struct Clip {
    pub files: Vec<PathBuf>,
    /// Loudness, 0..=1.
    pub gain: f32,
    /// Playback rate: 1 as recorded.
    pub rate: f32,
    /// Distance (metres) at which a positional sound has faded to silence;
    /// 0 plays it at the listener with no fade.
    pub range_m: f32,
}

impl Default for Clip {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            gain: 1.0,
            rate: 1.0,
            range_m: 0.0,
        }
    }
}

/// A sound to play now.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundEvent {
    pub path: PathBuf,
    pub gain: f32,
    pub pan: f32,
    pub rate: f32,
}

/// Which clip a tool swing makes, by the material under the cursor.
#[derive(Debug, Clone)]
pub struct DigRule {
    pub tool: u16,
    /// Materials it applies to; empty means any.
    pub materials: Vec<MaterialId>,
    pub clip: Clip,
}

/// Which clip picking something up makes, by words in its prompt.
#[derive(Debug, Clone)]
pub struct PickupRule {
    /// Lower-case words; empty means anything.
    pub words: Vec<String>,
    pub clip: Clip,
}

/// The game's sound table. An empty clip is silent.
#[derive(Debug, Clone, Default)]
pub struct ScenarioSounds {
    pub fire_loop: Clip,
    pub fire_ignite: Clip,
    pub fire_flare: Clip,
    pub fire_out: Clip,
    /// Ice and cracked ice: what a break (to air) and a creak underfoot mean.
    pub ice_materials: Vec<MaterialId>,
    /// The material a warning crack writes.
    pub ice_cracked: Option<MaterialId>,
    pub ice_creak: Clip,
    pub ice_crack: Clip,
    pub ice_break: Clip,
    pub splash: Clip,
    pub gasp: Clip,
    pub underwater_loop: Clip,
    pub digs: Vec<DigRule>,
    pub tree_fall: Clip,
    pub pickups: Vec<PickupRule>,
    pub note: Clip,
    pub door_open: Clip,
    pub door_latch: Clip,
    pub door_close: Clip,
    pub ladder_climb: Clip,
    pub ladder_slide: Clip,
    pub shiver_loop: Clip,
    pub hurt: Clip,
    pub death: Clip,
    pub heartbeat_slow: Clip,
    pub heartbeat_fast: Clip,
    pub ui_click: Clip,
}

impl ScenarioSounds {
    pub fn is_empty(&self) -> bool {
        self.fire_loop.files.is_empty() && self.digs.is_empty() && self.hurt.files.is_empty()
    }

    /// The tool clip for a swing at `material`, if any.
    pub fn dig_clip(&self, tool: u16, material: MaterialId) -> Option<&Clip> {
        self.digs
            .iter()
            .find(|rule| {
                rule.tool == tool
                    && (rule.materials.is_empty() || rule.materials.contains(&material))
            })
            .map(|rule| &rule.clip)
    }

    /// The pickup clip for a prompt such as "E  Pick up stone".
    pub fn pickup_clip(&self, prompt: &str) -> Option<&Clip> {
        let prompt = prompt.to_lowercase();
        self.pickups
            .iter()
            .find(|rule| {
                rule.words.is_empty() || rule.words.iter().any(|word| prompt.contains(word))
            })
            .map(|rule| &rule.clip)
    }
}

/// Dependency-free random numbers (splitmix64) for picking recordings.
#[derive(Debug)]
pub struct Dice(u64);

impl Dice {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// A number in 0..1.
    pub fn unit(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
    }
}

impl Clip {
    /// The clip heard at the listener, or `None` with no recordings.
    pub fn play(&self, dice: &mut Dice) -> Option<SoundEvent> {
        self.event(dice, 1.0, 0.0)
    }

    fn event(&self, dice: &mut Dice, loudness: f32, pan: f32) -> Option<SoundEvent> {
        let index = (dice.unit() * self.files.len() as f32) as usize;
        let path = self
            .files
            .get(index.min(self.files.len().saturating_sub(1)))?;
        Some(SoundEvent {
            path: path.clone(),
            gain: (self.gain * loudness).clamp(0.0, 1.0),
            pan,
            rate: self.rate,
        })
    }

    /// The clip heard from `at_m` by a listener at `listener_m` whose
    /// right-hand axis is `right` (x, z), or `None` out of range.
    pub fn play_at(
        &self,
        dice: &mut Dice,
        listener_m: [f64; 3],
        right: [f64; 2],
        at_m: [f64; 3],
    ) -> Option<SoundEvent> {
        if self.range_m <= 0.0 {
            return self.play(dice);
        }
        let (loudness, pan) = heard_from(listener_m, right, at_m, self.range_m)?;
        self.event(dice, loudness, pan)
    }

    /// How loud a looping bed at `at_m` is for the listener, 0 out of range.
    pub fn bed_level(&self, listener_m: [f64; 3], at_m: [f64; 3]) -> f32 {
        heard_from(listener_m, [1.0, 0.0], at_m, self.range_m).map_or(0.0, |(l, _)| l * self.gain)
    }
}

fn heard_from(
    listener_m: [f64; 3],
    right: [f64; 2],
    at_m: [f64; 3],
    range_m: f32,
) -> Option<(f32, f32)> {
    let d = [
        at_m[0] - listener_m[0],
        at_m[1] - listener_m[1],
        at_m[2] - listener_m[2],
    ];
    let distance = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() as f32;
    heard(
        distance,
        (d[0] * right[0] + d[2] * right[1]) as f32,
        range_m,
    )
}

/// Remembers each fire's state to notice it being lit, put out or fed.
#[derive(Debug, Default)]
pub struct FireTracker {
    seen: HashMap<u64, (bool, f32)>,
}

/// Fuel added (seconds) that makes a lit fire flare up.
const FLARE_FUEL_S: f32 = 5.0;

/// What the fires sound like this frame.
#[derive(Debug, Default, PartialEq)]
pub struct FireSounds {
    pub events: Vec<SoundEvent>,
    /// Loudness of the crackle bed: the nearest lit fire's.
    pub bed: f32,
}

impl FireTracker {
    /// A fire seen for the first time makes no sound: it did not just change.
    pub fn update(
        &mut self,
        sounds: &ScenarioSounds,
        dice: &mut Dice,
        listener_m: [f64; 3],
        right: [f64; 2],
        fires: &[WorldItem],
    ) -> FireSounds {
        self.seen
            .retain(|id, _| fires.iter().any(|fire| fire.item_id == *id));
        let mut out = FireSounds::default();
        for fire in fires {
            let fuel = fire.fire_fuel_s.unwrap_or(0.0);
            let lit = fire.is_lit();
            if lit {
                out.bed = out
                    .bed
                    .max(sounds.fire_loop.bed_level(listener_m, fire.translation_m));
            }
            let clip = match self.seen.insert(fire.item_id, (lit, fuel)) {
                None => continue,
                Some((was_lit, was_fuel)) => match (was_lit, lit) {
                    (false, true) => &sounds.fire_ignite,
                    (true, false) => &sounds.fire_out,
                    (true, true) if fuel - was_fuel >= FLARE_FUEL_S => &sounds.fire_flare,
                    _ => continue,
                },
            };
            out.events
                .extend(clip.play_at(dice, listener_m, right, fire.translation_m));
        }
        out
    }
}

/// What the player's body sounds like this frame.
#[derive(Debug, Default, PartialEq)]
pub struct BodySounds {
    pub events: Vec<SoundKind>,
    /// Loudness of the shivering bed, 0..=1.
    pub shiver: f32,
    /// The heartbeat bed: fast or slow, and its loudness.
    pub heartbeat: Option<(bool, f32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundKind {
    Hurt,
    Death,
}

/// Warmth (of 255) below which the player shivers; louder as it falls.
const SHIVER_BELOW: f32 = 130.0;
/// Health fractions below which the heartbeat is heard, and turns fast.
const HEARTBEAT_BELOW: f32 = 0.5;
const HEARTBEAT_FAST_BELOW: f32 = 0.25;
/// A health drop of at least this fraction of the maximum is a hurt sound.
const HURT_FRACTION: f32 = 0.05;
/// Fewest seconds between hurt sounds.
const HURT_SPACING: Duration = Duration::from_millis(1200);
/// A jump up of at least this fraction of the maximum is a death and respawn.
const RESPAWN_FRACTION: f32 = 0.5;

/// Turns the server's health records into sounds.
#[derive(Debug, Default)]
pub struct BodyTracker {
    last: Option<PlayerHealth>,
    hurt_at: Option<Instant>,
}

impl BodyTracker {
    pub fn update(&mut self, health: Option<PlayerHealth>, now: Instant) -> BodySounds {
        let mut out = BodySounds::default();
        let Some(health) = health.filter(|h| h.max_health > 0) else {
            return out;
        };
        let max = f32::from(health.max_health);
        if let Some(last) = self.last.filter(|last| *last != health) {
            let change = (f32::from(health.health) - f32::from(last.health)) / max;
            if change >= RESPAWN_FRACTION {
                out.events.push(SoundKind::Death);
            } else if change <= -HURT_FRACTION
                && self
                    .hurt_at
                    .is_none_or(|at| now.saturating_duration_since(at) >= HURT_SPACING)
            {
                self.hurt_at = Some(now);
                out.events.push(SoundKind::Hurt);
            }
        }
        self.last = Some(health);
        let warmth = f32::from(health.warmth_q8);
        out.shiver = ((SHIVER_BELOW - warmth) / SHIVER_BELOW).clamp(0.0, 1.0);
        let fraction = f32::from(health.health) / max;
        if fraction < HEARTBEAT_BELOW {
            let level = ((HEARTBEAT_BELOW - fraction) / HEARTBEAT_BELOW).clamp(0.0, 1.0);
            out.heartbeat = Some((fraction < HEARTBEAT_FAST_BELOW, 0.3 + 0.7 * level));
        }
        out
    }
}

/// What terrain edits sound like to a listener.
#[derive(Debug, Default, PartialEq)]
pub struct EditSounds {
    pub events: Vec<SoundEvent>,
    /// Ice broke within earshot.
    pub broke: bool,
}

/// The nearest warning crack and the nearest break among `edits` (one
/// transaction writes many cells; one sound is enough).
pub fn edit_sounds(
    sounds: &ScenarioSounds,
    dice: &mut Dice,
    listener_m: [f64; 3],
    right: [f64; 2],
    edits: &[TerrainEdit],
) -> EditSounds {
    let cell_m = f64::from(crate::predict::CELL_M);
    let at = |edit: &TerrainEdit| {
        [
            (edit.cell.x as f64 + 0.5) * cell_m,
            (edit.cell.y as f64 + 0.5) * cell_m,
            (edit.cell.z as f64 + 0.5) * cell_m,
        ]
    };
    let nearest = |pick: &dyn Fn(&TerrainEdit) -> bool| {
        edits
            .iter()
            .filter(|edit| pick(edit))
            .map(at)
            .min_by(|a, b| {
                let d = |p: &[f64; 3]| (0..3).map(|i| (p[i] - listener_m[i]).powi(2)).sum::<f64>();
                d(a).total_cmp(&d(b))
            })
    };
    let mut out = EditSounds::default();
    let cracked = sounds.ice_cracked;
    if let Some(place) = nearest(&|e| Some(e.to) == cracked && !sounds.ice_materials.is_empty())
        && let Some(event) = sounds.ice_crack.play_at(dice, listener_m, right, place)
    {
        out.events.push(event);
    }
    let broken = nearest(&|e| e.to == MaterialId::AIR && sounds.ice_materials.contains(&e.from));
    if let Some(event) =
        broken.and_then(|place| sounds.ice_break.play_at(dice, listener_m, right, place))
    {
        out.broke = true;
        out.events.push(event);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::GlobalCell;

    fn clip(name: &str, range_m: f32) -> Clip {
        Clip {
            files: vec![PathBuf::from(name)],
            gain: 0.8,
            rate: 1.0,
            range_m,
        }
    }

    fn table() -> ScenarioSounds {
        ScenarioSounds {
            fire_loop: clip("loop", 20.0),
            fire_ignite: clip("ignite", 30.0),
            fire_out: clip("out", 30.0),
            fire_flare: clip("flare", 30.0),
            ice_materials: vec![MaterialId(223), MaterialId(224)],
            ice_cracked: Some(MaterialId(224)),
            ice_crack: clip("crack", 40.0),
            ice_break: clip("break", 40.0),
            digs: vec![
                DigRule {
                    tool: 6,
                    materials: vec![MaterialId(217)],
                    clip: clip("snow", 0.0),
                },
                DigRule {
                    tool: 6,
                    materials: vec![],
                    clip: clip("dirt", 0.0),
                },
                DigRule {
                    tool: 7,
                    materials: vec![MaterialId(3)],
                    clip: clip("chop", 0.0),
                },
            ],
            pickups: vec![
                PickupRule {
                    words: vec!["stone".into()],
                    clip: clip("stone", 0.0),
                },
                PickupRule {
                    words: vec![],
                    clip: clip("item", 0.0),
                },
            ],
            ..Default::default()
        }
    }

    fn fire(id: u64, x: f64, fuel: f32) -> WorldItem {
        WorldItem {
            item_id: id,
            kind: 0,
            translation_m: [x, 0.0, 0.0],
            yaw_rad: 0.0,
            fire_fuel_s: Some(fuel),
        }
    }

    fn names(events: &[SoundEvent]) -> Vec<&str> {
        events.iter().map(|e| e.path.to_str().unwrap()).collect()
    }

    #[test]
    fn rules_pick_the_first_match() {
        let table = table();
        let name = |clip: Option<&Clip>| clip.map(|c| c.files[0].to_str().unwrap().to_owned());
        assert_eq!(
            name(table.dig_clip(6, MaterialId(217))),
            Some("snow".into())
        );
        assert_eq!(name(table.dig_clip(6, MaterialId(2))), Some("dirt".into()));
        assert_eq!(name(table.dig_clip(7, MaterialId(3))), Some("chop".into()));
        assert_eq!(name(table.dig_clip(7, MaterialId(2))), None);
        assert_eq!(
            name(table.pickup_clip("E  Pick up Stone")),
            Some("stone".into())
        );
        assert_eq!(
            name(table.pickup_clip("E  Pick up stick")),
            Some("item".into())
        );
    }

    #[test]
    fn fires_sound_when_they_change_not_when_first_seen() {
        let table = table();
        let (mut tracker, mut dice) = (FireTracker::default(), Dice::new(1));
        let mut step =
            |fires: &[WorldItem]| tracker.update(&table, &mut dice, [0.0; 3], [1.0, 0.0], fires);
        // Seen unlit, then lit: ignites. A far fire is silent, a near one is not.
        assert!(
            step(&[fire(1, 5.0, 0.0), fire(2, 500.0, 0.0)])
                .events
                .is_empty()
        );
        let lit = step(&[fire(1, 5.0, 60.0), fire(2, 500.0, 60.0)]);
        assert_eq!(names(&lit.events), ["ignite"]);
        assert!(lit.bed > 0.0 && lit.events[0].pan > 0.9);
        // Burning down is quiet, feeding flares, going out hisses.
        assert!(step(&[fire(1, 5.0, 50.0)]).events.is_empty());
        assert_eq!(names(&step(&[fire(1, 5.0, 90.0)]).events), ["flare"]);
        let out = step(&[fire(1, 5.0, 0.0)]);
        assert_eq!(names(&out.events), ["out"]);
        assert_eq!(out.bed, 0.0);
    }

    #[test]
    fn the_crackle_bed_fades_with_distance() {
        let table = table();
        let level = |x| table.fire_loop.bed_level([0.0; 3], [x, 0.0, 0.0]);
        assert!(level(1.0) > level(10.0) && level(10.0) > 0.0);
        assert_eq!(level(20.0), 0.0);
    }

    #[test]
    fn health_records_make_hurt_and_death_sounds_and_beds() {
        let health = |health, warmth| {
            Some(PlayerHealth {
                health,
                max_health: 100,
                warmth_q8: warmth,
                wetness_q8: 0,
            })
        };
        let mut body = BodyTracker::default();
        let t0 = Instant::now();
        // The first record is not a change.
        assert!(body.update(health(100, 255), t0).events.is_empty());
        assert_eq!(body.update(health(100, 255), t0).shiver, 0.0);
        // A small tick of freezing damage is quiet; a real hit is not, and
        // not twice in a row.
        assert!(body.update(health(98, 255), t0).events.is_empty());
        assert_eq!(body.update(health(60, 255), t0).events, [SoundKind::Hurt]);
        let soon = t0 + Duration::from_millis(300);
        assert!(body.update(health(40, 255), soon).events.is_empty());
        // Low health beats, fast when very low; cold shivers.
        let low = body.update(health(30, 20), soon);
        assert_eq!(low.heartbeat.map(|(fast, _)| fast), Some(false));
        assert!(low.shiver > 0.8);
        assert_eq!(
            body.update(health(10, 255), soon)
                .heartbeat
                .map(|(fast, _)| fast),
            Some(true)
        );
        // Back to full health at once is a respawn.
        assert_eq!(
            body.update(health(100, 255), soon).events,
            [SoundKind::Death]
        );
        assert_eq!(body.update(health(100, 255), soon).heartbeat, None);
    }

    #[test]
    fn ice_cracks_and_breaks_are_told_from_other_edits() {
        let table = table();
        let edit = |x, from, to| TerrainEdit {
            cell: GlobalCell::new(x, 0, 0),
            from: MaterialId(from),
            to: MaterialId(to),
        };
        let mut dice = Dice::new(3);
        let mut hear =
            |edits: &[TerrainEdit]| edit_sounds(&table, &mut dice, [0.0; 3], [1.0, 0.0], edits);
        let cracked = hear(&[edit(8, 223, 224), edit(9, 223, 224)]);
        assert_eq!(
            (names(&cracked.events), cracked.broke),
            (vec!["crack"], false)
        );
        let broke = hear(&[edit(8, 224, 0), edit(9, 223, 0)]);
        assert_eq!((names(&broke.events), broke.broke), (vec!["break"], true));
        // Digging dirt, filling with ice, or a break far away make no sound.
        assert!(hear(&[edit(8, 2, 0), edit(8, 0, 223)]).events.is_empty());
        assert!(hear(&[edit(4000, 223, 0)]).events.is_empty());
    }

    #[test]
    fn an_empty_clip_is_silent_and_gain_stays_in_range() {
        let mut dice = Dice::new(9);
        assert_eq!(Clip::default().play(&mut dice), None);
        let loud = Clip {
            gain: 3.0,
            ..clip("x", 0.0)
        };
        assert_eq!(loud.play(&mut dice).unwrap().gain, 1.0);
    }
}

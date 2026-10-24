//! Creature sounds: one-shots heard from where an animal is, started when its
//! replicated locomotion tag changes, and repeated now and then while a tag is
//! held. The game supplies the cues; the client decides what the listener
//! hears and how loud, and panned to which side.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// One game-owned sound for a species doing something.
#[derive(Debug, Clone)]
pub struct CreatureCue {
    pub species: String,
    /// The locomotion tag that starts it.
    pub tag: u8,
    /// Recordings, one picked at random each time.
    pub sounds: Vec<PathBuf>,
    /// Played instead of `sounds` when another creature of the species within
    /// [`CHORUS_M`] already had this tag: a pack joining a howl.
    pub chorus: Vec<PathBuf>,
    /// Distance (metres) at which it has faded to silence.
    pub range_m: f32,
    pub gain: f32,
    /// While the tag is held, plays again after a random wait in this range
    /// (seconds).
    pub repeat_s: Option<(f32, f32)>,
    /// Chance each start or repeat is heard at all.
    pub chance: f32,
}

#[derive(Debug, Clone, Default)]
pub struct CreatureSoundConfig {
    pub cues: Vec<CreatureCue>,
}

/// Another creature this close with the same tag makes it a chorus.
pub const CHORUS_M: f64 = 40.0;
/// Most sounds started in one update, so a whole herd changing gait at once
/// does not flood the audio worker.
const MAX_EVENTS: usize = 6;

/// A creature as the client presents it this frame.
#[derive(Debug, Clone, Copy)]
pub struct HeardCreature<'a> {
    pub id: u64,
    pub species: &'a str,
    pub tag: u8,
    pub at_m: [f64; 3],
}

/// A sound to play now, at a fixed loudness and pan (-1 left to 1 right).
#[derive(Debug, Clone, PartialEq)]
pub struct CreatureSoundEvent {
    pub path: PathBuf,
    pub gain: f32,
    pub pan: f32,
}

/// Loudness and pan of a sound `distance_m` away with range `range_m`, where
/// `right_m` is its offset along the listener's right-hand axis. Silent at
/// and past the range.
pub fn heard(distance_m: f32, right_m: f32, range_m: f32) -> Option<(f32, f32)> {
    if !distance_m.is_finite() || !right_m.is_finite() || distance_m >= range_m {
        return None;
    }
    let fade = 1.0 - distance_m / range_m;
    let pan = if distance_m > 0.5 {
        (right_m / distance_m).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    Some((fade * fade, pan))
}

#[derive(Debug, Clone, Copy)]
struct Seen {
    tag: u8,
    /// When a held cue next repeats.
    repeat_at: Option<Instant>,
}

/// Remembers each creature's last tag, to start sounds when it changes.
#[derive(Debug)]
pub struct CreatureSoundTracker {
    seen: HashMap<u64, Seen>,
    rng: u64,
}

impl CreatureSoundTracker {
    pub fn new(seed: u64) -> Self {
        Self {
            seen: HashMap::new(),
            rng: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn unit(&mut self) -> f32 {
        // splitmix64: deterministic and dependency-free.
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
    }

    fn repeat_at(&mut self, cue: &CreatureCue, now: Instant) -> Option<Instant> {
        let (min, max) = cue.repeat_s?;
        let wait = min + (max - min) * self.unit();
        Some(now + Duration::from_secs_f32(wait.max(0.05)))
    }

    /// The sounds to start now for `creatures`, heard by a listener at
    /// `listener_m` whose right-hand axis is `right` (x, z, unit length). A
    /// creature seen for the first time makes no sound: its tag did not just
    /// change.
    pub fn update(
        &mut self,
        config: &CreatureSoundConfig,
        now: Instant,
        listener_m: [f64; 3],
        right: [f64; 2],
        creatures: &[HeardCreature],
    ) -> Vec<CreatureSoundEvent> {
        self.seen
            .retain(|id, _| creatures.iter().any(|creature| creature.id == *id));
        let before = self.seen.clone();
        let mut events = Vec::new();
        for creature in creatures {
            let cue = config
                .cues
                .iter()
                .find(|cue| cue.species == creature.species && cue.tag == creature.tag);
            let Some(previous) = before.get(&creature.id).copied() else {
                let repeat_at = cue.and_then(|cue| self.repeat_at(cue, now));
                self.seen.insert(
                    creature.id,
                    Seen {
                        tag: creature.tag,
                        repeat_at,
                    },
                );
                continue;
            };
            let Some(cue) = cue else {
                self.seen.insert(
                    creature.id,
                    Seen {
                        tag: creature.tag,
                        repeat_at: None,
                    },
                );
                continue;
            };
            let started = previous.tag != creature.tag;
            let repeats = !started && previous.repeat_at.is_some_and(|at| now >= at);
            if !started && !repeats {
                continue;
            }
            let repeat_at = self.repeat_at(cue, now);
            self.seen.insert(
                creature.id,
                Seen {
                    tag: creature.tag,
                    repeat_at,
                },
            );
            if events.len() >= MAX_EVENTS || self.unit() >= cue.chance {
                continue;
            }
            let (dx, dz) = (
                creature.at_m[0] - listener_m[0],
                creature.at_m[2] - listener_m[2],
            );
            let distance = (dx.powi(2) + (creature.at_m[1] - listener_m[1]).powi(2) + dz.powi(2))
                .sqrt() as f32;
            let right_m = (dx * right[0] + dz * right[1]) as f32;
            let Some((loudness, pan)) = heard(distance, right_m, cue.range_m) else {
                continue;
            };
            // Joining others already at it: the chorus, if the cue has one.
            let joining = started
                && creatures.iter().any(|other| {
                    other.id != creature.id
                        && other.species == creature.species
                        && before
                            .get(&other.id)
                            .is_some_and(|seen| seen.tag == creature.tag)
                        && (other.at_m[0] - creature.at_m[0])
                            .hypot(other.at_m[2] - creature.at_m[2])
                            < CHORUS_M
                });
            let pool = if joining && !cue.chorus.is_empty() {
                &cue.chorus
            } else {
                &cue.sounds
            };
            if pool.is_empty() {
                continue;
            }
            let pick = ((self.unit() * pool.len() as f32) as usize).min(pool.len() - 1);
            events.push(CreatureSoundEvent {
                path: pool[pick].clone(),
                gain: cue.gain * loudness,
                pan,
            });
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOWL: u8 = 24;
    const WALK: u8 = 1;

    fn config() -> CreatureSoundConfig {
        CreatureSoundConfig {
            cues: vec![
                CreatureCue {
                    species: "wolf".into(),
                    tag: HOWL,
                    sounds: vec!["howl.ogg".into()],
                    chorus: vec!["chorus.ogg".into()],
                    range_m: 320.0,
                    gain: 1.0,
                    repeat_s: None,
                    chance: 1.0,
                },
                CreatureCue {
                    species: "wolf".into(),
                    tag: WALK,
                    sounds: vec!["steps.ogg".into()],
                    chorus: Vec::new(),
                    range_m: 10.0,
                    gain: 0.5,
                    repeat_s: Some((3.0, 3.0)),
                    chance: 1.0,
                },
            ],
        }
    }

    fn wolf(id: u64, tag: u8, at_m: [f64; 3]) -> HeardCreature<'static> {
        HeardCreature {
            id,
            species: "wolf",
            tag,
            at_m,
        }
    }

    const RIGHT: [f64; 2] = [1.0, 0.0];

    #[test]
    fn loudness_fades_to_silence_at_the_range_and_pans_by_side() {
        let (near, centre) = heard(1.0, 0.0, 300.0).unwrap();
        let (far, right) = heard(200.0, 200.0, 300.0).unwrap();
        assert!(near > 0.99 && far < near && far > 0.1);
        assert_eq!(centre, 0.0);
        assert_eq!(right, 1.0);
        assert_eq!(heard(30.0, -30.0, 300.0).unwrap().1, -1.0);
        assert!(heard(300.0, 0.0, 300.0).is_none());
        assert!(heard(f32::NAN, 0.0, 300.0).is_none());
    }

    #[test]
    fn a_howl_is_heard_from_far_off_and_a_second_wolf_joins_in_chorus() {
        let mut tracker = CreatureSoundTracker::new(1);
        let config = config();
        let now = Instant::now();
        let listener = [0.0; 3];
        let (a, b) = ([150.0, 0.0, 0.0], [160.0, 0.0, 10.0]);
        // First seen: no sound, the tag did not just change.
        let quiet = [wolf(1, 0, a), wolf(2, 0, b)];
        assert!(
            tracker
                .update(&config, now, listener, RIGHT, &quiet)
                .is_empty()
        );
        let first = tracker.update(
            &config,
            now,
            listener,
            RIGHT,
            &[wolf(1, HOWL, a), wolf(2, 0, b)],
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].path, PathBuf::from("howl.ogg"));
        assert!(first[0].gain > 0.1 && first[0].pan > 0.9, "{first:?}");
        let joined = tracker.update(
            &config,
            now,
            listener,
            RIGHT,
            &[wolf(1, HOWL, a), wolf(2, HOWL, b)],
        );
        assert_eq!(joined.len(), 1, "the first howl does not restart");
        assert_eq!(joined[0].path, PathBuf::from("chorus.ogg"));
    }

    #[test]
    fn steps_repeat_while_walking_and_only_within_their_range() {
        let mut tracker = CreatureSoundTracker::new(2);
        let config = config();
        let now = Instant::now();
        let near = [3.0, 0.0, 0.0];
        tracker.update(&config, now, [0.0; 3], RIGHT, &[wolf(1, 0, near)]);
        let start = tracker.update(&config, now, [0.0; 3], RIGHT, &[wolf(1, WALK, near)]);
        assert_eq!(start.len(), 1);
        let soon = now + Duration::from_secs(1);
        assert!(
            tracker
                .update(&config, soon, [0.0; 3], RIGHT, &[wolf(1, WALK, near)])
                .is_empty()
        );
        let later = now + Duration::from_secs(4);
        assert_eq!(
            tracker
                .update(&config, later, [0.0; 3], RIGHT, &[wolf(1, WALK, near)])
                .len(),
            1
        );
        // Out of range: the repeat comes round but nothing is heard.
        let far = [30.0, 0.0, 0.0];
        let much_later = now + Duration::from_secs(8);
        assert!(
            tracker
                .update(&config, much_later, [0.0; 3], RIGHT, &[wolf(1, WALK, far)])
                .is_empty()
        );
    }

    #[test]
    fn a_creature_that_leaves_is_forgotten() {
        let mut tracker = CreatureSoundTracker::new(3);
        let config = config();
        let now = Instant::now();
        tracker.update(
            &config,
            now,
            [0.0; 3],
            RIGHT,
            &[wolf(1, 0, [5.0, 0.0, 0.0])],
        );
        tracker.update(&config, now, [0.0; 3], RIGHT, &[]);
        // Back again already howling: first seen, so silent.
        let back = tracker.update(
            &config,
            now,
            [0.0; 3],
            RIGHT,
            &[wolf(1, HOWL, [5.0, 0.0, 0.0])],
        );
        assert!(back.is_empty());
    }
}

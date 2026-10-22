//! Replica-side creature store.
//!
//! Holds the creatures the server announced with `CreatureSpawn` (and removed
//! with `CreatureDespawn`) and the motion samples that move them. The store has
//! no physics or rendering dependency; the game draws what it holds. Motion for
//! an id the replica does not hold, or no newer than what it already applied, is
//! ignored.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use spall_protocol::{CreatureMotionFrame, CreatureRecord, CreatureSpawn};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReplicaCreatureError {
    #[error("invalid creature record")]
    Invalid,
}

/// One received root pose, stamped with its arrival time.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MotionSample {
    at: Instant,
    server_tick: u64,
    translation_m: [f64; 3],
    rotation_xyzw: [f32; 4],
    linear_velocity_m_s: [f32; 3],
    locomotion_tag: u8,
    clip_phase_tick: u64,
}

#[derive(Debug, Clone, Copy)]
struct MotionTrack {
    previous: Option<MotionSample>,
    current: MotionSample,
}

/// A creature's presented pose at a render instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreaturePresentation {
    pub translation_m: [f64; 3],
    pub rotation_xyzw: [f32; 4],
    pub locomotion_tag: u8,
    /// Clip phase in ticks at the render instant.
    pub clip_phase_tick: f64,
}

/// How far behind the newest received sample creatures are drawn, so two
/// samples usually bracket the render time at the 20 Hz send rate.
pub const CREATURE_RENDER_DELAY: Duration = Duration::from_millis(220);
/// Longest a creature is extrapolated along its velocity past its newest sample.
const MAX_EXTRAPOLATION: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Default)]
pub struct ReplicaCreatures {
    records: BTreeMap<u64, CreatureRecord>,
    motion: BTreeMap<u64, MotionTrack>,
    motion_applied: u64,
    revision: u64,
}

impl ReplicaCreatures {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every creature. A full baseline replaces the world, so previous
    /// creatures must not survive it.
    pub fn reset(&mut self) {
        self.records.clear();
        self.motion.clear();
        self.revision = self.revision.saturating_add(1);
    }

    /// Creatures sorted by id.
    pub fn records(&self) -> impl Iterator<Item = &CreatureRecord> {
        self.records.values()
    }

    pub fn get(&self, creature_id: u64) -> Option<&CreatureRecord> {
        self.records.get(&creature_id)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Bumps on every spawn and despawn so renderers can poll cheaply.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Installs a creature. Idempotent: a repeated spawn replaces the record and
    /// drops its motion, so a join list and the live stream can never disagree.
    pub fn spawn(&mut self, spawn: &CreatureSpawn) -> Result<(), ReplicaCreatureError> {
        spall_protocol::Record::validate(spawn).map_err(|_| ReplicaCreatureError::Invalid)?;
        let id = spawn.creature.creature_id;
        self.records.insert(id, spawn.creature.clone());
        self.motion.remove(&id);
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    /// Removes a creature. `false` if the replica did not hold it.
    pub fn despawn(&mut self, creature_id: u64) -> bool {
        self.motion.remove(&creature_id);
        let held = self.records.remove(&creature_id).is_some();
        if held {
            self.revision = self.revision.saturating_add(1);
        }
        held
    }

    /// Applies a presentation frame. Entries for creatures this replica does not
    /// hold, or not newer than the one already applied, are ignored. Returns how
    /// many entries were applied.
    pub fn apply_motion(&mut self, frame: &CreatureMotionFrame, now: Instant) -> usize {
        let mut applied = 0;
        for entry in &frame.entries {
            if !self.records.contains_key(&entry.creature_id) {
                continue;
            }
            let sample = MotionSample {
                at: now,
                server_tick: frame.server_tick,
                translation_m: entry.translation_m,
                rotation_xyzw: entry.rotation_xyzw,
                linear_velocity_m_s: entry.linear_velocity_m_s,
                locomotion_tag: entry.locomotion_tag,
                clip_phase_tick: entry.clip_phase_tick,
            };
            match self.motion.get_mut(&entry.creature_id) {
                Some(track) if frame.server_tick <= track.current.server_tick => continue,
                // The server only moves a creature every few ticks but sends its pose
                // more often. An unchanged pose must not become a new keyframe, or the
                // presented motion would hold, jump and hold. Keep the arrival time of
                // the sample that introduced the pose.
                Some(track)
                    if track.current.translation_m == sample.translation_m
                        && track.current.rotation_xyzw == sample.rotation_xyzw
                        && track.current.locomotion_tag == sample.locomotion_tag =>
                {
                    track.current.server_tick = sample.server_tick;
                    continue;
                }
                Some(track) => {
                    track.previous = Some(track.current);
                    track.current = sample;
                }
                None => {
                    self.motion.insert(
                        entry.creature_id,
                        MotionTrack {
                            previous: None,
                            current: sample,
                        },
                    );
                }
            }
            applied += 1;
        }
        self.motion_applied += applied as u64;
        applied
    }

    /// The pose to draw for `record` at `now`: interpolated between the two
    /// newest samples, held or briefly extrapolated past the newest, and the
    /// record's own pose when no motion has arrived yet.
    pub fn presentation(&self, record: &CreatureRecord, now: Instant) -> CreaturePresentation {
        let fallback = CreaturePresentation {
            translation_m: record.translation_m,
            rotation_xyzw: record.rotation_xyzw,
            locomotion_tag: record.locomotion_tag,
            clip_phase_tick: record.clip_phase_tick as f64,
        };
        let Some(track) = self.motion.get(&record.creature_id) else {
            return fallback;
        };
        let render_at = now.checked_sub(CREATURE_RENDER_DELAY).unwrap_or(now);
        let current = track.current;
        let phase_at = |sample: &MotionSample, at: Instant| {
            sample.clip_phase_tick as f64
                + at.saturating_duration_since(sample.at).as_secs_f64() * 60.0
        };
        if let Some(previous) = track.previous
            && render_at < current.at
        {
            let span = current
                .at
                .saturating_duration_since(previous.at)
                .as_secs_f64();
            let t = if span <= 0.0 {
                1.0
            } else {
                (render_at
                    .saturating_duration_since(previous.at)
                    .as_secs_f64()
                    / span)
                    .clamp(0.0, 1.0)
            };
            let lerp = |a: f64, b: f64| a + (b - a) * t;
            return CreaturePresentation {
                translation_m: [
                    lerp(previous.translation_m[0], current.translation_m[0]),
                    lerp(previous.translation_m[1], current.translation_m[1]),
                    lerp(previous.translation_m[2], current.translation_m[2]),
                ],
                rotation_xyzw: nlerp(previous.rotation_xyzw, current.rotation_xyzw, t as f32),
                locomotion_tag: current.locomotion_tag,
                clip_phase_tick: phase_at(&previous, render_at),
            };
        }
        let ahead = render_at
            .saturating_duration_since(current.at)
            .min(MAX_EXTRAPOLATION)
            .as_secs_f64();
        CreaturePresentation {
            translation_m: [
                current.translation_m[0] + f64::from(current.linear_velocity_m_s[0]) * ahead,
                current.translation_m[1] + f64::from(current.linear_velocity_m_s[1]) * ahead,
                current.translation_m[2] + f64::from(current.linear_velocity_m_s[2]) * ahead,
            ],
            rotation_xyzw: current.rotation_xyzw,
            locomotion_tag: current.locomotion_tag,
            clip_phase_tick: phase_at(&current, render_at),
        }
    }

    /// Largest horizontal distance any creature is presented from where its
    /// spawn record placed it. A rough "did anything move" measure.
    pub fn max_displacement_m(&self, now: Instant) -> f64 {
        self.records
            .values()
            .map(|record| {
                let presented = self.presentation(record, now).translation_m;
                let root = record.translation_m;
                (presented[0] - root[0]).hypot(presented[2] - root[2])
            })
            .fold(0.0, f64::max)
    }

    /// Motion entries applied so far.
    pub fn motion_entries_applied(&self) -> u64 {
        self.motion_applied
    }
}

fn nlerp(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    // Take the shorter arc, then renormalize.
    let dot: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
    let sign = if dot < 0.0 { -1.0 } else { 1.0 };
    let mut out = [0.0; 4];
    for i in 0..4 {
        out[i] = a[i] + (sign * b[i] - a[i]) * t;
    }
    let length = out.iter().map(|v| v * v).sum::<f32>().sqrt();
    if length <= f32::EPSILON {
        return a;
    }
    out.map(|v| v / length)
}

#[cfg(test)]
#[path = "replica_creatures_tests.rs"]
mod tests;

//! Live creature motion frames.
//!
//! Creatures appear and disappear through ordered [`crate::CreatureSpawn`] and
//! [`crate::CreatureDespawn`] records. What moves every tick is the root pose and
//! which locomotion the animal is performing; that travels in these frames. A
//! frame is a presentation update: it never creates, retires or edits a
//! creature, and a client ignores entries for creatures it does not hold or that
//! are not newer than the one it already applied.

use crate::records::WIRE_SCHEMA_VERSION;
use serde::{Deserialize, Serialize};

use crate::records::{Record, RecordError, WireTag};

pub const MAX_CREATURE_MOTION_ENTRIES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CreatureMotionEntry {
    pub creature_id: u64,
    /// Root translation in world metres.
    pub translation_m: [f64; 3],
    /// Unit quaternion `[x, y, z, w]`.
    pub rotation_xyzw: [f32; 4],
    pub linear_velocity_m_s: [f32; 3],
    /// Game-defined locomotion selector, matching `CreatureRecord::locomotion_tag`.
    pub locomotion_tag: u8,
    /// Clip phase in server ticks at `server_tick`.
    pub clip_phase_tick: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatureMotionFrame {
    pub schema: u16,
    pub server_tick: u64,
    pub entries: Vec<CreatureMotionEntry>,
}

impl Record for CreatureMotionFrame {
    const TAG: WireTag = WireTag::CreatureMotionFrame;

    fn validate(&self) -> Result<(), RecordError> {
        if self.schema != WIRE_SCHEMA_VERSION {
            return Err(RecordError::Inconsistent(
                "unsupported creature motion schema",
            ));
        }
        if self.entries.len() > MAX_CREATURE_MOTION_ENTRIES {
            return Err(RecordError::OutOfRange {
                field: "CreatureMotionFrame.entries",
                detail: "more than 256 entries",
            });
        }
        for entry in &self.entries {
            if entry.creature_id == 0
                || entry.linear_velocity_m_s.iter().any(|v| !v.is_finite())
                || !crate::creature::pose_is_valid(&entry.translation_m, &entry.rotation_xyzw)
            {
                return Err(RecordError::Inconsistent("invalid creature motion entry"));
            }
        }
        if self
            .entries
            .windows(2)
            .any(|pair| pair[0].creature_id >= pair[1].creature_id)
        {
            return Err(RecordError::Inconsistent(
                "creature motion entries must be sorted by id without duplicates",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_control, encode_control};

    fn entry(id: u64) -> CreatureMotionEntry {
        CreatureMotionEntry {
            creature_id: id,
            translation_m: [1.0, 2.0, 3.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [0.5, 0.0, 0.0],
            locomotion_tag: 1,
            clip_phase_tick: 42,
        }
    }

    fn frame(entries: Vec<CreatureMotionEntry>) -> CreatureMotionFrame {
        CreatureMotionFrame {
            schema: WIRE_SCHEMA_VERSION,
            server_tick: 100,
            entries,
        }
    }

    #[test]
    fn frames_round_trip_and_stay_small() {
        let frame = frame((1..=36).map(entry).collect());
        let bytes = encode_control(&frame).unwrap();
        assert_eq!(
            decode_control::<CreatureMotionFrame>(&bytes).unwrap(),
            frame
        );
        assert!(
            bytes.len() < 4096,
            "36 creatures encode to {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn invalid_frames_are_rejected() {
        let mut unsorted = frame(vec![entry(2), entry(1)]);
        assert!(unsorted.validate().is_err());
        unsorted.entries = vec![entry(1), entry(1)];
        assert!(unsorted.validate().is_err());

        let mut bad = entry(1);
        bad.rotation_xyzw = [0.0, 0.0, 0.0, 2.0];
        assert!(frame(vec![bad]).validate().is_err());
        let mut nan = entry(1);
        nan.translation_m[1] = f64::NAN;
        assert!(frame(vec![nan]).validate().is_err());
        assert!(frame(vec![entry(0)]).validate().is_err());

        let mut wrong = frame(vec![entry(1)]);
        wrong.schema = 9;
        assert!(wrong.validate().is_err());
        let too_many = frame((1..=300).map(entry).collect());
        assert!(too_many.validate().is_err());
    }
}

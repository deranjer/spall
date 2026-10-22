//! Replicated creature records.
//!
//! A creature is a server-authoritative entity: a species, a pose and a playing
//! clip. It is drawn from the species' SPVX asset and owns no world voxels. The
//! server announces each one with a [`CreatureSpawn`] and retires it with a
//! [`CreatureDespawn`], both on the ordered reliable stream, so a replica that
//! sees a motion entry for an id it does not hold can ignore it. What moves every
//! tick travels in [`crate::CreatureMotionFrame`]s.

use serde::{Deserialize, Serialize};

use crate::records::{Record, RecordError, WIRE_SCHEMA_VERSION, WireTag};

pub const MAX_CREATURE_SPECIES_KEY_BYTES: usize = 63;

/// One creature as a replica needs to draw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatureRecord {
    pub creature_id: u64,
    /// Game-defined species key, resolved by the game against its own assets.
    pub species: String,
    /// Root translation in world metres.
    pub translation_m: [f64; 3],
    /// Unit quaternion `[x, y, z, w]`.
    pub rotation_xyzw: [f32; 4],
    /// Game-defined locomotion selector; picks the clip.
    pub locomotion_tag: u8,
    /// Clip phase in server ticks.
    pub clip_phase_tick: u64,
}

/// A creature entered the world, or (in a join list) is present in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatureSpawn {
    pub schema: u16,
    pub creature: CreatureRecord,
}

/// A creature left the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatureDespawn {
    pub schema: u16,
    pub creature_id: u64,
}

pub(crate) fn pose_is_valid(translation_m: &[f64; 3], rotation_xyzw: &[f32; 4]) -> bool {
    let rotation_len_sq: f32 = rotation_xyzw.iter().map(|v| v * v).sum();
    translation_m.iter().all(|v| v.is_finite())
        && rotation_xyzw.iter().all(|v| v.is_finite())
        && (rotation_len_sq - 1.0).abs() <= 1.0e-3
}

impl Record for CreatureSpawn {
    const TAG: WireTag = WireTag::CreatureSpawn;

    fn validate(&self) -> Result<(), RecordError> {
        if self.schema != WIRE_SCHEMA_VERSION {
            return Err(RecordError::Inconsistent("unsupported creature schema"));
        }
        let creature = &self.creature;
        if creature.creature_id == 0
            || creature.species.is_empty()
            || creature.species.len() > MAX_CREATURE_SPECIES_KEY_BYTES
            || !pose_is_valid(&creature.translation_m, &creature.rotation_xyzw)
        {
            return Err(RecordError::Inconsistent("invalid creature record"));
        }
        Ok(())
    }
}

impl Record for CreatureDespawn {
    const TAG: WireTag = WireTag::CreatureDespawn;

    fn validate(&self) -> Result<(), RecordError> {
        if self.schema != WIRE_SCHEMA_VERSION {
            return Err(RecordError::Inconsistent("unsupported creature schema"));
        }
        if self.creature_id == 0 {
            return Err(RecordError::Inconsistent("creature id is zero"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_control, encode_control};

    fn record() -> CreatureRecord {
        CreatureRecord {
            creature_id: 7,
            species: "deer".into(),
            translation_m: [1.0, 2.0, 3.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            locomotion_tag: 1,
            clip_phase_tick: 42,
        }
    }

    #[test]
    fn spawn_and_despawn_round_trip() {
        let spawn = CreatureSpawn {
            schema: WIRE_SCHEMA_VERSION,
            creature: record(),
        };
        let bytes = encode_control(&spawn).unwrap();
        assert_eq!(decode_control::<CreatureSpawn>(&bytes).unwrap(), spawn);
        let despawn = CreatureDespawn {
            schema: WIRE_SCHEMA_VERSION,
            creature_id: 7,
        };
        let bytes = encode_control(&despawn).unwrap();
        assert_eq!(decode_control::<CreatureDespawn>(&bytes).unwrap(), despawn);
    }

    #[test]
    fn invalid_records_are_rejected() {
        let valid = CreatureSpawn {
            schema: WIRE_SCHEMA_VERSION,
            creature: record(),
        };
        assert!(valid.validate().is_ok());
        let mut zero = valid.clone();
        zero.creature.creature_id = 0;
        assert!(zero.validate().is_err());
        let mut nameless = valid.clone();
        nameless.creature.species.clear();
        assert!(nameless.validate().is_err());
        let mut long = valid.clone();
        long.creature.species = "x".repeat(MAX_CREATURE_SPECIES_KEY_BYTES + 1);
        assert!(long.validate().is_err());
        let mut skewed = valid.clone();
        skewed.creature.rotation_xyzw = [0.0, 0.0, 0.0, 2.0];
        assert!(skewed.validate().is_err());
        let mut nan = valid.clone();
        nan.creature.translation_m[1] = f64::NAN;
        assert!(nan.validate().is_err());
        let mut wrong = valid;
        wrong.schema = 9;
        assert!(wrong.validate().is_err());
        assert!(
            CreatureDespawn {
                schema: WIRE_SCHEMA_VERSION,
                creature_id: 0
            }
            .validate()
            .is_err()
        );
    }
}

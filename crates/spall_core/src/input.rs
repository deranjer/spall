//! Player movement input: the small value type shared by the wire
//! ([`spall_protocol::InputFrame`]), the authoritative simulation, the
//! deterministic capsule kernel, and the client predictor, so every layer reads
//! one definition of "what the player is asking for this tick".
//!
//! This is data only. The movement math (turning `movement` + `view_dir` into a
//! world-space wish velocity, gravity, jump) lives in the capsule kernel
//! (`spall_physics::character`) so the server and client run the *same*
//! function; physics is not lockstep (`docs/protocol.md`), so only bounded
//! agreement is promised.

use crate::EntityId;

/// Held-button bit: jump. Discriminants are permanent; new buttons take new bits.
pub const BUTTON_JUMP: u32 = 1 << 0;

/// Reserved id band for players. A player's [`EntityId`] is
/// `PLAYER_ENTITY_BASE + connection_slot` so the server and every client derive
/// the same id from the session's slot with no extra wire field. Split-body
/// entity ids are allocated monotonically from `1` by `spall_sim::IdRegistry`
/// and cannot reach this band in any bounded workload; formalising the split is
/// tracked as T19 follow-up.
pub const PLAYER_ENTITY_BASE: u64 = 1 << 48;

/// The [`EntityId`] of the player owned by connection `slot`.
pub fn player_entity_for(slot: u32) -> EntityId {
    EntityId::new(PLAYER_ENTITY_BASE + u64::from(slot)).expect("player id is non-zero")
}

/// `true` if `entity` is in the reserved player band.
pub fn is_player_entity(entity: EntityId) -> bool {
    entity.get() >= PLAYER_ENTITY_BASE
}

/// The connection slot a player [`EntityId`] belongs to, or `None` if it is not
/// in the reserved band.
pub fn slot_of_player(entity: EntityId) -> Option<u32> {
    entity
        .get()
        .checked_sub(PLAYER_ENTITY_BASE)
        .and_then(|s| u32::try_from(s).ok())
}

/// One tick of movement intent.
///
/// * `movement` — local wish direction, each axis clamped to `-1.0..=1.0`:
///   `x` strafes right, `z` drives forward (the camera looks down local `-Z`),
///   `y` is unused. Matches [`spall_protocol::InputFrame`]'s `unit_axis` bound.
/// * `view_dir` — the world-space look direction; only its heading (yaw) is used
///   for movement, pitch does not tilt walking.
/// * `buttons` — held-button bitset (`BUTTON_*`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerInput {
    pub movement: [f32; 3],
    pub view_dir: [f32; 3],
    pub buttons: u32,
}

impl PlayerInput {
    /// A neutral input: no movement, facing `-Z`, no buttons.
    pub const NEUTRAL: Self = Self {
        movement: [0.0, 0.0, 0.0],
        view_dir: [0.0, 0.0, -1.0],
        buttons: 0,
    };

    /// `true` if the jump button is held.
    pub fn wants_jump(self) -> bool {
        self.buttons & BUTTON_JUMP != 0
    }

    /// Heading angle (radians) of `view_dir` about `+Y`, measured so that a
    /// forward input walks along the flattened look direction. Falls back to `0`
    /// when the look direction is vertical or non-finite.
    pub fn yaw(self) -> f32 {
        let [x, _, z] = self.view_dir;
        if !x.is_finite() || !z.is_finite() || (x == 0.0 && z == 0.0) {
            return 0.0;
        }
        x.atan2(-z)
    }

    /// Clamps each `movement` axis into `-1.0..=1.0` and zeroes any non-finite
    /// axis; leaves a non-finite `view_dir` for the caller to reject.
    pub fn sanitized(mut self) -> Self {
        for axis in &mut self.movement {
            *axis = if axis.is_finite() {
                axis.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        self
    }

    /// `true` if every field is finite (a fully usable input).
    pub fn is_finite(self) -> bool {
        self.movement.iter().all(|v| v.is_finite()) && self.view_dir.iter().all(|v| v.is_finite())
    }
}

impl Default for PlayerInput {
    fn default() -> Self {
        Self::NEUTRAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_id_round_trips_through_the_slot() {
        for slot in [0u32, 1, 7, 4095] {
            let id = player_entity_for(slot);
            assert!(is_player_entity(id));
            assert_eq!(slot_of_player(id), Some(slot));
        }
    }

    #[test]
    fn split_body_ids_are_not_in_the_player_band() {
        // `spall_sim` hands out entity ids from 1 upward; nothing in a bounded
        // run approaches 2^48.
        assert!(!is_player_entity(EntityId::new(1).unwrap()));
        assert!(!is_player_entity(EntityId::new(1_000_000).unwrap()));
    }

    #[test]
    fn yaw_is_zero_looking_down_negative_z() {
        let input = PlayerInput {
            view_dir: [0.0, 0.0, -1.0],
            ..PlayerInput::NEUTRAL
        };
        assert!(input.yaw().abs() < 1e-6);
    }

    #[test]
    fn yaw_turns_with_the_look_direction() {
        // Looking along +X is a quarter turn from looking along -Z.
        let input = PlayerInput {
            view_dir: [1.0, 0.0, 0.0],
            ..PlayerInput::NEUTRAL
        };
        assert!((input.yaw() - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    }

    #[test]
    fn sanitized_clamps_and_scrubs_movement() {
        let dirty = PlayerInput {
            movement: [5.0, f32::NAN, -3.0],
            ..PlayerInput::NEUTRAL
        };
        assert_eq!(dirty.sanitized().movement, [1.0, 0.0, -1.0]);
    }
}

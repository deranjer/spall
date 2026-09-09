//! Authoritative player capsules (T19).
//!
//! A [`Player`] is the server-owned kinematic capsule for one connection. It is
//! **not** a [`crate::body::Body`] — it has no voxel volume, never splits, and
//! never enters the dynamic-body set — so it does not perturb the edit / split /
//! collider path. Each tick the simulation feeds it one validated
//! [`spall_core::PlayerInput`] and advances it with the deterministic
//! [`spall_physics::step_character`] kernel against the authoritative collider
//! world.
//!
//! `docs/protocol.md`: "Server consumes at most one validated input frame per
//! player/tick. … Reuse recent held movement briefly on loss; clear it after a
//! 250 ms silence timeout."

use spall_core::{EntityId, PlayerInput, VolumeId};
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::{InputSeq, TopologyOp, TopologyTransaction};

/// Ticks a held input is reused after the last fresh frame before it is dropped
/// to neutral — 250 ms at the fixed 60 Hz tick.
pub const HELD_INPUT_TIMEOUT_TICKS: u32 = 15;

/// Extra reach, in metres, beyond the capsule bounds within which a committed
/// topology edit invalidates the player's predicted history and forces an
/// authoritative depenetration.
pub const INVALIDATION_MARGIN_M: f64 = 2.0;

/// One authoritative player capsule.
#[derive(Debug, Clone)]
pub struct Player {
    /// Reserved-band id ([`spall_core::player_entity_for`]).
    pub entity: EntityId,
    pub params: CharacterParams,
    /// Current authoritative kinematic state (feet position).
    pub state: CharacterState,
    /// Where the capsule was spawned — the reset target if it ever leaves the
    /// world.
    pub spawn: CharacterState,
    /// The most recent accepted input.
    pub last_input: PlayerInput,
    /// Sequence of `last_input`; `0` means "no input yet".
    pub last_input_seq: InputSeq,
    /// Ticks elapsed since the last fresh input (drives the 250 ms timeout).
    pub ticks_since_input: u32,
    /// A fresh input arrived since the last [`Self::age_input`].
    pub pending_fresh: bool,
    /// Bumped whenever a nearby committed edit invalidates prediction history;
    /// the client compares it and rebuilds prediction from the authoritative
    /// snapshot.
    pub movement_epoch: u64,
}

impl Player {
    /// A player spawned standing at `feet_m`.
    pub fn new(entity: EntityId, params: CharacterParams, feet_m: [f64; 3]) -> Self {
        let spawn = CharacterState::at(feet_m);
        Self {
            entity,
            params,
            state: spawn,
            spawn,
            last_input: PlayerInput::NEUTRAL,
            last_input_seq: InputSeq(0),
            ticks_since_input: 0,
            pending_fresh: false,
            movement_epoch: 0,
        }
    }

    /// Records a fresh input frame. Rejects a non-monotonic sequence (a stale or
    /// duplicate datagram) and a non-finite payload; returns whether it was
    /// accepted.
    pub fn accept_input(&mut self, input: PlayerInput, seq: InputSeq) -> bool {
        if seq.0 <= self.last_input_seq.0 {
            return false;
        }
        if !input.is_finite() {
            return false;
        }
        self.last_input = input.sanitized();
        self.last_input_seq = seq;
        self.ticks_since_input = 0;
        self.pending_fresh = true;
        true
    }

    /// The input to integrate this tick. Inside the reuse window the last held
    /// input stands in for a dropped frame; past it, movement and buttons clear
    /// (so a lost "button up" cannot leave the player walking or holding jump
    /// forever) while the last facing is kept.
    pub fn effective_input(&self) -> PlayerInput {
        if self.pending_fresh || self.ticks_since_input < HELD_INPUT_TIMEOUT_TICKS {
            self.last_input
        } else {
            PlayerInput {
                view_dir: self.last_input.view_dir,
                ..PlayerInput::NEUTRAL
            }
        }
    }

    /// Whether the held input is currently being reused because no fresh frame
    /// has arrived (but the 250 ms timeout has not yet elapsed).
    pub fn is_coasting(&self) -> bool {
        !self.pending_fresh
            && self.ticks_since_input > 0
            && self.ticks_since_input < HELD_INPUT_TIMEOUT_TICKS
    }

    /// Advances the input-age bookkeeping after a tick's movement has run.
    pub fn age_input(&mut self) {
        if self.pending_fresh {
            self.pending_fresh = false;
        } else {
            self.ticks_since_input = self.ticks_since_input.saturating_add(1);
        }
    }

    /// World-space AABB of the capsule (feet at `position_m`), metres.
    pub fn capsule_aabb_m(&self) -> ([f64; 3], [f64; 3]) {
        let r = f64::from(self.params.radius_m);
        let h = f64::from(self.params.total_height_m());
        let [x, y, z] = self.state.position_m;
        ([x - r, y, z - r], [x + r, y + h, z + r])
    }

    /// Whether the world-space cell box `[min_m, max_m]` (metres) comes within
    /// [`INVALIDATION_MARGIN_M`] of the capsule.
    pub fn near_world_box(&self, min_m: [f64; 3], max_m: [f64; 3]) -> bool {
        let (lo, hi) = self.capsule_aabb_m();
        (0..3).all(|i| {
            min_m[i] - INVALIDATION_MARGIN_M <= hi[i] && max_m[i] + INVALIDATION_MARGIN_M >= lo[i]
        })
    }
}

/// World-space AABB (metres) of the terrain cells one committed transaction
/// touched, or `None` if it did not edit the terrain volume. Used to decide
/// whether a committed edit invalidates a player's prediction history.
///
/// Body-volume ops (a `SplitOff` child's fill runs) are ignored — a player
/// standing on a body is out of scope for the T19 headless core.
pub fn transaction_world_box(
    tx: &TopologyTransaction,
    terrain: VolumeId,
    cell_m: f64,
) -> Option<([f64; 3], [f64; 3])> {
    let unit = spall_core::BRUSH_UNIT as f64;
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut hit = false;

    for op in &tx.ops {
        let (lo, hi) = match op {
            TopologyOp::IntegerBrush { volume, brush, .. } if *volume == terrain => {
                let c = [
                    brush.centre.x as f64 / unit * cell_m,
                    brush.centre.y as f64 / unit * cell_m,
                    brush.centre.z as f64 / unit * cell_m,
                ];
                let r = (brush.radius_units() as f64 / unit + 1.0) * cell_m;
                (
                    [c[0] - r, c[1] - r, c[2] - r],
                    [c[0] + r, c[1] + r, c[2] + r],
                )
            }
            TopologyOp::CellRun {
                volume, start, len, ..
            } if *volume == terrain => (
                [
                    start.x as f64 * cell_m,
                    start.y as f64 * cell_m,
                    start.z as f64 * cell_m,
                ],
                [
                    (start.x as f64 + f64::from(*len)) * cell_m,
                    (start.y as f64 + 1.0) * cell_m,
                    (start.z as f64 + 1.0) * cell_m,
                ],
            ),
            _ => continue,
        };
        for i in 0..3 {
            min[i] = min[i].min(lo[i]);
            max[i] = max[i].max(hi[i]);
        }
        hit = true;
    }

    hit.then_some((min, max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::player_entity_for;

    fn player() -> Player {
        Player::new(
            player_entity_for(0),
            CharacterParams::DEFAULT,
            [0.0, 1.0, 0.0],
        )
    }

    #[test]
    fn rejects_a_non_monotonic_input_sequence() {
        let mut p = player();
        assert!(p.accept_input(PlayerInput::NEUTRAL, InputSeq(4)));
        assert!(!p.accept_input(PlayerInput::NEUTRAL, InputSeq(4)));
        assert!(!p.accept_input(PlayerInput::NEUTRAL, InputSeq(2)));
        assert!(p.accept_input(PlayerInput::NEUTRAL, InputSeq(5)));
    }

    #[test]
    fn held_input_is_reused_then_cleared_after_the_timeout() {
        let mut p = player();
        let walking = PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [0.0, 0.0, -1.0],
            buttons: spall_core::BUTTON_JUMP,
        };
        assert!(p.accept_input(walking, InputSeq(1)));
        assert_eq!(p.effective_input(), walking);
        p.age_input(); // consume "fresh"

        // No fresh frames: the held input is reused for the whole timeout window.
        for _ in 0..HELD_INPUT_TIMEOUT_TICKS {
            assert_eq!(p.effective_input().movement, walking.movement);
            p.age_input();
        }
        // Past the timeout: movement and buttons drop, facing stays.
        let coasted = p.effective_input();
        assert_eq!(coasted.movement, [0.0; 3]);
        assert_eq!(coasted.buttons, 0);
        assert_eq!(coasted.view_dir, walking.view_dir);
    }

    #[test]
    fn proximity_test_expands_by_the_margin() {
        let p = player(); // capsule feet at (0, 1, 0), ~1.8 m tall, 0.3 m radius
        // A cell box 2.1 m away in x is just out of range (margin is 2.0).
        assert!(!p.near_world_box([2.5, 1.0, 0.0], [2.75, 1.25, 0.25]));
        // ...but 1 m away is within range.
        assert!(p.near_world_box([1.3, 1.0, 0.0], [1.55, 1.25, 0.25]));
    }
}

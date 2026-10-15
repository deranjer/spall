//! Footfall timing from authoritative capsule movement. This module proposes
//! positions only; the snow owner must validate actual terrain contact, exposure,
//! depth and revision before publishing an imprint.

use std::collections::BTreeMap;

use spall_core::EntityId;

use crate::player::Player;

pub const FOOTPRINT_STRIDE_M: f64 = 0.65;
const FOOT_OFFSET_M: f64 = 0.09;
const MIN_STEP_TICKS: u64 = 6;
// Normal capsules cannot cross this distance in one 60 Hz movement step.
const MAX_CONTINUOUS_STEP_M: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FootfallCandidate {
    pub player: EntityId,
    pub position_m: [f64; 3],
    /// Heading in [-pi, pi], quantized to [-32767, 32767].
    pub heading_q15: i16,
    /// Zero is left, one is right.
    pub foot: u8,
    pub tick: u64,
}

#[derive(Debug, Clone, Copy)]
struct Walk {
    position: [f64; 3],
    epoch: u64,
    observed_tick: u64,
    last_step: u64,
    distance: f64,
    foot: u8,
}

/// Temporary per-connected-player stride credit; never saved or client-authored.
#[derive(Debug, Default)]
pub struct MotionFootprints {
    walks: BTreeMap<EntityId, Walk>,
}

impl MotionFootprints {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.walks.clear();
    }

    /// Remove disconnected players so reconnecting cannot inherit stride credit.
    pub fn retain_players(&mut self, players: impl IntoIterator<Item = EntityId>) {
        let live: std::collections::BTreeSet<_> = players.into_iter().collect();
        self.walks.retain(|entity, _| live.contains(entity));
    }

    /// Observe once, after the server's validated capsule sweep for this tick.
    /// Airborne movement, teleports and discontinuities discard accumulated credit.
    pub fn observe(&mut self, player: &Player, tick: u64) -> Option<FootfallCandidate> {
        let position = player.state.position_m;
        if !player.state.grounded || !position.iter().all(|v| v.is_finite()) {
            self.walks.remove(&player.entity);
            return None;
        }
        let initial = Walk {
            position,
            epoch: player.movement_epoch,
            observed_tick: tick,
            last_step: tick,
            distance: 0.0,
            foot: 0,
        };
        let Some(walk) = self.walks.get_mut(&player.entity) else {
            self.walks.insert(player.entity, initial);
            return None;
        };
        // Do not let a repeated observation create stamps or reset good credit.
        if tick <= walk.observed_tick {
            return None;
        }
        let dx = position[0] - walk.position[0];
        let dz = position[2] - walk.position[2];
        let horizontal = dx.hypot(dz);
        if walk.epoch != player.movement_epoch
            || tick - walk.observed_tick > 2
            || horizontal > MAX_CONTINUOUS_STEP_M
            || (position[1] - walk.position[1]).abs() > MAX_CONTINUOUS_STEP_M
        {
            *walk = initial;
            return None;
        }
        walk.position = position;
        walk.observed_tick = tick;
        // Bound residual credit: a delayed stamp must never produce a catch-up burst.
        walk.distance = (walk.distance + horizontal).min(FOOTPRINT_STRIDE_M * 2.0);
        if horizontal < 1.0e-6
            || walk.distance < FOOTPRINT_STRIDE_M
            || tick - walk.last_step < MIN_STEP_TICKS
        {
            return None;
        }
        walk.distance -= FOOTPRINT_STRIDE_M;
        walk.last_step = tick;
        let foot = walk.foot;
        walk.foot ^= 1;
        let heading = f64::from(player.effective_input().yaw());
        let lateral = if foot == 0 {
            -FOOT_OFFSET_M
        } else {
            FOOT_OFFSET_M
        };
        Some(FootfallCandidate {
            player: player.entity,
            position_m: [
                position[0] + heading.cos() * lateral,
                position[1],
                position[2] + heading.sin() * lateral,
            ],
            heading_q15: (heading / std::f64::consts::PI * 32767.0)
                .round()
                .clamp(-32767.0, 32767.0) as i16,
            foot,
            tick,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{PlayerInput, player_entity_for};
    use spall_physics::CharacterParams;
    use spall_protocol::InputSeq;

    fn walker(slot: u32) -> Player {
        let mut player = Player::new(player_entity_for(slot), CharacterParams::DEFAULT, [0.0; 3]);
        player.state.grounded = true;
        player
    }

    #[test]
    fn measured_ground_travel_alternates_feet_and_supports_turns() {
        let mut tracker = MotionFootprints::new();
        let mut player = walker(0);
        assert!(tracker.observe(&player, 0).is_none());
        let mut marks = Vec::new();
        for tick in 1..=100 {
            player.state.position_m[2] -= 0.1;
            if let Some(mark) = tracker.observe(&player, tick) {
                marks.push(mark);
            }
        }
        assert_eq!(
            marks.len(),
            15,
            "ten metres should produce fifteen 0.65m strides"
        );
        for (i, mark) in marks.iter().enumerate() {
            assert_eq!(mark.foot, (i % 2) as u8);
            assert!((mark.position_m[0].abs() - 0.09).abs() < 1.0e-9);
        }
        player.accept_input(
            PlayerInput {
                view_dir: [1.0, 0.0, 0.0],
                ..PlayerInput::NEUTRAL
            },
            InputSeq(1),
        );
        let mut turned = None;
        for tick in 101..=110 {
            player.state.position_m[0] += 0.1;
            if let Some(mark) = tracker.observe(&player, tick) {
                turned = Some(mark);
            }
        }
        let mark = turned.expect("turning still yields distance-driven footfalls");
        assert!((i32::from(mark.heading_q15) - 16384).abs() <= 1);
        assert!((mark.position_m[2] - player.state.position_m[2]).abs() > 0.08);
    }

    #[test]
    fn stationary_spectator_airborne_and_landing_never_backfill_tracks() {
        let mut tracker = MotionFootprints::new();
        let mut player = walker(0);
        for tick in 0..=100 {
            assert!(tracker.observe(&player, tick).is_none());
        }
        player.state.grounded = false;
        for tick in 101..=200 {
            player.state.position_m[0] += 0.2;
            assert!(tracker.observe(&player, tick).is_none());
        }
        player.state.grounded = true;
        assert!(tracker.observe(&player, 201).is_none());
        for tick in 202..=206 {
            assert!(tracker.observe(&player, tick).is_none());
        }
    }

    #[test]
    fn teleports_epoch_changes_and_missing_ticks_discard_stride_credit() {
        let mut tracker = MotionFootprints::new();
        let mut player = walker(0);
        tracker.observe(&player, 0);
        for tick in 1..=5 {
            player.state.position_m[0] += 0.1;
            assert!(tracker.observe(&player, tick).is_none());
        }
        player.state.position_m[0] += 10.0;
        assert!(tracker.observe(&player, 6).is_none());
        player.state.position_m[0] += 0.2;
        assert!(tracker.observe(&player, 7).is_none());
        player.movement_epoch += 1;
        assert!(tracker.observe(&player, 8).is_none());
        player.state.position_m[0] += 0.2;
        assert!(tracker.observe(&player, 20).is_none());
        assert!(tracker.observe(&player, 20).is_none());
        player.state.position_m[1] += 2.0;
        assert!(tracker.observe(&player, 21).is_none());
    }

    #[test]
    fn independent_walkers_rate_bound_and_disconnect_cleanup() {
        let mut tracker = MotionFootprints::new();
        let mut a = walker(0);
        let mut b = walker(1);
        tracker.observe(&a, 0);
        tracker.observe(&b, 0);
        let mut previous = 0;
        let mut marks = 0;
        for tick in 1..=60 {
            a.state.position_m[0] += 0.4;
            b.state.position_m[2] -= 0.1;
            if let Some(mark) = tracker.observe(&a, tick) {
                assert!(mark.tick - previous >= 6);
                previous = mark.tick;
                marks += 1;
            }
            tracker.observe(&b, tick);
        }
        assert_eq!(marks, 10);
        tracker.retain_players([a.entity]);
        assert_eq!(tracker.walks.len(), 1);
        assert!(tracker.observe(&b, 61).is_none());
        tracker.clear();
        assert!(tracker.walks.is_empty());
    }
}

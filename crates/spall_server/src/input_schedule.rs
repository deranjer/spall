//! Bounded tick scheduling for authoritative player input datagrams.

use std::collections::{BTreeMap, HashSet};

use spall_core::{EntityId, PlayerInput, Tick};
use spall_protocol::{InputSeq, SessionId};

/// A future input is useful for aligning a predicted edge. This covers the
/// lead controller's maximum 24-tick RTT target plus its 12-tick excess bound.
pub(crate) const MAX_FUTURE_INPUT_TICKS: u64 = 36;
/// Global admission bound (about 113 full per-session horizons).
const MAX_PENDING_GLOBAL: usize = 4096;
const MAX_SUPERSEDED_GLOBAL: usize = 4096;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScheduledInput {
    pub session: SessionId,
    pub entity: EntityId,
    pub input: PlayerInput,
    pub seq: InputSeq,
    pub tick: Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleResult {
    Immediate,
    Queued,
    Duplicate,
    Rejected,
}

/// Inputs are ordered by target tick and session. Newer input for the same tick
/// replaces older input, so one session can hold at most one entry per tick in
/// the bounded horizon. Queue entries are removed on disconnect.
#[derive(Default)]
pub(crate) struct PlayerInputSchedule {
    entries: BTreeMap<(u64, u64), ScheduledInput>,
    superseded: HashSet<(u64, u64)>,
}

impl PlayerInputSchedule {
    pub fn contains(&self, session: SessionId, seq: InputSeq) -> bool {
        self.entries
            .values()
            .any(|entry| entry.session == session && entry.seq == seq)
            || self.superseded.contains(&(session.raw(), seq.0))
    }

    /// The single admission rule shared by a frame's primary input and each
    /// of its redundant "recent" copies: skip anything this queue already
    /// tracks (`contains`) or that is not newer than what the sim has
    /// already accepted (`acked`), then hand everything else to
    /// [`Self::schedule`]. Returns `Some(input)` only when it is due this
    /// tick and the caller must apply it to the sim itself; every other
    /// outcome (queued, duplicate, rejected, already tracked, stale) is
    /// `None` and needs no further action from the caller.
    ///
    /// Recovering a redundant copy through this same path (instead of
    /// applying it immediately) is what keeps a dropped primary datagram's
    /// input pinned to its own `intended_tick` rather than snapping onto
    /// whichever tick happens to be next when the copy is finally seen.
    #[allow(clippy::too_many_arguments)]
    pub fn admit(
        &mut self,
        session: SessionId,
        entity: EntityId,
        input: PlayerInput,
        seq: InputSeq,
        intended_tick: Tick,
        current_tick: Tick,
        acked: Option<InputSeq>,
    ) -> Option<PlayerInput> {
        if self.contains(session, seq) || acked.is_some_and(|acked| seq.0 <= acked.0) {
            return None;
        }
        let (result, _) = self.schedule(session, entity, input, seq, intended_tick, current_tick);
        (result == ScheduleResult::Immediate).then_some(input)
    }

    pub fn schedule(
        &mut self,
        session: SessionId,
        entity: EntityId,
        input: PlayerInput,
        seq: InputSeq,
        intended_tick: Tick,
        current_tick: Tick,
    ) -> (ScheduleResult, Option<Tick>) {
        let existing: Vec<_> = self
            .entries
            .values()
            .filter(|entry| entry.session == session)
            .collect();
        if existing.iter().any(|entry| entry.seq.0 >= seq.0) {
            return (ScheduleResult::Duplicate, None);
        }

        let next_tick = current_tick.0.saturating_add(1);
        let latest_tick = next_tick.saturating_add(MAX_FUTURE_INPUT_TICKS);
        // Stale timestamps are late packets and are applied at the next tick;
        // excessive future timestamps are clamped to a small bounded horizon.
        let target_tick = intended_tick.0.clamp(next_tick, latest_tick).max(
            existing
                .iter()
                .map(|entry| entry.tick.0)
                .max()
                .unwrap_or(next_tick),
        );
        let scheduled = ScheduledInput {
            session,
            entity,
            input,
            seq,
            tick: Tick(target_tick),
        };

        if target_tick == next_tick {
            return (ScheduleResult::Immediate, Some(Tick(next_tick)));
        }

        let key = (target_tick, session.raw());
        let replacing = self.entries.contains_key(&key);
        if self.entries.len() >= MAX_PENDING_GLOBAL && !replacing {
            return (ScheduleResult::Rejected, None);
        }
        if replacing && self.superseded.len() >= MAX_SUPERSEDED_GLOBAL {
            return (ScheduleResult::Rejected, None);
        }

        if let Some(replaced) = self.entries.insert(key, scheduled) {
            self.superseded.insert((session.raw(), replaced.seq.0));
        }
        (ScheduleResult::Queued, None)
    }

    pub fn take_due(&mut self, next_tick: Tick) -> Vec<ScheduledInput> {
        let keys: Vec<_> = self
            .entries
            .range(..=(next_tick.0, u64::MAX))
            .map(|(key, _)| *key)
            .collect();
        let applied: Vec<_> = keys
            .into_iter()
            .filter_map(|key| self.entries.remove(&key))
            .collect();
        for entry in &applied {
            self.superseded
                .retain(|(raw, seq)| *raw != entry.session.raw() || *seq > entry.seq.0);
        }
        applied
    }

    pub fn remove_session(&mut self, session: SessionId) {
        self.entries.retain(|_, entry| entry.session != session);
        self.superseded.retain(|(raw, _)| *raw != session.raw());
    }

    pub fn remove_slot(&mut self, slot: u32) {
        self.entries
            .retain(|_, entry| entry.session.slot().0 != slot);
        self.superseded
            .retain(|(raw, _)| (raw >> 32) as u32 != slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_client::{ClientPhysics, PredictedPlayer};
    use spall_core::{Tick, player_entity_for};
    use spall_physics::{CharacterParams, CharacterState};
    use spall_protocol::{InputSeq, SessionId, SlotId};
    use spall_sim::{Simulation, SimulationConfig, fixtures};

    const DT: f32 = 1.0 / 60.0;

    fn session(generation: u32) -> SessionId {
        SessionId::from_parts(SlotId(2), generation)
    }

    fn player_input(forward: f32) -> PlayerInput {
        PlayerInput {
            movement: [0.0, 0.0, forward],
            view_dir: [1.0, 0.0, 0.0],
            ..PlayerInput::NEUTRAL
        }
    }

    #[test]
    fn future_edges_run_on_the_intended_server_tick_and_redundant_copies_do_not_run_early() {
        let session = session(1);
        let player = player_entity_for(2);
        let mut queue = PlayerInputSchedule::default();
        let (result, due) = queue.schedule(
            session,
            player,
            player_input(-1.0),
            InputSeq(11),
            Tick(104),
            Tick(100),
        );
        assert_eq!(result, ScheduleResult::Queued);
        assert_eq!(due, None);
        assert!(queue.contains(session, InputSeq(11)));
        assert!(queue.take_due(Tick(103)).is_empty());
        let applied = queue.take_due(Tick(104));
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].tick, Tick(104));
        assert_eq!(applied[0].seq, InputSeq(11));
        assert_eq!(applied[0].input.movement[2], -1.0);
        assert!(!queue.contains(session, InputSeq(11)));
    }

    #[test]
    fn a_recovered_redundant_copy_of_a_lost_primary_lands_on_its_own_intended_tick() {
        let session = session(1);
        let player = player_entity_for(2);
        let mut queue = PlayerInputSchedule::default();
        // Seq 11's own primary datagram never arrived; it is only recovered
        // from a later datagram's "recent" copies, carrying the same
        // intended_tick the primary would have. Admitting it must schedule
        // it against that tick, not apply it on whatever tick is next --
        // that immediate-apply is exactly the whole-tick edge snap this
        // scheduling exists to remove, and it is precisely the packet-loss
        // case the redundant-copy mechanism exists for.
        let admitted = queue.admit(
            session,
            player,
            player_input(-1.0),
            InputSeq(11),
            Tick(104),
            Tick(100),
            None,
        );
        assert_eq!(
            admitted, None,
            "a future-tick recovery must not be applied immediately"
        );
        assert!(queue.take_due(Tick(103)).is_empty());
        let due = queue.take_due(Tick(104));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].seq, InputSeq(11));
        assert_eq!(due[0].tick, Tick(104));
        assert_eq!(due[0].input.movement[2], -1.0);
    }

    #[test]
    fn admit_applies_a_next_tick_recovery_immediately_like_a_primary_frame() {
        let session = session(1);
        let player = player_entity_for(2);
        let mut queue = PlayerInputSchedule::default();
        // A recovered copy whose intended_tick has already arrived (a
        // stale/late redundant copy) behaves exactly like an ordinary
        // next-tick primary frame: the caller applies it right away.
        let admitted = queue.admit(
            session,
            player,
            player_input(1.0),
            InputSeq(5),
            Tick(50),
            Tick(100),
            None,
        );
        assert_eq!(admitted, Some(player_input(1.0)));
    }

    #[test]
    fn admit_skips_a_copy_already_tracked_or_already_applied() {
        let session = session(1);
        let player = player_entity_for(2);
        let mut queue = PlayerInputSchedule::default();
        assert!(
            queue
                .admit(
                    session,
                    player,
                    player_input(1.0),
                    InputSeq(11),
                    Tick(104),
                    Tick(100),
                    None,
                )
                .is_none()
        );
        // Already tracked by this queue: a second recovery of the same seq
        // (e.g. it rides along in more than one later datagram) does not
        // re-admit it.
        assert!(
            queue
                .admit(
                    session,
                    player,
                    player_input(1.0),
                    InputSeq(11),
                    Tick(104),
                    Tick(100),
                    None,
                )
                .is_none()
        );
        assert_eq!(queue.take_due(Tick(104)).len(), 1);
        // Already applied: the sim's acked seq has moved past it, so
        // recovering the same seq again is a no-op rather than re-injecting
        // stale input after it has already run.
        assert!(
            queue
                .admit(
                    session,
                    player,
                    player_input(1.0),
                    InputSeq(11),
                    Tick(104),
                    Tick(105),
                    Some(InputSeq(11)),
                )
                .is_none()
        );
        assert!(queue.take_due(Tick(105)).is_empty());
    }

    #[test]
    fn stale_and_far_future_ticks_are_clamped_and_pending_state_is_bounded() {
        let session = session(1);
        let player = player_entity_for(2);
        let mut queue = PlayerInputSchedule::default();
        assert_eq!(
            queue
                .schedule(
                    session,
                    player,
                    player_input(0.0),
                    InputSeq(1),
                    Tick(1),
                    Tick(100),
                )
                .1,
            Some(Tick(101))
        );
        let (result, due) = queue.schedule(
            session,
            player,
            player_input(1.0),
            InputSeq(2),
            Tick(u64::MAX),
            Tick(100),
        );
        assert_eq!(result, ScheduleResult::Queued);
        assert_eq!(due, None);
        assert!(queue.take_due(Tick(136)).is_empty());
        assert_eq!(queue.take_due(Tick(137)).len(), 1);
    }

    #[test]
    fn reconnect_or_disconnect_removes_only_the_old_session_queue() {
        let old = session(1);
        let new = session(2);
        let player = player_entity_for(2);
        let mut queue = PlayerInputSchedule::default();
        for (session, seq) in [(old, 1), (new, 2)] {
            assert_eq!(
                queue
                    .schedule(
                        session,
                        player,
                        player_input(1.0),
                        InputSeq(seq),
                        Tick(104),
                        Tick(100),
                    )
                    .0,
                ScheduleResult::Queued
            );
        }
        queue.remove_session(old);
        let applied = queue.take_due(Tick(104));
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].session, new);
        assert_eq!(applied[0].seq, InputSeq(2));
    }

    #[test]
    fn a_lost_older_recent_copy_stays_eligible_while_a_newer_frame_is_queued() {
        let session = session(1);
        let player = player_entity_for(2);
        let mut queue = PlayerInputSchedule::default();
        // Sequence 10 was lost; it has never entered the pending queue, so its
        // redundant copy remains eligible for the server's recovery path even
        // while a newer sequence is waiting for its intended tick.
        let (result, due) = queue.schedule(
            session,
            player,
            player_input(1.0),
            InputSeq(11),
            Tick(104),
            Tick(100),
        );
        assert_eq!(result, ScheduleResult::Queued);
        assert_eq!(due, None);
        assert!(!queue.contains(session, InputSeq(10)));
        assert!(queue.contains(session, InputSeq(11)));
    }

    #[test]
    fn newer_same_tick_input_coalesces_without_replaying_the_replaced_recent_copy_early() {
        let session = session(1);
        let player = player_entity_for(2);
        let mut queue = PlayerInputSchedule::default();
        assert_eq!(
            queue
                .schedule(
                    session,
                    player,
                    player_input(1.0),
                    InputSeq(11),
                    Tick(104),
                    Tick(100),
                )
                .0,
            ScheduleResult::Queued
        );
        assert_eq!(
            queue
                .schedule(
                    session,
                    player,
                    player_input(-1.0),
                    InputSeq(12),
                    Tick(102),
                    Tick(100),
                )
                .0,
            ScheduleResult::Queued
        );
        assert!(queue.contains(session, InputSeq(11)));
        assert!(queue.take_due(Tick(103)).is_empty());
        let due = queue.take_due(Tick(104));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].seq, InputSeq(12));
        assert_eq!(due[0].input.movement[2], -1.0);
        assert!(!queue.contains(session, InputSeq(11)));
    }

    #[test]
    fn increasing_sequences_keep_due_ticks_monotonic() {
        let session = session(1);
        let player = player_entity_for(2);
        let mut queue = PlayerInputSchedule::default();
        assert_eq!(
            queue
                .schedule(
                    session,
                    player,
                    player_input(1.0),
                    InputSeq(11),
                    Tick(102),
                    Tick(100),
                )
                .0,
            ScheduleResult::Queued
        );
        assert_eq!(
            queue
                .schedule(
                    session,
                    player,
                    player_input(-1.0),
                    InputSeq(12),
                    Tick(104),
                    Tick(100),
                )
                .0,
            ScheduleResult::Queued
        );
        assert!(queue.take_due(Tick(101)).is_empty());
        let first = queue.take_due(Tick(102));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].seq, InputSeq(11));
        assert_eq!(first[0].tick, Tick(102));
        assert!(queue.take_due(Tick(103)).is_empty());
        let second = queue.take_due(Tick(104));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].seq, InputSeq(12));
        assert_eq!(second[0].tick, Tick(104));
    }

    #[test]
    fn queue_memory_is_bounded_globally_without_applying_excess_frames_early() {
        let mut queue = PlayerInputSchedule::default();
        for slot in 0..MAX_PENDING_GLOBAL as u32 {
            let session = SessionId::from_parts(SlotId(slot), 1);
            assert_eq!(
                queue
                    .schedule(
                        session,
                        player_entity_for(slot),
                        player_input(1.0),
                        InputSeq(1),
                        Tick(104),
                        Tick(100),
                    )
                    .0,
                ScheduleResult::Queued
            );
        }
        let extra = SessionId::from_parts(SlotId(MAX_PENDING_GLOBAL as u32), 1);
        let (result, due) = queue.schedule(
            extra,
            player_entity_for(MAX_PENDING_GLOBAL as u32),
            player_input(-1.0),
            InputSeq(1),
            Tick(104),
            Tick(100),
        );
        assert_eq!(result, ScheduleResult::Rejected);
        assert_eq!(due, None);
        assert_eq!(queue.take_due(Tick(103)).len(), 0);
        assert_eq!(queue.take_due(Tick(104)).len(), MAX_PENDING_GLOBAL);
    }

    #[test]
    fn delayed_direction_reversal_matches_client_prediction_at_the_scheduled_boundary() {
        let setup = fixtures::walk_arena_setup();
        let terrain = setup.terrain.clone();
        let spawn = fixtures::WALK_ARENA_SPAWNS[0];
        let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
        let session = session(1);
        let entity = player_entity_for(session.slot().0);
        sim.add_player(entity, spawn);
        let mut client_physics = ClientPhysics::new();
        client_physics.set_terrain(&terrain);
        let mut predictor =
            PredictedPlayer::new(CharacterParams::DEFAULT, CharacterState::at(spawn), Tick(0));
        let forward = player_input(1.0);
        let reverse = player_input(-1.0);

        // Warm both sides in lockstep, then let the client run three predicted
        // ticks ahead before the reversal datagram reaches the server.
        for seq in 1..=12 {
            sim.set_player_input(entity, forward, InputSeq(seq));
            predictor.tick(&mut client_physics, &terrain, forward, InputSeq(seq), DT);
            sim.tick().unwrap();
        }
        let before_reverse = sim.player_state(entity).unwrap().position_m[0];
        for seq in 13..=15 {
            predictor.tick(&mut client_physics, &terrain, forward, InputSeq(seq), DT);
        }

        let intended_tick = Tick(sim.current_tick().0 + 4);
        assert_eq!(intended_tick, Tick(16));
        let mut queue = PlayerInputSchedule::default();
        assert_eq!(
            queue
                .schedule(
                    session,
                    entity,
                    reverse,
                    InputSeq(16),
                    intended_tick,
                    sim.current_tick(),
                )
                .0,
            ScheduleResult::Queued
        );

        for _ in 0..3 {
            let next = Tick(sim.current_tick().0 + 1);
            assert!(queue.take_due(next).is_empty());
            sim.tick().unwrap();
            let x = sim.player_state(entity).unwrap().position_m[0];
            assert!(
                x >= before_reverse - 0.01,
                "reversal applied early at tick {}",
                next.0
            );
        }
        let before_target = sim.player_state(entity).unwrap().position_m[0];
        let target = Tick(sim.current_tick().0 + 1);
        assert_eq!(target, intended_tick);
        let due = queue.take_due(target);
        assert_eq!(due.len(), 1);
        sim.set_player_input(entity, due[0].input, due[0].seq);
        sim.tick().unwrap();
        let after_reverse = sim.player_state(entity).unwrap().position_m[0];
        assert!(
            after_reverse < before_target,
            "reverse edge did not move along x: before={before_target}, after={after_reverse}"
        );

        for seq in 16..=18 {
            predictor.tick(&mut client_physics, &terrain, reverse, InputSeq(seq), DT);
        }
        let outcome = predictor.reconcile(
            &mut client_physics,
            &terrain,
            sim.player_state(entity).unwrap(),
            sim.player_acked_input(entity).unwrap(),
            sim.current_tick(),
        );
        let raw_snap_m = outcome
            .predicted_before
            .distance_m(&outcome.predicted_after);
        assert!(
            raw_snap_m <= 0.16,
            "scheduled edge snapped {raw_snap_m:.3} m"
        );
    }
}

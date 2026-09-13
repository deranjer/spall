//! ENG-69 round 21: does `PredictedPlayer::reconcile`'s prediction-time /
//! server-time mapping (`Record::tick`, self-healing against
//! `MotionSnapshot::server_tick` every reconcile — see `predict.rs`'s own
//! doc) actually hold up once the client's and server's clocks are *not*
//! guaranteed to advance in lockstep?
//!
//! Every other reconciliation harness in this ticket (`g1_ramp_trace.rs`,
//! `g1_tower_strafe_trace.rs`, `prediction.rs`,
//! `g1_realistic_input_timing_trace.rs`) calls `Simulation::tick()` and
//! `PredictedPlayer::tick()` together, once each, in the same loop
//! iteration, in one process — every single call. That is a genuine,
//! useful test of the *reconciliation logic itself* (retain/replay/compare
//! against realistic delivery gaps and held-input reuse), but it also
//! silently guarantees the one assumption a real client's independently-
//! scheduled ~60 Hz loop does not: that exactly one local tick happens for
//! every server tick that elapses between two reconciles. A round-19/20 fix
//! that only ever ran under that guarantee reported a clean regression
//! result while a real hands-on session's felt jitter visibly got *worse*
//! — this file is the harness that was missing.
//!
//! Each test below drives `Simulation` (the "server") and `PredictedPlayer`
//! (the "client") through their `tick()`/`reconcile()` calls *independently*
//! — different call counts, different timing, sometimes not at all for a
//! stretch — and checks two things a comparison-only report cannot: (1) the
//! `ReconcileOutcome` returned by *every* call, matched or not, has sane,
//! bounded numbers (no panic, no exploding displacement); and (2) the
//! predictor's actual position (queried independently of `reconcile`'s own
//! bookkeeping, via `Simulation::player_state`) stays close to the real
//! server state throughout, not just at the instants a comparison happened
//! to be available.
//!
//! Run with `cargo test -p spall_client --test reconcile_clock_mapping --
//! --nocapture` to see the per-scenario reports.

use spall_client::predict::{ClientPhysics, PredictedPlayer, ReconcileOutcome};
use spall_core::{EntityId, PlayerInput, player_entity_for};
use spall_physics::CharacterParams;
use spall_protocol::InputSeq;
use spall_sim::fixtures::{WALK_ARENA_SPAWNS, walk_arena_setup};
use spall_sim::{Simulation, SimulationConfig, TICK_DT_S};

fn forward() -> PlayerInput {
    PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    }
}

/// Drives a `Simulation` ("server") and a `PredictedPlayer` ("client")
/// through independently-controlled `tick()`/`reconcile()` calls — unlike
/// every other harness in this ticket, `server_tick_n`/`client_tick_n`/
/// `reconcile_now` are separate calls a test can invoke in whatever
/// pattern (and count) it wants, rather than one `step()` that always
/// advances both together.
struct ClockHarness {
    sim: Simulation,
    player: EntityId,
    phys: ClientPhysics,
    predictor: PredictedPlayer,
    seq: u64,
    /// Every `ReconcileOutcome` this harness has produced, in call order —
    /// for a test to inspect the whole trace afterward rather than only
    /// running maxima.
    outcomes: Vec<ReconcileOutcome>,
}

impl ClockHarness {
    /// `spawn_at_tick`: how many server ticks elapse *before*
    /// `PredictedPlayer::new` is constructed — `0` for the ordinary case
    /// (predictor spawns the moment the player exists, `Tick(0)`); nonzero
    /// models a client that only starts predicting once a first snapshot
    /// arrives for an already-running server (a delayed-initial-snapshot
    /// scenario a live join always faces).
    fn new(spawn_at_tick: u32) -> Self {
        let mut sim =
            Simulation::new(SimulationConfig::new(walk_arena_setup())).expect("arena valid");
        let player = player_entity_for(0);
        let spawn = WALK_ARENA_SPAWNS[0];
        sim.add_player(player, spawn);

        let mut seq = 0_u64;
        for _ in 0..spawn_at_tick {
            seq += 1;
            sim.set_player_input(player, PlayerInput::NEUTRAL, InputSeq(seq));
            sim.tick().unwrap();
        }

        let mut phys = ClientPhysics::new();
        phys.set_terrain(&sim.world().terrain().volume);
        let spawn_state = sim.player_state(player).unwrap();
        let predictor =
            PredictedPlayer::new(CharacterParams::DEFAULT, spawn_state, sim.current_tick());

        Self {
            sim,
            player,
            phys,
            predictor,
            seq,
            outcomes: Vec::new(),
        }
    }

    /// Advances the *server* only, `n` ticks, with `input` freshly sent
    /// every tick (no held-input reuse in play here — that path is already
    /// covered by `g1_realistic_input_timing_trace.rs`; this file is about
    /// the clock mapping, kept orthogonal).
    fn server_tick_n(&mut self, input: PlayerInput, n: u32) {
        for _ in 0..n {
            self.seq += 1;
            self.sim
                .set_player_input(self.player, input, InputSeq(self.seq));
            self.sim.tick().unwrap();
        }
    }

    /// Advances the *client* prediction only, `n` local ticks — genuinely
    /// independent of `server_tick_n`'s own call count, exactly the
    /// decoupling a real client's own wall-clock loop has from the
    /// server's.
    fn client_tick_n(&mut self, input: PlayerInput, n: u32) {
        let volume = self.sim.world().terrain().volume.clone();
        for _ in 0..n {
            self.seq += 1;
            self.predictor.tick(
                &mut self.phys,
                &volume,
                input,
                InputSeq(self.seq),
                TICK_DT_S,
            );
        }
    }

    /// Reconciles against the server's *current* real state — whatever
    /// `server_tick_n`/`client_tick_n` calls happened before this one, in
    /// whatever ratio the test chose.
    fn reconcile_now(&mut self) -> ReconcileOutcome {
        let volume = self.sim.world().terrain().volume.clone();
        let auth = self.sim.player_state(self.player).unwrap();
        let acked = self.sim.player_acked_input(self.player).unwrap();
        let outcome = self.predictor.reconcile(
            &mut self.phys,
            &volume,
            auth,
            acked,
            self.sim.current_tick(),
        );
        self.outcomes.push(outcome);
        outcome
    }

    /// Gap between the predictor's *current* prediction and the server's
    /// *actual, current* state — independent of whatever `reconcile`
    /// itself last recorded as `authoritative` (which can trail). This is
    /// the real "does this still track the server" measure a test should
    /// use, not `PredictedPlayer::prediction_error_m` (which compares
    /// against its own possibly-stale `authoritative` field).
    fn live_error_m(&self) -> f64 {
        let server = self.sim.player_state(self.player).unwrap();
        self.predictor.predicted().distance_m(&server)
    }
}

/// Every `ReconcileOutcome` a test collected must be internally sane
/// regardless of whether it matched: finite displacement, a
/// `records_removed + records_replayed` that never exceeds what was in
/// history, and a small comparison error whenever one exists — physics
/// itself did not change this round, only the bookkeeping deciding what to
/// replay, so a matched comparison should still be near-zero even under
/// clock skew.
fn assert_outcomes_are_sane(outcomes: &[ReconcileOutcome]) {
    for (i, o) in outcomes.iter().enumerate() {
        let displacement_m = o.predicted_before.distance_m(&o.predicted_after);
        assert!(
            displacement_m.is_finite() && displacement_m < 100.0,
            "outcome #{i}: non-finite or absurd displacement {displacement_m} m \
             (server_tick={:?} delta={} history_len_before={} removed={} replayed={})",
            o.server_tick,
            o.server_tick_delta,
            o.history_len_before,
            o.records_removed,
            o.records_replayed,
        );
        assert_eq!(
            o.records_removed + o.records_replayed,
            o.history_len_before,
            "outcome #{i}: removed ({}) + replayed ({}) != history_len_before ({}) — retain \
             accounting is wrong",
            o.records_removed,
            o.records_replayed,
            o.history_len_before,
        );
        if let Some(c) = o.comparison {
            // A one-tick-scale gap (`WALK_SPEED_M_S / 60 = 0.075 m`) is
            // ordinary collision-resolution noise on `walk_arena_setup`
            // (unlike G1, never specifically validated seam-free —
            // `prediction.rs`'s own existing tests already tolerate up to
            // 0.15-0.5 m here) — this bound exists to catch the *old*
            // double-counting bug's signature (0.5-0.75 m, many ticks'
            // worth, compounding across a held stretch), not to police
            // ordinary per-tick physics noise this round never touched.
            assert!(
                c.error_m < 0.1,
                "outcome #{i}: matched comparison error {:.6} m is far more than one tick's \
                 worth of ordinary noise — looks like the old double-counting signature, not \
                 collision noise",
                c.error_m
            );
        }
    }
}

/// Server and client tick at genuinely different, irregular cadences —
/// never a fixed ratio, so no single "ack_delay"-style constant could
/// paper over it. Reconciles every round regardless.
#[test]
fn independently_scheduled_clocks_stay_bounded_and_reconverge() {
    let mut h = ClockHarness::new(0);
    // Deliberately irregular: server and client never advance the same
    // amount in the same round, and the *difference* between them varies
    // round to round instead of drifting monotonically in one direction —
    // closer to real independent-clock jitter than a clean linear skew.
    let server_pattern = [3_u32, 3, 3, 3, 3, 3, 3, 3];
    let client_pattern = [2_u32, 4, 3, 1, 5, 2, 3, 4];
    let rounds = 60;

    for i in 0..rounds {
        let s = server_pattern[i % server_pattern.len()];
        let c = client_pattern[i % client_pattern.len()];
        h.server_tick_n(forward(), s);
        h.client_tick_n(forward(), c);
        h.reconcile_now();
    }

    assert_outcomes_are_sane(&h.outcomes);

    let unmatched = h.outcomes.iter().filter(|o| o.comparison.is_none()).count();
    let matched = h.outcomes.len() - unmatched;
    eprintln!(
        "independently-scheduled clocks: {} reconciles, {matched} matched, {unmatched} \
         unmatched, final live error {:.4} m",
        h.outcomes.len(),
        h.live_error_m()
    );
    // Sanity check on the *scenario*, mirroring this ticket's established
    // pattern (`g1_realistic_input_timing_trace.rs`'s own `max_held`
    // assertion): the irregular schedule above must actually produce both
    // outcomes, or this test isn't exercising what it claims to.
    assert!(
        unmatched > 0,
        "schedule never produced an unmatched reconcile — not exercising drift"
    );
    assert!(
        matched > 0,
        "schedule never produced a matched reconcile — too extreme to be realistic"
    );

    // The real correctness bar: even under this irregular schedule, the
    // predictor's live position never diverges far from the server's —
    // "no comparison available" must not mean "silently wrong forever".
    assert!(
        h.live_error_m() < 1.0,
        "predictor drifted {:.4} m from the live server position under independently-scheduled \
         clocks — should stay closely tracked even without a per-tick comparison",
        h.live_error_m()
    );
}

/// The client stops ticking entirely for a stretch (a frozen render/mover
/// thread — a GC pause, a blocked syscall, anything that stalls the loop
/// but not the process) while the server keeps advancing and reconcile
/// keeps getting called (a real client's motion-snapshot reader task is
/// typically a separate task/thread from the mover loop, so it can keep
/// receiving and reconciling even while the mover itself is stuck).
#[test]
fn client_stall_reconverges_after_resuming() {
    let mut h = ClockHarness::new(0);

    // Normal lockstep warm-up: matched every time.
    for _ in 0..10 {
        h.server_tick_n(forward(), 1);
        h.client_tick_n(forward(), 1);
        h.reconcile_now();
    }
    let warm_up_unmatched = h.outcomes.iter().filter(|o| o.comparison.is_none()).count();

    // The stall: server and reconcile keep running, client does not tick
    // at all for 30 rounds.
    for _ in 0..30 {
        h.server_tick_n(forward(), 1);
        h.reconcile_now();
    }

    // During the stall, the predictor must track the server's actual
    // position exactly (no local prediction to diverge with — every
    // reconcile has nothing to replay and hard-snaps straight onto
    // `authoritative`), not merely "eventually".
    assert!(
        h.live_error_m() < 1e-6,
        "predictor should exactly mirror the server while stalled (nothing local to replay), \
         got {:.6} m",
        h.live_error_m()
    );

    // Resume: normal lockstep again.
    for _ in 0..20 {
        h.server_tick_n(forward(), 1);
        h.client_tick_n(forward(), 1);
        h.reconcile_now();
    }

    assert_outcomes_are_sane(&h.outcomes);
    let total_unmatched = h.outcomes.iter().filter(|o| o.comparison.is_none()).count();
    eprintln!(
        "client stall: {} reconciles ({warm_up_unmatched} unmatched before the stall, {} \
         unmatched total), final live error {:.6} m",
        h.outcomes.len(),
        total_unmatched,
        h.live_error_m()
    );
    assert_eq!(
        warm_up_unmatched, 0,
        "lockstep warm-up should match every time"
    );
    assert!(
        total_unmatched >= 30,
        "expected at least the 30 stalled reconciles to be unmatched, got {total_unmatched}"
    );
    // After resuming lockstep, prediction should be tightly tracking again
    // — the stall must not leave a permanent residual.
    assert!(
        h.live_error_m() < 1e-3,
        "predictor did not reconverge after the client resumed ticking: {:.6} m",
        h.live_error_m()
    );
}

/// The predictor doesn't spawn at the same instant the server does — it
/// only starts existing once a first snapshot for this player arrives,
/// same as a real client joining a session the server has already been
/// running (`net.rs` constructs `PredictedPlayer::new` from the first
/// `MotionSnapshot` it ever sees for this entity). Confirms `spawn_tick`
/// anchoring one used to needing a "no anchor" special case (ENG-69 round
/// 19's own bug) — extends that to a genuinely delayed spawn, not just
/// `Tick(0)`.
#[test]
fn delayed_initial_snapshot_anchors_cleanly() {
    // The server has already been running for 500 ticks before this
    // player (and its predictor) ever exist.
    let mut h = ClockHarness::new(500);

    for _ in 0..40 {
        h.server_tick_n(forward(), 1);
        h.client_tick_n(forward(), 1);
        h.reconcile_now();
    }

    assert_outcomes_are_sane(&h.outcomes);
    let unmatched = h.outcomes.iter().filter(|o| o.comparison.is_none()).count();
    eprintln!(
        "delayed initial snapshot (spawn at tick 500): {} reconciles, {unmatched} unmatched, \
         final live error {:.6} m",
        h.outcomes.len(),
        h.live_error_m()
    );
    assert_eq!(
        unmatched, 0,
        "a predictor anchored at its real spawn tick, ticking in lockstep from there, should \
         match every reconcile regardless of how late that spawn tick was"
    );
    assert!(
        h.live_error_m() < 1e-3,
        "predictor did not track the server: {:.6} m",
        h.live_error_m()
    );
}

/// The client predicts far more locally than `PREDICTION_HISTORY` (128)
/// covers before its first reconcile ever arrives — old records get
/// evicted by `PredictedPlayer::tick`'s own bound before `reconcile` ever
/// sees them. The first reconcile's `server_tick` corresponds to a tick
/// whose record no longer exists at all.
#[test]
fn history_overflow_reports_unmatched_not_a_false_match() {
    use spall_client::predict::PREDICTION_HISTORY;

    let mut h = ClockHarness::new(0);

    // Advance the server by exactly 1 tick — reconcile will ask about
    // *that* tick, which (after the overflow below) can no longer be in
    // `history` at all.
    h.server_tick_n(forward(), 1);
    // Predict locally far past the history bound without ever reconciling
    // — every record for the server's 1 tick above gets evicted well
    // before this loop ends.
    h.client_tick_n(forward(), PREDICTION_HISTORY as u32 * 3);

    let outcome = h.reconcile_now();
    assert_outcomes_are_sane(&h.outcomes);
    eprintln!(
        "history overflow: history_len_before={} removed={} replayed={} matched={} \
         displacement={:.4} m, live error {:.6} m",
        outcome.history_len_before,
        outcome.records_removed,
        outcome.records_replayed,
        outcome.comparison.is_some(),
        outcome
            .predicted_before
            .distance_m(&outcome.predicted_after),
        h.live_error_m()
    );
    assert!(
        outcome.comparison.is_none(),
        "the record this reconcile asked about was evicted by PREDICTION_HISTORY's own bound \
         well before this call — a comparison here would have to be a false match against the \
         wrong tick, not a real one"
    );
    // The overflowed local ticks *are* still real, valid prediction —
    // `history` holds the most recent `PREDICTION_HISTORY` of them, and
    // retain/replay over that remainder must still behave: every kept
    // record replays cleanly, `history_len_before` is capped at exactly
    // `PREDICTION_HISTORY` (the bound `tick` itself enforces), and nothing
    // panics.
    assert_eq!(outcome.history_len_before, PREDICTION_HISTORY);
}

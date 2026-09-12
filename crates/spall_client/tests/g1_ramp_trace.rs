//! ENG-69 round 10: a detailed G1 ramp movement trace through the *real*
//! prediction/reconciliation path (`spall_client::predict::PredictedPlayer` /
//! `ClientPhysics` against a real `spall_sim::Simulation`), added on review
//! feedback that round 9's evidence — an endpoint-only ramp-crossing CPU test
//! (`spall_physics::character::tests::
//! walking_the_g1_ramp_diverges_between_representations`) plus the HUD's
//! lifetime-maxima-only reporting (`predict.rs:375` before this round) —
//! supports "the seam explanation is real" but not "every remaining live
//! correction is explained and bounded". Those are different measurements:
//! the ramp test compares two representations' *endpoints* after a fixed
//! tick count; a live correction compares predicted-vs-authoritative at each
//! *acknowledged input*, individually. This file records the latter,
//! directly, through `PredictedPlayer::reconcile`'s new [`CorrectionEvent`]
//! return value, and reports moving vs settled-idle maxima *and* p95
//! separately rather than only a lifetime maximum that a single early spike
//! can pin indefinitely.
//!
//! **Scope, stated plainly rather than left implicit:** this harness (like
//! `prediction.rs`'s own) shares the *same* `Simulation`-owned `Volume`
//! object between "client" and "server" physics — it does not stand up QUIC
//! transport or exercise `ReplicaWorld` reconstruction from wire baseline/
//! patch traffic. ENG-69 rounds 8-9 already isolated and ruled out
//! partial-occupancy-view and representation-type mismatches as the live
//! residual's cause at exactly this level of fidelity (the real
//! `PredictedPlayer` / `ClientPhysics` / `step_character` code, the real
//! `spall_sim` collider policy) — so it is the right tool for what this file
//! asks: does the confirmed `MergedCuboids`-seam mechanism, exercised
//! through the actual reconciliation loop over a repeated ramp traversal
//! rather than one isolated endpoint measurement, look like it explains the
//! *pattern* of live corrections (clustered at the ramp, not recurring once
//! settled on flat ground, not worsening with repeated crossings)? It is
//! **not** a claim that this covers every possible source of live prediction
//! error — a genuine replica desync, or real QUIC jitter under packet loss,
//! needs the real network path this deliberately does not stand up. `100 ms
//! RTT` here means `ack_delay = 6` ticks (`docs/validation.md`'s canonical
//! impaired-link figure, applied the same way `prediction.rs`'s own
//! `Harness::new(ack_delay)` already does), not a live QUIC connection.
//!
//! Run with `cargo test -p spall_client --test g1_ramp_trace -- --nocapture`
//! to see the reports; deliberately not asserting numeric bounds (see this
//! file's own findings for why a hard threshold would be premature) — this
//! is a diagnostic instrument, not an acceptance gate.
//!
//! **Update, ENG-69 round 19:** the original run of this file (round 10)
//! reported a genuine ~0.0750 m moving-max residual, attributed to the
//! `MergedCuboids` seam mechanism this file's own doc above describes.
//! Two things have since changed what that number means: round 18
//! integrated `CharacterQueryCache`, so player movement no longer touches
//! the real terrain collider's representation at all (the seam mechanism
//! is gone from this path); and round 19 fixed a *reconciliation* bug
//! (`PredictedPlayer::reconcile` — see `predict.rs`'s own doc) that,
//! independently, was producing a same-order-of-magnitude ~0.075 m
//! artifact of its own at every phase transition in *this exact harness's*
//! `ack_delay = 0` case (a one-tick-stale comparison, not a seam catch).
//! With both fixed, this file now reports 0 events and a 0.000000 m max —
//! the ~0.0750 m this doc originally described no longer reproduces
//! through either mechanism.

use spall_client::predict::{ClientPhysics, CorrectionEvent, PredictedPlayer};
use spall_core::{EntityId, PlayerInput, Tick, player_entity_for};
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::InputSeq;
use spall_sim::fixtures::g1_full_envelope_setup;
use spall_sim::{Simulation, SimulationConfig, TICK_DT_S};

/// Just before the ramp (`g1_full_envelope_scene`'s doc: ramp cell `x` in
/// `[180, 211]`, `z` in `[100, 115]`, flat height `46` cells outside it —
/// `46 * 0.25 m = 11.5 m`), facing `+X`.
const RAMP_APPROACH_M: [f64; 3] = [44.5, 11.5, 26.5];
/// Ticks to walk one direction across the ramp at `WALK_SPEED_M_S = 4.5`:
/// `100 * 4.5 / 60 = 7.5 m`, from `x = 44.5` to `x = 52.0` — short of the
/// ramp's far wall at `x = 52.75 m` (height jumps straight back up to the
/// flat `46` the instant `x` leaves `[180, 211]` within this `z` band;
/// walking into it is a real vertical-wall collision case this trace
/// deliberately avoids, to keep the measured signal isolated to the ramp's
/// own internal tread seams rather than mixed with wall-collision behavior).
const RAMP_LEG_TICKS: usize = 100;
const SETTLE_TICKS: usize = 30;
const IDLE_PAUSE_TICKS: usize = 20;
const ROUND_TRIPS: usize = 3;
const FINAL_IDLE_TICKS: usize = 200;
/// `docs/validation.md`'s canonical impaired-link figure, in ticks at the
/// server's fixed `1/60 s` tick.
const RTT_100MS_ACK_DELAY: usize = 6;

fn idle() -> PlayerInput {
    PlayerInput::NEUTRAL
}

fn downhill() -> PlayerInput {
    PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    }
}

fn uphill() -> PlayerInput {
    PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [-1.0, 0.0, 0.0],
        buttons: 0,
    }
}

#[derive(Clone, Copy)]
struct Sample {
    tick: usize,
    phase: &'static str,
    event: CorrectionEvent,
}

struct Harness {
    sim: Simulation,
    player: EntityId,
    phys: ClientPhysics,
    predictor: PredictedPlayer,
    server_log: Vec<(CharacterState, InputSeq, Tick)>,
    /// The phase label active on each tick, index-parallel to `server_log` —
    /// needed because a reconciled event at `ack_delay > 0` belongs to an
    /// *older* tick than the one triggering the `reconcile()` call. Tagging
    /// with the *current* tick's phase instead would mislabel any event
    /// whose acked input predates a phase transition still inside the ack
    /// window (this bit a first draft of this file: several `idle-*`-phase
    /// events came out `idle=false`, correctly — `event.idle` reflects the
    /// actual historical record — but with a misleading phase string).
    phase_log: Vec<&'static str>,
    seq: u64,
    ack_delay: usize,
    /// `reconcile` calls whose `ReconcileOutcome::comparison` was `None` —
    /// see `g1_tower_strafe_trace.rs`'s identical field for why this
    /// harness (client and server ticked together every loop iteration, in
    /// one process) expects it to always be `0`.
    unmatched: u64,
    /// Count of `ClientPhysics::set_terrain` / `PredictedPlayer::invalidate`
    /// calls — the "terrain rebuild/invalidation events" the review asked to
    /// see tracked. `1` after `new` (the initial build); this trace submits
    /// no edits and enables no residency/eviction, so none further are
    /// expected — matching the live `cargo xtask play` session's own steady
    /// state (residency is default-off there too).
    terrain_events: u32,
}

impl Harness {
    fn new(ack_delay: usize) -> Self {
        let mut sim = Simulation::new(SimulationConfig::new(g1_full_envelope_setup()))
            .expect("g1 world valid");
        let player = player_entity_for(0);
        sim.add_player(player, RAMP_APPROACH_M);

        let mut phys = ClientPhysics::new();
        phys.set_terrain(&sim.world().terrain().volume);
        let predictor = PredictedPlayer::new(
            CharacterParams::DEFAULT,
            CharacterState::at(RAMP_APPROACH_M),
            sim.current_tick(),
        );

        Self {
            sim,
            player,
            phys,
            predictor,
            server_log: Vec::new(),
            phase_log: Vec::new(),
            seq: 0,
            ack_delay,
            unmatched: 0,
            terrain_events: 1,
        }
    }

    fn step(&mut self, input: PlayerInput, phase: &'static str, samples: &mut Vec<Sample>) {
        self.seq += 1;
        let seq = InputSeq(self.seq);
        self.sim.set_player_input(self.player, input, seq);
        self.sim.tick().unwrap();
        self.server_log.push((
            self.sim.player_state(self.player).unwrap(),
            self.sim.player_acked_input(self.player).unwrap(),
            self.sim.current_tick(),
        ));
        self.phase_log.push(phase);
        let volume = self.sim.world().terrain().volume.clone();
        self.predictor
            .tick(&mut self.phys, &volume, input, seq, TICK_DT_S);
        if self.server_log.len() > self.ack_delay {
            let record_index = self.server_log.len() - 1 - self.ack_delay;
            let (auth, acked, server_tick) = self.server_log[record_index];
            let outcome =
                self.predictor
                    .reconcile(&mut self.phys, &volume, auth, acked, server_tick);
            if let Some(event) = outcome.comparison {
                samples.push(Sample {
                    tick: record_index,
                    phase: self.phase_log[record_index],
                    event,
                });
            } else {
                self.unmatched += 1;
            }
        }
    }
}

/// Nearest-rank percentile (this crate's stated convention — see
/// `spall_physics::metrics`'s own `percentiles_use_nearest_rank` test).
fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Returns the reconciled samples plus how many `reconcile` calls had no
/// `comparison` at all — see `Harness::unmatched`'s own doc.
fn run_trace(ack_delay: usize) -> (Vec<Sample>, u64) {
    let mut h = Harness::new(ack_delay);
    let mut samples = Vec::new();

    let leg = |h: &mut Harness,
               input: PlayerInput,
               phase: &'static str,
               ticks: usize,
               samples: &mut Vec<Sample>| {
        for _ in 0..ticks {
            h.step(input, phase, samples);
        }
    };

    leg(&mut h, idle(), "settle", SETTLE_TICKS, &mut samples);
    for _ in 0..ROUND_TRIPS {
        leg(&mut h, downhill(), "downhill", RAMP_LEG_TICKS, &mut samples);
        leg(
            &mut h,
            idle(),
            "idle-bottom",
            IDLE_PAUSE_TICKS,
            &mut samples,
        );
        leg(&mut h, uphill(), "uphill", RAMP_LEG_TICKS, &mut samples);
        leg(&mut h, idle(), "idle-top", IDLE_PAUSE_TICKS, &mut samples);
    }
    leg(&mut h, idle(), "final-idle", FINAL_IDLE_TICKS, &mut samples);

    eprintln!(
        "[ack_delay={ack_delay}] terrain rebuild/invalidation events: {} \
         (1 initial ClientPhysics::set_terrain at Harness::new; no edits are \
         submitted and no residency/eviction is enabled in this trace, so \
         none further are expected)",
        h.terrain_events
    );
    (samples, h.unmatched)
}

fn report(label: &str, samples: &[Sample]) {
    let moving: Vec<&Sample> = samples.iter().filter(|s| !s.event.idle).collect();
    let settled_idle: Vec<&Sample> = samples.iter().filter(|s| s.event.idle).collect();
    eprintln!("--- {label}: {} total reconciled events ---", samples.len());
    for (name, group) in [("moving", &moving), ("settled-idle", &settled_idle)] {
        if group.is_empty() {
            eprintln!("[{label}] {name}: no events");
            continue;
        }
        let errs: Vec<f64> = group.iter().map(|s| s.event.error_m).collect();
        let horiz: Vec<f64> = group.iter().map(|s| s.event.horizontal_m).collect();
        let vert: Vec<f64> = group.iter().map(|s| s.event.vertical_m).collect();
        let max_err = errs.iter().cloned().fold(0.0_f64, f64::max);
        let max_h = horiz.iter().cloned().fold(0.0_f64, f64::max);
        let max_v = vert.iter().cloned().fold(0.0_f64, f64::max);
        eprintln!(
            "[{label}] {name}: n={} | error_m max={:.4} p95={:.4} | horiz_m max={:.4} p95={:.4} | vert_m max={:.4} p95={:.4}",
            group.len(),
            max_err,
            percentile(&errs, 95.0),
            max_h,
            percentile(&horiz, 95.0),
            max_v,
            percentile(&vert, 95.0),
        );
    }

    // Individual correction vectors above a "would be felt" threshold —
    // listing every one of ~1000 near-zero events would bury the ones that
    // matter, but the review specifically asked for individual vectors, so
    // print the ones actually worth reading rather than only summary stats.
    let notable: Vec<&Sample> = samples.iter().filter(|s| s.event.error_m > 0.01).collect();
    eprintln!(
        "[{label}] {} of {} events exceed 0.01 m (per-event vectors):",
        notable.len(),
        samples.len()
    );
    for s in &notable {
        eprintln!(
            "  tick={:4} phase={:<11} idle={:<5} error_m={:.4} vertical_m={:.4} horizontal_m={:.4}",
            s.tick,
            s.phase,
            s.event.idle,
            s.event.error_m,
            s.event.vertical_m,
            s.event.horizontal_m
        );
    }
}

#[test]
fn g1_ramp_trace_loopback() {
    let (samples, unmatched) = run_trace(0);
    report("loopback (ack_delay=0)", &samples);
    assert_eq!(unmatched, 0, "lockstep harness had unmatched reconciles");
}

#[test]
fn g1_ramp_trace_100ms_rtt() {
    let (samples, unmatched) = run_trace(RTT_100MS_ACK_DELAY);
    report("100ms RTT (ack_delay=6)", &samples);
    assert_eq!(unmatched, 0, "lockstep harness had unmatched reconciles");
}

//! ENG-69 round 19: does the live, human-driven session's repeated ~0.150 m
//! horizontal correction survive once the *input timing* — not the collider
//! representation, confirmed stable and corrections-free by round 18 — is
//! modelled realistically?
//!
//! Every earlier reconciliation harness in this ticket (`g1_ramp_trace.rs`,
//! `g1_tower_strafe_trace.rs`, `prediction.rs`) shares the same
//! simplification: one fresh input, with a strictly-incrementing sequence
//! number, sent on *every single server tick*, reconciled on *every single
//! tick* against `server_log[len - 1 - ack_delay]` — a fixed, uniform
//! offset. Real client/server timing is neither: `spall_sim::player::
//! Player::effective_input` reuses the last accepted input for up to
//! `HELD_INPUT_TIMEOUT_TICKS` (15 ticks / 250 ms) whenever a fresh frame
//! hasn't arrived that tick — a `PlayerInput` datagram is best-effort, not
//! guaranteed once per tick — and `MotionPublisher` (`spall_sim::
//! replication`) only *publishes* an authoritative snapshot (the only thing
//! that ever triggers a real `reconcile()` call) at `MOTION_SNAPSHOT_HZ`
//! (20 Hz — `interval_ticks = 60 / 20 = 3`), not every server tick. A
//! one-fresh-input-per-tick harness can never exercise held-input reuse at
//! all (every tick already has a fresh input, so `Player::pending_fresh` is
//! always true) and reconciles 3x more often than the real client ever
//! does, each time against a hand-picked fixed offset rather than whatever
//! `acked_input` a real snapshot happens to carry.
//!
//! This harness fixes both: input delivery is scripted with deliberate
//! gaps (so `effective_input`'s held-input-reuse path actually engages,
//! including a stretch that holds close to the 15-tick timeout), and
//! `PredictedPlayer::reconcile` is called only on ticks a real
//! `MotionPublisher` would actually publish on, using whatever
//! `Simulation::player_acked_input` naturally reports there — no synthetic
//! per-tick offset. Uses the same G1-tower wall-hug movement pattern as
//! `g1_tower_strafe_trace.rs` (the scenario the live session's jitter was
//! actually reported in), since round 18 already confirmed flat, evenly-
//! ticked input on that same geometry is corrections-free.
//!
//! Per the review that asked for this file: records, for every reconcile
//! event, the snapshot's server tick, its `acked_input` sequence, how many
//! ticks that specific input had already been held/reused by the time it
//! was accepted, which locally-recorded input frame it corresponds to, and
//! the resulting correction — plus, separately, each side's window-cache
//! usage/fallback state that tick, so a correction can be cross-checked
//! against "was either side actually using its window right then" without
//! re-deriving it. Run with `cargo test -p spall_client --test
//! g1_realistic_input_timing_trace -- --nocapture` to see the reports.

use spall_client::predict::{ClientPhysics, CorrectionEvent, PredictedPlayer};
use spall_core::{EntityId, PlayerInput, player_entity_for};
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::InputSeq;
use spall_sim::fixtures::g1_full_envelope_setup;
use spall_sim::{Simulation, SimulationConfig, TICK_DT_S};

/// Same start point as `g1_tower_strafe_trace.rs` / `spall_physics::
/// character::tests::strafing_the_g1_tower_wall_diverges_between_
/// representations`: 1 m west of the G1 tower's west wall, mid-span on z.
const TOWER_APPROACH_M: [f64; 3] = [5.0, 11.5, 10.0];
const APPROACH_TICKS: usize = 30;
const ROUND_TRIPS: usize = 3;
const FINAL_IDLE_TICKS: usize = 200;

/// `spall_protocol::handshake::MOTION_SNAPSHOT_HZ` (20) at the server's
/// fixed 60 Hz tick — `MotionPublisher::interval_ticks`, re-derived here
/// rather than constructing a `MotionPublisher` just for this one constant.
/// A real client's `reconcile()` only ever fires on a multiple of this.
const SNAPSHOT_INTERVAL_TICKS: u64 = 3;
/// `spall_sim::player::HELD_INPUT_TIMEOUT_TICKS` — after this many ticks
/// without a fresh frame, the server drops movement/buttons to neutral
/// rather than continuing to reuse them (facing is kept). The held-input
/// script below stays under this everywhere it means to model "still
/// moving, just not freshly acknowledged yet" rather than "actually let
/// go of the key".
const HELD_INPUT_TIMEOUT_TICKS: u32 = 15;

fn idle() -> PlayerInput {
    PlayerInput::NEUTRAL
}

fn approach() -> PlayerInput {
    PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    }
}

fn hug_positive() -> PlayerInput {
    PlayerInput {
        movement: [1.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    }
}

fn hug_negative() -> PlayerInput {
    PlayerInput {
        movement: [-1.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    }
}

/// One locally-recorded input frame this harness actually "sent" (called
/// `accept_input` with) — the ground truth `reconcile`'s `acked_input`
/// gets cross-referenced against, since neither `PredictedPlayer` nor
/// `Player` exposes "which frame is this seq" as a queryable record.
#[derive(Clone, Copy)]
struct SentInput {
    seq: InputSeq,
    input: PlayerInput,
    sent_at_tick: u64,
    /// How many consecutive ticks (including this one) this exact input
    /// had been the *active* one — i.e. this harness's own count of what
    /// `Player::ticks_since_input`/`pending_fresh` track server-side. `1`
    /// the tick it's freshly sent; increments on every later tick the
    /// server would have reused it (a gap in this harness's own send
    /// schedule) up to (not including) the next fresh send.
    held_for_ticks: u32,
}

/// One reconcile event, with the timing context the review asked to see
/// alongside the correction itself.
struct ReconcileRecord {
    snapshot_tick: u64,
    acked_seq: InputSeq,
    /// The locally-recorded frame `acked_seq` corresponds to — `None` only
    /// if the snapshot acknowledges a seq older than this harness kept
    /// around (shouldn't happen at this trace's length, checked below).
    acked_frame: Option<SentInput>,
    event: CorrectionEvent,
    /// Server-vs-predicted displacement this reconcile actually produced:
    /// the *authoritative* position minus what was predicted for the same
    /// acked input before reconciling — i.e. `event.error_m`'s signed,
    /// axis-broken-out source, kept alongside it rather than re-derived.
    displacement_m: [f64; 3],
    server_window_sweeps: u64,
    server_window_rebuilds: u64,
    server_terrain_fallbacks: u64,
    client_window_sweeps: u64,
    client_window_rebuilds: u64,
    client_terrain_fallbacks: u64,
}

struct Harness {
    sim: Simulation,
    player: EntityId,
    phys: ClientPhysics,
    predictor: PredictedPlayer,
    /// The *client's own* local tick/input-generation counter — increments
    /// on **every** local tick, unconditionally, exactly like `net.rs`'s
    /// real mover loop (`p.input_seq += 1` every iteration, regardless of
    /// whether that frame's datagram ends up delivered). Never to be
    /// confused with whether *this* tick's frame reached the server —
    /// those are genuinely independent in reality, and conflating them was
    /// this file's own first-draft bug (see git history): reusing the last
    /// *delivered* seq/input for `predictor.tick` during a withheld tick
    /// silently duplicated history records under one seq, which is not
    /// what a real client ever does.
    client_local_seq: u64,
    tick: u64,
    /// Every frame this harness actually delivered to the server (i.e.
    /// called `set_player_input` with) — a strict subset of every tick,
    /// since `client_local_seq` increments unconditionally but delivery
    /// doesn't. `reconcile`'s `acked_input` can only ever be one of these.
    sent_log: Vec<SentInput>,
    /// The most recently *delivered* frame's `held_for_ticks` counter —
    /// bumped each tick this harness withholds delivery (so the server's
    /// `effective_input` reuses the prior one), reset to `1` on every
    /// delivered frame.
    current_held_for_ticks: u32,
    /// Window-cache counters as of the last processed reconcile tick, for
    /// computing this-interval deltas the same way the interactive HUD
    /// does (`window.rs`'s `Hud::report`). `spall_sim::world::SimWorld::
    /// window_stats` (aggregate across all players — this harness only
    /// ever has one) and `ClientPhysics::window_stats` share the same
    /// `spall_physics::WindowStats` shape (ENG-69 round 19).
    last_server_window: spall_physics::WindowStats,
    last_client_window: spall_physics::WindowStats,
}

impl Harness {
    fn new() -> Self {
        let mut sim = Simulation::new(SimulationConfig::new(g1_full_envelope_setup()))
            .expect("g1 world valid");
        let player = player_entity_for(0);
        sim.add_player(player, TOWER_APPROACH_M);

        let mut phys = ClientPhysics::new();
        phys.set_terrain(&sim.world().terrain().volume);
        let predictor = PredictedPlayer::new(
            CharacterParams::DEFAULT,
            CharacterState::at(TOWER_APPROACH_M),
        );

        Self {
            sim,
            player,
            phys,
            predictor,
            client_local_seq: 0,
            tick: 0,
            sent_log: Vec::new(),
            current_held_for_ticks: 0,
            last_server_window: spall_physics::WindowStats::default(),
            last_client_window: spall_physics::WindowStats::default(),
        }
    }

    /// One full local tick: the client generates a fresh local seq and
    /// predicts against it *unconditionally* (exactly like a real mover
    /// loop — the client never skips ticking its own prediction just
    /// because a network frame might not land), then this tick's frame is
    /// either delivered to the server (`deliver = true`, arrives this
    /// tick) or withheld (`deliver = false` — network jitter or a delayed/
    /// dropped datagram; `Player::effective_input` reuses whatever the
    /// server last accepted, automatically, exactly as `spall_sim::player`
    /// already implements). Reconciles *only* when `tick` is one a real
    /// `MotionPublisher` would actually have just published on, using
    /// whatever `acked_input` that snapshot naturally reports — never a
    /// synthetic fixed offset.
    fn step(&mut self, input: PlayerInput, deliver: bool, records: &mut Vec<ReconcileRecord>) {
        self.client_local_seq += 1;
        let client_seq = InputSeq(self.client_local_seq);

        if deliver {
            self.sim.set_player_input(self.player, input, client_seq);
            self.current_held_for_ticks = 1;
            self.sent_log.push(SentInput {
                seq: client_seq,
                input,
                sent_at_tick: self.tick + 1,
                held_for_ticks: 1,
            });
        } else {
            self.current_held_for_ticks += 1;
            if let Some(last) = self.sent_log.last_mut() {
                last.held_for_ticks = self.current_held_for_ticks;
            }
        }

        self.sim.tick().unwrap();
        self.tick += 1;

        let volume = self.sim.world().terrain().volume.clone();
        // The client's *own* local prediction always ticks, every local
        // frame, with its own ever-incrementing seq and whatever input is
        // physically held right now — completely independent of whether
        // this specific frame's datagram happens to reach the server this
        // tick. This is the fix for this file's own first-draft bug: it
        // must never reuse a seq across multiple `predictor.tick` calls.
        self.predictor
            .tick(&mut self.phys, &volume, input, client_seq, TICK_DT_S);

        if !self.tick.is_multiple_of(SNAPSHOT_INTERVAL_TICKS) {
            return;
        }
        let auth = self.sim.player_state(self.player).unwrap();
        let acked = self.sim.player_acked_input(self.player).unwrap();
        let acked_frame = self.sent_log.iter().rev().find(|s| s.seq == acked).copied();

        let client_before = self.last_client_window;
        let server_before = self.last_server_window;
        let predicted_before = self.predictor.predicted();
        let event = self.predictor.reconcile(&mut self.phys, &volume, auth, acked);
        let predicted_after = self.predictor.predicted();
        let client_after = self.phys.window_stats();
        // Aggregate (all players — this harness only ever has one) server
        // window usage across every server tick since the last snapshot,
        // matching how the correction itself aggregates prediction error
        // over that same 3-tick interval.
        let server_after = self.sim.world().window_stats();

        if let Some(event) = event {
            // The actually-*felt* displacement this reconcile produced: how
            // far the live "now" prediction (what a camera would be
            // rendering) moved as a direct result of this call — distinct
            // from `event`'s own fields, which measure the *retrospective*
            // gap at the specific acked tick, not the visible jump in the
            // current position.
            let displacement_m = [
                predicted_after.position_m[0] - predicted_before.position_m[0],
                predicted_after.position_m[1] - predicted_before.position_m[1],
                predicted_after.position_m[2] - predicted_before.position_m[2],
            ];
            records.push(ReconcileRecord {
                snapshot_tick: self.tick,
                acked_seq: acked,
                acked_frame,
                event,
                displacement_m,
                server_window_sweeps: server_after
                    .window_sweeps
                    .saturating_sub(server_before.window_sweeps),
                server_window_rebuilds: server_after
                    .window_rebuilds
                    .saturating_sub(server_before.window_rebuilds),
                server_terrain_fallbacks: server_after
                    .terrain_fallbacks
                    .saturating_sub(server_before.terrain_fallbacks),
                client_window_sweeps: client_after
                    .window_sweeps
                    .saturating_sub(client_before.window_sweeps),
                client_window_rebuilds: client_after
                    .window_rebuilds
                    .saturating_sub(client_before.window_rebuilds),
                client_terrain_fallbacks: client_after
                    .terrain_fallbacks
                    .saturating_sub(client_before.terrain_fallbacks),
            });
        }
        self.last_client_window = client_after;
        self.last_server_window = server_after;
    }
}

/// The input-delivery script: a phase label, the input to hold during it,
/// and — per tick within the phase — whether this harness sends a fresh
/// frame (`true`) or withholds one, reusing the last (`false`). Modelled
/// on real, everyday network behaviour, not an adversarial worst case:
/// mostly-steady delivery with occasional single-tick gaps (ordinary
/// jitter), plus one deliberately longer held stretch (a several-tick
/// stall) well under `HELD_INPUT_TIMEOUT_TICKS` so the movement itself
/// never actually stops, only its acknowledgement lags.
fn delivery_schedule(input: PlayerInput, ticks: usize) -> Vec<(PlayerInput, bool)> {
    let mut out = Vec::with_capacity(ticks);
    for i in 0..ticks {
        // Every 7th tick: a single-tick gap (ordinary jitter).
        let jitter_gap = i % 7 == 6;
        // Ticks 40..=49 of this phase (if long enough): a longer held
        // stretch, ~10 ticks — well under the 15-tick timeout, modelling a
        // real several-frame delivery stall while the player keeps
        // physically holding the same key.
        let held_stretch = (40..50).contains(&i);
        let deliver = !jitter_gap && !held_stretch;
        out.push((input, deliver));
    }
    out
}

fn run_trace() -> Vec<ReconcileRecord> {
    let mut h = Harness::new();
    let mut records = Vec::new();

    let run_phase = |h: &mut Harness, schedule: Vec<(PlayerInput, bool)>, records: &mut Vec<ReconcileRecord>| {
        for (input, deliver) in schedule {
            h.step(input, deliver, records);
        }
    };

    run_phase(&mut h, delivery_schedule(approach(), APPROACH_TICKS), &mut records);
    for _ in 0..ROUND_TRIPS {
        run_phase(&mut h, delivery_schedule(hug_positive(), 150), &mut records);
        run_phase(&mut h, delivery_schedule(idle(), 20), &mut records);
        run_phase(&mut h, delivery_schedule(hug_negative(), 150), &mut records);
        run_phase(&mut h, delivery_schedule(idle(), 20), &mut records);
    }
    run_phase(&mut h, delivery_schedule(idle(), FINAL_IDLE_TICKS), &mut records);

    // Sanity check on the harness itself: the held-input path must have
    // actually engaged, or this trace isn't testing what it claims to.
    let max_held = h
        .sent_log
        .iter()
        .map(|s| s.held_for_ticks)
        .max()
        .unwrap_or(0);
    assert!(
        max_held >= 8 && max_held < HELD_INPUT_TIMEOUT_TICKS,
        "expected the scripted held stretch to reuse one input for close to (but under) the \
         {HELD_INPUT_TIMEOUT_TICKS}-tick timeout, got a max of {max_held} ticks — the delivery \
         schedule isn't exercising held-input reuse as intended"
    );

    records
}

fn report(records: &[ReconcileRecord]) {
    let notable: Vec<&ReconcileRecord> = records.iter().filter(|r| r.event.error_m > 0.01).collect();
    eprintln!(
        "--- realistic input timing: {} reconcile events (one per {SNAPSHOT_INTERVAL_TICKS}-tick \
         snapshot, not every tick), {} exceed 0.01 m ---",
        records.len(),
        notable.len()
    );
    for r in &notable {
        let held = r.acked_frame.map(|f| f.held_for_ticks).unwrap_or(0);
        let sent_at = r.acked_frame.map(|f| f.sent_at_tick).unwrap_or(0);
        let acked_movement = r.acked_frame.map(|f| f.input.movement).unwrap_or([0.0; 3]);
        eprintln!(
            "  snapshot_tick={:5} acked_seq={:<5} sent_at_tick={:5} held_for_ticks={held:<3} \
             acked_movement={acked_movement:?} error_m={:.4} vert_m={:.4} horiz_m={:.4} \
             idle={:<5} displacement=[{:.4},{:.4},{:.4}] \
             | server window sweeps={} rebuilds={} fallbacks={} | client window sweeps={} \
             rebuilds={} fallbacks={}",
            r.snapshot_tick,
            r.acked_seq.0,
            sent_at,
            r.event.error_m,
            r.event.vertical_m,
            r.event.horizontal_m,
            r.event.idle,
            r.displacement_m[0],
            r.displacement_m[1],
            r.displacement_m[2],
            r.server_window_sweeps,
            r.server_window_rebuilds,
            r.server_terrain_fallbacks,
            r.client_window_sweeps,
            r.client_window_rebuilds,
            r.client_terrain_fallbacks,
        );
    }
    let max_err = records
        .iter()
        .map(|r| r.event.error_m)
        .fold(0.0_f64, f64::max);
    eprintln!("max error across the trace: {max_err:.6} m");
}

#[test]
fn g1_realistic_input_timing_reproduces_or_clears_the_150mm_signature() {
    let records = run_trace();
    report(&records);

    // This is the actual open question, not a pre-decided pass/fail: does
    // realistic input timing alone (round 18's collider fix left
    // unchanged, confirmed stable) reproduce something close to the live
    // session's repeated ~0.150 m signature? Report either outcome
    // plainly rather than asserting a bound that presupposes the answer.
    let near_150mm = records
        .iter()
        .filter(|r| (r.event.horizontal_m - 0.150).abs() < 0.02)
        .count();
    eprintln!(
        "{near_150mm} of {} events land within 2 cm of the live session's ~0.150 m horizontal \
         signature",
        records.len()
    );
}

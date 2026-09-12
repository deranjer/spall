//! ENG-69 round 16: a detailed G1 tower-wall strafe trace through the *real*
//! prediction/reconciliation path (`spall_client::predict::PredictedPlayer` /
//! `ClientPhysics` against a real `spall_sim::Simulation`) — same harness
//! style as `g1_ramp_trace.rs`, applied to a different movement pattern.
//!
//! **Why this scenario, not the ramp:** a hands-on session's felt jitter was
//! reported while strafing *around the G1 tower's base*, not crossing the
//! ramp; `g1_ramp_trace.rs`'s own doc already disclaims covering "every
//! possible source", and a review correctly flagged that a head-on ramp
//! crossing doesn't by itself establish that lateral wall-hugging/corner-
//! rounding has the same cause. `spall_physics::character::tests::
//! strafing_the_g1_tower_wall_diverges_between_representations` already
//! confirmed (CPU-only, no reconciliation) that the two representations'
//! *trajectories* diverge by a comparable order of magnitude (0.0805 m)
//! during exactly this movement pattern. This file asks the next question:
//! does the *real* reconciliation loop, walking this same pattern, actually
//! reproduce the *live* signature — repeated corrections, not a single
//! endpoint diff?
//!
//! **Scope, stated plainly (same as `g1_ramp_trace.rs`):** this harness
//! shares the *same* `Simulation`-owned `Volume` object between "client" and
//! "server" physics — it does not stand up QUIC transport or exercise
//! `ReplicaWorld` reconstruction from wire baseline/patch traffic. The
//! *movement path* is the real production one though — and, as of round 18,
//! that path no longer sweeps player movement against the real terrain
//! collider's own representation at all on either side: `SimWorld::
//! advance_players` and `ClientPhysics::sweep` both use their own small
//! `CharacterQueryCache` window (always `NativeVoxels`, real terrain
//! excluded) by default. `g1_tower_strafe_trace_is_corrections_free_by_
//! default` asserts the consequence of that directly. What the real
//! whole-terrain collider itself builds (`MergedCuboids` for
//! `g1_full_envelope_scene`, confirmed by `spall_sim::collider::tests::
//! g1_full_envelope_scene_representation_choice`) no longer matters to
//! player movement either way — it's still what *other* physics (dynamic
//! bodies, non-character queries) uses, untouched by this round.
//!
//! Run with `cargo test -p spall_client --test g1_tower_strafe_trace --
//! --nocapture` to see the reports.

use spall_client::predict::{CELL_M, ClientPhysics, CorrectionEvent, PredictedPlayer};
use spall_core::{EntityId, PlayerInput, Tick, player_entity_for};
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::InputSeq;
use spall_sim::fixtures::g1_full_envelope_setup;
use spall_sim::{Simulation, SimulationConfig, TICK_DT_S};

/// 1 m west of the G1 tower's west wall (`G1_TOWER_X0` = cell 24 = 6.0 m),
/// mid-span on z (tower z 32..=47 -> 8.0..12.0 m — z=10.0 m sits 2 m clear
/// of either corner), standing on the flat plain (cell height 46 -> 11.5 m).
/// Same start point `spall_physics::character::tests::
/// strafing_the_g1_tower_wall_diverges_between_representations` uses.
const TOWER_APPROACH_M: [f64; 3] = [5.0, 11.5, 10.0];
const APPROACH_TICKS: usize = 30;
/// One strafe leg's length — long enough to round a corner and continue
/// along the next wall face, matching the live session's ~1.1 s continuous
/// episode (`WALK_SPEED_M_S` * a diagonal leg covers several metres over
/// this many ticks).
const HUG_LEG_TICKS: usize = 150;
const IDLE_PAUSE_TICKS: usize = 20;
const ROUND_TRIPS: usize = 3;
const FINAL_IDLE_TICKS: usize = 200;
/// `docs/validation.md`'s canonical impaired-link figure, in ticks at the
/// server's fixed `1/60 s` tick.
const RTT_100MS_ACK_DELAY: usize = 6;

fn idle() -> PlayerInput {
    PlayerInput::NEUTRAL
}

/// Straight toward the wall, no strafe — closes the initial 1 m gap and
/// settles flush against it.
fn approach() -> PlayerInput {
    PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    }
}

/// Forward (into the wall, keeping contact pressure) plus strafe — the
/// lateral component that slides along the face and around a corner. Same
/// input `strafing_the_g1_tower_wall_diverges_between_representations` uses.
fn hug_positive() -> PlayerInput {
    PlayerInput {
        movement: [1.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    }
}

/// The opposite strafe direction — reverses back along the wall/corner
/// already crossed, so a `ROUND_TRIPS` loop revisits the same seam-prone
/// region repeatedly rather than wandering further away each leg (the ramp
/// trace's `downhill`/`uphill` pair does the same thing for the same
/// reason).
fn hug_negative() -> PlayerInput {
    PlayerInput {
        movement: [-1.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
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
    phase_log: Vec<&'static str>,
    seq: u64,
    ack_delay: usize,
}

impl Harness {
    /// `force_server_native_voxels`: when `true`, rebuilds the server's
    /// terrain collider as `NativeVoxels` (matching the client's, which
    /// always builds `NativeVoxels` — round 8) via `SimWorld::
    /// force_volume_representation_for_test`, bypassing `plan_collider`'s
    /// budget entirely. **Not a production-viable fix on its own** — see
    /// that method's doc — this exists purely to test, through the real
    /// reconciliation path, whether matching representations eliminates the
    /// divergence this file's traces otherwise reproduce.
    fn new_with(ack_delay: usize, force_server_native_voxels: bool) -> Self {
        let mut sim = Simulation::new(SimulationConfig::new(g1_full_envelope_setup()))
            .expect("g1 world valid");
        let player = player_entity_for(0);
        sim.add_player(player, TOWER_APPROACH_M);
        if force_server_native_voxels {
            let terrain_id = sim.world().terrain_volume_id();
            sim.world_mut()
                .force_volume_representation_for_test(
                    terrain_id,
                    spall_physics::Representation::NativeVoxels,
                )
                .expect("forcing the server's terrain to NativeVoxels for this diagnostic");
        }

        let mut phys = ClientPhysics::new();
        phys.set_terrain(&sim.world().terrain().volume);
        let predictor = PredictedPlayer::new(
            CharacterParams::DEFAULT,
            CharacterState::at(TOWER_APPROACH_M),
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
            .tick(&mut self.phys, &volume, input, TICK_DT_S);
        if self.server_log.len() > self.ack_delay {
            let record_index = self.server_log.len() - 1 - self.ack_delay;
            let (auth, acked, server_tick) = self.server_log[record_index];
            if let Some(event) =
                self.predictor
                    .reconcile(&mut self.phys, &volume, auth, acked, server_tick)
            {
                samples.push(Sample {
                    tick: record_index,
                    phase: self.phase_log[record_index],
                    event,
                });
            }
        }
    }
}

/// Nearest-rank percentile (this crate's stated convention).
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

fn run_trace(ack_delay: usize) -> Vec<Sample> {
    run_trace_with(ack_delay, false)
}

fn run_trace_with(ack_delay: usize, force_server_native_voxels: bool) -> Vec<Sample> {
    let mut h = Harness::new_with(ack_delay, force_server_native_voxels);
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

    leg(&mut h, approach(), "approach", APPROACH_TICKS, &mut samples);
    for _ in 0..ROUND_TRIPS {
        leg(&mut h, hug_positive(), "hug+", HUG_LEG_TICKS, &mut samples);
        leg(
            &mut h,
            idle(),
            "idle-corner",
            IDLE_PAUSE_TICKS,
            &mut samples,
        );
        leg(&mut h, hug_negative(), "hug-", HUG_LEG_TICKS, &mut samples);
        leg(&mut h, idle(), "idle-wall", IDLE_PAUSE_TICKS, &mut samples);
    }
    leg(&mut h, idle(), "final-idle", FINAL_IDLE_TICKS, &mut samples);

    samples
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

    // How many corrections come suspiciously close to an exact multiple of
    // one movement tick's distance (`WALK_SPEED_M_S / 60 = 0.075 m`) — the
    // review's alternative input-acknowledgement-timing hypothesis predicts
    // *every* correction clusters there; the seam hypothesis predicts sizes
    // scattered across whatever each individual seam happens to produce.
    const TICK_DIST_M: f64 = 4.5 / 60.0;
    let near_tick_multiple = moving
        .iter()
        .filter(|s| {
            let ratio = s.event.horizontal_m / TICK_DIST_M;
            (ratio - ratio.round()).abs() < 0.05 && s.event.horizontal_m > 0.01
        })
        .count();
    eprintln!(
        "[{label}] {} of {} moving events land within 5% of an exact multiple of one tick's \
         distance ({TICK_DIST_M:.4} m)",
        near_tick_multiple,
        moving.len()
    );

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
fn g1_tower_strafe_trace_loopback() {
    let samples = run_trace(0);
    report("loopback (ack_delay=0)", &samples);
}

#[test]
fn g1_tower_strafe_trace_100ms_rtt() {
    let samples = run_trace(RTT_100MS_ACK_DELAY);
    report("100ms RTT (ack_delay=6)", &samples);
}

/// ENG-69 round 16 found that forcing the server's terrain representation to
/// match the client's (both `NativeVoxels`, via `SimWorld::
/// force_volume_representation_for_test`) eliminates the seam divergence
/// through the real reconciliation path. Round 18 then *integrated* the
/// underlying fix for real — `spall_sim::world::SimWorld::advance_players`
/// and `spall_client::predict::ClientPhysics::sweep` both now sweep each
/// character against its own small `NativeVoxels` window
/// (`spall_physics::query_cache::CharacterQueryCache`) instead of the real
/// terrain collider by default, unconditionally — so `force_server_native_
/// voxels` no longer has anything left to change: player movement never
/// touches the real terrain collider's representation at all any more,
/// only each character's own window. This test now asserts that directly:
/// `run_trace(0)` (the exact default path `g1_tower_strafe_trace_loopback`
/// above already exercises, unasserted) should show zero — not merely
/// small — corrections, because the representation mismatch this whole
/// file was built to chase no longer exists in the default configuration.
/// The `force_server_native_voxels` comparison is kept as a secondary
/// check that the flag is now inert (not silently broken), not the primary
/// claim.
#[test]
fn g1_tower_strafe_trace_is_corrections_free_by_default() {
    let default_path = run_trace(0);
    let forced_flag = run_trace_with(0, true);

    let default_notable = default_path
        .iter()
        .filter(|s| s.event.error_m > 0.01)
        .count();
    let default_max = default_path
        .iter()
        .map(|s| s.event.error_m)
        .fold(0.0_f64, f64::max);
    let forced_notable = forced_flag
        .iter()
        .filter(|s| s.event.error_m > 0.01)
        .count();
    let forced_max = forced_flag
        .iter()
        .map(|s| s.event.error_m)
        .fold(0.0_f64, f64::max);

    eprintln!(
        "default path (both sides use their own window cache): {default_notable} events > \
         0.01m, max {default_max:.6}m | force_server_native_voxels=true (now inert): \
         {forced_notable} events > 0.01m, max {forced_max:.6}m"
    );
    report("default (window-cache) path", &default_path);

    // The primary claim: the real, default, no-flags-needed path is
    // corrections-free — round 18's actual integration goal, not just the
    // round-16 diagnostic proxy for it.
    assert!(
        default_max < 1.0e-4,
        "the default path (both sides' own window cache, no manual force) should be \
         corrections-free — got a max error of {default_max:.6}m across {default_notable} \
         events > 0.01m; the window-cache integration in SimWorld::advance_players / \
         ClientPhysics::sweep may have regressed"
    );
    // The secondary claim: forcing the (now-bypassed) whole-terrain
    // representation changes nothing, confirming the flag didn't silently
    // start doing something unexpected instead of nothing.
    assert!(
        (forced_max - default_max).abs() < 1.0e-4,
        "force_server_native_voxels=true produced a different result ({forced_max:.6}m) than \
         the default path ({default_max:.6}m) — it should be fully inert now that player \
         movement never touches the real terrain collider's own representation"
    );
}

/// Sanity check on the fixed start point, independent of the rest of the
/// trace: confirms `TOWER_APPROACH_M` is actually clear of the tower and at
/// the flat-plain height this file's doc claims (`G1_FLAT_HEIGHT` = 46
/// cells), so a future fixture change is caught here rather than silently
/// producing a trace that no longer approaches the wall as intended.
#[test]
fn tower_approach_point_is_on_the_flat_plain() {
    let expected_y = 46.0 * f64::from(CELL_M);
    assert!(
        (TOWER_APPROACH_M[1] - expected_y).abs() < 1e-9,
        "TOWER_APPROACH_M's y ({}) should sit on the flat plain ({expected_y})",
        TOWER_APPROACH_M[1]
    );
}

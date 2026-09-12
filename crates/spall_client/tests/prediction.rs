//! T19 client prediction / reconciliation acceptance, end to end against the
//! real authoritative [`spall_sim::Simulation`] (no transport).
//!
//! Each test runs the server simulation and a [`spall_client::predict::PredictedPlayer`]
//! in lockstep, feeding the *same* scripted input to both and reconciling the
//! predictor against a delayed authoritative snapshot — the CPU stand-in for a
//! 100 ms link. It checks the T19 acceptance bullets: movement stays responsive,
//! corrections stay bounded, removing a floor during replay cannot leave the
//! player hovering, and a lost "button up" cannot leave it walking forever.

use spall_client::predict::{ClientPhysics, PredictedPlayer};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, PlayerInput, SphereBrush, player_entity_for};
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::{InputSeq, RequestId};
use spall_sim::fixtures::{WALK_ARENA_SPAWNS, walk_arena_setup};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, TICK_DT_S};

fn forward() -> PlayerInput {
    PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0], // +X
        buttons: 0,
    }
}

fn idle() -> PlayerInput {
    PlayerInput::NEUTRAL
}

fn brush_at_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

struct Harness {
    sim: Simulation,
    player: EntityId,
    phys: ClientPhysics,
    predictor: PredictedPlayer,
    /// `(authoritative state, acked input seq)` per elapsed tick.
    server_log: Vec<(CharacterState, InputSeq)>,
    seq: u64,
    ack_delay: usize,
}

impl Harness {
    fn new(ack_delay: usize) -> Self {
        let mut sim =
            Simulation::new(SimulationConfig::new(walk_arena_setup())).expect("arena valid");
        let player = player_entity_for(0);
        let spawn = WALK_ARENA_SPAWNS[0];
        sim.add_player(player, spawn);

        let mut phys = ClientPhysics::new();
        phys.set_terrain(&sim.world().terrain().volume);
        let predictor = PredictedPlayer::new(CharacterParams::DEFAULT, CharacterState::at(spawn));

        Self {
            sim,
            player,
            phys,
            predictor,
            server_log: Vec::new(),
            seq: 0,
            ack_delay,
        }
    }

    /// One tick: same input to server and predictor, then a delayed reconcile.
    fn step(&mut self, input: PlayerInput) {
        self.seq += 1;
        let seq = InputSeq(self.seq);

        self.sim.set_player_input(self.player, input, seq);
        self.sim.tick().unwrap();
        self.server_log.push((
            self.sim.player_state(self.player).unwrap(),
            self.sim.player_acked_input(self.player).unwrap(),
        ));

        let volume = self.sim.world().terrain().volume.clone();
        self.predictor
            .tick(&mut self.phys, &volume, input, seq, TICK_DT_S);

        if self.server_log.len() > self.ack_delay {
            let (auth, acked) = self.server_log[self.server_log.len() - 1 - self.ack_delay];
            self.predictor
                .reconcile(&mut self.phys, &volume, auth, acked);
        }
    }

    /// Rebuild the predictor's terrain collider from the server and invalidate
    /// the prediction history — the client's response to a nearby committed
    /// edit.
    fn resync_terrain_and_invalidate(&mut self) {
        self.phys.set_terrain(&self.sim.world().terrain().volume);
        self.predictor.invalidate();
    }

    fn server_state(&self) -> CharacterState {
        self.sim.player_state(self.player).unwrap()
    }

    /// Gap between the predictor's "now" and the *current* authoritative state —
    /// the real prediction quality (the predictor's `authoritative` field holds
    /// the delayed snapshot, so it always trails by the link delay).
    fn predicted_vs_server_m(&self) -> f64 {
        self.predictor.predicted().distance_m(&self.server_state())
    }
}

#[test]
fn prediction_converges_with_a_100ms_link() {
    // ~200 ms round trip: 12 ticks of ack delay.
    let mut h = Harness::new(12);
    for _ in 0..240 {
        h.step(forward());
    }

    let err = h.predicted_vs_server_m();
    assert!(
        err < 0.35,
        "predicted feet ended {err:.3} m from the live server state — not responsive"
    );
    assert!(
        h.predictor.max_correction_m < 0.5,
        "per-input replay corrections grew to {:.3} m under a 100 ms link",
        h.predictor.max_correction_m
    );
    assert!(
        h.predictor.distance_travelled_m() > 3.0,
        "predicted player barely moved: {:.2} m",
        h.predictor.distance_travelled_m()
    );
    assert!(
        h.predictor.ground_contact_ratio() > 0.9,
        "predicted player kept leaving the ground: contact ratio {:.2}",
        h.predictor.ground_contact_ratio()
    );
    assert!(!h.predictor.hovered_after_floor_removal);
}

#[test]
fn replaying_unacked_inputs_reproduces_the_server_state() {
    // Minimal delay: replay should track the server closely.
    let mut h = Harness::new(2);
    for _ in 0..180 {
        h.step(forward());
    }
    assert!(
        h.predicted_vs_server_m() < 0.15,
        "tight-loop prediction error {:.3} m",
        h.predicted_vs_server_m()
    );
}

#[test]
fn removing_the_floor_mid_run_does_not_leave_the_player_hovering() {
    let mut h = Harness::new(10);
    // Settle in place (do not walk past the hole we are about to cut).
    for _ in 0..25 {
        h.step(idle());
    }
    let before_y = h.server_state().position_m[1];

    // Server cuts the floor out from under the player at the spawn: feet ≈
    // (1.0, ·, 1.5) m -> cells (4, ·, 6); punch a wide sphere through the slab.
    h.sim
        .submit(EditIntent::cut(
            RequestId(1),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            brush_at_cell(4, 2, 6, 6),
        ))
        .unwrap();
    h.sim.run_until_idle(60).unwrap();
    // Client learns the terrain changed near it and rebases prediction.
    h.resync_terrain_and_invalidate();

    for _ in 0..80 {
        h.step(idle());
    }

    let server_end = h.server_state();
    let pred_end = h.predictor.predicted();
    assert!(
        !h.predictor.hovered_after_floor_removal,
        "predictor reported hovering after the floor was removed"
    );
    assert!(
        server_end.position_m[1] < before_y - 0.5,
        "authoritative player did not fall through the hole: {} -> {}",
        before_y,
        server_end.position_m[1]
    );
    assert!(
        (pred_end.position_m[1] - server_end.position_m[1]).abs() < 1.0,
        "predicted feet {:.2} did not follow authoritative feet {:.2} down",
        pred_end.position_m[1],
        server_end.position_m[1]
    );
    assert!(!pred_end.grounded, "predictor still thinks it is standing");
}

#[test]
fn idle_correction_breakdown_stays_consistent_with_the_combined_counters() {
    // ENG-69 round 7: the interactive HUD now splits `corrections` into an
    // idle subset plus a vertical/horizontal decomposition, to tell a
    // resting-contact disagreement (idle, vertical) apart from a
    // collision-sweep one incurred while moving (horizontal). This harness
    // shares the server's own volume directly (no wire/replica
    // reconstruction), so it is not expected to reproduce the live-session
    // divergence itself — this only guards the accounting: whatever fires,
    // the idle subset and either component must never exceed the combined
    // lifetime counters they were derived from.
    let mut h = Harness::new(3);
    for _ in 0..200 {
        h.step(idle());
    }
    assert!(
        h.predictor.idle_corrections <= h.predictor.corrections,
        "idle corrections {} exceeded total corrections {}",
        h.predictor.idle_corrections,
        h.predictor.corrections
    );
    assert!(
        h.predictor.max_idle_correction_m <= h.predictor.max_correction_m + 1e-9,
        "idle max {:.6} exceeded overall max {:.6}",
        h.predictor.max_idle_correction_m,
        h.predictor.max_correction_m
    );
    assert!(
        h.predictor.max_vertical_correction_m <= h.predictor.max_correction_m + 1e-9,
        "vertical max {:.6} exceeded combined max {:.6}",
        h.predictor.max_vertical_correction_m,
        h.predictor.max_correction_m
    );
    assert!(
        h.predictor.max_horizontal_correction_m <= h.predictor.max_correction_m + 1e-9,
        "horizontal max {:.6} exceeded combined max {:.6}",
        h.predictor.max_horizontal_correction_m,
        h.predictor.max_correction_m
    );
}

#[test]
fn a_lost_button_release_leaves_both_sides_at_rest() {
    let mut h = Harness::new(8);
    // 20 ticks of held-forward input, then total silence (every later datagram,
    // including the "released" frame, is lost). The predictor keeps ticking with
    // neutral input locally; the server reuses then times the held input out.
    for _ in 0..20 {
        h.step(forward());
    }
    for _ in 0..120 {
        // No new sequence: the server sees no fresh frame.
        h.sim.tick().unwrap();
        h.server_log.push((
            h.sim.player_state(h.player).unwrap(),
            h.sim.player_acked_input(h.player).unwrap(),
        ));
        let volume = h.sim.world().terrain().volume.clone();
        h.predictor
            .tick(&mut h.phys, &volume, idle(), InputSeq(h.seq), TICK_DT_S);
        let (auth, acked) = h.server_log[h.server_log.len() - 1 - h.ack_delay];
        h.predictor.reconcile(&mut h.phys, &volume, auth, acked);
    }

    let server_end = h.server_state();
    assert!(
        server_end.velocity_m_s[0].abs() < 0.2 && server_end.velocity_m_s[2].abs() < 0.2,
        "authoritative player kept walking after the held input timed out: v = {:?}",
        server_end.velocity_m_s
    );
    assert!(server_end.grounded);
    assert!(
        h.predictor.prediction_error_m() < 0.5,
        "predictor diverged from the rested server state: {:.3} m",
        h.predictor.prediction_error_m()
    );
}

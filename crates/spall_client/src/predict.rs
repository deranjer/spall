//! Client-side player prediction and reconciliation (T19).
//!
//! `docs/protocol.md`: "The local client keeps a bounded input/state history,
//! predicts capsule movement, and replays unacknowledged inputs after an
//! authoritative correction. Collision topology changes invalidate affected
//! history: restore the authoritative player state and rebuild prediction
//! against a known revision."
//!
//! [`PredictedPlayer`] runs the *same* [`spall_physics::step_character`] kernel
//! the server runs, against [`ClientPhysics`] — a physics world holding just the
//! terrain collider, rebuilt from the replica. Because physics is not lockstep,
//! the predicted state drifts from the authoritative one; [`PredictedPlayer::reconcile`]
//! snaps to each snapshot and replays the still-unacknowledged inputs, and the
//! residual is reported as a bounded correction.

use std::collections::VecDeque;

use serde::Serialize;
use spall_core::PlayerInput;
use spall_physics::{
    BodyId, BodyKind, BodySpec, CharacterMove, CharacterParams, CharacterState, OccupancyGrid,
    PhysicsConfig, PhysicsWorld, Representation, step_character,
};
use spall_protocol::InputSeq;
use spall_voxel::Volume;

/// Bounded predicted-input history depth.
pub const PREDICTION_HISTORY: usize = 128;
/// Terrain cell size, metres (the 0.25 m world grid).
pub const CELL_M: f32 = 0.25;

/// A physics world that mirrors only the replica's terrain collider, so the
/// predictor sweeps the capsule against the geometry the server used.
pub struct ClientPhysics {
    world: PhysicsWorld,
    terrain: Option<BodyId>,
}

impl Default for ClientPhysics {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientPhysics {
    pub fn new() -> Self {
        Self {
            world: PhysicsWorld::new(PhysicsConfig::default()),
            terrain: None,
        }
    }

    /// (Re)builds the terrain collider from `volume`. The caller gates this on a
    /// cheap dirty check (`ReplicaWorld::terrain_hash`). An empty terrain drops
    /// the collider entirely so the capsule falls through a fully-removed floor.
    pub fn set_terrain(&mut self, volume: &Volume) {
        match OccupancyGrid::from_volume(volume) {
            Ok(Some(grid)) => match self.terrain {
                Some(id) => {
                    self.world
                        .rebuild_collider(id, &grid, Representation::MergedCuboids);
                }
                None => {
                    let id = self.world.add_body(BodySpec {
                        kind: BodyKind::Fixed,
                        representation: Representation::MergedCuboids,
                        grid,
                        cell_m: CELL_M,
                        density_kg_m3: 1.0,
                        mass_properties: None,
                        translation_m: [0.0; 3],
                        linvel_m_s: [0.0; 3],
                    });
                    self.terrain = Some(id);
                }
            },
            _ => {
                if let Some(id) = self.terrain {
                    self.world.remove_collider(id);
                }
            }
        }
        // Refresh the broad-phase BVH so the next character sweep sees the new
        // collider (the sweep runs no physics step of its own).
        self.world.step();
    }

    /// Sweeps the capsule one tick against the terrain collider.
    pub fn sweep(
        &self,
        params: CharacterParams,
        feet_m: [f64; 3],
        desired_m: [f32; 3],
        dt_s: f32,
    ) -> CharacterMove {
        self.world.sweep_character(params, feet_m, desired_m, dt_s)
    }

    pub fn has_terrain(&self) -> bool {
        self.terrain.is_some()
    }
}

/// One predicted input, kept so it can be re-simulated after a correction.
#[derive(Debug, Clone, Copy)]
struct Record {
    seq: InputSeq,
    input: PlayerInput,
    dt: f32,
    predicted_after: CharacterState,
}

/// The local player's predicted state plus the history needed to reconcile it.
pub struct PredictedPlayer {
    pub params: CharacterParams,
    predicted: CharacterState,
    authoritative: CharacterState,
    acked: InputSeq,
    history: VecDeque<Record>,
    start_pos_m: [f64; 3],

    // --- metrics -------------------------------------------------------------
    /// Snapshots whose predicted-at-ack state differed from authoritative.
    pub corrections: u64,
    /// Largest such difference, metres.
    pub max_correction_m: f64,
    pub total_ticks: u64,
    pub grounded_ticks: u64,
    /// Set only if the predictor ever reported "grounded" while authority was
    /// clearly falling — the bug "removing a floor during replay leaves the
    /// player hovering". Correct code never sets it.
    pub hovered_after_floor_removal: bool,
}

impl PredictedPlayer {
    pub fn new(params: CharacterParams, spawn: CharacterState) -> Self {
        Self {
            params,
            predicted: spawn,
            authoritative: spawn,
            acked: InputSeq(0),
            history: VecDeque::new(),
            start_pos_m: spawn.position_m,
            corrections: 0,
            max_correction_m: 0.0,
            total_ticks: 0,
            grounded_ticks: 0,
            hovered_after_floor_removal: false,
        }
    }

    pub fn predicted(&self) -> CharacterState {
        self.predicted
    }

    pub fn authoritative(&self) -> CharacterState {
        self.authoritative
    }

    /// Advances the prediction one tick and records the input for replay.
    pub fn tick(
        &mut self,
        phys: &ClientPhysics,
        input: PlayerInput,
        seq: InputSeq,
        dt_s: f32,
    ) -> CharacterState {
        let params = self.params;
        self.predicted = step_character(self.predicted, input, dt_s, |p, d| {
            phys.sweep(params, p, d, dt_s)
        });
        self.history.push_back(Record {
            seq,
            input,
            dt: dt_s,
            predicted_after: self.predicted,
        });
        while self.history.len() > PREDICTION_HISTORY {
            self.history.pop_front();
        }
        self.total_ticks += 1;
        if self.predicted.grounded {
            self.grounded_ticks += 1;
        }
        self.predicted
    }

    /// Reconciles against an authoritative snapshot. `acked` is the last input
    /// sequence the server consumed for this player: everything up to and
    /// including it is dropped, and the rest is replayed from the authoritative
    /// state to produce the new predicted "now".
    pub fn reconcile(
        &mut self,
        phys: &ClientPhysics,
        authoritative: CharacterState,
        acked: InputSeq,
    ) {
        if let Some(rec) = self.history.iter().find(|r| r.seq == acked) {
            let err = rec.predicted_after.distance_m(&authoritative);
            if err > 1.0e-4 {
                self.corrections += 1;
            }
            self.max_correction_m = self.max_correction_m.max(err);
        }
        if !authoritative.grounded
            && authoritative.velocity_m_s[1] < -1.0
            && self.predicted.grounded
            && self.predicted.position_m[1] > authoritative.position_m[1] + 0.3
        {
            self.hovered_after_floor_removal = true;
        }

        self.authoritative = authoritative;
        self.acked = acked;
        self.history.retain(|r| r.seq.0 > acked.0);

        let params = self.params;
        let mut state = authoritative;
        for rec in self.history.iter_mut() {
            let dt = rec.dt;
            state = step_character(state, rec.input, dt, |p, d| phys.sweep(params, p, d, dt));
            rec.predicted_after = state;
        }
        self.predicted = state;
    }

    /// A committed edit invalidated the geometry the predicted history walked
    /// through: drop it and rebase on the last authoritative state rather than
    /// replaying movement through geometry that no longer exists.
    pub fn invalidate(&mut self) {
        self.history.clear();
        self.predicted = self.authoritative;
    }

    /// Fraction of predicted ticks spent grounded.
    pub fn ground_contact_ratio(&self) -> f64 {
        if self.total_ticks == 0 {
            0.0
        } else {
            self.grounded_ticks as f64 / self.total_ticks as f64
        }
    }

    /// Horizontal distance the predicted feet have travelled from spawn, metres.
    pub fn distance_travelled_m(&self) -> f64 {
        let dx = self.predicted.position_m[0] - self.start_pos_m[0];
        let dz = self.predicted.position_m[2] - self.start_pos_m[2];
        (dx * dx + dz * dz).sqrt()
    }

    /// Current gap between the predicted and authoritative feet, metres.
    pub fn prediction_error_m(&self) -> f64 {
        self.predicted.distance_m(&self.authoritative)
    }

    /// A machine-readable summary of one scripted run.
    pub fn summary(&self, held_button_release_ok: bool) -> PlayerMovementSummary {
        PlayerMovementSummary {
            ticks: self.total_ticks,
            distance_travelled_m: self.distance_travelled_m(),
            final_prediction_error_m: self.prediction_error_m(),
            corrections: self.corrections,
            max_correction_m: self.max_correction_m,
            ground_contact_ratio: self.ground_contact_ratio(),
            hovered_after_floor_removal: self.hovered_after_floor_removal,
            held_button_release_ok,
            final_predicted_pos_m: self.predicted.position_m,
            final_authoritative_pos_m: self.authoritative.position_m,
        }
    }
}

/// Reported per scripted player in [`crate::ClientSummary`].
#[derive(Debug, Clone, Serialize)]
pub struct PlayerMovementSummary {
    pub ticks: u64,
    pub distance_travelled_m: f64,
    pub final_prediction_error_m: f64,
    pub corrections: u64,
    pub max_correction_m: f64,
    pub ground_contact_ratio: f64,
    pub hovered_after_floor_removal: bool,
    pub held_button_release_ok: bool,
    pub final_predicted_pos_m: [f64; 3],
    pub final_authoritative_pos_m: [f64; 3],
}

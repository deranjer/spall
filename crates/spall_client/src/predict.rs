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

use std::collections::{BTreeSet, VecDeque};

use serde::Serialize;
use spall_core::{BrickCoord, GlobalCell, MaterialId, PlayerInput};
use spall_physics::{
    BodyId, BodyKind, BodySpec, CharacterMove, CharacterParams, CharacterState, OccupancyGrid,
    PhysicsConfig, PhysicsWorld, choose_representation, step_character,
};
use spall_protocol::InputSeq;
use spall_voxel::{Sample, Volume};

/// Bounded predicted-input history depth.
pub const PREDICTION_HISTORY: usize = 128;
/// Terrain cell size, metres (the 0.25 m world grid).
pub const CELL_M: f32 = 0.25;
/// Cells per brick edge (`docs/architecture.md`'s fixed brick size).
const BRICK_CELLS: i64 = 32;

/// A physics world that mirrors only the replica's terrain collider, so the
/// predictor sweeps the capsule against the geometry the server used.
pub struct ClientPhysics {
    world: PhysicsWorld,
    terrain: Option<BodyId>,
    /// Bricks the collider currently installed on `terrain` was actually built
    /// from, as of the last [`set_terrain`](Self::set_terrain) — empty
    /// whenever nothing is resident yet. Residency streams bricks
    /// independently of each other, so this set is routinely non-contiguous
    /// for a step or two while the player walks (one trailing brick evicted,
    /// one leading brick still mid-reload); [`covers`](Self::covers) is what
    /// tells a caller whether *the player's own* brick is actually one of
    /// them, as opposed to merely "some geometry exists somewhere".
    resident_bricks: BTreeSet<BrickCoord>,
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
            resident_bricks: BTreeSet::new(),
        }
    }

    /// (Re)builds the terrain collider from `volume`. The caller gates this on a
    /// cheap dirty check (`ReplicaWorld::terrain_hash`).
    ///
    /// Unlike [`OccupancyGrid::from_volume`], this tolerates a *non-contiguous*
    /// resident set: it still takes the tight bounding box of every resident
    /// solid cell, but a cell whose brick is not (yet) resident is treated as
    /// empty — no collision — rather than failing the whole extraction. Under
    /// eviction/reload churn the resident set routinely has a gap for a step
    /// or two (one trailing brick evicted, one leading brick still mid-reload
    /// after a lossy repair round trip); failing the entire collider over that
    /// gap would blank out geometry that *is* loaded and correct, well away
    /// from the gap. An empty resident set (no solid cell anywhere) still
    /// drops the collider entirely so the capsule falls through a
    /// fully-removed floor.
    ///
    /// The representation is chosen by [`spall_physics::choose_representation`]
    /// — the same budget `spall_sim::collider::plan_collider` uses for the
    /// server's own terrain body — rather than a hardcoded
    /// `Representation::MergedCuboids`. ENG-69 round 7 found this hardcoding
    /// was a real bug, not a style choice: once the server's terrain grew
    /// fragmented enough to cross the budget and fall back to
    /// `NativeVoxels`, the client kept building `MergedCuboids` regardless —
    /// two structurally different colliders over the same logical geometry.
    /// `MergedCuboids`' internal box seams can deflect a sliding kinematic
    /// character sideways where `NativeVoxels` (parry suppresses
    /// internal-edge contacts between adjacent voxels) would not, which
    /// showed up as a small, purely-horizontal correction that fired even
    /// while the player stood perfectly still.
    pub fn set_terrain(&mut self, volume: &Volume) {
        self.resident_bricks = volume.resident_brick_coords().into_iter().collect();
        match lenient_occupancy(volume) {
            Some(grid) => {
                let representation = choose_representation(&grid);
                match self.terrain {
                    Some(id) => {
                        self.world.rebuild_collider(id, &grid, representation);
                    }
                    None => {
                        let id = self.world.add_body(BodySpec {
                            kind: BodyKind::Fixed,
                            representation,
                            grid,
                            cell_m: CELL_M,
                            density_kg_m3: 1.0,
                            mass_properties: None,
                            translation_m: [0.0; 3],
                            linvel_m_s: [0.0; 3],
                        });
                        self.terrain = Some(id);
                    }
                }
            }
            None => {
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

    /// Whether the brick under `feet_m` was actually resident (and so part of
    /// the collider) as of the last [`set_terrain`]. `has_terrain` alone
    /// cannot tell "the player's own footing is loaded" from "some other,
    /// unrelated patch of the world is loaded" — the collider body persists
    /// across a residency gap so a later refill can rebuild it, and stays
    /// "present" throughout. A caller predicting movement should hold rather
    /// than extrapolate through a point this returns `false` for: it means
    /// the client does not yet know whether that ground is solid, not that it
    /// has confirmed open air.
    pub fn covers(&self, feet_m: [f64; 3]) -> bool {
        let cell_m = f64::from(CELL_M);
        let cell = GlobalCell::new(
            (feet_m[0] / cell_m).floor() as i64,
            (feet_m[1] / cell_m).floor() as i64,
            (feet_m[2] / cell_m).floor() as i64,
        );
        self.resident_bricks.contains(&cell.split().0)
    }
}

/// See [`ClientPhysics::set_terrain`]. `None` when no resident brick holds a
/// solid cell at all (nothing to collide with yet, or a genuinely empty
/// world); `Some` grid otherwise, built over the tight bounding box of every
/// resident solid cell with non-resident cells inside that box left empty.
fn lenient_occupancy(volume: &Volume) -> Option<OccupancyGrid> {
    let coords = volume.resident_brick_coords();
    let mut min = [i64::MAX; 3];
    let mut max = [i64::MIN; 3];
    for c in &coords {
        let base = [c.x * BRICK_CELLS, c.y * BRICK_CELLS, c.z * BRICK_CELLS];
        for lz in 0..BRICK_CELLS {
            for ly in 0..BRICK_CELLS {
                for lx in 0..BRICK_CELLS {
                    let cell = GlobalCell::new(base[0] + lx, base[1] + ly, base[2] + lz);
                    if let Ok(Sample::Filled(_)) = volume.sample(cell) {
                        min[0] = min[0].min(cell.x);
                        min[1] = min[1].min(cell.y);
                        min[2] = min[2].min(cell.z);
                        max[0] = max[0].max(cell.x);
                        max[1] = max[1].max(cell.y);
                        max[2] = max[2].max(cell.z);
                    }
                }
            }
        }
    }
    if min[0] > max[0] {
        return None;
    }
    let dims = [
        (max[0] - min[0] + 1) as u32,
        (max[1] - min[1] + 1) as u32,
        (max[2] - min[2] + 1) as u32,
    ];
    let cells = dims[0] as usize * dims[1] as usize * dims[2] as usize;
    let mut solid = vec![false; cells];
    let mut material = vec![MaterialId::AIR; cells];
    for gz in 0..dims[2] {
        for gy in 0..dims[1] {
            for gx in 0..dims[0] {
                let cell =
                    GlobalCell::new(min[0] + gx as i64, min[1] + gy as i64, min[2] + gz as i64);
                if let Ok(Sample::Filled(m)) = volume.sample(cell) {
                    let idx = (gx + dims[0] * (gy + dims[1] * gz)) as usize;
                    solid[idx] = true;
                    material[idx] = m;
                }
                // `Sample::Empty` and `Sample::Unknown` both leave this cell
                // as air: real air collides with nothing, and an unresident
                // cell must not be treated as solid either — but nor may it
                // fail the whole box the way `OccupancyGrid::from_region`
                // rightly does for callers who need a *complete* region (a
                // body's own geometry, a checkpoint capture). `covers` is
                // this module's answer to "was this cell actually resident",
                // kept separately from the grid itself.
            }
        }
    }
    OccupancyGrid::from_solid_mask(
        GlobalCell::new(min[0], min[1], min[2]),
        dims,
        solid,
        material,
    )
    .ok()
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
    max_distance_from_start_m: f64,

    // --- metrics -------------------------------------------------------------
    /// Snapshots whose predicted-at-ack state differed from authoritative.
    pub corrections: u64,
    /// Largest such difference, metres.
    pub max_correction_m: f64,
    /// Of `corrections`, how many happened on a record whose input was
    /// perfectly neutral (no movement, no buttons) — isolates a resting-contact
    /// disagreement (both sides idle, nothing to sweep) from a collision-sweep
    /// difference incurred while actually walking, which the plain lifetime
    /// counters above cannot tell apart (see ENG-69's round-6/7 investigation
    /// into corrections that keep firing "even while standing still").
    pub idle_corrections: u64,
    /// Largest correction magnitude ever seen on an idle record.
    pub max_idle_correction_m: f64,
    /// Largest vertical (Y) component of `err` ever seen across all
    /// corrections — large relative to `max_correction_m` points at the two
    /// sides resting at different heights (a terrain/ground-snap disagreement);
    /// small relative to it (with `max_horizontal_correction_m` large instead)
    /// points at a swept-move/collision-response disagreement instead.
    pub max_vertical_correction_m: f64,
    /// Largest horizontal (XZ) component of `err` ever seen across all
    /// corrections. See `max_vertical_correction_m`.
    pub max_horizontal_correction_m: f64,
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
            max_distance_from_start_m: 0.0,
            corrections: 0,
            max_correction_m: 0.0,
            idle_corrections: 0,
            max_idle_correction_m: 0.0,
            max_vertical_correction_m: 0.0,
            max_horizontal_correction_m: 0.0,
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
        self.max_distance_from_start_m = self
            .max_distance_from_start_m
            .max(self.distance_travelled_m());
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
                // A record only ever holds the *sanitized* input actually fed to
                // `step_character` (see `tick`), so this is exactly the input
                // that produced `predicted_after` — comparing it to `NEUTRAL`
                // tells whether the two sides had anything to sweep at all.
                if rec.input.movement == [0.0, 0.0, 0.0] && rec.input.buttons == 0 {
                    self.idle_corrections += 1;
                    self.max_idle_correction_m = self.max_idle_correction_m.max(err);
                }
            }
            self.max_correction_m = self.max_correction_m.max(err);
            let dy = (rec.predicted_after.position_m[1] - authoritative.position_m[1]).abs();
            let dx = rec.predicted_after.position_m[0] - authoritative.position_m[0];
            let dz = rec.predicted_after.position_m[2] - authoritative.position_m[2];
            self.max_vertical_correction_m = self.max_vertical_correction_m.max(dy);
            self.max_horizontal_correction_m = self
                .max_horizontal_correction_m
                .max((dx * dx + dz * dz).sqrt());
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
            max_distance_from_start_m: self.max_distance_from_start_m,
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
    /// Farthest horizontal displacement reached at any predicted tick. Paired
    /// with the final displacement to prove an outbound-and-return traversal.
    pub max_distance_from_start_m: f64,
    pub final_prediction_error_m: f64,
    pub corrections: u64,
    pub max_correction_m: f64,
    pub ground_contact_ratio: f64,
    pub hovered_after_floor_removal: bool,
    pub held_button_release_ok: bool,
    pub final_predicted_pos_m: [f64; 3],
    pub final_authoritative_pos_m: [f64; 3],
}

//! Deterministic capsule character kernel (T19).
//!
//! `docs/architecture.md`: "Use a capsule character controller with grounded
//! state, gravity, jump, slope/step constraints, swept movement, and
//! server-authoritative interaction with dynamic bodies."
//!
//! [`step_character`] is the **pure** movement function: given the previous
//! [`CharacterState`], one tick of [`PlayerInput`], the timestep, and a
//! collision *sweep* closure, it returns the next state. The server runs it
//! against its authoritative [`crate::PhysicsWorld`]
//! ([`crate::PhysicsWorld::sweep_character`]); the client runs the *same*
//! function against a physics world rebuilt from its replica. Physics is not
//! lockstep (`docs/protocol.md`), so this buys bounded agreement — the client
//! predictor reconciles the residual against authoritative snapshots.
//!
//! The swept collision resolution (wall slide, autostep over stairs, slope
//! limit, ground snap) is Rapier's `KinematicCharacterController`, wrapped so
//! Rapier types never leave [`crate::PhysicsWorld`].

use rapier3d::control::{CharacterAutostep, CharacterLength, KinematicCharacterController};
use rapier3d::prelude::*;

pub use spall_core::PlayerInput;

/// Frozen T19 movement model. These are gameplay constants, not tunables a
/// scenario may vary — the server and client must integrate identically.
pub mod tuning {
    /// Ground move speed, m/s.
    pub const WALK_SPEED_M_S: f32 = 4.5;
    /// Upward speed imparted by a jump, m/s (≈ 1.1 m apex under `GRAVITY_M_S2`).
    pub const JUMP_SPEED_M_S: f32 = 4.7;
    /// Downward acceleration applied to the capsule, m/s². Matches the default
    /// [`crate::PhysicsConfig`] gravity magnitude.
    pub const GRAVITY_M_S2: f32 = 9.81;
    /// Tallest obstacle the capsule steps onto without jumping, m (2 × 0.25 m
    /// terrain cells).
    pub const MAX_STEP_M: f32 = 0.5;
    /// Widest free space required on top of a step before autostep commits, m.
    pub const MIN_STEP_WIDTH_M: f32 = 0.2;
    /// Steepest floor the capsule can walk up, radians (~50°).
    pub const MAX_SLOPE_CLIMB_RAD: f32 = 0.87;
    /// Floor angle past which the capsule slides back down, radians (~40°).
    pub const MIN_SLOPE_SLIDE_RAD: f32 = 0.70;
    /// Gap kept between the capsule and the world for solver stability, m.
    pub const SKIN_OFFSET_M: f32 = 0.02;
    /// Distance below the feet still treated as "on the ground" for snapping, m.
    pub const GROUND_SNAP_M: f32 = 0.20;
    /// Terminal fall speed clamp, m/s.
    pub const MAX_FALL_SPEED_M_S: f32 = 55.0;
}

/// Capsule dimensions for one player, metres. The capsule is upright (its axis
/// is world `+Y`); [`CharacterState::position_m`] is the **feet** point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterParams {
    /// Half the height of the capsule's cylindrical segment (excludes the two
    /// hemispherical caps).
    pub half_height_m: f32,
    /// Capsule radius.
    pub radius_m: f32,
}

impl CharacterParams {
    /// A ~1.8 m tall humanoid: 0.3 m radius, 0.6 m half-segment
    /// (`2 * (0.6 + 0.3) = 1.8`).
    pub const DEFAULT: Self = Self {
        half_height_m: 0.6,
        radius_m: 0.3,
    };

    /// Total standing height, feet to crown, metres.
    pub fn total_height_m(&self) -> f32 {
        2.0 * (self.half_height_m + self.radius_m)
    }

    /// Body-centre height above the feet, metres.
    pub fn centre_offset_m(&self) -> f32 {
        self.half_height_m + self.radius_m
    }
}

impl Default for CharacterParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Authoritative kinematic state of one capsule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterState {
    /// World position of the capsule's feet, `f64` metres (authority positions
    /// are `f64`).
    pub position_m: [f64; 3],
    /// Velocity, m/s. Horizontal components are the intended walk velocity;
    /// the vertical component integrates gravity / jump and is zeroed on
    /// ground or ceiling contact.
    pub velocity_m_s: [f32; 3],
    /// Whether the capsule is resting on a walkable surface after the last step.
    pub grounded: bool,
    /// Jump-button level on the previous step, so a held button jumps once
    /// (rising edge) rather than every grounded tick.
    pub jump_held_last: bool,
}

impl CharacterState {
    /// A state at rest at `position_m` (feet), not yet grounded.
    pub fn at(position_m: [f64; 3]) -> Self {
        Self {
            position_m,
            velocity_m_s: [0.0; 3],
            grounded: false,
            jump_held_last: false,
        }
    }

    /// Distance between two states' feet positions, metres.
    pub fn distance_m(&self, other: &CharacterState) -> f64 {
        let d = [
            self.position_m[0] - other.position_m[0],
            self.position_m[1] - other.position_m[1],
            self.position_m[2] - other.position_m[2],
        ];
        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
    }

    /// `true` if every field is finite.
    pub fn is_finite(&self) -> bool {
        self.position_m.iter().all(|v| v.is_finite())
            && self.velocity_m_s.iter().all(|v| v.is_finite())
    }
}

/// The outcome of one swept capsule move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterMove {
    /// The translation actually applied to the feet after collision, metres.
    pub translation_m: [f32; 3],
    /// Whether the capsule is touching walkable ground afterwards.
    pub grounded: bool,
}

/// Advances one capsule by a fixed timestep.
///
/// `sweep(feet_position_m, desired_translation_m) -> CharacterMove` performs the
/// collision resolution against whatever world the caller owns. The kernel only
/// does the deterministic input → wish-velocity → gravity/jump integration and
/// the post-move velocity feedback.
pub fn step_character(
    mut state: CharacterState,
    input: PlayerInput,
    dt_s: f32,
    mut sweep: impl FnMut([f64; 3], [f32; 3]) -> CharacterMove,
) -> CharacterState {
    let input = input.sanitized();
    let dt = dt_s.max(0.0);

    // --- horizontal wish velocity, in the world frame -----------------------
    let (sin_y, cos_y) = input.yaw().sin_cos();
    let forward = input.movement[2];
    let strafe = input.movement[0];
    // yaw is atan2(view.x, -view.z): flat look dir = (sin, 0, -cos),
    // right = (cos, 0, sin).
    let mut wish = [
        forward * sin_y + strafe * cos_y,
        forward * -cos_y + strafe * sin_y,
    ];
    let wish_len = (wish[0] * wish[0] + wish[1] * wish[1]).sqrt();
    if wish_len > 1.0 {
        wish[0] /= wish_len;
        wish[1] /= wish_len;
    }
    let horiz = [
        wish[0] * tuning::WALK_SPEED_M_S,
        wish[1] * tuning::WALK_SPEED_M_S,
    ];

    // --- vertical velocity: jump (rising edge, grounded) then gravity -------
    let jump_now = input.wants_jump();
    let mut vy = state.velocity_m_s[1];
    if jump_now && !state.jump_held_last && state.grounded {
        vy = tuning::JUMP_SPEED_M_S;
    }
    vy -= tuning::GRAVITY_M_S2 * dt;
    vy = vy.clamp(-tuning::MAX_FALL_SPEED_M_S, tuning::MAX_FALL_SPEED_M_S);

    // --- swept move -------------------------------------------------------
    let desired = [horiz[0] * dt, vy * dt, horiz[1] * dt];
    let mv = sweep(state.position_m, desired);
    let applied = mv.translation_m;

    let new_pos = [
        state.position_m[0] + f64::from(applied[0]),
        state.position_m[1] + f64::from(applied[1]),
        state.position_m[2] + f64::from(applied[2]),
    ];

    // Vertical velocity feedback: zero it when the capsule landed / is standing
    // (downward velocity absorbed by the floor) or when a rising capsule's
    // vertical progress was blocked by a ceiling.
    let landed = mv.grounded && vy <= 0.0;
    let bonked_head = desired[1] > 0.0 && applied[1] < desired[1] - 1e-4;
    let new_vy = if landed || bonked_head { 0.0 } else { vy };

    state.position_m = new_pos;
    state.velocity_m_s = [horiz[0], new_vy, horiz[1]];
    state.grounded = mv.grounded;
    state.jump_held_last = jump_now;
    state
}

/// Builds the Rapier controller carrying the frozen T19 tuning.
pub(crate) fn controller() -> KinematicCharacterController {
    KinematicCharacterController {
        up: Vector::Y,
        offset: CharacterLength::Absolute(tuning::SKIN_OFFSET_M),
        slide: true,
        autostep: Some(CharacterAutostep {
            max_height: CharacterLength::Absolute(tuning::MAX_STEP_M),
            min_width: CharacterLength::Absolute(tuning::MIN_STEP_WIDTH_M),
            include_dynamic_bodies: false,
        }),
        max_slope_climb_angle: tuning::MAX_SLOPE_CLIMB_RAD,
        min_slope_slide_angle: tuning::MIN_SLOPE_SLIDE_RAD,
        snap_to_ground: Some(CharacterLength::Absolute(tuning::GROUND_SNAP_M)),
        normal_nudge_factor: 1.0e-4,
    }
}

/// The upright capsule shape for `params`, centred on the body centre.
pub(crate) fn capsule(params: CharacterParams) -> Capsule {
    Capsule::new_y(
        params.half_height_m.max(1.0e-3),
        params.radius_m.max(1.0e-3),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::occupancy::OccupancyGrid;
    use crate::world::{BodyId, BodyKind, BodySpec, PhysicsConfig, PhysicsWorld};
    use crate::{Representation, fixtures};
    use spall_core::{CellSizeCode, GlobalCell, VolumeId};
    use spall_voxel::{EditPlan, Volume, fixtures as vox};

    const DT: f32 = 1.0 / 60.0;
    const CELL_M: f32 = fixtures::CELL_M;

    fn add_fixed(world: &mut PhysicsWorld, volume: &Volume) -> BodyId {
        add_fixed_rep(world, volume, Representation::MergedCuboids)
    }

    fn add_fixed_rep(world: &mut PhysicsWorld, volume: &Volume, rep: Representation) -> BodyId {
        let grid = OccupancyGrid::from_volume(volume)
            .expect("extract")
            .expect("non-empty");
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: rep,
            grid,
            cell_m: CELL_M,
            density_kg_m3: 1.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        })
    }

    /// A world with a 2-brick-square, 4-cell-thick floor; returns the world, the
    /// floor's top surface height in metres, and the floor body id.
    fn floor_world() -> (PhysicsWorld, f64, BodyId) {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let floor = fixtures::floor_slab(VolumeId::new(1).unwrap(), 2, 2, 4);
        let id = add_fixed(&mut world, &floor);
        world.step();
        (world, 4.0 * f64::from(CELL_M), id)
    }

    /// Runs the capsule kernel against `world` for `ticks` steps.
    fn run(
        world: &mut PhysicsWorld,
        mut state: CharacterState,
        input: PlayerInput,
        ticks: usize,
    ) -> CharacterState {
        let params = CharacterParams::DEFAULT;
        for _ in 0..ticks {
            state = step_character(state, input, DT, |pos, desired| {
                world.sweep_character(params, pos, desired, DT)
            });
        }
        state
    }

    /// Feet spawn at the centre of the 16 m × 16 m floor.
    fn spawn(top: f64) -> CharacterState {
        CharacterState::at([8.0, top, 8.0])
    }

    #[test]
    fn walks_forward_on_flat_floor() {
        let (mut world, top, _floor) = floor_world();
        let start = spawn(top);
        let input = PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [0.0, 0.0, -1.0],
            buttons: 0,
        };
        let end = run(&mut world, start, input, 60);
        // ~1 s of walking at WALK_SPEED, along -Z (forward with this view).
        let dz = start.position_m[2] - end.position_m[2];
        assert!(
            dz > f64::from(tuning::WALK_SPEED_M_S) * 0.7,
            "expected ~{} m of forward travel, got {dz}",
            tuning::WALK_SPEED_M_S
        );
        assert!(end.grounded, "should stay grounded while walking");
        assert!(
            (end.position_m[1] - top).abs() < 0.05,
            "feet drifted off the floor: {} vs {top}",
            end.position_m[1]
        );
    }

    #[test]
    fn diagonal_input_is_not_faster() {
        let (mut world, top, _floor) = floor_world();
        let start = spawn(top);
        let straight = run(
            &mut world,
            start,
            PlayerInput {
                movement: [0.0, 0.0, 1.0],
                view_dir: [0.0, 0.0, -1.0],
                buttons: 0,
            },
            60,
        );
        let (mut world2, _, _) = floor_world();
        let diag = run(
            &mut world2,
            start,
            PlayerInput {
                movement: [1.0, 0.0, 1.0],
                view_dir: [0.0, 0.0, -1.0],
                buttons: 0,
            },
            60,
        );
        let straight_d = start.distance_m(&straight);
        let diag_d = start.distance_m(&diag);
        assert!(
            (straight_d - diag_d).abs() < 0.3,
            "diagonal travel {diag_d} should match straight travel {straight_d}"
        );
    }

    #[test]
    fn a_tall_wall_blocks_forward_travel_without_penetration() {
        let (mut world, top, _floor) = floor_world();
        // Wall: solid stone x in [48..49] (12.0–12.5 m), 16 cells tall, full z.
        let wall_id = VolumeId::new(2).unwrap();
        let mut wall = Volume::new(wall_id, CellSizeCode::Quarter);
        wall.apply_edit(&EditPlan::filled_box(
            wall_id,
            GlobalCell::new(48, 0, 0),
            GlobalCell::new(49, 15, 63),
            vox::STONE,
        ))
        .unwrap();
        add_fixed(&mut world, &wall);
        world.step();

        let start = spawn(top);
        let input = PlayerInput {
            // walk toward +X, into the wall at x = 12 m.
            movement: [0.0, 0.0, 1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        };
        let end = run(&mut world, start, input, 120);
        let wall_face_m = 48.0 * f64::from(CELL_M);
        assert!(
            end.position_m[0] < wall_face_m - f64::from(CharacterParams::DEFAULT.radius_m) + 0.05,
            "capsule penetrated the wall: x={} face={wall_face_m}",
            end.position_m[0]
        );
        assert!(
            end.position_m[0] > start.position_m[0] + 2.0,
            "capsule should still have advanced up to the wall, x={}",
            end.position_m[0]
        );
    }

    #[test]
    fn autostep_climbs_a_low_step() {
        let (mut world, top, _floor) = floor_world();
        // A 2-cell (0.5 m) ledge sitting on the floor top: cells y in [4..5]
        // (1.0–1.5 m), covering x in [40..63] (10–16 m), full z. It does not
        // overlap the floor body (floor is y in [0..3]).
        let step_id = VolumeId::new(2).unwrap();
        let mut step = Volume::new(step_id, CellSizeCode::Quarter);
        step.apply_edit(&EditPlan::filled_box(
            step_id,
            GlobalCell::new(40, 4, 0),
            GlobalCell::new(63, 5, 63),
            vox::STONE,
        ))
        .unwrap();
        add_fixed(&mut world, &step);
        world.step();

        let start = spawn(top);
        let input = PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        };
        // Walk to the ledge face (x = 10 m, ~0.44 s) and step up; stop before
        // the far edge at x = 16 m.
        let end = run(&mut world, start, input, 110);
        let ledge_top = 6.0 * f64::from(CELL_M);
        assert!(
            end.position_m[0] > 40.0 * f64::from(CELL_M) - 0.5,
            "capsule never reached the ledge, x={}",
            end.position_m[0]
        );
        assert!(
            (end.position_m[1] - ledge_top).abs() < 0.2,
            "capsule did not step up onto the 0.5 m ledge: y={} expected {ledge_top}",
            end.position_m[1]
        );
        assert!(end.grounded, "capsule should be grounded on the ledge");
    }

    #[test]
    fn gravity_pulls_an_airborne_capsule_down_and_it_comes_to_rest() {
        let (mut world, top, _floor) = floor_world();
        let start = CharacterState::at([8.0, top + 2.0, 8.0]);
        let mid = run(&mut world, start, PlayerInput::NEUTRAL, 25);
        assert!(
            mid.position_m[1] < start.position_m[1] - 0.2,
            "capsule did not fall: {}",
            mid.position_m[1]
        );
        let end = run(&mut world, mid, PlayerInput::NEUTRAL, 150);
        assert!(end.grounded, "capsule never landed");
        assert!(
            (end.position_m[1] - top).abs() < 0.05,
            "resting feet height {} should equal the floor top {top}",
            end.position_m[1]
        );
        assert!(
            end.velocity_m_s[1].abs() < 0.5,
            "resting vertical velocity should be ~0, got {}",
            end.velocity_m_s[1]
        );
    }

    #[test]
    fn resting_xz_agrees_between_merged_cuboids_and_native_voxels() {
        // ENG-69 round 8: after eliminating the client's own MergedCuboids
        // seam artifact (client always builds NativeVoxels now), a live
        // session against a server terrain body built as MergedCuboids still
        // showed a small (~0.15 m), stable, purely-horizontal, idle-reproducible
        // prediction/authoritative disagreement. This isolates that specific
        // remaining variable in a pure CPU test, no networking involved: the
        // *same* flat floor, built once as each representation, with an
        // identical idle capsule run against each. If Rapier's contact
        // resolution for a Voxels shape and a Cuboid shape disagree even on
        // perfectly flat, gap-free, identical geometry, resting XZ drifts
        // between the two runs — confirming the representation *type* itself
        // (not terrain fragmentation/seams) is the remaining source, which
        // would mean client and server must use the *same* representation
        // for genuinely matching prediction, not just each build a
        // individually-reasonable one.
        let floor = fixtures::floor_slab(VolumeId::new(1).unwrap(), 2, 2, 4);
        let top = 4.0 * f64::from(CELL_M);
        let start = CharacterState::at([8.0, top + 2.0, 8.0]);

        let mut cuboid_world = PhysicsWorld::new(PhysicsConfig::default());
        add_fixed_rep(&mut cuboid_world, &floor, Representation::MergedCuboids);
        cuboid_world.step();
        let cuboid_end = run(&mut cuboid_world, start, PlayerInput::NEUTRAL, 300);

        let mut voxel_world = PhysicsWorld::new(PhysicsConfig::default());
        add_fixed_rep(&mut voxel_world, &floor, Representation::NativeVoxels);
        voxel_world.step();
        let voxel_end = run(&mut voxel_world, start, PlayerInput::NEUTRAL, 300);

        assert!(
            cuboid_end.grounded && voxel_end.grounded,
            "both should land"
        );
        let dx = cuboid_end.position_m[0] - voxel_end.position_m[0];
        let dz = cuboid_end.position_m[2] - voxel_end.position_m[2];
        let horiz_gap = (dx * dx + dz * dz).sqrt();
        assert!(
            horiz_gap < 0.01,
            "resting XZ disagrees between representations on identical flat \
             geometry: cuboid {:?} vs voxels {:?} (horizontal gap {horiz_gap:.4} m) \
             — the same shape drifts sideways differently depending on which \
             Rapier collider type resolves its rest contact, so client and \
             server predicting from different representations of the same \
             terrain can never fully agree even with zero seams on either side",
            cuboid_end.position_m,
            voxel_end.position_m
        );
    }

    #[test]
    fn walking_the_g1_ramp_diverges_between_representations() {
        // ENG-69 round 9: the flat-floor test above proved Voxels and Cuboid
        // shapes agree on identical geometry *without* internal seams. Does a
        // real feature with genuine internal MergedCuboids seams — not an
        // artificial partial-view crop — actually produce a resolvable
        // disagreement? Use the exact fixture behind the live
        // `cargo xtask play --scene g1` session the correction was measured
        // on: `g1_full_envelope_scene` merges to only ~39 total boxes for its
        // ~3M solid cells (comfortably inside the budget, confirmed by
        // `spall_sim::collider`'s own `g1_full_envelope_scene_representation_choice`
        // test — this is why the server picks `MergedCuboids` for it), but
        // its one deliberate ramp feature (`g1_full_envelope_scene`'s own
        // doc: 12 cells of height dropped over 32 cells of x, cell y in
        // `[180, 211]`, cell z in `[100, 115]`) is a staircase of several
        // tread boxes meeting at right-angle seams — exactly the kind of
        // internal seam the flat floor above has none of.
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());

        let mut cuboid_world = PhysicsWorld::new(PhysicsConfig::default());
        add_fixed_rep(&mut cuboid_world, &volume, Representation::MergedCuboids);
        cuboid_world.step();

        let mut voxel_world = PhysicsWorld::new(PhysicsConfig::default());
        add_fixed_rep(&mut voxel_world, &volume, Representation::NativeVoxels);
        voxel_world.step();

        // Just before the ramp, on the flat plain (cell height 46), walking
        // +X across the whole ramp and stopping just short of its bottom
        // (cell x = 211) so the run never reaches whatever geometry lies
        // beyond it.
        let start = CharacterState::at([
            179.0 * f64::from(CELL_M),
            46.0 * f64::from(CELL_M),
            106.0 * f64::from(CELL_M),
        ]);
        let input = PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        };
        let cuboid_end = run(&mut cuboid_world, start, input, 100);
        let voxel_end = run(&mut voxel_world, start, input, 100);

        let dx = cuboid_end.position_m[0] - voxel_end.position_m[0];
        let dz = cuboid_end.position_m[2] - voxel_end.position_m[2];
        let dy = cuboid_end.position_m[1] - voxel_end.position_m[1];
        let horiz_gap = (dx * dx + dz * dz).sqrt();
        eprintln!(
            "g1 ramp walk: cuboid {:?} (grounded {}) vs voxels {:?} (grounded {}) \
             -> horiz gap {horiz_gap:.4} m, vert gap {:.4} m",
            cuboid_end.position_m,
            cuboid_end.grounded,
            voxel_end.position_m,
            voxel_end.grounded,
            dy
        );
        // Deliberately not asserting a bound here (unlike the flat-floor
        // test): this test's purpose is the eprintln! above (run with
        // `-- --nocapture`) — measuring whether real ramp/staircase seams
        // move XZ at all, to settle whether ENG-69's round-9 theory (the
        // server's own MergedCuboids seams, not any client-side issue, are
        // the residual's source) holds up against the actual scene, not just
        // an idealized flat floor.
    }

    #[test]
    fn strafing_the_g1_tower_wall_diverges_between_representations() {
        // ENG-69 round 15: the ramp test above confirms the seam mechanism
        // is real on a staircase feature crossed head-on; it does not by
        // itself establish that a hands-on session's felt jitter — reported
        // while strafing *around the G1 tower's base* specifically, not
        // crossing the ramp — has the same cause. A reviewer correctly
        // flagged this gap (`g1_ramp_trace.rs`'s own doc already disclaims
        // covering "every possible source"), and separately noted the
        // measured ~0.150 m horizontal correction equals exactly two
        // movement ticks (`WALK_SPEED_M_S * 2 / 60`) — worth keeping open as
        // an alternative, input-acknowledgement-timing explanation rather
        // than assuming this test alone settles it. Same methodology as the
        // ramp test, applied to a *lateral wall-slide* instead of a
        // head-on ramp crossing: approach flush against the tower's west
        // face (`G1_TOWER_X0` = cell 24 = 6.0 m) then strafe along it while
        // still pressing in — hugging a corner, exactly what rounding an
        // obstacle while strafing does — and diff the resulting trajectory
        // between representations built from the identical complete
        // geometry (no partial-occupancy confound — see this file's own
        // flat-floor test above for why that variable has to be held
        // fixed to isolate representation type).
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());

        let mut cuboid_world = PhysicsWorld::new(PhysicsConfig::default());
        add_fixed_rep(&mut cuboid_world, &volume, Representation::MergedCuboids);
        cuboid_world.step();

        let mut voxel_world = PhysicsWorld::new(PhysicsConfig::default());
        add_fixed_rep(&mut voxel_world, &volume, Representation::NativeVoxels);
        voxel_world.step();

        // 1 m west of the tower's west wall (cell x=24 -> 6.0 m), mid-span
        // on z (tower z 32..=47 -> 8.0..12.0 m; z=10.0 m sits 2 m clear of
        // either corner at the start, so a ~2.5 s strafe reaches and rounds
        // one of them regardless of which way "strafe right" resolves),
        // standing on the flat plain (cell height 46 -> 11.5 m).
        let start = CharacterState::at([5.0, 46.0 * f64::from(CELL_M), 10.0]);
        let approach = PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        };
        // Forward (into the wall, keeping contact pressure) plus strafe
        // (the lateral component that actually slides along the face and
        // around the corner) — `diagonal_input_is_not_faster` above already
        // confirms this doesn't move any faster than pure-forward, just at
        // an angle.
        let hug = PlayerInput {
            movement: [1.0, 0.0, 1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        };
        let cuboid_mid = run(&mut cuboid_world, start, approach, 30);
        let voxel_mid = run(&mut voxel_world, start, approach, 30);

        // Per-tick trace of the "hug" phase specifically: does the gap
        // between representations open in discrete jumps concentrated at
        // specific ticks (consistent with crossing individual MergedCuboids
        // box seams, one per contact with a new box), or drift smoothly and
        // continuously throughout (which would instead point at ordinary
        // per-tick contact-resolution differences between the two shape
        // types, unrelated to any specific seam)? Distinguishes the seam
        // theory from an alternative the review raised: an input-
        // acknowledgement/replay-timing bug, which would not correlate with
        // geometric contact events at all.
        let params = CharacterParams::DEFAULT;
        let mut cuboid_state = cuboid_mid;
        let mut voxel_state = voxel_mid;
        let mut prev_gap = 0.0_f64;
        for tick in 0..150 {
            // `sweep_character_with` instead of `sweep_character`: surfaces
            // the `CharacterCollision`s Rapier's controller reports (each
            // hit's collider, contact normal, time of impact) instead of
            // discarding them — directly confirms a divergence-producing
            // tick is a real contact against real geometry, not a
            // coincidental integration difference with no contact at all.
            let mut cuboid_hits = Vec::new();
            cuboid_state = step_character(cuboid_state, hug, DT, |pos, desired| {
                cuboid_world.sweep_character_with(params, pos, desired, DT, |c| {
                    cuboid_hits.push(*c);
                })
            });
            let mut voxel_hits = Vec::new();
            voxel_state = step_character(voxel_state, hug, DT, |pos, desired| {
                voxel_world.sweep_character_with(params, pos, desired, DT, |c| {
                    voxel_hits.push(*c);
                })
            });
            let dx = cuboid_state.position_m[0] - voxel_state.position_m[0];
            let dz = cuboid_state.position_m[2] - voxel_state.position_m[2];
            let gap = (dx * dx + dz * dz).sqrt();
            let step = gap - prev_gap;
            if step.abs() > 0.002 {
                eprintln!(
                    "  tick {tick:3}: horiz gap {gap:.4} m (+{step:.4} this tick) | \
                     cuboid x={:.3} z={:.3} grounded={} hits={} | voxels x={:.3} z={:.3} \
                     grounded={} hits={}",
                    cuboid_state.position_m[0],
                    cuboid_state.position_m[2],
                    cuboid_state.grounded,
                    cuboid_hits.len(),
                    voxel_state.position_m[0],
                    voxel_state.position_m[2],
                    voxel_state.grounded,
                    voxel_hits.len(),
                );
                for hit in &cuboid_hits {
                    eprintln!(
                        "    cuboid hit: normal1={:?} toi={:.4} remaining={:?}",
                        hit.hit.normal1, hit.hit.time_of_impact, hit.translation_remaining
                    );
                }
                for hit in &voxel_hits {
                    eprintln!(
                        "    voxel  hit: normal1={:?} toi={:.4} remaining={:?}",
                        hit.hit.normal1, hit.hit.time_of_impact, hit.translation_remaining
                    );
                }
            }
            prev_gap = gap;
        }

        let dx = cuboid_state.position_m[0] - voxel_state.position_m[0];
        let dz = cuboid_state.position_m[2] - voxel_state.position_m[2];
        let dy = cuboid_state.position_m[1] - voxel_state.position_m[1];
        let horiz_gap = (dx * dx + dz * dz).sqrt();
        eprintln!(
            "g1 tower-wall strafe: cuboid mid={:?} end={:?} (grounded {}) vs \
             voxels mid={:?} end={:?} (grounded {}) -> horiz gap {horiz_gap:.4} m, \
             vert gap {:.4} m",
            cuboid_mid.position_m,
            cuboid_state.position_m,
            cuboid_state.grounded,
            voxel_mid.position_m,
            voxel_state.position_m,
            voxel_state.grounded,
            dy
        );
        // Deliberately not asserting a bound here, same rationale as the
        // ramp test: this is a measurement to settle whether the seam
        // mechanism extends to lateral wall-hugging, not an acceptance
        // gate.
    }

    #[test]
    fn tick_131_isolates_the_slope_handling_divergence() {
        // ENG-69 round 16: the wall-strafe test above found the whole
        // 0.0805 m gap opens in exactly one tick (131) with a real contact
        // on both sides — but review correctly pushed back that "ground
        // snap" (the operation nearest the end of `move_shape`) cannot by
        // itself explain a *horizontal* displacement, so which controller
        // operation actually produces it needs to be established, not
        // inferred from proximity to a floor-ish contact normal. Isolates
        // that one tick: reproduces the state immediately before it (from
        // the `MergedCuboids` trajectory only — the two runs are ~3 mm apart
        // by then, see the previous test's per-tick trace), then re-runs
        // *that exact same* starting state and input against a fresh
        // instance of each representation for exactly one more tick, so any
        // difference in the result can only come from this one tick's own
        // contact resolution, never an accumulated prior difference.
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        let params = CharacterParams::DEFAULT;

        let start = CharacterState::at([5.0, 46.0 * f64::from(CELL_M), 10.0]);
        let approach = PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        };
        let hug = PlayerInput {
            movement: [1.0, 0.0, 1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        };

        // The state immediately before tick 131: 30 approach ticks + 130
        // hug ticks, against the `MergedCuboids` representation only.
        let mut reference_world = PhysicsWorld::new(PhysicsConfig::default());
        add_fixed_rep(&mut reference_world, &volume, Representation::MergedCuboids);
        reference_world.step();
        let pre_approach = run(&mut reference_world, start, approach, 30);
        let pre_state = run(&mut reference_world, pre_approach, hug, 130);

        // The isolated experiment: from that identical state, one tick
        // against each representation, in fresh worlds (so neither carries
        // any broad-phase state from `reference_world`'s own run).
        let mut cuboid_world = PhysicsWorld::new(PhysicsConfig::default());
        add_fixed_rep(&mut cuboid_world, &volume, Representation::MergedCuboids);
        cuboid_world.step();
        let mut voxel_world = PhysicsWorld::new(PhysicsConfig::default());
        add_fixed_rep(&mut voxel_world, &volume, Representation::NativeVoxels);
        voxel_world.step();

        let mut cuboid_hits = Vec::new();
        let mut cuboid_desired = [0.0f32; 3];
        let cuboid_end = step_character(pre_state, hug, DT, |pos, desired| {
            cuboid_desired = desired;
            cuboid_world.sweep_character_with(params, pos, desired, DT, |c| {
                cuboid_hits.push(*c);
            })
        });
        let mut voxel_hits = Vec::new();
        let mut voxel_desired = [0.0f32; 3];
        let voxel_end = step_character(pre_state, hug, DT, |pos, desired| {
            voxel_desired = desired;
            voxel_world.sweep_character_with(params, pos, desired, DT, |c| {
                voxel_hits.push(*c);
            })
        });

        let cuboid_actual = [
            (cuboid_end.position_m[0] - pre_state.position_m[0]) as f32,
            (cuboid_end.position_m[1] - pre_state.position_m[1]) as f32,
            (cuboid_end.position_m[2] - pre_state.position_m[2]) as f32,
        ];
        let voxel_actual = [
            (voxel_end.position_m[0] - pre_state.position_m[0]) as f32,
            (voxel_end.position_m[1] - pre_state.position_m[1]) as f32,
            (voxel_end.position_m[2] - pre_state.position_m[2]) as f32,
        ];
        let dx = cuboid_end.position_m[0] - voxel_end.position_m[0];
        let dz = cuboid_end.position_m[2] - voxel_end.position_m[2];
        let horiz_gap_this_tick = (dx * dx + dz * dz).sqrt();

        eprintln!(
            "tick 131 isolated: pre_state pos={:?} grounded={}",
            pre_state.position_m, pre_state.grounded
        );
        eprintln!(
            "  requested translation (desired, both identical): cuboid={cuboid_desired:?} \
             voxel={voxel_desired:?}"
        );
        eprintln!(
            "  cuboid: {} hit(s), actual translation={cuboid_actual:?}, end grounded={}",
            cuboid_hits.len(),
            cuboid_end.grounded
        );
        for hit in &cuboid_hits {
            eprintln!(
                "    hit: normal1={:?} toi={:.5} remaining={:?}",
                hit.hit.normal1, hit.hit.time_of_impact, hit.translation_remaining
            );
        }
        eprintln!(
            "  voxel : {} hit(s), actual translation={voxel_actual:?}, end grounded={}",
            voxel_hits.len(),
            voxel_end.grounded
        );
        for hit in &voxel_hits {
            eprintln!(
                "    hit: normal1={:?} toi={:.5} remaining={:?}",
                hit.hit.normal1, hit.hit.time_of_impact, hit.translation_remaining
            );
        }
        eprintln!(
            "  -> from the *identical* pre-state and input, one tick alone produces a \
             {horiz_gap_this_tick:.4} m horizontal gap"
        );

        // The mechanism, not just the symptom: `KinematicCharacterController
        // ::decompose_hit` (Rapier's own slope-handling step, called from
        // `handle_slopes` — not `handle_stairs`/autostep, which only runs
        // for a wall-steep contact, and not `snap_to_ground`, which runs
        // after and only adjusts the up-axis) derives a
        // "horizontal tangent direction" as `hit.normal1.cross(up)`, then
        // resolves the remaining wish vector against it. For a *perfectly*
        // vertical normal — which only `MergedCuboids`' exactly-axis-aligned
        // box-top faces can produce — that cross product is the zero vector
        // (mathematically degenerate: a perfectly flat, perfectly
        // horizontal surface has no privileged "along the surface"
        // direction), and Rapier's own fallback (`try_normalize().
        // unwrap_or_default()`) dumps the entire tangential remainder into
        // the *vertical* tangent component instead of splitting it
        // horizontally. `NativeVoxels`' contact normals are essentially
        // never exactly vertical (this run's tilt: ~3-6 degrees off), so the
        // cross product is well-defined there, and the wish vector's
        // horizontal component is retained and applied along a real
        // direction. Same code path, same branch (`is_wall`/`is_nonslip_
        // slope` should classify identically for two contacts this close to
        // vertical — verified by the two runs each reporting exactly one
        // hit, not two), genuinely different result: this is why "ground
        // snap alone" doesn't explain it, and why "the contact normal
        // happened to be near-vertical" isn't itself the culprit — it's
        // specifically the *exactness* of a `MergedCuboids` box-top normal
        // that hits this degenerate case.
        for (label, hits) in [("cuboid", &cuboid_hits), ("voxel", &voxel_hits)] {
            for hit in hits {
                let up = Vector::new(0.0, 1.0, 0.0);
                let cross_len = hit.hit.normal1.cross(up).length();
                eprintln!(
                    "  {label} hit: |normal x up| = {cross_len:.6} (near 0 = degenerate \
                     horizontal-tangent-direction case)"
                );
            }
        }
    }

    #[test]
    fn removing_the_floor_leaves_no_hover() {
        let (mut world, top, floor) = floor_world();
        // Settle on the floor.
        let grounded = run(&mut world, spawn(top), PlayerInput::NEUTRAL, 30);
        assert!(grounded.grounded);

        // Remove the floor collider (its authoritative volume became air).
        world.remove_collider(floor);
        world.step();

        let after = run(&mut world, grounded, PlayerInput::NEUTRAL, 30);
        assert!(
            !after.grounded,
            "capsule still reports grounded over removed floor"
        );
        assert!(
            after.position_m[1] < grounded.position_m[1] - 0.3,
            "capsule hovered instead of falling: {} -> {}",
            grounded.position_m[1],
            after.position_m[1]
        );
    }
}

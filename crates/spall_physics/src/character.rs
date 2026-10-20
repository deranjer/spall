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
    /// Ground move speed while the sprint button is held, m/s (1.73× walk).
    pub const RUN_SPEED_M_S: f32 = 7.8;
    /// Upward speed imparted by a jump, m/s (≈ 1.0 m apex under `GRAVITY_M_S2`).
    pub const JUMP_SPEED_M_S: f32 = 5.7;
    /// Downward acceleration applied to the capsule, m/s². Characters use a
    /// deliberately tighter arc than free rigid bodies: the nearly unchanged
    /// apex remains useful, while takeoff-to-landing time falls from about
    /// `0.96 s` to `0.71 s` and no longer feels floaty.
    pub const GRAVITY_M_S2: f32 = 16.0;
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

    // ---- swimming ---------------------------------------------------------
    /// Water this deep over the feet (m) and the capsule swims instead of
    /// walking: roughly chest to neck deep.
    pub const SWIM_ENTER_DEPTH_M: f32 = 1.1;
    /// How deep the feet hang when treading water, m: the head and shoulders
    /// stay out. Deeper than [`SWIM_ENTER_DEPTH_M`], so treading is stable.
    pub const FLOAT_DEPTH_M: f32 = 1.25;
    /// Swim speed along the look direction, m/s.
    pub const SWIM_SPEED_M_S: f32 = 3.2;
    /// Upward speed while the jump button is held under water, m/s.
    pub const SWIM_UP_M_S: f32 = 3.0;
    /// Upward speed of a jump out of the water from the surface, m/s: enough
    /// for the feet to clear about 0.5 m above the surface, which gets a swimmer
    /// onto a bank or a raft deck.
    pub const WATER_EXIT_JUMP_M_S: f32 = 7.5;
    /// How quickly swim velocity follows its target, 1/s.
    pub const SWIM_RESPONSE_PER_S: f32 = 8.0;
    /// Strength of the pull back to the treading depth, m/s per m.
    pub const FLOAT_PULL_PER_S: f32 = 2.5;
    /// Fastest the pull to the treading depth moves the capsule, m/s.
    pub const FLOAT_PULL_MAX_M_S: f32 = 1.5;
    /// Wading slows walking by up to this fraction as the water deepens.
    pub const WADE_SLOWDOWN: f32 = 0.3;
    /// Fraction of the current's speed that shoves a wader standing on the bed,
    /// at knee depth or deeper.
    pub const WADE_PUSH: f32 = 0.6;
    /// Fastest current the kernel will apply, m/s per axis.
    pub const MAX_CURRENT_M_S: f32 = 8.0;
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
    /// Effective player mass used when an authoritative character sweep
    /// transfers momentum to a dynamic body. The capsule remains kinematic;
    /// this value only controls equal-and-opposite contact impulse magnitude.
    pub mass_kg: f32,
}

impl CharacterParams {
    /// A ~1.8 m tall humanoid: 0.3 m radius, 0.6 m half-segment
    /// (`2 * (0.6 + 0.3) = 1.8`).
    pub const DEFAULT: Self = Self {
        half_height_m: 0.6,
        radius_m: 0.3,
        mass_kg: 80.0,
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
    state: CharacterState,
    input: PlayerInput,
    dt_s: f32,
    sweep: impl FnMut([f64; 3], [f32; 3]) -> CharacterMove,
) -> CharacterState {
    step_character_in_water(state, input, dt_s, None, sweep)
}

/// The water around a character: where its surface is and how fast it runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaterSample {
    /// Height (metres) of the water surface in the column the capsule stands in.
    pub surface_y: f64,
    /// Horizontal velocity `[x, z]` of that water, m/s.
    pub flow_m_s: [f32; 2],
}

impl WaterSample {
    /// Standing water at `surface_y`.
    pub fn still(surface_y: f64) -> Self {
        Self {
            surface_y,
            flow_m_s: [0.0; 2],
        }
    }
}

/// How far the water stands over a character whose feet are at `feet_y`;
/// negative above the surface, `-inf` where there is no (finite) water.
fn water_depth_m(water: Option<WaterSample>, feet_y: f64) -> f32 {
    water
        .map(|w| (w.surface_y - feet_y) as f32)
        .filter(|d| d.is_finite())
        .unwrap_or(f32::NEG_INFINITY)
}

/// `true` if a character with its feet at `feet_y` swims in `water`: the same
/// test [`step_character_in_water`] uses to switch from walking to swimming.
pub fn is_swimming(water: Option<WaterSample>, feet_y: f64) -> bool {
    water_depth_m(water, feet_y) >= tuning::SWIM_ENTER_DEPTH_M
}

/// [`step_character`] with water: `water` is the surface and current in the
/// column the capsule stands in, or `None` where there is no water. Server and
/// client must pass the same value for the same position. A current carries a
/// swimmer (who swims relative to the water) and shoves a wader. Water deeper than [`tuning::SWIM_ENTER_DEPTH_M`] over the feet makes the capsule swim
/// (look-direction movement, treading at [`tuning::FLOAT_DEPTH_M`], jump to rise
/// and to leap out of the water); shallower water only slows walking.
pub fn step_character_in_water(
    mut state: CharacterState,
    input: PlayerInput,
    dt_s: f32,
    water: Option<WaterSample>,
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
    // How far the water stands over the feet; negative above it. Not finite
    // water (a bad query) counts as dry.
    let depth_m = water_depth_m(water, state.position_m[1]);
    let flow = water.map_or([0.0; 2], |w| {
        w.flow_m_s.map(|v| {
            if v.is_finite() {
                v.clamp(-tuning::MAX_CURRENT_M_S, tuning::MAX_CURRENT_M_S)
            } else {
                0.0
            }
        })
    });
    if depth_m >= tuning::SWIM_ENTER_DEPTH_M {
        return swim_step(state, input, dt, depth_m, wish, flow, &mut sweep);
    }
    // Wading: deeper water slows the walk, smoothly, so crossing a shoreline
    // has no step in speed.
    let wade = if depth_m > 0.0 {
        let t = (depth_m / tuning::SWIM_ENTER_DEPTH_M).clamp(0.0, 1.0);
        1.0 - tuning::WADE_SLOWDOWN * t * t * (3.0 - 2.0 * t)
    } else {
        1.0
    };
    // A current shoves a wader in proportion to how much of them it reaches.
    let push = if depth_m > 0.0 {
        let t = (depth_m / tuning::SWIM_ENTER_DEPTH_M).clamp(0.0, 1.0);
        tuning::WADE_PUSH * t * t * (3.0 - 2.0 * t)
    } else {
        0.0
    };
    // Sprinting only changes the ground speed; the kernel is the one place the
    // server and the client predictor both read it.
    let ground_speed = if input.wants_sprint() {
        tuning::RUN_SPEED_M_S
    } else {
        tuning::WALK_SPEED_M_S
    };
    let wish_horiz = [
        wish[0] * ground_speed * wade + flow[0] * push,
        wish[1] * ground_speed * wade + flow[1] * push,
    ];

    // Ground input chooses horizontal velocity. Once airborne, preserve the
    // takeoff momentum until contact is regained: changing/letting go of WASD
    // must not let a player steer or brake in mid-air. A jump begins from a
    // grounded state, so the input on the takeoff tick still determines the
    // launch direction.
    let horiz = if state.grounded {
        wish_horiz
    } else {
        [state.velocity_m_s[0], state.velocity_m_s[2]]
    };

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

/// One tick of swimming. `wish` is the horizontal wish direction in the world
/// frame (unit length at most); the look direction adds the vertical part.
fn swim_step(
    mut state: CharacterState,
    input: PlayerInput,
    dt: f32,
    depth_m: f32,
    wish: [f32; 2],
    flow: [f32; 2],
    sweep: &mut impl FnMut([f64; 3], [f32; 3]) -> CharacterMove,
) -> CharacterState {
    let jump_now = input.wants_jump();
    let rising_edge = jump_now && !state.jump_held_last;
    let mut v = state.velocity_m_s;

    // A leap already under way (from a jump out of the water) stays ballistic
    // until the feet leave the swim depth: the swim response must not cancel it.
    let leaping = v[1] > tuning::SWIM_UP_M_S + 0.5;
    if leaping {
        v[1] -= tuning::GRAVITY_M_S2 * dt;
    } else {
        let moving = wish[0] != 0.0 || wish[1] != 0.0;
        // Where the feet should be going vertically.
        let float_v = ((tuning::FLOAT_DEPTH_M - depth_m) * -tuning::FLOAT_PULL_PER_S)
            .clamp(-tuning::FLOAT_PULL_MAX_M_S, tuning::FLOAT_PULL_MAX_M_S);
        // Look direction decides how a swimmer dives or climbs; only the part
        // that goes down, or up while still below the treading depth, is kept.
        let look_y = input.view_dir[1].clamp(-1.0, 1.0);
        let fwd = input.movement[2];
        let mut target_y = float_v;
        if moving && fwd != 0.0 {
            let swim_y = look_y * fwd * tuning::SWIM_SPEED_M_S;
            if swim_y < 0.0 {
                // Looking down while swimming forward dives.
                target_y = swim_y;
            } else if depth_m > tuning::FLOAT_DEPTH_M {
                // Looking up from below climbs, but only as far as the treading
                // depth: the swimmer does not lift out of the water.
                target_y = swim_y.max(float_v);
            }
        }
        if jump_now {
            if rising_edge && depth_m <= tuning::FLOAT_DEPTH_M + 0.3 {
                // Leap out of the water.
                target_y = tuning::WATER_EXIT_JUMP_M_S;
            } else {
                target_y = target_y.max(tuning::SWIM_UP_M_S);
            }
        }
        // The swimmer swims relative to the water, which itself runs.
        let target = [
            wish[0] * tuning::SWIM_SPEED_M_S + flow[0],
            target_y,
            wish[1] * tuning::SWIM_SPEED_M_S + flow[1],
        ];
        if target_y >= tuning::WATER_EXIT_JUMP_M_S - 1e-3 {
            // The leap takes its speed at once and keeps the horizontal speed.
            v[1] = target_y;
            v[0] += (target[0] - v[0]) * 0.5;
            v[2] += (target[2] - v[2]) * 0.5;
        } else {
            let k = 1.0 - (-tuning::SWIM_RESPONSE_PER_S * dt).exp();
            for i in 0..3 {
                v[i] += (target[i] - v[i]) * k;
            }
        }
    }
    v[1] = v[1].clamp(-tuning::MAX_FALL_SPEED_M_S, tuning::MAX_FALL_SPEED_M_S);

    let desired = [v[0] * dt, v[1] * dt, v[2] * dt];
    let mv = sweep(state.position_m, desired);
    let applied = mv.translation_m;
    state.position_m = [
        state.position_m[0] + f64::from(applied[0]),
        state.position_m[1] + f64::from(applied[1]),
        state.position_m[2] + f64::from(applied[2]),
    ];
    // Resting on the bed, or bumping the ceiling, stops the vertical motion.
    let landed = mv.grounded && v[1] <= 0.0;
    let bonked_head = desired[1] > 0.0 && applied[1] < desired[1] - 1e-4;
    if landed || bonked_head {
        v[1] = 0.0;
    }
    state.velocity_m_s = v;
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
    use spall_core::{BUTTON_JUMP, BUTTON_SPRINT, CellSizeCode, GlobalCell, VolumeId};
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

    fn forward_input(buttons: u32) -> PlayerInput {
        PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [0.0, 0.0, -1.0],
            buttons,
        }
    }

    fn free_move(_: [f64; 3], desired: [f32; 3]) -> CharacterMove {
        CharacterMove {
            translation_m: desired,
            grounded: true,
        }
    }

    #[test]
    fn sprint_raises_ground_speed_to_run_speed_and_only_on_the_ground() {
        let grounded = CharacterState {
            grounded: true,
            ..CharacterState::at([0.0; 3])
        };
        let walk = step_character(grounded, forward_input(0), DT, free_move);
        let run = step_character(grounded, forward_input(BUTTON_SPRINT), DT, free_move);
        let speed = |s: CharacterState| s.velocity_m_s[0].hypot(s.velocity_m_s[2]);
        assert!((speed(walk) - tuning::WALK_SPEED_M_S).abs() < 1e-4);
        assert!((speed(run) - tuning::RUN_SPEED_M_S).abs() < 1e-4);
        let ratio = tuning::RUN_SPEED_M_S / tuning::WALK_SPEED_M_S;
        assert!((1.7..=1.8).contains(&ratio), "run/walk = {ratio}");

        // Airborne keeps its takeoff momentum: pressing sprint mid-air adds none.
        let airborne = CharacterState {
            grounded: false,
            velocity_m_s: [0.0, 0.0, -tuning::WALK_SPEED_M_S],
            ..CharacterState::at([0.0; 3])
        };
        let held = step_character(airborne, forward_input(BUTTON_SPRINT), DT, |_, d| {
            CharacterMove {
                translation_m: d,
                grounded: false,
            }
        });
        assert_eq!(held.velocity_m_s[2], -tuning::WALK_SPEED_M_S);

        // Swimming ignores sprint.
        let water = Some(WaterSample::still(5.0));
        let swim = |buttons| {
            let mut s = CharacterState::at([0.0, 5.0 - tuning::FLOAT_DEPTH_M as f64, 0.0]);
            for _ in 0..120 {
                s = step_character_in_water(s, forward_input(buttons), DT, water, |_, d| {
                    CharacterMove {
                        translation_m: d,
                        grounded: false,
                    }
                });
            }
            speed(s)
        };
        assert!((swim(BUTTON_SPRINT) - swim(0)).abs() < 1e-3);
    }

    #[test]
    fn is_swimming_matches_the_depth_the_kernel_swims_at() {
        let water = Some(WaterSample::still(10.0));
        let deep = 10.0 - f64::from(tuning::SWIM_ENTER_DEPTH_M);
        assert!(is_swimming(water, deep - 0.01));
        assert!(!is_swimming(water, deep + 0.01));
        assert!(!is_swimming(None, 0.0));
    }

    #[test]
    fn airborne_input_cannot_revector_or_brake_takeoff_momentum() {
        let mut state = CharacterState {
            position_m: [0.0; 3],
            velocity_m_s: [0.0; 3],
            grounded: true,
            jump_held_last: false,
        };
        let launch = PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [0.0, 0.0, -1.0],
            buttons: BUTTON_JUMP,
        };
        state = step_character(state, launch, DT, |_, desired| CharacterMove {
            translation_m: desired,
            grounded: false,
        });
        let takeoff = [state.velocity_m_s[0], state.velocity_m_s[2]];
        assert!(takeoff[1] < -4.0, "launch should carry forward momentum");

        let reverse_and_turn = PlayerInput {
            movement: [1.0, 0.0, -1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        };
        for input in [reverse_and_turn, PlayerInput::NEUTRAL, reverse_and_turn] {
            state = step_character(state, input, DT, |_, desired| CharacterMove {
                translation_m: desired,
                grounded: false,
            });
            assert_eq!([state.velocity_m_s[0], state.velocity_m_s[2]], takeoff);
        }
    }

    #[test]
    fn jump_arc_keeps_useful_height_but_lands_in_under_eight_tenths() {
        let mut state = CharacterState {
            position_m: [0.0; 3],
            velocity_m_s: [0.0; 3],
            grounded: true,
            jump_held_last: false,
        };
        let jump = PlayerInput {
            buttons: BUTTON_JUMP,
            ..PlayerInput::NEUTRAL
        };
        let mut apex = 0.0_f64;
        let mut airborne_ticks = 0usize;
        for tick in 0..120 {
            let input = if tick == 0 {
                jump
            } else {
                PlayerInput::NEUTRAL
            };
            state = step_character(state, input, DT, |pos, desired| {
                let next_y = pos[1] + f64::from(desired[1]);
                if desired[1] <= 0.0 && next_y <= 0.0 {
                    CharacterMove {
                        translation_m: [desired[0], -(pos[1] as f32), desired[2]],
                        grounded: true,
                    }
                } else {
                    CharacterMove {
                        translation_m: desired,
                        grounded: false,
                    }
                }
            });
            apex = apex.max(state.position_m[1]);
            airborne_ticks += 1;
            if tick > 0 && state.grounded {
                break;
            }
        }
        let airtime_s = airborne_ticks as f32 * DT;
        assert!((0.9..=1.1).contains(&apex), "jump apex was {apex:.3} m");
        assert!(
            (0.6..0.8).contains(&airtime_s),
            "jump airtime was {airtime_s:.3} s"
        );
    }

    /// T23 / G3 row 15 follow-up: a valid near-origin spawn outside the raised
    /// beam's footprint walks normally. The old `[1, 1, 1]` repro was invalid:
    /// the floor top is at `y = 1.0 m`, the beam underside is at `y = 2.5 m`,
    /// and the standing capsule is `1.8 m` tall, so it began overlapping the
    /// beam by `0.3 m`. Rapier's sweep is not a spawn-depenetration operation;
    /// its zero-time contacts from that overlapping state cannot establish an
    /// unobstructed horizontal path. Moving only `z` outside the beam footprint
    /// preserves `x = 1 m` and clears the overlap.
    #[test]
    fn row15_near_origin_spawn_clear_of_beam_moves_horizontally() {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let v = vox::separated_regions_scene(VolumeId::new(1).unwrap());
        add_fixed(&mut world, &v);
        world.step();

        let params = CharacterParams::DEFAULT;
        let mut state = CharacterState::at([1.0, 1.0, 1.6]);
        let input = PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        };
        for _ in 0..200 {
            state = step_character(state, input, DT, |pos, desired| {
                world.sweep_character(params, pos, desired, DT)
            });
        }
        assert!(
            state.position_m[0] - 1.0 > 1.0,
            "a capsule outside the beam footprint should move; ended at x={}",
            state.position_m[0]
        );
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

    // ---- swimming --------------------------------------------------------

    /// A world with no terrain: the sweep just applies the desired move.
    fn open_water(
        mut state: CharacterState,
        input: PlayerInput,
        surface: f64,
        ticks: usize,
    ) -> CharacterState {
        for _ in 0..ticks {
            state = step_character_in_water(
                state,
                input,
                DT,
                Some(WaterSample::still(surface)),
                |_, d| CharacterMove {
                    translation_m: d,
                    grounded: false,
                },
            );
        }
        state
    }

    fn look(yaw_forward: f32, pitch_y: f32, jump: bool) -> PlayerInput {
        let h = (1.0 - pitch_y * pitch_y).max(0.0).sqrt();
        PlayerInput {
            movement: [0.0, 0.0, yaw_forward],
            view_dir: [0.0, pitch_y, -h],
            buttons: if jump { BUTTON_JUMP } else { 0 },
        }
    }

    #[test]
    fn a_player_who_falls_into_deep_water_treads_with_head_and_shoulders_out() {
        let state = CharacterState::at([0.0, 12.0, 0.0]);
        let end = open_water(state, look(0.0, 0.0, false), 10.0, 600);
        let depth = 10.0 - end.position_m[1];
        assert!(
            (depth - f64::from(tuning::FLOAT_DEPTH_M)).abs() < 0.05,
            "treads at {depth} m deep (expected {})",
            tuning::FLOAT_DEPTH_M
        );
        assert!(end.velocity_m_s[1].abs() < 0.05, "and stays put");
    }

    #[test]
    fn swimming_forward_moves_at_swim_speed_and_looking_down_dives() {
        let start = CharacterState::at([0.0, 10.0 - f64::from(tuning::FLOAT_DEPTH_M), 0.0]);
        let level = open_water(start, look(1.0, 0.0, false), 10.0, 120);
        let travelled = -level.position_m[2];
        assert!(
            (travelled - f64::from(tuning::SWIM_SPEED_M_S) * 2.0).abs() < 0.4,
            "swam {travelled} m in 2 s"
        );
        let dive = open_water(start, look(1.0, -0.7, false), 10.0, 60);
        assert!(
            dive.position_m[1] < start.position_m[1] - 1.0,
            "looking down while swimming dives: {}",
            dive.position_m[1]
        );
        // Looking up does not lift a swimmer out of the water.
        let up = open_water(start, look(1.0, 0.8, false), 10.0, 120);
        assert!(
            10.0 - up.position_m[1] > f64::from(tuning::FLOAT_DEPTH_M) - 0.08,
            "stayed in the water: depth {}",
            10.0 - up.position_m[1]
        );
    }

    #[test]
    fn jumping_from_the_surface_leaps_clear_of_the_water() {
        let start = CharacterState::at([0.0, 10.0 - f64::from(tuning::FLOAT_DEPTH_M), 0.0]);
        let settled = open_water(start, look(0.0, 0.0, false), 10.0, 30);
        // Swim forward and jump together, as the player does to get up a bank.
        let mut state = settled;
        let mut apex = f64::MIN;
        for tick in 0..90 {
            let input = look(1.0, 0.0, tick == 0);
            state = step_character_in_water(
                state,
                input,
                DT,
                Some(WaterSample::still(10.0)),
                |_, d| CharacterMove {
                    translation_m: d,
                    grounded: false,
                },
            );
            apex = apex.max(state.position_m[1]);
        }
        assert!(
            apex > 10.0 + 0.35,
            "the feet cleared the surface by {} m",
            apex - 10.0
        );
    }

    #[test]
    fn holding_jump_swims_up_from_the_depths() {
        let start = CharacterState::at([0.0, 5.0, 0.0]);
        let end = open_water(start, look(0.0, 0.0, true), 10.0, 60);
        assert!(
            end.position_m[1] > 5.0 + 1.5,
            "rose to {}",
            end.position_m[1]
        );
    }

    #[test]
    fn shallow_water_only_slows_walking_and_no_water_changes_nothing() {
        let walk = PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [0.0, 0.0, -1.0],
            buttons: 0,
        };
        let on_ground = CharacterState {
            grounded: true,
            ..CharacterState::at([0.0, 0.0, 0.0])
        };
        let step = |surface: Option<f64>| {
            step_character_in_water(
                on_ground,
                walk,
                DT,
                surface.map(WaterSample::still),
                |_, d| CharacterMove {
                    translation_m: d,
                    grounded: true,
                },
            )
        };
        let dry = step(None);
        let knee = step(Some(0.5));
        assert!(
            knee.velocity_m_s[2].abs() < dry.velocity_m_s[2].abs(),
            "wading is slower"
        );
        assert!(knee.velocity_m_s[2].abs() > 0.6 * dry.velocity_m_s[2].abs());
        // The wrapper without water is exactly the dry step.
        let plain = step_character(on_ground, walk, DT, |_, d| CharacterMove {
            translation_m: d,
            grounded: true,
        });
        assert_eq!(plain, dry);
    }

    fn in_current(
        mut state: CharacterState,
        input: PlayerInput,
        flow: [f32; 2],
        ticks: usize,
    ) -> CharacterState {
        let water = WaterSample {
            surface_y: 10.0,
            flow_m_s: flow,
        };
        for _ in 0..ticks {
            state = step_character_in_water(state, input, DT, Some(water), |_, d| CharacterMove {
                translation_m: d,
                grounded: false,
            });
        }
        state
    }

    #[test]
    fn a_current_carries_an_idle_swimmer_and_slows_one_swimming_against_it() {
        let start = CharacterState::at([0.0, 10.0 - f64::from(tuning::FLOAT_DEPTH_M), 0.0]);
        // Idle: the swimmer drifts to the water's own speed (+x at 1.5 m/s).
        let drifted = in_current(start, look(0.0, 0.0, false), [1.5, 0.0], 180);
        assert!(
            (drifted.velocity_m_s[0] - 1.5).abs() < 0.05,
            "drifts at the current's speed: {}",
            drifted.velocity_m_s[0]
        );
        assert!(
            drifted.position_m[0] > 3.5,
            "carried {} m",
            drifted.position_m[0]
        );
        // Swimming upstream (-z is forward at yaw 0; current runs -z too, so
        // flip it): net speed is swim speed minus the current.
        let against = in_current(start, look(1.0, 0.0, false), [0.0, 1.5], 120);
        let net = -against.velocity_m_s[2];
        assert!(
            (net - (tuning::SWIM_SPEED_M_S - 1.5)).abs() < 0.1,
            "net upstream speed {net}"
        );
        // The same water at rest changes nothing.
        let still = in_current(start, look(0.0, 0.0, false), [0.0, 0.0], 120);
        assert!(still.position_m[0].abs() < 1e-3 && still.position_m[2].abs() < 1e-3);
    }

    #[test]
    fn a_current_shoves_a_wader_but_not_one_on_dry_ground() {
        let idle = PlayerInput {
            movement: [0.0; 3],
            view_dir: [0.0, 0.0, -1.0],
            buttons: 0,
        };
        let on_ground = CharacterState {
            grounded: true,
            ..CharacterState::at([0.0, 9.0, 0.0])
        };
        let step = |water: Option<WaterSample>| {
            step_character_in_water(on_ground, idle, DT, water, |_, d| CharacterMove {
                translation_m: d,
                grounded: true,
            })
        };
        // Knee deep (surface 0.6 m over the feet): pushed along the current.
        let wet = step(Some(WaterSample {
            surface_y: 9.6,
            flow_m_s: [2.0, 0.0],
        }));
        assert!(wet.velocity_m_s[0] > 0.5, "pushed {}", wet.velocity_m_s[0]);
        // Dry ground has no water sample, so nothing pushes.
        assert_eq!(step(None).velocity_m_s[0], 0.0);
        // Only a film over the feet barely moves them.
        let film = step(Some(WaterSample {
            surface_y: 9.02,
            flow_m_s: [2.0, 0.0],
        }));
        assert!(film.velocity_m_s[0] < 0.1, "film {}", film.velocity_m_s[0]);
    }
}

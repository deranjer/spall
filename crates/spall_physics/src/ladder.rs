//! Ladders (ENG-263): climbing voxel cells whose material carries
//! [`MaterialFlags::CLIMBABLE`].
//!
//! A ladder is ordinary solid terrain: it collides, it can be dug out, and it
//! replicates like every other cell. What makes it a ladder is the material
//! flag. [`ladder_contact`] probes the voxels around a capsule for the nearest
//! climbable cell within reach; the character kernel
//! ([`crate::step_character_in_water`]) takes that probe as an input, exactly
//! like the water sample, so the server and the client predictor run one code
//! path against their own copy of the same terrain.
//!
//! Controls: walk into a ladder with W (forward) while facing it to start
//! climbing. W climbs, S climbs down, no input holds on, and jump (SPACE) slides
//! down fast. Gravity is off while climbing. Climbing past the top carries the
//! climber forward onto the platform; reaching the ground while going down,
//! turning away and walking, or leaving the ladder's reach returns to normal
//! movement. A falling capsule grabs a ladder it faces if W or S is held.

use spall_core::{GlobalCell, MaterialFlags, MaterialId, MaterialManifest};
use spall_voxel::{Sample, Volume};

use crate::character::{CharacterMove, CharacterParams, CharacterState, PlayerInput, tuning};

/// The material ids a manifest marks [`MaterialFlags::CLIMBABLE`], sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Climbable(Vec<MaterialId>);

impl Climbable {
    /// Every climbable material of `manifest`.
    pub fn from_manifest(manifest: &MaterialManifest) -> Self {
        // Manifest entries are validated to be in ascending id order.
        Self(
            manifest
                .entries()
                .iter()
                .filter(|def| def.sim.flags.contains(MaterialFlags::CLIMBABLE))
                .map(|def| def.id)
                .collect(),
        )
    }

    /// `true` if no material is climbable (the probe can then skip the voxels).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `true` if `id` is climbable.
    pub fn contains(&self, id: MaterialId) -> bool {
        self.0.binary_search(&id).is_ok()
    }
}

/// The ladder a capsule can reach, as [`ladder_contact`] found it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderContact {
    /// Unit horizontal direction `[x, z]` from the capsule axis to the nearest
    /// climbable cell.
    pub toward: [f32; 2],
    /// World height (metres) of the top face of that cell's ladder column.
    pub top_y: f64,
}

/// The nearest climbable cell of `volume` within reach of a capsule with its
/// feet at `feet_m`: horizontally within [`tuning::LADDER_REACH_M`] of the
/// capsule surface, vertically from [`tuning::LADDER_BELOW_FEET_M`] under the
/// feet to the crown. Volume cell `(i, j, k)` spans `[i, i + 1) * cell` metres
/// on each axis (the terrain frame both server and client use). Unloaded cells
/// are not ladders. `None` when nothing climbable is in reach.
pub fn ladder_contact(
    volume: &Volume,
    climbable: &Climbable,
    params: CharacterParams,
    feet_m: [f64; 3],
) -> Option<LadderContact> {
    if climbable.is_empty() || !feet_m.iter().all(|v| v.is_finite()) {
        return None;
    }
    let cell = volume.cell_size().metres();
    let reach = f64::from(params.radius_m + tuning::LADDER_REACH_M);
    let [x, y, z] = feet_m;
    let span = |lo: f64, hi: f64| (lo / cell).floor() as i64..=(hi / cell).floor() as i64;
    let rows = span(
        y - f64::from(tuning::LADDER_BELOW_FEET_M),
        y + f64::from(params.total_height_m()),
    );
    let is_ladder = |i: i64, j: i64, k: i64| {
        matches!(
            volume.sample(GlobalCell::new(i, j, k)),
            Ok(Sample::Filled(id)) if climbable.contains(id)
        )
    };

    // (squared distance, direction, column, highest ladder row in the band)
    let mut best: Option<(f64, [f64; 2], i64, i64, i64)> = None;
    for i in span(x - reach, x + reach) {
        for k in span(z - reach, z + reach) {
            let dx = x.clamp(i as f64 * cell, (i + 1) as f64 * cell) - x;
            let dz = z.clamp(k as f64 * cell, (k + 1) as f64 * cell) - z;
            let mut d2 = dx * dx + dz * dz;
            if d2 > reach * reach || best.is_some_and(|b| b.0 <= d2) {
                continue;
            }
            let Some(j) = rows.clone().rev().find(|&j| is_ladder(i, j, k)) else {
                continue;
            };
            // The capsule never overlaps solid, but fall back to the cell centre
            // rather than divide by zero if it does.
            let mut dir = [dx, dz];
            if d2 < 1e-12 {
                dir = [(i as f64 + 0.5) * cell - x, (k as f64 + 0.5) * cell - z];
                d2 = 0.0;
            }
            best = Some((d2, dir, i, j, k));
        }
    }
    let (_, dir, i, mut j, k) = best?;
    let len = (dir[0] * dir[0] + dir[1] * dir[1]).sqrt();
    if len < 1e-9 {
        return None;
    }
    // Follow the column up to its top, bounded so a malformed world cannot stall
    // a tick.
    for _ in 0..tuning::LADDER_MAX_SCAN_CELLS {
        if !is_ladder(i, j + 1, k) {
            break;
        }
        j += 1;
    }
    Some(LadderContact {
        toward: [(dir[0] / len) as f32, (dir[1] / len) as f32],
        top_y: (j + 1) as f64 * cell,
    })
}

/// One tick on a ladder, or `None` when the capsule is not (or no longer)
/// climbing and the caller should run the ordinary walk/swim step. Clears
/// `state.climbing` (and any climb velocity) on the way out.
pub(crate) fn climb_step(
    state: &mut CharacterState,
    input: PlayerInput,
    dt: f32,
    ladder: Option<LadderContact>,
    sweep: &mut impl FnMut([f64; 3], [f32; 3]) -> CharacterMove,
) -> Option<CharacterState> {
    let forward = input.movement[2];
    let (sin_y, cos_y) = input.yaw().sin_cos();
    let facing = [sin_y, -cos_y];
    let right = [cos_y, sin_y];
    let leave = |state: &mut CharacterState| {
        if state.climbing {
            state.climbing = false;
            state.velocity_m_s[1] = 0.0;
        }
        None
    };
    let Some(contact) = ladder else {
        return leave(state);
    };
    let toward = contact.toward;
    let facing_dot = facing[0] * toward[0] + facing[1] * toward[1];
    let feet_y = state.position_m[1];
    if !state.climbing {
        // Walk into the ladder with W, or grab it on the way down with W or S.
        let grabs = forward > 0.0 || (forward < 0.0 && !state.grounded);
        let rises_above_feet = contact.top_y > feet_y + f64::from(tuning::LADDER_ENTER_ABOVE_M);
        if !(grabs && rises_above_feet && facing_dot >= tuning::LADDER_ENTER_FACING) {
            return None;
        }
    } else if forward > 0.0 && facing_dot < 0.0 {
        // Turned away and walking: let go.
        return leave(state);
    }

    let jump_now = input.wants_jump();
    let moving = forward != 0.0 || input.movement[0] != 0.0 || jump_now;
    state.tick_endurance(dt, false, false, moving);

    // Over the top: step forward onto the platform and walk on from there.
    if forward > 0.0 && feet_y >= contact.top_y + f64::from(tuning::LADDER_MOUNT_CLEARANCE_M) {
        let v = [
            toward[0] * tuning::WALK_SPEED_M_S,
            0.0,
            toward[1] * tuning::WALK_SPEED_M_S,
        ];
        let mv = sweep(state.position_m, [v[0] * dt, 0.0, v[2] * dt]);
        apply(state, mv.translation_m);
        state.velocity_m_s = v;
        state.grounded = mv.grounded;
        state.climbing = false;
        state.jump_held_last = jump_now;
        return Some(*state);
    }

    let vy = if jump_now {
        -tuning::LADDER_SLIDE_M_S
    } else if forward > 0.0 {
        tuning::LADDER_CLIMB_M_S
    } else if forward < 0.0 {
        -tuning::LADDER_DESCEND_M_S
    } else {
        0.0
    };
    // A/D shuffles along the ladder face; off its edge the probe loses it.
    let along_dot = right[0] * toward[0] + right[1] * toward[1];
    let mut side = [
        right[0] - along_dot * toward[0],
        right[1] - along_dot * toward[1],
    ];
    let side_len = (side[0] * side[0] + side[1] * side[1]).sqrt();
    if side_len > 1e-6 {
        let s = input.movement[0] * tuning::LADDER_SIDE_M_S / side_len;
        side = [side[0] * s, side[1] * s];
    } else {
        side = [0.0; 2];
    }
    let desired = [side[0] * dt, vy * dt, side[1] * dt];
    let mv = sweep(state.position_m, desired);
    apply(state, mv.translation_m);
    state.grounded = mv.grounded;
    state.jump_held_last = jump_now;
    if vy < 0.0 && mv.grounded {
        // Climbed down to the ground.
        state.climbing = false;
        state.velocity_m_s = [0.0; 3];
        return Some(*state);
    }
    let blocked = (desired[1] - mv.translation_m[1]).abs() > 1e-4;
    state.velocity_m_s = [side[0], if blocked { 0.0 } else { vy }, side[1]];
    state.climbing = true;
    Some(*state)
}

fn apply(state: &mut CharacterState, translation_m: [f32; 3]) {
    for (p, t) in state.position_m.iter_mut().zip(translation_m) {
        *p += f64::from(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, ladder::*};
    use crate::occupancy::OccupancyGrid;
    use crate::world::{BodyKind, BodySpec, PhysicsConfig, PhysicsWorld};
    use crate::{Representation, step_character_in_water};
    use spall_core::{BUTTON_JUMP, MaterialDef, RenderProps, SimProps, VolumeId};

    const DT: f32 = 1.0 / 60.0;
    const LADDER: MaterialId = MaterialId(3);
    const FACE_LADDER: [f32; 3] = [1.0, 0.0, 0.0];

    struct Scene {
        world: PhysicsWorld,
        volume: Volume,
        climbable: Climbable,
    }

    impl Scene {
        fn new() -> Self {
            let volume = fixtures::ladder_wall(VolumeId::new(1).unwrap(), LADDER);
            let mut world = PhysicsWorld::new(PhysicsConfig::default());
            world.add_body(BodySpec {
                kind: BodyKind::Fixed,
                representation: Representation::MergedCuboids,
                grid: OccupancyGrid::from_volume(&volume).unwrap().unwrap(),
                cell_m: fixtures::CELL_M,
                density_kg_m3: 1.0,
                mass_properties: None,
                translation_m: [0.0; 3],
                linvel_m_s: [0.0; 3],
            });
            world.step();
            Self {
                world,
                volume,
                climbable: Climbable(vec![LADDER]),
            }
        }

        fn step(&mut self, state: CharacterState, input: PlayerInput) -> CharacterState {
            let params = CharacterParams::DEFAULT;
            let ladder = ladder_contact(&self.volume, &self.climbable, params, state.position_m);
            let world = &mut self.world;
            step_character_in_water(state, input, DT, None, ladder, |p, d| {
                world.sweep_character(params, p, d, DT)
            })
        }

        fn run(
            &mut self,
            mut state: CharacterState,
            input: PlayerInput,
            ticks: usize,
        ) -> CharacterState {
            for _ in 0..ticks {
                state = self.step(state, input);
            }
            state
        }

        /// Settled on the floor at the spawn, then walked into the ladder and
        /// climbed for `climb_ticks`.
        fn on_ladder(&mut self, climb_ticks: usize) -> CharacterState {
            let mut state = self.run(CharacterState::at(SPAWN_M), PlayerInput::NEUTRAL, 10);
            let mut ticks = 0;
            for _ in 0..climb_ticks + 120 {
                state = self.step(state, input(1.0, 0.0, FACE_LADDER, 0));
                ticks += usize::from(state.climbing);
                if ticks == climb_ticks {
                    return state;
                }
            }
            panic!("never climbed {climb_ticks} ticks");
        }
    }

    fn input(forward: f32, strafe: f32, view_dir: [f32; 3], buttons: u32) -> PlayerInput {
        PlayerInput {
            movement: [strafe, 0.0, forward],
            view_dir,
            buttons,
        }
    }

    /// Ticks until the climber is back on the floor with `input` held.
    fn ticks_to_floor(scene: &mut Scene, mut state: CharacterState, input: PlayerInput) -> usize {
        for tick in 1..600 {
            state = scene.step(state, input);
            if !state.climbing {
                assert!(state.grounded, "left the ladder in mid-air: {state:?}");
                // Rapier calls the capsule grounded a few centimetres up; the
                // next walking step snaps it down.
                assert!((state.position_m[1] - FLOOR_TOP_M).abs() < 0.1, "{state:?}");
                return tick;
            }
        }
        panic!("never reached the floor");
    }

    #[test]
    fn walking_into_the_ladder_with_w_climbs_it_and_steps_onto_the_platform() {
        let mut scene = Scene::new();
        let w = input(1.0, 0.0, FACE_LADDER, 0);
        let mut state = scene.run(CharacterState::at(SPAWN_M), PlayerInput::NEUTRAL, 10);
        let mut climbed = 0usize;
        for _ in 0..600 {
            state = scene.step(state, w);
            if state.climbing {
                climbed += 1;
                // Gravity is off: the climber only goes up.
                assert!(state.velocity_m_s[1] >= 0.0);
            } else if climbed > 0 && state.grounded {
                break;
            }
        }
        // 4 m at the climb speed, give or take the start and the step off.
        let climb_s = climbed as f32 * DT;
        let expected = 4.0 / tuning::LADDER_CLIMB_M_S;
        assert!(
            (expected - 0.2..expected + 0.4).contains(&climb_s),
            "climbed for {climb_s} s"
        );
        let state = scene.run(state, w, 6);
        assert!(!state.climbing && state.grounded);
        assert!(
            (state.position_m[1] - PLATFORM_TOP_M).abs() < 0.05,
            "feet at {:?}",
            state.position_m
        );
        assert!(
            state.position_m[0] > LADDER_FACE_X_M,
            "not on the platform: {:?}",
            state.position_m
        );
    }

    #[test]
    fn no_input_holds_on_and_s_climbs_down_to_the_floor() {
        let mut scene = Scene::new();
        let state = scene.on_ladder(60);
        let held = scene.run(state, input(0.0, 0.0, FACE_LADDER, 0), 120);
        assert!(held.climbing);
        assert!(
            (held.position_m[1] - state.position_m[1]).abs() < 1e-6,
            "slipped"
        );
        let height = state.position_m[1] - FLOOR_TOP_M;
        assert!(height > 2.0, "climbed {height} m");
        let ticks = ticks_to_floor(&mut scene, held, input(-1.0, 0.0, FACE_LADDER, 0));
        let expected = height / f64::from(tuning::LADDER_DESCEND_M_S) * 60.0;
        assert!(
            (ticks as f64 - expected).abs() < 6.0,
            "{ticks} ticks vs {expected}"
        );
    }

    #[test]
    fn space_slides_down_faster_than_s() {
        let mut scene = Scene::new();
        let state = scene.on_ladder(90);
        let s = ticks_to_floor(&mut scene, state, input(-1.0, 0.0, FACE_LADDER, 0));
        let slide = ticks_to_floor(&mut scene, state, input(0.0, 0.0, FACE_LADDER, BUTTON_JUMP));
        assert!(slide * 2 < s, "slide {slide} ticks, S {s} ticks");
    }

    #[test]
    fn turning_away_and_walking_detaches() {
        let mut scene = Scene::new();
        // Just above the floor: turn round and walk away.
        let state = scene.on_ladder(6);
        assert!(state.climbing);
        let away = scene.run(state, input(1.0, 0.0, [-1.0, 0.0, 0.0], 0), 60);
        assert!(!away.climbing && away.grounded);
        assert!(
            away.position_m[0] < SPAWN_M[0] - 1.5,
            "{:?}",
            away.position_m
        );
    }

    #[test]
    fn shuffling_off_the_side_lets_go_and_falls() {
        let mut scene = Scene::new();
        let mut state = scene.on_ladder(30);
        for _ in 0..120 {
            state = scene.step(state, input(0.0, 1.0, FACE_LADDER, 0));
            if !state.climbing {
                break;
            }
        }
        assert!(
            !state.climbing,
            "still on the ladder at {:?}",
            state.position_m
        );
        assert!((state.position_m[2] - LADDER_CENTRE_Z_M).abs() > 0.5);
        let landed = scene.run(state, PlayerInput::NEUTRAL, 120);
        assert!(landed.grounded && (landed.position_m[1] - FLOOR_TOP_M).abs() < 0.05);
    }

    #[test]
    fn looking_along_the_wall_or_without_climbable_materials_does_not_grab() {
        let mut scene = Scene::new();
        let start = scene.run(
            CharacterState::at([7.3, FLOOR_TOP_M, 3.0]),
            PlayerInput::NEUTRAL,
            10,
        );
        // Walks along the wall past the ladder, looking +z.
        let along = scene.run(start, input(1.0, 0.0, [0.0, 0.0, 1.0], 0), 30);
        assert!(!along.climbing && along.position_m[1] < FLOOR_TOP_M + 0.05);
        assert!(along.position_m[2] > 4.5);

        scene.climbable = Climbable::default();
        let blocked = scene.run(
            CharacterState::at(SPAWN_M),
            input(1.0, 0.0, FACE_LADDER, 0),
            60,
        );
        assert!(!blocked.climbing && blocked.position_m[1] < FLOOR_TOP_M + 0.05);
    }

    #[test]
    fn the_probe_reports_the_ladder_direction_and_top() {
        let scene = Scene::new();
        let params = CharacterParams::DEFAULT;
        let near = [LADDER_FACE_X_M - 0.35, FLOOR_TOP_M + 1.0, LADDER_CENTRE_Z_M];
        let contact = ladder_contact(&scene.volume, &scene.climbable, params, near).unwrap();
        assert_eq!(contact.toward, [1.0, 0.0]);
        assert_eq!(contact.top_y, PLATFORM_TOP_M);
        let far = [LADDER_FACE_X_M - 0.6, FLOOR_TOP_M + 1.0, LADDER_CENTRE_Z_M];
        assert!(ladder_contact(&scene.volume, &scene.climbable, params, far).is_none());
    }

    #[test]
    fn climbable_comes_from_the_manifest_flag() {
        let def = |id: u16, name: &str, flags: u32| MaterialDef {
            id: MaterialId(id),
            name: name.into(),
            render: RenderProps {
                albedo: [0.5; 3],
                roughness: 1.0,
                metalness: 0.0,
                emissive: [0.0; 3],
            },
            sim: SimProps {
                density_kg_m3: if id == 0 { 0.0 } else { 600.0 },
                friction: 0.5,
                restitution: 0.0,
                hardness: 1.0,
                bond_strength: 1.0,
                flags: MaterialFlags(flags),
            },
        };
        let manifest = MaterialManifest::validated(vec![
            def(0, "air", 0),
            def(1, "stone", MaterialFlags::COLLIDES.0),
            def(
                7,
                "ladder",
                MaterialFlags::COLLIDES.0 | MaterialFlags::CLIMBABLE.0,
            ),
        ])
        .unwrap();
        let climbable = Climbable::from_manifest(&manifest);
        assert!(climbable.contains(MaterialId(7)));
        assert!(!climbable.contains(MaterialId(1)));
    }
}

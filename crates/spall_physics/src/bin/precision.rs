//! ENG-31's first post-scope experiment: compare fixed-world-coordinate physics
//! against an explicit local physics origin for voxel contacts and characters.

use std::io::Write;
use std::path::PathBuf;

use spall_core::VolumeId;
use spall_physics::collider::Representation;
use spall_physics::fixtures::{CELL_M, STONE_DENSITY, debris_pieces, floor_slab};
use spall_physics::occupancy::OccupancyGrid;
use spall_physics::world::{BodyKind, BodySpec, PhysicsConfig, PhysicsWorld};
use spall_physics::{CharacterParams, CharacterState, PlayerInput, step_character};

const DISTANCES_M: [f64; 11] = [
    0.0, 100.0, 250.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0, 20_000.0, 50_000.0, 100_000.0,
];
const SETTLE_STEPS: usize = 900;

pub(super) fn run(out: Option<PathBuf>) -> std::process::ExitCode {
    let mut rows = DISTANCES_M.map(measure);
    let two_origin = measure_two_local_origins();
    let baseline_character_travel = rows[0].character_travel_m;
    let baseline_rebased_character_travel = rows[0].rebased_character_travel_m;
    let baseline_interaction_speed = rows[0].interacting_target_peak_speed_m_s;
    let baseline_rebased_interaction_speed = rows[0].rebased_interacting_target_peak_speed_m_s;
    for row in &mut rows {
        row.character_travel_delta_m = row.character_travel_m - baseline_character_travel;
        row.rebased_character_travel_delta_m =
            row.rebased_character_travel_m - baseline_rebased_character_travel;
        row.interacting_target_peak_speed_delta_m_s =
            row.interacting_target_peak_speed_m_s - baseline_interaction_speed;
        row.rebased_interacting_target_peak_speed_delta_m_s =
            row.rebased_interacting_target_peak_speed_m_s - baseline_rebased_interaction_speed;
    }
    let json = format!(
        "{{\"schema_version\":3,\"experiment\":\"fixed_vs_rebased_precision_sweep\",\
         \"representation\":\"merged_cuboids\",\"sample_count\":{},\
         \"settle_steps\":{SETTLE_STEPS},\"character_walk_ticks\":60,\
         \"interaction_steps\":240,\"two_origin_probe\":{},\
         \"samples\":[{}]}}\n",
        rows.len(),
        two_origin.to_json(),
        rows.iter()
            .map(Measurement::to_json)
            .collect::<Vec<_>>()
            .join(",")
    );
    print!("{json}");

    if let Some(dir) = out {
        if let Err(error) = std::fs::create_dir_all(&dir) {
            eprintln!("collision-bench: cannot create {}: {error}", dir.display());
            return std::process::ExitCode::from(1);
        }
        let path = dir.join("collision-precision.json");
        if let Err(error) =
            std::fs::File::create(&path).and_then(|mut f| f.write_all(json.as_bytes()))
        {
            eprintln!("collision-bench: cannot write {}: {error}", path.display());
            return std::process::ExitCode::from(1);
        }
        eprintln!("collision-bench: wrote {}", path.display());
    }

    if rows.iter().all(|r| {
        r.contact_seen
            && r.finite
            && r.max_penetration_m < 0.125
            && r.character_finite
            && r.character_grounded_ticks >= 55
            && r.rebased_character_finite
            && r.rebased_character_grounded_ticks >= 55
            && r.interacting_contact_seen
            && r.interacting_target_peak_speed_m_s > 0.1
            && r.interacting_max_penetration_m < 0.125
            && r.interacting_bodies_finite
            && r.rebased_interacting_contact_seen
            && r.rebased_interacting_target_peak_speed_m_s > 0.1
            && r.rebased_interacting_bodies_finite
    }) && two_origin.finite
        && two_origin.near_player_grounded_ticks >= 55
        && two_origin.far_player_grounded_ticks >= 55
    {
        std::process::ExitCode::SUCCESS
    } else {
        eprintln!("collision-bench: a precision sample lost contact or became unstable");
        std::process::ExitCode::from(1)
    }
}

struct Measurement {
    origin_distance_m: f64,
    initial_x_rounding_error_m: f64,
    initial_y_m: f64,
    final_y_m: f64,
    rest_height_error_m: f64,
    final_x_drift_m: f64,
    max_penetration_m: f64,
    contact_seen: bool,
    finite: bool,
    character_travel_m: f64,
    character_travel_delta_m: f64,
    character_grounded_ticks: usize,
    character_short_steps: usize,
    character_rest_height_error_m: f64,
    character_finite: bool,
    rebased_character_travel_m: f64,
    rebased_character_travel_delta_m: f64,
    rebased_character_grounded_ticks: usize,
    rebased_character_short_steps: usize,
    rebased_character_rest_height_error_m: f64,
    rebased_character_finite: bool,
    interacting_contact_seen: bool,
    interacting_target_peak_speed_m_s: f64,
    interacting_target_peak_speed_delta_m_s: f64,
    interacting_max_penetration_m: f64,
    interacting_bodies_finite: bool,
    rebased_interacting_contact_seen: bool,
    rebased_interacting_target_peak_speed_m_s: f64,
    rebased_interacting_target_peak_speed_delta_m_s: f64,
    rebased_interacting_max_penetration_m: f64,
    rebased_interacting_bodies_finite: bool,
}

struct TwoOriginProbe {
    separation_m: f64,
    near_player_travel_m: f64,
    far_player_travel_m: f64,
    travel_delta_m: f64,
    near_player_grounded_ticks: usize,
    far_player_grounded_ticks: usize,
    finite: bool,
}

impl TwoOriginProbe {
    fn to_json(&self) -> String {
        format!(
            "{{\"separation_m\":{},\"near_player_travel_m\":{},\
             \"far_player_travel_m\":{},\"travel_delta_m\":{},\
             \"near_player_grounded_ticks\":{},\"far_player_grounded_ticks\":{},\
             \"finite\":{}}}",
            self.separation_m,
            self.near_player_travel_m,
            self.far_player_travel_m,
            self.travel_delta_m,
            self.near_player_grounded_ticks,
            self.far_player_grounded_ticks,
            self.finite,
        )
    }
}

impl Measurement {
    fn to_json(&self) -> String {
        format!(
            "{{\"origin_distance_m\":{},\"initial_x_rounding_error_m\":{},\
             \"initial_y_m\":{},\"final_y_m\":{},\"rest_height_error_m\":{},\
             \"final_x_drift_m\":{},\
             \"max_penetration_m\":{},\"contact_seen\":{},\"finite\":{},\
             \"character_travel_m\":{},\"character_travel_delta_m\":{},\
             \"character_grounded_ticks\":{},\"character_short_steps\":{},\
             \"character_rest_height_error_m\":{},\"character_finite\":{},\
             \"rebased_character_travel_m\":{},\"rebased_character_travel_delta_m\":{},\
             \"rebased_character_grounded_ticks\":{},\"rebased_character_short_steps\":{},\
             \"rebased_character_rest_height_error_m\":{},\"rebased_character_finite\":{},\
             \"interacting_contact_seen\":{},\"interacting_target_peak_speed_m_s\":{},\
             \"interacting_target_peak_speed_delta_m_s\":{},\
             \"interacting_max_penetration_m\":{},\"interacting_bodies_finite\":{},\
             \"rebased_interacting_contact_seen\":{},\
             \"rebased_interacting_target_peak_speed_m_s\":{},\
             \"rebased_interacting_target_peak_speed_delta_m_s\":{},\
             \"rebased_interacting_max_penetration_m\":{},\
             \"rebased_interacting_bodies_finite\":{}}}",
            self.origin_distance_m,
            self.initial_x_rounding_error_m,
            self.initial_y_m,
            self.final_y_m,
            self.rest_height_error_m,
            self.final_x_drift_m,
            self.max_penetration_m,
            self.contact_seen,
            self.finite,
            self.character_travel_m,
            self.character_travel_delta_m,
            self.character_grounded_ticks,
            self.character_short_steps,
            self.character_rest_height_error_m,
            self.character_finite,
            self.rebased_character_travel_m,
            self.rebased_character_travel_delta_m,
            self.rebased_character_grounded_ticks,
            self.rebased_character_short_steps,
            self.rebased_character_rest_height_error_m,
            self.rebased_character_finite,
            self.interacting_contact_seen,
            self.interacting_target_peak_speed_m_s,
            self.interacting_target_peak_speed_delta_m_s,
            self.interacting_max_penetration_m,
            self.interacting_bodies_finite,
            self.rebased_interacting_contact_seen,
            self.rebased_interacting_target_peak_speed_m_s,
            self.rebased_interacting_target_peak_speed_delta_m_s,
            self.rebased_interacting_max_penetration_m,
            self.rebased_interacting_bodies_finite,
        )
    }
}

fn measure(distance_m: f64) -> Measurement {
    let distance = distance_m as f32;
    let floor_id = VolumeId::new(100).expect("nonzero fixture id");
    let floor = floor_slab(floor_id, 1, 1, 1);
    let floor_grid = OccupancyGrid::from_volume(&floor)
        .expect("floor occupancy extraction")
        .expect("floor is solid");
    let (_, cube) = debris_pieces(101, 1, 2).pop().expect("one cube");
    let cube_grid = OccupancyGrid::from_volume(&cube)
        .expect("cube occupancy extraction")
        .expect("cube is solid");

    // Deliberately use a non-binary-fraction offset so the f32 rounding error
    // is observable as the coordinate magnitude grows.
    let ideal_x = distance_m + 3.83;
    let expected_x = ideal_x as f32;
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: floor_grid,
        cell_m: CELL_M,
        density_kg_m3: STONE_DENSITY,
        mass_properties: None,
        translation_m: [distance, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });
    let cube_body = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid: cube_grid,
        cell_m: CELL_M,
        density_kg_m3: STONE_DENSITY,
        mass_properties: None,
        translation_m: [expected_x, 1.25, 3.83],
        linvel_m_s: [0.0; 3],
    });

    let initial_x = f64::from(world.body_state(cube_body).translation_m[0]);
    let mut contact_seen = false;
    let mut finite = true;
    let mut max_penetration_m = 0.0_f32;
    for _ in 0..SETTLE_STEPS {
        world.step();
        contact_seen |= world.contact_pair_count() > 0;
        max_penetration_m = max_penetration_m.max(world.max_penetration_m());
        finite &= world
            .body_state(cube_body)
            .translation_m
            .iter()
            .all(|v| v.is_finite());
    }
    let final_state = world.body_state(cube_body);
    let (
        character_travel_m,
        character_grounded_ticks,
        character_short_steps,
        character_rest_height_error_m,
        character_finite,
    ) = measure_character(distance, 0.0);
    let (
        rebased_character_travel_m,
        rebased_character_grounded_ticks,
        rebased_character_short_steps,
        rebased_character_rest_height_error_m,
        rebased_character_finite,
    ) = measure_character(distance, distance);
    let (
        interacting_contact_seen,
        interacting_target_peak_speed_m_s,
        interacting_max_penetration_m,
        interacting_bodies_finite,
    ) = measure_interacting_bodies(distance, 0.0);
    let (
        rebased_interacting_contact_seen,
        rebased_interacting_target_peak_speed_m_s,
        rebased_interacting_max_penetration_m,
        rebased_interacting_bodies_finite,
    ) = measure_interacting_bodies(distance, distance);
    Measurement {
        origin_distance_m: distance_m,
        initial_x_rounding_error_m: initial_x - ideal_x,
        initial_y_m: 1.25,
        final_y_m: f64::from(final_state.translation_m[1]),
        rest_height_error_m: f64::from(final_state.translation_m[1]) - 0.25,
        final_x_drift_m: f64::from(final_state.translation_m[0]) - initial_x,
        max_penetration_m: f64::from(max_penetration_m),
        contact_seen,
        finite,
        character_travel_m,
        character_travel_delta_m: 0.0,
        character_grounded_ticks,
        character_short_steps,
        character_rest_height_error_m,
        character_finite,
        rebased_character_travel_m,
        rebased_character_travel_delta_m: 0.0,
        rebased_character_grounded_ticks,
        rebased_character_short_steps,
        rebased_character_rest_height_error_m,
        rebased_character_finite,
        interacting_contact_seen,
        interacting_target_peak_speed_m_s,
        interacting_target_peak_speed_delta_m_s: 0.0,
        interacting_max_penetration_m,
        interacting_bodies_finite,
        rebased_interacting_contact_seen,
        rebased_interacting_target_peak_speed_m_s,
        rebased_interacting_target_peak_speed_delta_m_s: 0.0,
        rebased_interacting_max_penetration_m,
        rebased_interacting_bodies_finite,
    }
}

fn add_floor(world: &mut PhysicsWorld, distance: f32) {
    let floor_id = VolumeId::new(200).expect("nonzero fixture id");
    let floor = floor_slab(floor_id, 1, 1, 1);
    let grid = OccupancyGrid::from_volume(&floor)
        .expect("floor occupancy extraction")
        .expect("floor is solid");
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid,
        cell_m: CELL_M,
        density_kg_m3: STONE_DENSITY,
        mass_properties: None,
        translation_m: [distance, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });
}

fn measure_two_local_origins() -> TwoOriginProbe {
    const SEPARATION_M: f64 = 100_000.0;
    const TICKS: usize = 60;
    const DT: f32 = 1.0 / 60.0;
    let mut near_world = PhysicsWorld::new_in_namespace(PhysicsConfig::default(), 10);
    let mut far_world = PhysicsWorld::new_in_namespace(PhysicsConfig::default(), 11);
    add_floor(&mut near_world, 0.0);
    add_floor(&mut far_world, 0.0);
    near_world.step();
    far_world.step();

    let origins = [0.0, SEPARATION_M];
    let starts = [1.0, SEPARATION_M + 1.0];
    let mut states = [
        CharacterState::at([starts[0], 0.25, 4.0]),
        CharacterState::at([starts[1], 0.25, 4.0]),
    ];
    let input = PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    };
    let mut grounded_ticks = [0usize; 2];
    let mut finite = true;
    for _ in 0..TICKS {
        near_world.step();
        far_world.step();
        states[0] = step_character(states[0], input, DT, |position, desired| {
            near_world.sweep_character(
                CharacterParams::DEFAULT,
                [position[0] - origins[0], position[1], position[2]],
                desired,
                DT,
            )
        });
        states[1] = step_character(states[1], input, DT, |position, desired| {
            far_world.sweep_character(
                CharacterParams::DEFAULT,
                [position[0] - origins[1], position[1], position[2]],
                desired,
                DT,
            )
        });
        grounded_ticks[0] += usize::from(states[0].grounded);
        grounded_ticks[1] += usize::from(states[1].grounded);
        finite &= states[0].is_finite() && states[1].is_finite();
    }
    let near_travel = states[0].position_m[0] - starts[0];
    let far_travel = states[1].position_m[0] - starts[1];
    TwoOriginProbe {
        separation_m: SEPARATION_M,
        near_player_travel_m: near_travel,
        far_player_travel_m: far_travel,
        travel_delta_m: far_travel - near_travel,
        near_player_grounded_ticks: grounded_ticks[0],
        far_player_grounded_ticks: grounded_ticks[1],
        finite,
    }
}

fn measure_character(distance: f32, physics_origin_m: f32) -> (f64, usize, usize, f64, bool) {
    const TICKS: usize = 60;
    const DT: f32 = 1.0 / 60.0;
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    add_floor(&mut world, distance - physics_origin_m);
    world.step();
    let start_x = f64::from(distance) + 1.0;
    let mut state = CharacterState::at([start_x, 0.25, 4.0]);
    let input = PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    };
    let mut grounded_ticks = 0;
    let mut short_steps = 0;
    let mut finite = true;
    for _ in 0..TICKS {
        world.step();
        state = step_character(state, input, DT, |position, desired| {
            let local_position = [
                position[0] - f64::from(physics_origin_m),
                position[1],
                position[2],
            ];
            let movement =
                world.sweep_character(CharacterParams::DEFAULT, local_position, desired, DT);
            if desired[0] > 0.05 && movement.translation_m[0] + 0.001 < desired[0] {
                short_steps += 1;
            }
            movement
        });
        grounded_ticks += usize::from(state.grounded);
        finite &= state.is_finite();
    }
    (
        state.position_m[0] - start_x,
        grounded_ticks,
        short_steps,
        state.position_m[1] - 0.25,
        finite,
    )
}

fn measure_interacting_bodies(distance: f32, physics_origin_m: f32) -> (bool, f64, f64, bool) {
    const TICKS: usize = 240;
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    add_floor(&mut world, distance - physics_origin_m);
    let mut pieces = debris_pieces(201, 2, 2).into_iter();
    let (_, first_volume) = pieces.next().expect("first cube");
    let (_, second_volume) = pieces.next().expect("second cube");
    let first_grid = OccupancyGrid::from_volume(&first_volume)
        .expect("first cube occupancy")
        .expect("first cube is solid");
    let second_grid = OccupancyGrid::from_volume(&second_volume)
        .expect("second cube occupancy")
        .expect("second cube is solid");
    let first = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid: first_grid,
        cell_m: CELL_M,
        density_kg_m3: STONE_DENSITY,
        mass_properties: None,
        translation_m: [distance - physics_origin_m + 2.0, 0.25, 3.75],
        linvel_m_s: [0.0; 3],
    });
    let second = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid: second_grid,
        cell_m: CELL_M,
        density_kg_m3: STONE_DENSITY,
        mass_properties: None,
        translation_m: [distance - physics_origin_m + 3.0, 0.25, 3.75],
        linvel_m_s: [0.0; 3],
    });
    world.step();
    let mass = world.body_state(first).mass_kg;
    world.apply_impulse(first, [mass * 4.0, 0.0, 0.0]);
    let mut contact_seen = false;
    let mut target_peak_speed = 0.0_f32;
    let mut max_penetration = 0.0_f32;
    let mut finite = true;
    for _ in 0..TICKS {
        world.step();
        contact_seen |= world.contact_pair_count() > 0;
        target_peak_speed = target_peak_speed.max(world.body_state(second).speed_m_s());
        max_penetration = max_penetration.max(world.max_penetration_m());
        finite &= world
            .body_state(first)
            .translation_m
            .iter()
            .all(|v| v.is_finite());
        finite &= world
            .body_state(second)
            .translation_m
            .iter()
            .all(|v| v.is_finite());
    }
    (
        contact_seen,
        f64::from(target_peak_speed),
        f64::from(max_penetration),
        finite,
    )
}

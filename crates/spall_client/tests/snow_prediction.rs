//! ENG-252: deep snow slows walking, and the client predictor (which reads the
//! replicated snow) agrees with the authoritative [`spall_sim::Simulation`]
//! (which reads its own snow state) because both feed the one character kernel
//! the same `snow_speed_scale`.

use spall_client::predict::{ClientPhysics, PredictedPlayer};
use spall_client::replica::{ReplicaConfig, ReplicaWorld};
use spall_core::{
    BrickCoord, CellSizeCode, GlobalCell, MaterialId, PlayerInput, Revision, VolumeId,
    player_entity_for,
};
use spall_physics::character::tuning;
use spall_physics::{CharacterParams, CharacterState, snow_speed_scale};
use spall_protocol::{InputSeq, WeatherKind};
use spall_sim::{Simulation, SimulationConfig, TICK_DT_S};
use spall_voxel::{Brick, BrickBounds, EditPlan, Volume};

const SPAWN_M: [f64; 3] = [2.0, 0.52, 2.0];

/// A 16 m flat stone floor in winter with the weather forced to `kind`.
fn snowy_sim(kind: WeatherKind) -> Simulation {
    let bounds = BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(1, 0, 1)).unwrap();
    let mut terrain = Volume::bounded(VolumeId::new(1).unwrap(), CellSizeCode::Quarter, bounds);
    for z in 0..2 {
        for x in 0..2 {
            terrain
                .insert_brick(
                    BrickCoord::new(x, 0, z),
                    Brick::uniform(MaterialId::AIR, Revision(1)),
                )
                .unwrap();
        }
    }
    let mut floor = EditPlan::new(terrain.id());
    for z in 0..64 {
        for x in 0..64 {
            for y in 0..2 {
                floor.set(GlobalCell::new(x, y, z), MaterialId(1));
            }
        }
    }
    terrain.apply_edit(&floor).unwrap();
    let mut world = spall_sim::fixtures::flat_terrain_setup();
    world.terrain = terrain;
    world.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 31, 63));
    let mut sim = Simulation::new(SimulationConfig::new(world)).unwrap();
    let mut weather = sim.weather_state().clone();
    weather.start_season = 3; // winter
    sim.restore_weather(weather).unwrap();
    sim.set_weather_override(Some(kind), None);
    sim
}

fn forward() -> PlayerInput {
    PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    }
}

/// Snow depth (Q16) the server holds under the spawn.
fn spawn_depth_q16(sim: &Simulation) -> u16 {
    sim.snow_state().depth_q16_at(8, 1, 8)
}

#[test]
fn deep_snow_slows_walking_and_the_predictor_agrees_with_the_server() {
    let mut sim = snowy_sim(WeatherKind::Snow);
    let player = player_entity_for(0);
    sim.add_player(player, SPAWN_M);
    let mut ticks = 0;
    while spawn_depth_q16(&sim) < 16_384 && ticks < 40_000 {
        sim.tick().unwrap();
        ticks += 1;
    }
    let depth = spawn_depth_q16(&sim);
    assert!(
        depth >= 16_384,
        "only {depth} Q16 of snow after {ticks} ticks"
    );
    // Freeze the depth: clear weather in winter neither adds nor melts snow.
    sim.set_weather_override(Some(WeatherKind::Clear), None);
    sim.tick().unwrap();
    let depth = spawn_depth_q16(&sim);
    let expected = snow_speed_scale(depth, false);
    assert!(expected < 0.9, "depth {depth} gives scale {expected}");

    // The client's copy of the snow and weather, as replication delivers them.
    let volume = sim.world().terrain().volume.clone();
    let mut replica = ReplicaWorld::empty(ReplicaConfig::default());
    replica.apply_weather(sim.weather_state().clone()).unwrap();
    for frame in sim.snow_snapshot_frames(None) {
        replica.apply_snow_frame(frame).unwrap();
    }
    let mut phys = ClientPhysics::new();
    phys.set_terrain(&volume);
    let mut predictor = PredictedPlayer::new(
        CharacterParams::DEFAULT,
        CharacterState::at(SPAWN_M),
        sim.current_tick(),
    );

    // Let both settle on the floor, then walk.
    let mut seq = 0u64;
    let mut max_speed_error = 0.0_f32;
    let mut walked_speed = 0.0_f32;
    for tick in 0..90 {
        seq += 1;
        let inp = if tick < 20 {
            PlayerInput::NEUTRAL
        } else {
            forward()
        };
        sim.set_player_input(player, inp, InputSeq(seq));
        sim.tick().unwrap();
        let server = sim.player_state(player).unwrap();
        let scale = replica.ground_speed_scale(&volume, predictor.predicted().position_m);
        predictor.set_ground_scale(scale);
        let predicted = predictor.tick(&mut phys, &volume, inp, InputSeq(seq), TICK_DT_S);
        if tick >= 30 {
            let speed = |s: CharacterState| s.velocity_m_s[0].hypot(s.velocity_m_s[2]);
            max_speed_error = max_speed_error.max((speed(server) - speed(predicted)).abs());
            walked_speed = speed(server);
        }
    }
    let end = sim.player_state(player).unwrap();
    assert!(end.position_m[0] > SPAWN_M[0] + 1.0, "walked: {end:?}");
    assert!(
        (walked_speed - tuning::WALK_SPEED_M_S * expected).abs() < 1e-3,
        "server walks {walked_speed} m/s, expected {} (scale {expected})",
        tuning::WALK_SPEED_M_S * expected
    );
    assert!(walked_speed < tuning::WALK_SPEED_M_S - 0.3);
    assert!(
        max_speed_error < 1e-3,
        "predictor and server speeds differ by {max_speed_error} m/s"
    );
}

#[test]
fn bare_ground_keeps_full_speed() {
    let mut sim = snowy_sim(WeatherKind::Clear);
    let player = player_entity_for(0);
    sim.add_player(player, SPAWN_M);
    for tick in 0..60u64 {
        sim.set_player_input(player, forward(), InputSeq(tick + 1));
        sim.tick().unwrap();
    }
    let state = sim.player_state(player).unwrap();
    let speed = state.velocity_m_s[0].hypot(state.velocity_m_s[2]);
    assert!((speed - tuning::WALK_SPEED_M_S).abs() < 1e-3, "{speed}");
}

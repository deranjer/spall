#![cfg(feature = "client")]
//! ENG-269: how many animals the Winter Crossing profile places in a generated
//! world, and what the creature hooks cost per tick while it does. Prints a
//! table; the 1 km run is ignored because it needs several GiB.
//!
//! Run: `WINTER_SIZE=4096 cargo test --release -p sandbox --features client --test winter_crossing_animals -- --ignored --nocapture`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use sandbox::animals::AnimalDirector;
use sandbox::{winter_crossing as winter, worldgen_scene};
use spall_server::creature_service::CreatureService;
use spall_sim::{Simulation, SimulationConfig};

fn counts(sim: &Simulation) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for record in sim.creature_records() {
        *counts.entry(record.species).or_default() += 1;
    }
    counts
}

#[test]
#[ignore = "generates a world; set WINTER_SIZE (cells, default 1024)"]
fn the_winter_profile_populates_the_map() {
    let size: u32 = std::env::var("WINTER_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    let started = Instant::now();
    let scene = worldgen_scene::generate_with_season(
        "natural",
        1,
        size,
        spall_ecology::living::Season::Winter,
    )
    .expect("generate");
    eprintln!("generated {size} cells in {:?}", started.elapsed());
    let spawn_y = scene.player_spawns()[0][1];
    let mut config = SimulationConfig::new(scene.world_setup());
    config.weather_profile = spall_protocol::WeatherProfile::WinterCrossing;
    let mut sim = Simulation::new(config).unwrap();
    sim.initialize_creature_host().unwrap();
    sim.set_creature_shape(sandbox::animal_visual::creature_shape().unwrap())
        .unwrap();
    let director = AnimalDirector::new(
        winter::animal_anchors(size, spawn_y),
        &winter::ANIMAL_TARGETS,
        1,
    )
    .unwrap()
    .with_radius_m(winter::ANIMAL_RADIUS_M.0, winter::ANIMAL_RADIUS_M.1);
    let service = CreatureService::new(Box::new(director));

    let (mut births, mut births_worst) = (Duration::ZERO, Duration::ZERO);
    let (mut motion, mut motion_worst) = (Duration::ZERO, Duration::ZERO);
    let ticks = 1500u32;
    let mut settled_worst = Duration::ZERO;
    for tick in 0..ticks {
        sim.tick().unwrap();
        let t = Instant::now();
        service.run_tick(&mut sim).unwrap();
        let spawn = t.elapsed();
        births += spawn;
        births_worst = births_worst.max(spawn);
        let t = Instant::now();
        service.run_motion(&mut sim).unwrap();
        let step = t.elapsed();
        motion += step;
        motion_worst = motion_worst.max(step);
        if tick >= ticks / 2 {
            settled_worst = settled_worst.max(step);
        }
    }
    eprintln!("targets {:?}", winter::ANIMAL_TARGETS);
    eprintln!("spawned {:?}", counts(&sim));
    eprintln!(
        "creature spawn hook per tick: avg {:?}, worst {births_worst:?}; motion hook: avg {:?}, worst {motion_worst:?} over {ticks} ticks; motion worst in the second half {settled_worst:?}",
        births / ticks,
        motion / ticks,
    );
    eprintln!(
        "weather: {:?}, {} cC",
        sim.weather_state().kind,
        sim.weather_state().temperature_centi_c
    );
}

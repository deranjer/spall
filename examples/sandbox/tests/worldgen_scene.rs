//! A generated world becomes a playable server world: the sandbox material
//! palette covers everything the generator paints, water is server state in
//! one fluid region, the terrain stands up in the authoritative simulation,
//! and a player placed at a generated spawn stays on the ground.

use std::collections::BTreeSet;
use std::time::Instant;

use sandbox::game::{self, materials};
use sandbox::worldgen_scene::{self, WorldgenSceneError};
use spall_core::{GlobalCell, MaterialId, player_entity_for};
use spall_sim::{Simulation, SimulationConfig};
use spall_voxel::Sample;

const SMALL: u32 = 256;

fn palette_ids() -> Vec<MaterialId> {
    let p = game::terrain_palette();
    vec![
        p.bedrock,
        p.deep_stone,
        p.stone,
        p.slate,
        p.dirt,
        p.grass,
        p.sand,
        p.sandstone,
        p.mud,
        p.moss,
        p.gravel,
        p.clay,
        p.snow,
    ]
}

#[test]
fn every_terrain_role_is_a_distinct_solid_material_in_the_manifest() {
    let manifest = game::manifest();
    let ids = palette_ids();
    assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
    for id in ids {
        let def = manifest
            .get(id)
            .unwrap_or_else(|| panic!("{id:?} undefined"));
        assert!(
            def.sim.flags.0 & spall_core::MaterialFlags::COLLIDES.0 != 0,
            "{id:?}"
        );
        assert!(
            def.sim.flags.0 & spall_core::MaterialFlags::OPAQUE.0 != 0,
            "{id:?}"
        );
    }
    // New terrain materials sit above the lamp and below the appearance
    // extension range, as a manifest extension requires.
    for id in [
        materials::BEDROCK,
        materials::DEEP_STONE,
        materials::SAND,
        materials::MUD,
        materials::MOSS,
        materials::GRAVEL,
        materials::CLAY,
        materials::SNOW,
    ] {
        assert!(id.0 > materials::LAMP.0 && id.0 < sandbox::appearance::EXTENSION_ID_BASE);
    }
}

#[test]
fn generated_terrain_only_uses_defined_materials() {
    let scene = worldgen_scene::generate("showcase", 1, SMALL).expect("generate");
    let manifest = game::manifest();
    let terrain = &scene.world().terrain;
    let mut used = BTreeSet::new();
    for z in (0..i64::from(SMALL)).step_by(3) {
        for x in (0..i64::from(SMALL)).step_by(3) {
            for y in (0..384).step_by(2) {
                if let Ok(Sample::Filled(m)) = terrain.sample(GlobalCell::new(x, y, z)) {
                    used.insert(m);
                }
            }
        }
    }
    assert!(used.len() >= 8, "a varied world, got {used:?}");
    for m in used {
        assert!(manifest.contains(m), "{m:?} is painted but undefined");
    }
}

#[test]
fn water_is_one_server_fluid_region_inside_the_arena() {
    let scene = worldgen_scene::generate("showcase", 1, SMALL).expect("generate");
    let water = scene.water_setup().expect("showcase has water");
    assert_eq!(
        water.initial_fractions.len(),
        scene.world().water.cells.len()
    );
    let terrain = &scene.world().terrain;
    for (cell, fraction) in &water.initial_fractions {
        assert_eq!(*fraction, 1.0);
        assert!(
            matches!(terrain.sample(*cell), Ok(Sample::Empty { .. })),
            "{cell:?}"
        );
    }
    // The domain covers every water cell and stays inside the arena.
    let (lo, hi) = scene.world().region;
    let d = water.domain;
    let (o, dims) = (d.origin(), d.dimensions());
    assert!(o.x >= lo.x && o.y >= lo.y && o.z >= lo.z);
    assert!(o.x + i64::from(dims[0]) - 1 <= hi.x);
    assert!(o.y + i64::from(dims[1]) - 1 <= hi.y);
    assert!(o.z + i64::from(dims[2]) - 1 <= hi.z);
    for (cell, _) in &water.initial_fractions {
        assert!(cell.x >= o.x && cell.x < o.x + i64::from(dims[0]));
        assert!(cell.y >= o.y && cell.y < o.y + i64::from(dims[1]));
        assert!(cell.z >= o.z && cell.z < o.z + i64::from(dims[2]));
    }
    assert_eq!(water.coarsen, 2);
    assert!(
        water.sources.is_empty() && water.sinks.is_empty(),
        "static water"
    );
}

#[test]
fn bad_requests_fail_loudly() {
    assert!(matches!(
        worldgen_scene::generate("nonsense", 1, SMALL),
        Err(WorldgenSceneError::UnknownPreset(_))
    ));
    assert!(matches!(
        worldgen_scene::generate("showcase", 1, 100),
        Err(WorldgenSceneError::Generate(_))
    ));
}

#[test]
fn a_player_at_a_generated_spawn_stands_on_the_ground_with_water_running() {
    let scene = worldgen_scene::generate("showcase", 7, SMALL).expect("generate");
    let spawn = scene.player_spawns()[0];
    let mut config = SimulationConfig::new(scene.world_setup());
    config.water = scene.water_setup().cloned();
    let mut sim = Simulation::new(config).expect("the generated world stands up");
    let player = player_entity_for(0);
    sim.add_player(player, spawn);
    for _ in 0..90 {
        sim.tick().expect("tick");
    }
    let state = sim.player_state(player).expect("player exists");
    assert!(
        state.grounded,
        "player should rest on the terrain: {state:?}"
    );
    assert!(
        (state.position_m[1] - spawn[1]).abs() < 0.5,
        "player fell or floated: spawn {spawn:?} now {:?}",
        state.position_m
    );
    assert!(sim.water().is_some(), "server owns the fluid region");
}

/// Measures the cost of standing up the full 256 m arena. Not a pass/fail
/// gate: `cargo test -p sandbox --release --test worldgen_scene -- --ignored --nocapture measure`.
#[test]
#[ignore]
fn measure_full_arena_stand_up() {
    let t = Instant::now();
    let scene = worldgen_scene::generate("showcase", 1, worldgen_scene::DEFAULT_SIZE_CELLS)
        .expect("generate");
    println!("generate + water plan: {:?}", t.elapsed());
    println!("{:?}", scene.world().terrain.memory_report());
    let spawn = scene.player_spawns()[0];
    let mut config = SimulationConfig::new(scene.world_setup());
    config.water = scene.water_setup().cloned();
    let t = Instant::now();
    let mut sim = Simulation::new(config).expect("stands up");
    println!("Simulation::new (colliders + water): {:?}", t.elapsed());
    let player = player_entity_for(0);
    sim.add_player(player, spawn);
    let t = Instant::now();
    let mut worst = std::time::Duration::ZERO;
    for _ in 0..120 {
        let s = Instant::now();
        sim.tick().expect("tick");
        worst = worst.max(s.elapsed());
    }
    println!("120 ticks: {:?}, worst {:?}", t.elapsed(), worst);
    println!("player {:?}", sim.player_state(player));
}

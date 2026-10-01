#![cfg(feature = "client")]
//! Client-side cost of drawing a generated world: how many instances the debug
//! terrain renderer emits around the camera and how long a rebuild takes.
//!
//! Run: `cargo test -p sandbox --features client --release --test worldgen_render_cost -- --ignored --nocapture`.

use std::time::Instant;

use sandbox::worldgen_scene;
use spall_client::window::build_instances;

/// Camera positions in metres (flying views from the ENG-114 tour, plus the
/// spawn): `(name, [x, y, z])`.
const VIEWS: [(&str, [f64; 3]); 5] = [
    ("spawn", [128.1, 41.5, 128.1]),
    ("overview", [200.0, 110.0, 235.0]),
    ("swamp", [60.0, 50.0, 236.0]),
    ("desert", [200.0, 58.0, 200.0]),
    ("mountains", [128.0, 70.0, 60.0]),
];

#[test]
#[ignore]
fn measure_terrain_rebuild_on_the_showcase() {
    let scene = worldgen_scene::generate("showcase", 1, worldgen_scene::DEFAULT_SIZE_CELLS)
        .expect("generate");
    let volume = &scene.world().terrain;
    for (name, center) in VIEWS {
        let start = Instant::now();
        let instances = build_instances(volume, center);
        println!(
            "{name:10} build_instances {:>8.1} ms, {:>8} instances",
            start.elapsed().as_secs_f64() * 1e3,
            instances.len()
        );
        // The same window rebuilt after a hammer-sized cut, reusing the cache.
        let mut cache = spall_client::window::TerrainInstanceCache::new();
        let _ = spall_client::window::build_instances_cached(volume, center, &mut cache);
        let mut edited = volume.clone();
        let cell = spall_core::GlobalCell::new(
            (center[0] / 0.25) as i64,
            (center[1] / 0.25) as i64 - 8,
            (center[2] / 0.25) as i64,
        );
        let mut plan = spall_voxel::EditPlan::new(edited.id());
        for d in -2..=2 {
            plan.set(
                spall_core::GlobalCell::new(cell.x + d, cell.y, cell.z),
                spall_core::MaterialId::AIR,
            );
        }
        edited.apply_edit(&plan).unwrap();
        let start = Instant::now();
        let again = spall_client::window::build_instances_cached(&edited, center, &mut cache);
        println!(
            "{name:10} cached rebuild after an edit {:>6.1} ms, {:>8} instances",
            start.elapsed().as_secs_f64() * 1e3,
            again.len()
        );
        let start = Instant::now();
        let _ = spall_client::sky::build_sky_occupancy(volume, center, true);
        println!(
            "{name:10} build_sky_occupancy {:>6.1} ms",
            start.elapsed().as_secs_f64() * 1e3
        );
    }
}

/// The client's collision window for a large world (rebuilt when the player nears
/// its edge and on terrain changes) and the lighting rebuild, timed on the real
/// generated terrain: these run on the network/mover and rebuild threads.
#[test]
#[ignore]
fn measure_client_physics_window_and_sky() {
    let scene = worldgen_scene::generate("showcase", 1, worldgen_scene::DEFAULT_SIZE_CELLS)
        .expect("generate");
    let volume = &scene.world().terrain;
    for (name, feet) in [
        ("spawn", [128.1, 41.5, 128.1]),
        ("mountains", [128.0, 70.0, 60.0]),
    ] {
        let mut phys = spall_client::predict::ClientPhysics::new();
        phys.set_focus(feet);
        let start = Instant::now();
        phys.set_terrain(volume);
        println!(
            "{name:10} ClientPhysics::set_terrain (window around the player) {:>7.1} ms",
            start.elapsed().as_secs_f64() * 1e3
        );
        let start = Instant::now();
        let (_, stats) = spall_client::sky::build_sky_occupancy(volume, feet, true);
        println!(
            "{name:10} build_sky_occupancy {:>7.1} ms ({stats:?})",
            start.elapsed().as_secs_f64() * 1e3
        );
    }
}

/// Walking: the cached terrain rebuild as the centre moves one metre at a time
/// across the world (bricks enter and leave the window), timed per step.
#[test]
#[ignore]
fn measure_rebuild_while_walking() {
    let scene = worldgen_scene::generate("showcase", 1, worldgen_scene::DEFAULT_SIZE_CELLS)
        .expect("generate");
    let volume = &scene.world().terrain;
    let mut cache = spall_client::window::TerrainInstanceCache::new();
    let start = [60.0, 41.5, 128.0];
    let _ = spall_client::window::build_instances_cached(volume, start, &mut cache);
    let mut times = Vec::new();
    for step in 1..=120 {
        let center = [start[0] + f64::from(step), start[1], start[2]];
        let t = Instant::now();
        let _ = spall_client::window::build_instances_cached(volume, center, &mut cache);
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let max = times.iter().copied().fold(0.0, f64::max);
    let over_16 = times.iter().filter(|t| **t > 16.0).count();
    println!(
        "walking 120 m, 1 m steps: mean {mean:.1} ms, max {max:.1} ms, {over_16} steps over 16 ms"
    );
}

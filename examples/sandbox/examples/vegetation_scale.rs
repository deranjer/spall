//! Counts and costs of the living vegetation of a generated world: organisms
//! per biome, saved state bytes, the per-second keyframe bytes, and how long
//! generation, the keyframe build and a fair-work tick take.
//!
//! `cargo run --release -p sandbox --example vegetation_scale -- [preset] [size] [seed]`

use std::collections::BTreeMap;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `RUST_LOG=info` shows the startup profile (terrain, vegetation, water setup).
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mut args = std::env::args().skip(1);
    let preset = args.next().unwrap_or_else(|| "natural".into());
    let size: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2048);
    let seed: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let started = Instant::now();
    let scene = sandbox::worldgen_scene::generate(&preset, seed, size)?;
    println!(
        "{preset} seed {seed} size {size}: generated in {:.1} s (terrain and vegetation)",
        started.elapsed().as_secs_f64()
    );
    let world = scene.world();
    let state = scene.vegetation().clone();
    let columns = &world.columns;
    let mut per_biome: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for p in &state.organisms {
        let tree = !p.wood.is_empty();
        let entry = per_biome
            .entry(format!("{:?}", columns.biome(p.root[0], p.root[2])))
            .or_default();
        if tree {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    }
    let land: usize = (0..i64::from(size))
        .step_by(8)
        .flat_map(|z| (0..i64::from(size)).step_by(8).map(move |x| (x, z)))
        .filter(|&(x, z)| !columns.is_water(x, z))
        .count()
        * 64;
    let km2 = land as f64 * 0.0625 / 1.0e6;
    println!("land about {km2:.2} km2");
    println!("{:<18} {:>8} {:>8}", "biome", "trees", "ground");
    let (mut trees, mut ground) = (0, 0);
    for (name, (t, g)) in &per_biome {
        println!("{name:<18} {t:>8} {g:>8}");
        trees += t;
        ground += g;
    }
    println!("{:<18} {trees:>8} {ground:>8}", "total");
    let wood_cells: usize = state.organisms.iter().map(|p| p.wood.len()).sum();
    let encoded = state.encode()?;
    println!(
        "wood cells {wood_cells}; saved state {:.2} MiB (limit {:.0} MiB)",
        encoded.len() as f64 / 1_048_576.0,
        living_limit()
    );
    // Keyframe build: the server does this once a second on a worker.
    let terrain = &world.terrain;
    let started = Instant::now();
    let frame = state.visual(terrain);
    let build = started.elapsed();
    let bytes = frame.encode()?;
    println!(
        "keyframe: {} plants, {:.2} MiB encoded, built in {:.1} ms (wire limit 4 MiB)",
        frame.plants.len(),
        bytes.len() as f64 / 1_048_576.0,
        build.as_secs_f64() * 1000.0
    );
    let part = |name: &str, bytes: usize| {
        println!("  keyframe part {name}: {:.1} KiB", bytes as f64 / 1024.0)
    };
    part("plants", postcard::to_stdvec(&frame.plants)?.len());
    part("species", postcard::to_stdvec(&frame.species)?.len());
    part("cover species", postcard::to_stdvec(&frame.cover)?.len());
    part("climate raster", postcard::to_stdvec(&frame.field)?.len());
    part("cleared", postcard::to_stdvec(&frame.cleared)?.len());
    // What a regional client is sent instead: the plants around its player.
    let spawn = scene.player_spawns().first().copied().unwrap_or([0.0; 3]);
    let centre = [(spawn[0] / 0.25) as i64, (spawn[2] / 0.25) as i64];
    for bricks in [4_i64, 8, 16] {
        let window = spall_ecology::living::Window::around(centre[0], centre[1], (bricks + 2) * 32);
        let part = frame.windowed(window, false);
        let with_field = frame.windowed(window, true);
        println!(
            "regional window {bricks} bricks at the first spawn: {} plants, {:.1} KiB per keyframe ({:.1} KiB with the raster)",
            part.plants.len(),
            part.encode()?.len() as f64 / 1024.0,
            with_field.encode()?.len() as f64 / 1024.0
        );
    }
    // What the server does on its tick thread each second.
    let started = Instant::now();
    let copy = state.clone();
    println!(
        "state clone (tick thread, per keyframe): {:.2} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    drop(copy);
    let started = Instant::now();
    let _ = state.encode()?;
    println!(
        "state encode incl. validation (per dirty checkpoint): {:.2} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    // One fair-work tick per second of ecological time, over a minute.
    let mut live = state.clone();
    let mut worst = 0.0f64;
    let mut total = 0.0f64;
    for _ in 0..60 {
        let started = Instant::now();
        live.advance(columns, terrain, 1000);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        worst = worst.max(ms);
        total += ms;
    }
    println!(
        "advance: mean {:.2} ms, worst {:.2} ms per second of ecological time",
        total / 60.0,
        worst
    );
    Ok(())
}

fn living_limit() -> f64 {
    spall_ecology::living::MAX_STATE_BYTES as f64 / 1_048_576.0
}

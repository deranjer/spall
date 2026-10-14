//! Prints where a generated world's water is, to aim a screenshot camera.
//! `cargo run --release -p sandbox --example water_where -- SIZE SEED`
use sandbox::worldgen_scene;
use std::collections::HashMap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let size: u32 = a.first().map_or(Ok(1024), |s| s.parse())?;
    let seed: u64 = a.get(1).map_or(Ok(7), |s| s.parse())?;
    let scene = worldgen_scene::generate("showcase", seed, size)?;
    let plan = &scene.world().water;
    println!("surface_y={} cells={}", plan.surface_y, plan.cell_count());
    println!("bounds={:?}", plan.bounds);
    println!("spawns={:?}", scene.player_spawns());
    // Deepest column and the centroid of the water, in metres.
    let mut depth: HashMap<(i64, i64), i64> = HashMap::new();
    for c in plan.cells() {
        *depth.entry((c.x, c.z)).or_default() += 1;
    }
    let (n, sx, sz) = depth.keys().fold((0.0, 0.0, 0.0), |(n, sx, sz), (x, z)| {
        (n + 1.0, sx + *x as f64, sz + *z as f64)
    });
    let (deep, d) = depth.iter().max_by_key(|(_, d)| **d).unwrap();
    println!(
        "wet columns {n}, centroid m ({:.1}, {:.1}), deepest column cell {deep:?} = {d} cells ({:.1} m)",
        sx / n * 0.25,
        sz / n * 0.25,
        *d as f64 * 0.25
    );
    // A shore column: wet with a dry 4-neighbour.
    let shore = depth
        .keys()
        .find(|(x, z)| !depth.contains_key(&(x + 1, *z)) && depth.contains_key(&(x - 1, *z)))
        .unwrap();
    println!(
        "a shore column (cells) {shore:?} = ({:.1} m, {:.1} m)",
        shore.0 as f64 * 0.25,
        shore.1 as f64 * 0.25
    );
    Ok(())
}

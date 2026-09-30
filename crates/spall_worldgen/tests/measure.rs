//! Manual measurement of a generated world (asserts nothing): timing, brick
//! memory, water domain and cave share. Run with
//! `SIZE=1024 cargo test -p spall_worldgen --release --test measure -- --ignored --nocapture`.
use spall_core::GlobalCell;
use spall_voxel::volume::Sample;
use spall_worldgen::*;
use std::time::Instant;

#[test]
#[ignore]
fn measure_showcase() {
    let size: u32 = std::env::var("SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    let spec = WorldGenSpec::new(Preset::Showcase, 1, size, WorldgenPalette::sequential(1));
    let t = Instant::now();
    let cols = ColumnMap::compute(&spec).unwrap();
    println!("columns {:?}", t.elapsed());
    println!("{:#?}", debug::column_stats(&cols));
    let t = Instant::now();
    let w = generate(&spec).unwrap();
    println!("generate {:?}", t.elapsed());
    let m = w.terrain.memory_report();
    println!("{m:?} bricks {}", w.terrain.resident_brick_count());
    println!(
        "water cells {} bounds {:?} domain {}",
        w.water.cells.len(),
        w.water.bounds,
        w.water.domain_cells()
    );
    println!("mouths {:?} spawns {:?}", w.cave_mouths, w.spawns);
    for (x, z) in &w.cave_mouths {
        println!(
            "mouth metres x={} z={} surface_y={}",
            *x as f64 * 0.25,
            *z as f64 * 0.25,
            w.columns.height(*x, *z) as f64 * 0.25
        );
    }
    // cave fraction among rock cells below the surface and above floor.
    let (mut solid, mut caves) = (0u64, 0u64);
    for z in (0..size as i64).step_by(3) {
        for x in (0..size as i64).step_by(3) {
            let h = w.columns.height(x, z) as i64;
            for y in 40..h - 6 {
                match w.terrain.sample(GlobalCell::new(x, y, z)).unwrap() {
                    Sample::Filled(_) => solid += 1,
                    Sample::Empty { .. } => caves += 1,
                    s => panic!("{s:?}"),
                }
            }
        }
    }
    println!(
        "cave fraction of deep rock {:.3}",
        caves as f64 / (solid + caves) as f64
    );
}

/// Writes the top-down preview as raw RGBA to `$PREVIEW` (size from `$SIZE`).
#[test]
#[ignore]
fn dump_preview() {
    let size: u32 = std::env::var("SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024);
    let path = std::env::var("PREVIEW").expect("PREVIEW path");
    let spec = WorldGenSpec::new(Preset::Showcase, 1, size, WorldgenPalette::sequential(1));
    let cols = ColumnMap::compute(&spec).unwrap();
    std::fs::write(path, debug::top_down_rgba(&cols)).unwrap();
}

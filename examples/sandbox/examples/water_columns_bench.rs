//! ENG-166 (W2) headless bench of the column water solver on a generated world.
//! `cargo run --release -p sandbox --example water_columns_bench -- SIZE SEED [BOX]`
//! Reports the resting-world step cost, a large release ("dam break" proxy: a
//! BOX x BOX column slab of water dropped on the real terrain), and memory.
use std::sync::Arc;
use std::time::Instant;

use sandbox::worldgen_scene::{self, GeneratedScene};
use spall_fluid::columns::{ColumnConfig, ColumnWorld, VolumeTerrain};

struct Terrain(Arc<GeneratedScene>);

impl std::ops::Deref for Terrain {
    type Target = spall_voxel::Volume;
    fn deref(&self) -> &Self::Target {
        &self.0.world().terrain
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let size: u32 = a.first().map_or(Ok(1024), |s| s.parse())?;
    let seed: u64 = a.get(1).map_or(Ok(1), |s| s.parse())?;
    let slab: i64 = a.get(2).map_or(Ok(316), |s| s.parse())?;
    println!(
        "cpu={:?} logical_threads={} size={size} seed={seed}",
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default(),
        std::thread::available_parallelism()?
    );
    let t = Instant::now();
    let scene = Arc::new(worldgen_scene::generate("showcase", seed, size)?);
    println!("worldgen {:.2}s", t.elapsed().as_secs_f64());
    let (lo, hi) = scene.world().region;
    let y_lo = (lo.y.div_euclid(32) * 32) as i32;
    let y_hi = ((hi.y + 1 + 31).div_euclid(32) * 32) as i32;
    let plan = &scene.world().water;
    println!(
        "y range {y_lo}..{y_hi}, water surface y={}, plan cells {}",
        plan.surface_y,
        plan.cells.len()
    );

    let mut world = ColumnWorld::with_terrain(
        Arc::new(VolumeTerrain::new(Terrain(scene.clone()), y_lo, y_hi)),
        ColumnConfig {
            cfl: std::env::var("CFL").ok().and_then(|v| v.parse().ok()).unwrap_or(0.5),
            ..ColumnConfig::default()
        },
    );
    let t = Instant::now();
    let rejected = world.seed_cells(&plan.cells);
    println!(
        "seed {:.2}s: regions {} rejected {} volume {:.1} m3",
        t.elapsed().as_secs_f64(),
        world.region_count(),
        rejected.len(),
        world.total_volume_m3()
    );
    let mem = world.memory();
    println!(
        "memory: {} regions, {} columns, {} spans, {} pipes, {:.1} MiB, {:.1} B/column",
        mem.regions,
        mem.columns,
        mem.spans,
        mem.pipes,
        mem.bytes as f64 / 1048576.0,
        mem.bytes as f64 / mem.columns.max(1) as f64
    );

    // Resting world.
    let mut ms = Vec::new();
    for _ in 0..200 {
        let t = Instant::now();
        let m = world.step(0.05);
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(m.active_regions, 0, "seeded water must start at rest");
    }
    ms.sort_by(|a, b| a.total_cmp(b));
    println!(
        "RESTING world: step ms p50 {:.4} p95 {:.4} max {:.4} (target < 0.1)",
        pct(&ms, 0.5),
        pct(&ms, 0.95),
        ms[ms.len() - 1]
    );

    // Release: a slab of water dropped over the middle of the world.
    let c = i64::from(size) / 2;
    let level = (plan.surface_y + 20) as f64;
    let (x0, z0) = (c - slab / 2, c - slab / 2);
    let t = Instant::now();
    world.allocate_rect(x0, z0, x0 + slab, z0 + slab);
    for z in z0..z0 + slab {
        for x in x0..x0 + slab {
            world.fill_column_to(x, z, level);
        }
    }
    println!(
        "release setup {:.2}s: {} columns, regions {}",
        t.elapsed().as_secs_f64(),
        slab * slab,
        world.region_count()
    );
    let start = world.total_volume();
    let mut ms = Vec::new();
    let mut last = Default::default();
    let mut max_active_spans = 0;
    for step in 0..400 {
        let t = Instant::now();
        let m = world.step(0.05);
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        max_active_spans = max_active_spans.max(m.active_spans);
        if step % 50 == 0 {
            println!(
                "  t={:5.1}s step {:7.3} ms regions {:4} spans {:7} pipes {:7} substeps {:2} peak {:5.2} m/s",
                f64::from(step) * 0.05,
                ms[ms.len() - 1],
                m.active_regions,
                m.active_spans,
                m.active_pipes,
                m.substeps,
                m.peak_speed_m_s
            );
        }
        last = m;
    }
    let mut sorted = ms.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    println!(
        "RELEASE ({} columns seeded, max active spans {max_active_spans}): step ms mean {mean:.3} p50 {:.3} p95 {:.3} max {:.3} (target < 5 per 50 ms step)",
        slab * slab,
        pct(&sorted, 0.5),
        pct(&sorted, 0.95),
        sorted[sorted.len() - 1]
    );
    println!(
        "end: regions {} active {} skipped_s {:.3} volume drift {:.3e} relative; memory {:.1} MiB",
        world.region_count(),
        last.active_regions,
        last.skipped_s,
        (world.total_volume() - start).abs() / start,
        world.memory().bytes as f64 / 1048576.0
    );
    Ok(())
}

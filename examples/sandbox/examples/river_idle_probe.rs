//! How a generated world's rivers cost the server while nothing is edited: runs
//! the column water for a while and reports, every 10 s of fluid time, the
//! active and steady regions, the fluid step time, and the spring and drain
//! mass balance.
//! `cargo run --release -p sandbox --example river_idle_probe -- PRESET SIZE SEED SECONDS`
//! Environment: `STEADY=0` turns steady-flow sleep off (the always-active
//! upper bound), `WORKER=1` steps the water on its own thread as the server
//! does.
use std::time::Instant;

use sandbox::worldgen_scene;
use spall_sim::{Simulation, SimulationConfig};

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let preset = a.first().map_or("natural", String::as_str);
    let size: u32 = a.get(1).map_or(Ok(1024), |s| s.parse())?;
    let seed: u64 = a.get(2).map_or(Ok(1), |s| s.parse())?;
    let seconds: u32 = a.get(3).map_or(Ok(120), |s| s.parse())?;
    let scene = worldgen_scene::generate(preset, seed, size)?;
    let mut setup = scene.column_water_setup().ok_or("dry world")?.clone();
    // STEADY=1 turns steady-flow sleep on (it is off by default), STEADY=0 off.
    if let Ok(v) = std::env::var("STEADY") {
        setup.config.steady_steps = if v == "0" { 0 } else { 40 };
    }
    if std::env::var("NOSPRINGS").is_ok() {
        setup.sources.clear();
        setup.gated_sources[0].clear();
        setup.discharges.clear();
    }
    // LIVE=1 turns the (gated) springs on, as `--live-rivers` does.
    if std::env::var("LIVE").is_ok() {
        setup.gated_rate = 1;
    }
    if let Some(flow) = std::env::var("SLEEPFLOW").ok().and_then(|v| v.parse().ok()) {
        setup.config.sleep_flow = flow;
    }
    if let Some(seconds) = std::env::var("PREWARM").ok().and_then(|v| v.parse().ok()) {
        setup.prewarm_s = seconds;
    }
    println!(
        "{preset} seed {seed} size {size}: {} wet columns, {} springs, {} sinks",
        setup.initial_levels.len(),
        setup.discharges.len() + setup.sources.len(),
        setup.sinks.len()
    );
    let planned: std::collections::HashMap<(i64, i64), (i64, i64)> = scene
        .world()
        .water
        .columns
        .iter()
        .map(|c| {
            (
                (i64::from(c.x), i64::from(c.z)),
                (i64::from(c.bed), i64::from(c.top)),
            )
        })
        .collect();
    let mut config = SimulationConfig::new(scene.world_setup());
    config.column_water = Some(setup);
    config.column_water_worker = std::env::var("WORKER").is_ok();
    let started = Instant::now();
    let mut sim = Simulation::new(config)?;
    println!(
        "stand-up {:.2} s, prewarm {:?}",
        started.elapsed().as_secs_f64(),
        sim.column_water().and_then(|w| w.prewarm_report())
    );

    let mut window: Vec<f64> = Vec::new();
    let mut step_ms: Vec<f64> = Vec::new();
    let ticks_per_s = 60;
    for tick in 0..seconds * ticks_per_s {
        let t = Instant::now();
        let r = sim.tick()?;
        window.push(t.elapsed().as_secs_f64() * 1e3);
        let m = r.column_water.expect("column water runs");
        if m.stepped {
            step_ms.push(m.step_duration.as_secs_f64() * 1e3 / f64::from(m.steps.max(1)));
        }
        if tick % (10 * ticks_per_s) == 10 * ticks_per_s - 1 {
            let frame = sim.column_water().expect("water").frame();
            println!(
                "t={:>4}s regions {:>5} active {:>4} steady {:>5} | step p50 {:>6.2} p95 {:>6.2} ms | tick p50 {:>5.2} p99 {:>6.2} ms | \
                 volume {:>10.1} m3 springs +{:>8.2} m3 sinks -{:>8.2} m3 skipped {:.2}s",
                (tick + 1) / ticks_per_s,
                m.allocated_regions,
                m.active_regions,
                m.steady_regions,
                pct(&mut step_ms.clone(), 0.5),
                pct(&mut step_ms.clone(), 0.95),
                pct(&mut window.clone(), 0.5),
                pct(&mut window.clone(), 0.99),
                frame.volume_m3,
                frame.spring_added_m3,
                frame.drain_removed_m3,
                m.skipped_s
            );
            // Where the water is, against the plan: flooding is water standing
            // deeper than a cell on columns the plan left dry.
            if std::env::var("WHERE").is_ok() {
                let water = sim.column_water().expect("water");
                let world = water.world();
                let (mut wet, mut outside, mut deep_outside, mut vol_outside) =
                    (0u64, 0u64, 0u64, 0.0f64);
                let (mut off_channel, mut off_channel_deep, mut off_channel_vol) =
                    (0u64, 0u64, 0.0f64);
                for key in world.region_keys() {
                    for (col, floor_y, volume) in world.region_wet_spans(key, 0.05) {
                        let (lx, lz) = (i64::from(col % 32), i64::from(col / 32));
                        let (x, z) = (i64::from(key.0) * 32 + lx, i64::from(key.1) * 32 + lz);
                        let _ = floor_y;
                        wet += 1;
                        if !planned.contains_key(&(x, z)) {
                            outside += 1;
                            vol_outside += volume;
                            if volume > 1.0 {
                                deep_outside += 1;
                            }
                            // Flooding: water standing off the river channels and lake shores.
                            if scene.world().columns.river_dist(x, z) > 12 {
                                off_channel += 1;
                                if volume > 1.0 {
                                    off_channel_deep += 1;
                                    off_channel_vol += volume;
                                }
                            }
                        }
                    }
                }
                println!(
                    "      wet columns {wet}, outside the plan {outside} ({deep_outside} deeper than a cell), {:.1} m3 on them;                      OFF-CHANNEL (over 3 m from any channel or lake): {off_channel} wet, {off_channel_deep} deeper than a cell, {:.1} m3",
                    vol_outside * 0.25f64.powi(3),
                    off_channel_vol * 0.25f64.powi(3)
                );
                if std::env::var("WHERE").is_ok_and(|v| v == "2") {
                    // How far from planned water the flooded columns are, and how deep.
                    let mut by_distance = [0u64; 6];
                    let mut by_depth = [0u64; 5];
                    let mut touching_depth = [0u64; 5];
                    let mut cross_section_done = false;
                    let mut samples: Vec<(f64, i64, i64, i64)> = Vec::new();
                    for key in world.region_keys() {
                        for (col, _floor_y, volume) in world.region_wet_spans(key, 0.05) {
                            let (lx, lz) = (i64::from(col % 32), i64::from(col / 32));
                            let (x, z) = (i64::from(key.0) * 32 + lx, i64::from(key.1) * 32 + lz);
                            if planned.contains_key(&(x, z)) {
                                continue;
                            }
                            let mut nearest = i64::MAX;
                            'search: for r in 1..=24i64 {
                                for d in -r..=r {
                                    for (dx, dz) in [(d, -r), (d, r), (-r, d), (r, d)] {
                                        if planned.contains_key(&(x + dx, z + dz)) {
                                            nearest = r;
                                            break 'search;
                                        }
                                    }
                                }
                            }
                            if nearest <= 2 && volume >= 4.0 && !cross_section_done {
                                // One cross-section: ground height, planned water depth, actual water.
                                let cols = &scene.world().columns;
                                let actual = |x: i64, z: i64| -> f64 {
                                    let key = (x.div_euclid(32) as i32, z.div_euclid(32) as i32);
                                    let col = (x.rem_euclid(32) + 32 * z.rem_euclid(32)) as u16;
                                    world
                                        .region_wet_spans(key, 0.0)
                                        .iter()
                                        .filter(|(c, _, _)| *c == col)
                                        .map(|(_, _, v)| *v)
                                        .sum()
                                };
                                println!(
                                    "      cross-section along x at z={z}, x from {} to {} (ground y / planned depth / actual depth):",
                                    x - 10,
                                    x + 10
                                );
                                for xx in x - 10..=x + 10 {
                                    let planned_depth =
                                        planned.get(&(xx, z)).map_or(0, |&(b, t)| t - b);
                                    println!(
                                        "        x={xx}: ground {} planned {planned_depth} actual {:.2}",
                                        cols.height(xx, z),
                                        actual(xx, z)
                                    );
                                }
                                cross_section_done = true;
                            }
                            if nearest <= 2 && volume >= 4.0 {
                                // The planned water this one touches: shallow is a river, deep a lake.
                                let mut touching = 0i64;
                                for (dx, dz) in [
                                    (1, 0),
                                    (-1, 0),
                                    (0, 1),
                                    (0, -1),
                                    (2, 0),
                                    (-2, 0),
                                    (0, 2),
                                    (0, -2),
                                ] {
                                    if let Some(&(bed, top)) = planned.get(&(x + dx, z + dz)) {
                                        touching = touching.max(top - bed);
                                    }
                                }
                                let b = match touching {
                                    0 => 0,
                                    1..=3 => 1,
                                    4..=8 => 2,
                                    9..=20 => 3,
                                    _ => 4,
                                };
                                touching_depth[b] += 1;
                            }
                            let bucket = match nearest {
                                1..=2 => 0,
                                3..=5 => 1,
                                6..=10 => 2,
                                11..=20 => 3,
                                21..=24 => 4,
                                _ => 5,
                            };
                            by_distance[bucket] += 1;
                            let depth_bucket = match volume {
                                v if v < 0.5 => 0,
                                v if v < 1.0 => 1,
                                v if v < 2.0 => 2,
                                v if v < 4.0 => 3,
                                _ => 4,
                            };
                            by_depth[depth_bucket] += 1;
                            if nearest > 10 && samples.len() < 6 {
                                samples.push((volume, x, z, nearest));
                            }
                        }
                    }
                    println!(
                        "      deep fringe columns by depth of the planned water they touch (none, <=3, 4-8, 9-20, >20 cells): {touching_depth:?}"
                    );
                    println!(
                        "      flooded columns by distance to planned water (<=2, 3-5, 6-10, 11-20, 21-24, >24 cells): {by_distance:?}"
                    );
                    println!(
                        "      by depth (<0.5, <1, <2, <4, >=4 cells): {by_depth:?}; far samples (depth, x, z, dist): {samples:?}"
                    );
                }
            }
            window.clear();
            step_ms.clear();
        }
    }
    Ok(())
}

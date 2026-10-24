//! Headless long run of the server's column water on a generated world, with
//! one player standing still (or flying a loop, `FLY=speed_m_s`): how many
//! regions stay awake, how long a step takes, and whether water stands
//! unsupported anywhere. `NOSPRINGS=1` drops springs and drains, `NOSINKS=1`
//! drains only, `SINKS=1` lists the drains, `SETTLE=max_seconds` first runs the
//! water with no player until its regions stop changing, and
//! `SPALL_WATER_COARSE=1` runs it at 1 m columns. `SETTLED_CACHE=<dir>` starts
//! from the server's settled-water cache in that folder.
//! `cargo run --release -p sandbox --example water_long_probe -- PRESET SEED SIZE SECONDS X_M Z_M`
use std::time::Instant;

use sandbox::worldgen_scene;
use spall_fluid::columns::{ColumnWorld, region_of};
use spall_sim::column_water::AuthoritativeColumnWater;

/// Wet spans standing over a metre above an open neighbouring span they
/// overlap: nothing holds that water back.
fn unsupported_walls(world: &ColumnWorld) -> Vec<(f64, i64, i64, i64, i64)> {
    let mut walls = Vec::new();
    for key in world.region_keys() {
        for c in 0..1024i64 {
            let (x, z) = (
                i64::from(key.0) * 32 + c % 32,
                i64::from(key.1) * 32 + c / 32,
            );
            for a in world.spans_in_column(x, z) {
                if a.depth < 1.0 || a.overfill > 0.0 {
                    continue;
                }
                for (nx, nz) in [(x + 1, z), (x - 1, z), (x, z + 1), (x, z - 1)] {
                    for b in world.spans_in_column(nx, nz) {
                        let lo = a.floor_y.max(b.floor_y) as f64;
                        let hi = a.ceiling_y.min(b.ceiling_y) as f64;
                        let b_surface = b.floor_y as f64 + b.depth;
                        if hi > lo && a.head_y > lo + 1.0 && a.head_y - b_surface.max(lo) > 4.0 {
                            walls.push((a.head_y - b_surface.max(lo), x, z, nx, nz));
                        }
                    }
                }
            }
        }
    }
    walls.sort_by(|a, b| b.0.total_cmp(&a.0));
    walls
}

/// Per (region, neighbour region): walls, the worst drop, and where it is.
type WallsByRegion = std::collections::BTreeMap<((i32, i32), (i32, i32)), (usize, f64, (i64, i64))>;

/// Walls that were not there at the start, grouped by the region they stand in.
fn report_walls(world: &ColumnWorld, seeded: &std::collections::HashSet<(i64, i64, i64, i64)>) {
    let walls = unsupported_walls(world);
    let fresh: Vec<_> = walls
        .iter()
        .filter(|w| !seeded.contains(&(w.1, w.2, w.3, w.4)))
        .collect();
    eprintln!(
        "unsupported walls over 1 m: {} ({} new)",
        walls.len(),
        fresh.len()
    );
    let mut by = WallsByRegion::new();
    for &&(drop, x, z, nx, nz) in &fresh {
        let e = by
            .entry((region_of(x, z), region_of(nx, nz)))
            .or_insert((0, 0.0, (x, z)));
        e.0 += 1;
        if drop > e.1 {
            e.1 = drop;
            e.2 = (x, z);
        }
    }
    let mut rows: Vec<_> = by.into_iter().collect();
    rows.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    for ((ka, kb), (n, drop, at)) in rows.iter().take(16) {
        eprintln!(
            "  {ka:?} active {} owed {:.0}s -> {kb:?} active {} owed {:.0}s | {n} walls, worst {:.2} m at {at:?}",
            world.is_region_active(*ka),
            world.region_owed_s(*ka),
            world.is_region_active(*kb),
            world.region_owed_s(*kb),
            drop * 0.25,
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let preset = a.first().map_or("natural", String::as_str).to_string();
    let seed: u64 = a.get(1).map_or(Ok(1), |s| s.parse())?;
    let size: u32 = a.get(2).map_or(Ok(4096), |s| s.parse())?;
    let seconds: f64 = a.get(3).map_or(Ok(600.0), |s| s.parse())?;
    let x_m: f64 = a.get(4).map_or(Ok(374.3), |s| s.parse())?;
    let z_m: f64 = a.get(5).map_or(Ok(388.3), |s| s.parse())?;
    let fly: f64 = std::env::var("FLY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let t = Instant::now();
    let mut scene = worldgen_scene::generate(&preset, seed, size)?;
    // `SETTLED_CACHE=<dir>`: start from the settled water the server caches
    // there (settling and caching it first if missing), as play does.
    if let Ok(dir) = std::env::var("SETTLED_CACHE") {
        scene = scene.settle_water(
            std::path::Path::new(&dir),
            &format!("{preset}-{seed}-{size}-summer"),
        );
    }
    eprintln!("worldgen {:.1}s", t.elapsed().as_secs_f64());
    let mut setup = scene.column_water_setup().cloned().ok_or("no water")?;
    // `NOSPRINGS=1`: no spring discharges or drains, only the seeded water.
    if std::env::var_os("NOSPRINGS").is_some() {
        setup.discharges.clear();
        setup.sinks.clear();
    }
    if std::env::var_os("NOSINKS").is_some() {
        setup.sinks.clear();
    }
    if std::env::var_os("SINKS").is_some() {
        let mut by: std::collections::BTreeMap<(i32, i32), usize> = Default::default();
        for c in &setup.sinks {
            *by.entry(region_of(c.x, c.z)).or_default() += 1;
        }
        eprintln!("{} sinks in regions {by:?}", setup.sinks.len());
    }
    let mut water = AuthoritativeColumnWater::new(&scene.world().terrain, setup)?;
    water.set_focus(&[[x_m, z_m]]);
    {
        // Wet columns open to the sky, above the sea and at it: what evaporation
        // would act on.
        let sea = scene.world().water.surface_y as f64 + 2.0;
        let world = water.world();
        let (mut inland, mut sea_cols) = (0usize, 0usize);
        for key in world.region_keys() {
            for c in 0..1024i64 {
                let (x, z) = (
                    i64::from(key.0) * 32 + c % 32,
                    i64::from(key.1) * 32 + c / 32,
                );
                for s in world.spans_in_column(x, z) {
                    if s.sky && s.volume > 1e-6 {
                        if s.floor_y as f64 + s.volume > sea {
                            inland += 1;
                        } else {
                            sea_cols += 1;
                        }
                    }
                }
            }
        }
        let springs: f64 = water
            .setup()
            .discharges
            .iter()
            .map(|d| d.flow_cells3_s)
            .sum();
        eprintln!(
            "wet sky columns: {inland} above the sea ({:.0} m2), {sea_cols} sea (dormant not counted); springs {springs:.1} cells3/s: balance at {:.2e} cells/s",
            inland as f64 * 0.0625,
            springs / inland.max(1) as f64
        );
    }
    // A 600 m square loop around the world centre.
    let half = f64::from(size) * 0.25 * 0.5;
    let corners = [
        [half - 300.0, half - 300.0],
        [half + 300.0, half - 300.0],
        [half + 300.0, half + 300.0],
        [half - 300.0, half + 300.0],
    ];
    let at = |t: f64| -> [f64; 2] {
        let d = (t * fly) % 2400.0;
        let (leg, f) = ((d / 600.0) as usize, (d % 600.0) / 600.0);
        let (a, b) = (corners[leg], corners[(leg + 1) % 4]);
        [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f]
    };
    let dt = 1.0 / 60.0;
    let ticks = (seconds / dt) as u64;
    let seeded: std::collections::HashSet<_> = unsupported_walls(&water.world())
        .into_iter()
        .map(|w| (w.1, w.2, w.3, w.4))
        .collect();
    // `SETTLE=max_seconds`: first run the water with no player (nothing
    // throttled) until the active and allocated regions stop changing, checked
    // every 100 s, then play as usual from the settled state.
    if let Some(max) = std::env::var("SETTLE")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
    {
        water.set_focus(&[]);
        let settle_start = Instant::now();
        let (mut prev, mut calm) = ((0usize, 0usize), 0u32);
        let mut t = 0.0;
        while t < max {
            let mut m = water.tick(&scene.world().terrain, &[], &[], dt)?;
            for _ in 1..6000 {
                m = water.tick(&scene.world().terrain, &[], &[], dt)?;
            }
            t += 100.0;
            let now = (m.active_regions, m.allocated_regions);
            let still = now.1 == prev.1
                && (now.0 as f64 - prev.0 as f64).abs() <= 0.03 * prev.0.max(1) as f64;
            calm = if still { calm + 1 } else { 0 };
            eprintln!(
                "settle t {t:6.0}s | wall {:6.1}s | active {} | allocated {} | steady {} | {:.0} m3 | calm {calm}",
                settle_start.elapsed().as_secs_f64(),
                now.0,
                now.1,
                m.steady_regions,
                m.volume_m3
            );
            prev = now;
            if calm >= 2 {
                break;
            }
        }
        eprintln!("settled after {t:.0} s of water time");
        report_walls(&water.world(), &seeded);
        water.set_focus(&[[x_m, z_m]]);
    }
    let started = Instant::now();
    let mut last = Instant::now();
    let mut steps = 0u64;
    // Time per stage of the water tick (`spall_sim::prof` spans), per interval.
    let mut stages: std::collections::BTreeMap<&'static str, f64> = Default::default();
    // Per interval: time in the solver steps, substeps, spans stepped, regions
    // resting and caught up (summed over steps).
    let (mut solve, mut subs, mut spans, mut resting, mut caught) =
        (0.0f64, 0u64, 0u64, 0u64, 0u64);
    for tick in 0..ticks {
        if fly > 0.0 {
            water.set_focus(&[at(tick as f64 * dt)]);
        }
        let m = water.tick(&scene.world().terrain, &[], &[], dt)?;
        for (name, took) in spall_sim::prof::drain() {
            *stages.entry(name).or_insert(0.0) += took.as_secs_f64();
        }
        steps += u64::from(m.steps);
        solve += m.step_duration.as_secs_f64();
        if let Some(st) = m.step {
            subs += u64::from(st.substeps);
            spans += st.active_spans as u64;
            resting += st.resting_regions as u64;
            caught += st.caught_up_regions as u64;
        }
        if tick % 3000 == 0 || tick + 1 == ticks {
            let wall = last.elapsed().as_secs_f64();
            let n = steps.max(1) as f64;
            eprintln!(
                "t {:7.1}s | wall {:6.1}s (+{:5.1}, {:.2} ms/step, solver {:.2}) | per step: {:.1} substeps, {:.0}k spans, {:.0} resting, {:.2} caught up | active {} | allocated {} | {:.0} m3 | skipped {:.1}s | {}",
                tick as f64 * dt,
                started.elapsed().as_secs_f64(),
                wall,
                wall * 1e3 / n,
                solve * 1e3 / n,
                subs as f64 / n,
                spans as f64 / n / 1e3,
                resting as f64 / n,
                caught as f64 / n,
                m.active_regions,
                m.allocated_regions,
                m.volume_m3,
                m.skipped_s,
                water.diag_summary(),
            );
            let per: Vec<String> = stages
                .iter()
                .map(|(name, t)| format!("{name} {:.2}", t * 1e3 / n))
                .collect();
            eprintln!("stages, ms per step: {}", per.join(", "));
            stages.clear();
            eprintln!(
                "springs +{:.0} m3, drains, evaporation and the held sea -{:.0} m3",
                water.frame().spring_added_m3,
                water.frame().drain_removed_m3
            );
            steps = 0;
            (solve, subs, spans, resting, caught) = (0.0, 0, 0, 0, 0);
            last = Instant::now();
        }
        if tick % 18000 == 17999 {
            report_walls(&water.world(), &seeded);
        }
    }
    let world = water.world();
    report_walls(&world, &seeded);
    // How much of what the active regions step is wet.
    let (mut cols, mut wet) = (0usize, 0usize);
    for d in world.active_regions_diag() {
        cols += 1024;
        let mut seen = std::collections::HashSet::new();
        for (c, _, v) in world.region_wet_spans(d.key, 1e-6) {
            if v > 0.0 {
                seen.insert(c);
            }
        }
        wet += seen.len();
    }
    // The fastest water in each active region (m/s), against its largest flux.
    let mut speeds: Vec<(f32, f32)> = world
        .active_regions_diag()
        .iter()
        .map(|d| {
            let v = world
                .region_wet_spans_with_flow(d.key, 1e-6)
                .iter()
                .map(|s| s.4[0].hypot(s.4[1]))
                .fold(0.0f32, f32::max);
            (v, d.max_flow)
        })
        .collect();
    speeds.sort_by(|a, b| a.0.total_cmp(&b.0));
    let pct = |p: f64| {
        speeds
            .get(((speeds.len() as f64 - 1.0) * p) as usize)
            .copied()
            .unwrap_or_default()
    };
    eprintln!(
        "active region top speed m/s (with max flux): p10 {:?} p50 {:?} p90 {:?}; under 1 cm/s {}, under 3 cm/s {} of {}",
        pct(0.1),
        pct(0.5),
        pct(0.9),
        speeds.iter().filter(|s| s.0 < 0.01).count(),
        speeds.iter().filter(|s| s.0 < 0.03).count(),
        speeds.len()
    );
    // Map of active regions, 4 x 4 regions a character: '#' many, '+' some.
    let edge = (size / 32) as i32;
    let mut grid = vec![0u8; ((edge / 4) * (edge / 4)) as usize];
    for d in world.active_regions_diag() {
        let (gx, gz) = (d.key.0 / 4, d.key.1 / 4);
        if (0..edge / 4).contains(&gx) && (0..edge / 4).contains(&gz) {
            grid[(gx + gz * (edge / 4)) as usize] += 1;
        }
    }
    for gz in 0..edge / 4 {
        let row: String = (0..edge / 4)
            .map(|gx| match grid[(gx + gz * (edge / 4)) as usize] {
                0 => '.',
                1..=7 => '+',
                _ => '#',
            })
            .collect();
        eprintln!("map {row}");
    }
    eprintln!(
        "active regions: {} columns, {} wet ({:.1}%)",
        cols,
        wet,
        100.0 * wet as f64 / cols.max(1) as f64
    );
    // The water around the focus column, by span.
    let (cx, cz) = ((x_m / 0.25) as i64, (z_m / 0.25) as i64);
    eprintln!("focus region {:?}", region_of(cx, cz));
    for dz in (-12..=12).step_by(4) {
        let mut line = String::new();
        for dx in (-12..=12).step_by(2) {
            let wet: Vec<String> = world
                .spans_in_column(cx + dx, cz + dz)
                .iter()
                .filter(|s| s.volume > 1e-3)
                .map(|s| format!("{}+{:.1}", s.floor_y, s.depth))
                .collect();
            line.push_str(&format!("{:>11}", wet.join("/")));
        }
        eprintln!("z{:+3} {line}", dz);
    }
    Ok(())
}

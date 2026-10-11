//! Cuts a hole in a generated lake's shore and reports how level the server's
//! water really is afterwards, to separate simulation artifacts from rendering
//! ones. `cargo run --release -p sandbox --example water_cut_probe -- SIZE SEED SECONDS`
use sandbox::worldgen_scene;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, GlobalCell, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::{EditIntent, EditKind, EditTarget, Simulation, SimulationConfig};

fn roughness(
    sim: &Simulation,
    x0: i64,
    x1: i64,
    z0: i64,
    z1: i64,
) -> (f64, f64, usize, (i64, i64)) {
    let w = sim.column_water().unwrap().world();
    let top = |x: i64, z: i64| -> Option<f64> {
        w.spans_in_column(x, z)
            .iter()
            .filter(|s| s.depth > 0.02)
            .map(|s| s.floor_y as f64 + s.depth)
            .reduce(f64::max)
    };
    let (mut worst, mut sum, mut n, mut at) = (0.0f64, 0.0f64, 0usize, (0, 0));
    for z in z0..z1 {
        for x in x0..x1 {
            let Some(h) = top(x, z) else { continue };
            let nb: Vec<f64> = [
                (1, 0),
                (-1, 0),
                (0, 1),
                (0, -1),
                (1, 1),
                (-1, -1),
                (1, -1),
                (-1, 1),
            ]
            .iter()
            .filter_map(|(dx, dz)| top(x + dx, z + dz))
            .collect();
            if nb.len() < 8 {
                continue;
            }
            let mut sorted = nb.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let median = (sorted[3] + sorted[4]) / 2.0;
            let dev = (h - median).abs();
            sum += dev;
            n += 1;
            if dev > worst {
                worst = dev;
                at = (x, z);
            }
        }
    }
    (worst, if n > 0 { sum / n as f64 } else { 0.0 }, n, at)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let size: u32 = a.first().map_or(Ok(1024), |s| s.parse())?;
    let seed: u64 = a.get(1).map_or(Ok(7), |s| s.parse())?;
    let seconds: u32 = a.get(2).map_or(Ok(120), |s| s.parse())?;
    let scene = worldgen_scene::generate("showcase", seed, size)?;
    let setup = scene.column_water_setup().ok_or("dry world")?.clone();
    let mut setup = setup;
    // SINKS=n adds an n x n patch of drains at the bottom near the shore, so
    // water keeps flowing for the whole run (an ongoing drain, never at rest).
    if let Some(n) = std::env::var("SINKS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
    {
        let plan = &scene.world().water;
        let wet: std::collections::HashSet<(i64, i64)> =
            plan.cells.iter().map(|c| (c.x, c.z)).collect();
        let shore = wet
            .iter()
            .copied()
            .filter(|(x, z)| !wet.contains(&(x + 1, *z)) && wet.contains(&(x - 8, *z)))
            .min()
            .unwrap();
        for dz in 0..n {
            for dx in 0..n {
                let (x, z) = (shore.0 - 4 - dx, shore.1 + dz);
                let low = plan
                    .cells
                    .iter()
                    .filter(|c| c.x == x && c.z == z)
                    .map(|c| c.y)
                    .min();
                if let Some(y) = low {
                    setup.sinks.push(GlobalCell::new(x, y, z));
                }
            }
        }
        println!("{} sink cells near {shore:?}", setup.sinks.len());
    }
    if let Some(v) = std::env::var("CFL").ok().and_then(|v| v.parse().ok()) {
        setup.config.cfl = v;
    }
    if let Some(v) = std::env::var("DAMP").ok().and_then(|v| v.parse().ok()) {
        setup.config.damping_per_s = v;
    }
    if std::env::var("NOSLEEP").is_ok() {
        setup.config.sleep_steps = u32::MAX;
    }
    let mut config = SimulationConfig::new(scene.world_setup());
    config.column_water = Some(setup);
    let mut sim = Simulation::new(config)?;
    let plan = &scene.world().water;
    let (lo, hi) = plan.bounds.ok_or("no water")?;
    // A shore column of the biggest lake: wet with a dry +x neighbour.
    let wet: std::collections::HashSet<(i64, i64)> =
        plan.cells.iter().map(|c| (c.x, c.z)).collect();
    let shore = wet
        .iter()
        .copied()
        .filter(|(x, z)| {
            !wet.contains(&(x + 1, *z)) && wet.contains(&(x - 1, *z)) && wet.contains(&(x - 6, *z))
        })
        .min()
        .unwrap();
    let cut_at = GlobalCell::new(shore.0 + 1, plan.surface_y - 2, shore.1);
    println!("lake bounds {lo:?}..{hi:?}, cutting at {cut_at:?}");
    let half = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            cut_at.x * BRUSH_UNIT + half,
            cut_at.y * BRUSH_UNIT + half,
            cut_at.z * BRUSH_UNIT + half,
        ),
        10 * BRUSH_UNIT,
    )?;
    let (x0, x1, z0, z1) = (lo.x - 5, hi.x + 5, lo.z - 5, hi.z + 5);
    println!("before: {:?}", roughness(&sim, x0, x1, z0, z1));
    sim.submit(EditIntent {
        request_id: RequestId(1),
        actor: EntityId::new(1).unwrap(),
        target: EditTarget::Terrain,
        kind: EditKind::Cut,
        brush,
        explosion: None,
    })?;
    let start_volume = sim.column_water().unwrap().frame().volume_m3;
    for tick in 0..seconds * 60 {
        let r = sim.tick()?;
        if tick % 1200 == 0 || tick == seconds * 60 - 1 {
            let m = r.column_water.unwrap();
            let w = sim.column_water().unwrap().world();
            let (worst, mean, n, at) = roughness(&sim, x0, x1, z0, z1);
            if tick > 0 && std::env::var("DETAIL").is_ok() {
                println!(
                    "   overfill {:.1} cells3 of {:.0}; substeps {} skipped_s {:.2}; worst column spans:",
                    w.overfill_volume(),
                    w.total_volume(),
                    m.step.map_or(0, |st| st.substeps),
                    m.skipped_s
                );
                for sp in w.spans_in_column(at.0, at.1) {
                    println!(
                        "     floor {} ceil {} sky {} volume {:.2} depth {:.2} overfill {:.2} head {:.2}",
                        sp.floor_y,
                        sp.ceiling_y,
                        sp.sky,
                        sp.volume,
                        sp.depth,
                        sp.overfill,
                        sp.head_y
                    );
                }
            }
            if tick > 0
                && let Ok(spec) = std::env::var("PROFILE")
            {
                let (cx, cz) = spec
                    .split_once(',')
                    .map(|(a, b)| (a.parse::<i64>().unwrap(), b.parse::<i64>().unwrap()))
                    .unwrap();
                println!("   surface tops (floor/top) along x at z={cz}:");
                let mut line = String::new();
                for x in cx - 12..=cx + 12 {
                    let top = w
                        .spans_in_column(x, cz)
                        .iter()
                        .filter(|sp| sp.depth > 0.02)
                        .map(|sp| (sp.floor_y, sp.floor_y as f64 + sp.depth))
                        .next_back();
                    match top {
                        Some((f, t)) => line.push_str(&format!(" {f}/{t:.1}")),
                        None => line.push_str(" dry"),
                    }
                }
                println!("   {line}");
            }
            println!(
                "t={:5.1}s active_regions={:3} step={:6.2}ms head_diff={:.3} maxflow={:.4} rough: worst {:.3} cells at {:?}, mean {:.4} over {} cols; volume drift {:.2e}",
                f64::from(tick) / 60.0,
                m.active_regions,
                m.step_duration.as_secs_f64() * 1e3,
                w.max_wetted_head_difference(),
                w.max_abs_flow(),
                worst,
                at,
                mean,
                n,
                (m.volume_m3 - start_volume).abs() / start_volume
            );
        }
    }
    Ok(())
}

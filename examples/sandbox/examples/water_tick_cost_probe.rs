//! Measures what the column water costs the server tick after a big release:
//! cuts the biggest lake's shore and reports whole-tick and fluid-step wall
//! time, steps run and fluid time skipped, per phase of the run.
//! `cargo run --release -p sandbox --example water_tick_cost_probe -- SIZE SEED SECONDS`
use std::time::Instant;

use sandbox::worldgen_scene;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, GlobalCell, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::{EditIntent, EditKind, EditTarget, Simulation, SimulationConfig};

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let size: u32 = a.first().map_or(Ok(1024), |s| s.parse())?;
    let seed: u64 = a.get(1).map_or(Ok(7), |s| s.parse())?;
    let seconds: u32 = a.get(2).map_or(Ok(60), |s| s.parse())?;
    let scene = worldgen_scene::generate("showcase", seed, size)?;
    let mut setup = scene.column_water_setup().ok_or("dry world")?.clone();
    // NOSLEEP=1 keeps every region awake: the upper bound on fluid work.
    if std::env::var("NOSLEEP").is_ok() {
        setup.config.sleep_steps = u32::MAX;
    }
    let mut config = SimulationConfig::new(scene.world_setup());
    config.column_water = Some(setup);
    let mut sim = Simulation::new(config)?;
    let plan = &scene.world().water;
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
    let half = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            cut_at.x * BRUSH_UNIT + half,
            cut_at.y * BRUSH_UNIT + half,
            cut_at.z * BRUSH_UNIT + half,
        ),
        std::env::var("BRUSH_M")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(10)
            * BRUSH_UNIT,
    )?;
    // Quiet baseline first: resting water.
    let mut rest = Vec::new();
    for _ in 0..120 {
        let t = Instant::now();
        sim.tick()?;
        rest.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "resting: tick p50 {:.2} p99 {:.2} max {:.2} ms",
        pct(&mut rest.clone(), 0.5),
        pct(&mut rest.clone(), 0.99),
        pct(&mut rest, 1.0)
    );
    sim.submit(EditIntent {
        request_id: RequestId(1),
        actor: EntityId::new(1).unwrap(),
        target: EditTarget::Terrain,
        kind: EditKind::Cut,
        brush,
        explosion: None,
    })?;
    let phases = [(0u32, 5u32), (5, 20), (20, seconds)];
    let mut rows: Vec<(f64, f64, u32, usize)> = Vec::new(); // tick ms, step ms, steps, active
    let mut max_skipped = 0.0f64;
    for tick in 0..seconds * 60 {
        if tick == 120 {
            // A second edit, once everything is warm: what an edit costs in steady state.
            let brush = SphereBrush::new(
                BrushPoint::from_units(
                    (cut_at.x + 6) * BRUSH_UNIT + half,
                    cut_at.y * BRUSH_UNIT + half,
                    cut_at.z * BRUSH_UNIT + half,
                ),
                6 * BRUSH_UNIT,
            )?;
            sim.submit(EditIntent {
                request_id: RequestId(2),
                actor: EntityId::new(1).unwrap(),
                target: EditTarget::Terrain,
                kind: EditKind::Cut,
                brush,
                explosion: None,
            })?;
        }
        let t = Instant::now();
        let r = sim.tick()?;
        let total = t.elapsed().as_secs_f64() * 1e3;
        let m = r.column_water.unwrap();
        max_skipped = max_skipped.max(m.skipped_s);
        if total > 16.7 {
            println!(
                "  slow tick: {total:.1} ms (terrain refresh {:.1} ms, fluid {:.1} ms, remapped {} columns, relocated {:.1} m3)",
                m.boundary_refresh.as_secs_f64() * 1e3,
                m.step_duration.as_secs_f64() * 1e3,
                m.columns_remapped,
                m.relocated_m3
            );
        }
        rows.push((
            total,
            m.step_duration.as_secs_f64() * 1e3,
            m.steps,
            m.active_regions,
        ));
    }
    for (lo, hi) in phases {
        let slice = &rows[((lo.min(seconds)) * 60) as usize..((hi.min(seconds)) * 60) as usize];
        if slice.is_empty() {
            continue;
        }
        let mut tick: Vec<f64> = slice.iter().map(|r| r.0).collect();
        let mut step: Vec<f64> = slice.iter().filter(|r| r.2 > 0).map(|r| r.1).collect();
        let over = slice.iter().filter(|r| r.0 > 16.7).count();
        println!(
            "after cut {lo:>2}-{hi:>2} s: tick p50 {:.2} p99 {:.2} max {:.2} ms | fluid step p50 {:.2} p99 {:.2} max {:.2} ms | ticks over 16.7 ms: {over}/{} | active regions {}..{}",
            pct(&mut tick, 0.5),
            pct(&mut tick, 0.99),
            pct(&mut tick, 1.0),
            pct(&mut step, 0.5),
            pct(&mut step, 0.99),
            pct(&mut step, 1.0),
            slice.len(),
            slice.iter().map(|r| r.3).min().unwrap(),
            slice.iter().map(|r| r.3).max().unwrap(),
        );
    }
    println!("max skipped fluid time: {max_skipped:.2} s");
    Ok(())
}

//! Reconstructed ENG-121 paced owner/worker trench workload, without rendering/network.
#![recursion_limit = "256"]
use sandbox::worldgen_scene;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, GlobalCell, SphereBrush};
use spall_fluid::SolidBoundary;
use spall_protocol::RequestId;
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig};
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let size = std::env::args()
        .nth(1)
        .map_or(Ok(1024), |v| v.parse::<u32>())?;
    if ![512, 1024].contains(&size) {
        return Err("size must be 512 or 1024".into());
    }
    let scene = worldgen_scene::generate_with_season(
        "showcase",
        1,
        size,
        spall_ecology::living::Season::Autumn,
    )?;
    let setup = scene.water_setup().ok_or("dry world")?.clone();
    let shoreline = setup
        .initial_fractions
        .iter()
        .max_by_key(|(p, _)| (p.x, p.y, p.z))
        .ok_or("no water")?
        .0;
    let old_end = setup.domain.origin().x + i64::from(setup.domain.dimensions()[0]);
    let cut_end = old_end + 40;
    let cuts = (cut_end - shoreline.x) as u32;
    let dig_ticks = cuts * 6;
    let requested_ticks = dig_ticks + 3000;
    let mut config = SimulationConfig::new(scene.world_setup());
    config.water = Some(setup.clone());
    config.vegetation = Some(scene.vegetation().clone());
    let mut sim = Simulation::new(config)?;
    let accounting = |sim: &Simulation| {
        let f = sim.water().unwrap().frame();
        f.volume_m3
            + f.trapped.iter().sum::<f64>() * f.cell_size_m.powi(3)
            + f.open_outflow_m3
            + f.drain_removed_m3
            - f.spring_added_m3
    };
    let initial = accounting(&sim);
    let seed = sim.water().unwrap().seed_report();
    let mut step_times = Vec::new();
    let mut tick_times = Vec::new();
    let mut growth_ticks = Vec::new();
    let (mut committed, mut rejected, mut nonconverged) = (0, 0, 0);
    let mut hammer_committed = 0;
    let (mut rows, mut iterations, mut skipped, mut skipped_s) = (0, 0, 0, 0.0);
    let mut growths = 0;
    let mut growth_refused = 0;
    let mut accepted_ticks = 0;
    let mut failure = None;
    let start = Instant::now();
    for tick in 0..requested_ticks {
        if tick < dig_ticks && tick % 6 == 0 {
            let x = shoreline.x + i64::from(tick / 6);
            let half = BRUSH_UNIT / 2;
            let brush = SphereBrush::new(
                BrushPoint::from_units(
                    x * BRUSH_UNIT + half,
                    shoreline.y * BRUSH_UNIT + half,
                    shoreline.z * BRUSH_UNIT + half,
                ),
                3 * BRUSH_UNIT,
            )?;
            sim.submit(EditIntent::cut(
                RequestId(u64::from(tick / 6) + 1),
                EntityId::new(1)?,
                EditTarget::Terrain,
                brush,
            ))?;
        }
        let timer = Instant::now();
        let report = match sim.tick() {
            Ok(r) => r,
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        };
        let tick_us = timer.elapsed().as_micros() as u64;
        tick_times.push(tick_us);
        accepted_ticks += 1;
        committed += report.committed.len();
        hammer_committed += report
            .committed
            .iter()
            .filter(|(id, _)| id.0 > 0 && id.0 <= u64::from(cuts))
            .count();
        rejected += report.rejected.len();
        if let Some(m) = report.water {
            if m.domain_growths > growths {
                growth_ticks.push(tick_us);
                growths = m.domain_growths;
            }
            skipped = m.skipped_ticks;
            growth_refused = m.growth_refused;
            skipped_s = m.skipped_duration.as_secs_f64();
            if let Some(step) = m.step {
                step_times.push(m.step_duration.as_micros() as u64);
                rows += step.pressure_active_rows_total;
                iterations += step.pressure_iterations;
                nonconverged += u64::from(step.substeps - step.pressure_converged_substeps);
            }
        }
        if tick % 600 == 0 {
            eprintln!(
                "size={size} tick={tick}/{requested_ticks} fluid_s={:.2} steps={} skipped={skipped}",
                sim.water().unwrap().frame().fluid_time_s,
                step_times.len()
            );
        }
        // Pace from each tick's start; never catch up in a burst after a hitch.
        let budget = Duration::from_secs_f64(1.0 / 60.0);
        if let Some(left) = budget.checked_sub(timer.elapsed()) {
            std::thread::sleep(left);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let water = sim.water().unwrap();
    let f = water.frame();
    let nx = f.dimensions[0] as usize;
    let mut downstream = 0.0;
    for (i, amount) in f.exact_fractions.iter().enumerate() {
        let x = f.origin.x + (i % nx) as i64 * i64::from(f.coarsen);
        if x >= old_end {
            downstream += amount * f.cell_size_m.powi(3);
        }
    }
    step_times.sort_unstable();
    tick_times.sort_unstable();
    // Diagnostic only, after the timed run: compare actual carved terrain
    // with the majority-solid cells and exact water along the channel centre.
    let boundary = SolidBoundary::capture(&sim.world().terrain().volume, water.domain())?
        .coarsened(f.coarsen)?;
    let c = i64::from(f.coarsen);
    let ny = f.dimensions[1] as usize;
    let k = (shoreline.z - f.origin.z).div_euclid(c);
    let mut section = Vec::new();
    for x in
        ((shoreline.x - f.origin.x).div_euclid(c) - 2)..((cut_end - f.origin.x).div_euclid(c) + 1)
    {
        for y in ((shoreline.y - f.origin.y).div_euclid(c) - 2)
            ..=((shoreline.y - f.origin.y).div_euclid(c) + 1)
        {
            let cell = GlobalCell::new(f.origin.x + x, f.origin.y + y, f.origin.z + k);
            let i = x as usize + nx * (y as usize + ny * k as usize);
            section.push(serde_json::json!({"voxel_origin":[f.origin.x+x*c,f.origin.y+y*c,f.origin.z+k*c], "solid":boundary.is_solid(cell), "water_fraction":f.exact_fractions[i]}));
        }
    }
    let crossed = downstream > 1e-9;
    let percentile = |v: &[u64], p: usize| v.get(v.len().saturating_sub(1) * p / 100).copied();
    println!(
        "{}",
        serde_json::json!({
            "scenario":"reconstructed_eng121_paced_hammer_trench", "size":size,"seed":1,"season":"autumn",
            "commit":"186aea8","water_only":true,"coarsen":setup.coarsen,
            "initial_domain_dimensions":setup.domain.dimensions(),"final_domain_dimensions":water.domain().dimensions(),
            "initial_fluid_cells":setup.domain.cell_count() / (setup.coarsen as usize).pow(3),"final_fluid_cells":f.exact_fractions.len(),
            "shoreline":[shoreline.x,shoreline.y,shoreline.z],"old_end_x_exclusive":old_end,"cut_end_x_exclusive":cut_end,
            "radius_voxels":3,"cut_interval_ticks":6,"submitted_cuts":cuts,"committed_edits":committed,"rejected_edits":rejected,
            "hammer_committed":hammer_committed,"other_committed_edits":committed-hammer_committed,
            "requested_ticks":requested_ticks,"accepted_ticks":accepted_ticks,"post_dig_ticks":3000,"elapsed_s":elapsed,
            "fluid_time_s":f.fluid_time_s,"fluid_realtime_ratio":f.fluid_time_s/elapsed,
            "solver_steps":step_times.len(),"step_median_us":percentile(&step_times,50),"step_p95_us":percentile(&step_times,95),"step_max_us":step_times.last(),
            "tick_median_us":percentile(&tick_times,50),"tick_p95_us":percentile(&tick_times,95),"tick_max_us":tick_times.last(),
            "growths":growths,"growth_tick_us":growth_ticks,"growth_refused":growth_refused,
            "skipped_steps":skipped,"skipped_fluid_s":skipped_s,"nonconverged_substeps":nonconverged,
            "pressure_rows_total":rows,"pressure_iterations_total":iterations,"water_past_old_edge_m3":downstream,
            "initial_accounted_water_m3":initial,"water_error_m3":accounting(&sim)-initial,"seed_report":format!("{seed:?}"),
            "fraction_min":f.exact_fractions.iter().copied().fold(1.0,f64::min),"fraction_max":f.exact_fractions.iter().copied().fold(0.0,f64::max),
            "failure":failure,"water_crossed_old_edge":crossed,"trench_center_section":section,"graphical_acceptance":false,"network_acceptance":false
        })
    );
    if failure.is_some()
        || rejected > 0
        || accepted_ticks != requested_ticks
        || nonconverged > 0
        || hammer_committed != cuts as usize
        || !crossed
    {
        return Err("paced trench run failed; see JSON evidence".into());
    }
    Ok(())
}

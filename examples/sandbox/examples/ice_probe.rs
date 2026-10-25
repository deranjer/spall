//! Headless winter run of a generated world as the server builds it (column
//! water on its worker, real-time 60 Hz ticks, one player standing at the
//! spawn, off-thread staging): how much ice forms and what it costs the tick (ENG-268).
//! `NOICE=1` runs the same world without ice, for comparison.
//! `cargo run --release -p sandbox --example ice_probe -- PRESET SEED SIZE SECONDS`
use std::collections::HashMap;
use std::time::{Duration, Instant};

use sandbox::{game, worldgen_scene};
use spall_core::EntityId;
use spall_sim::{Simulation, SimulationConfig};

fn percentile(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i].as_secs_f64() * 1e3
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let preset = a.first().map_or("natural", String::as_str).to_string();
    let seed: u64 = a.get(1).map_or(Ok(1), |s| s.parse())?;
    let size: u32 = a.get(2).map_or(Ok(4096), |s| s.parse())?;
    let seconds: f64 = a.get(3).map_or(Ok(240.0), |s| s.parse())?;
    let with_ice = std::env::var_os("NOICE").is_none();
    let t = Instant::now();
    let mut scene = worldgen_scene::generate_with_season(
        &preset,
        seed,
        size,
        spall_ecology::living::Season::Winter,
    )?;
    eprintln!("worldgen {:.1}s", t.elapsed().as_secs_f64());
    // As the server does: a winter world starts frozen (`NOSTARTFREEZE=1`
    // leaves all of it to play, for comparison).
    if with_ice && std::env::var_os("NOSTARTFREEZE").is_none() {
        let t = Instant::now();
        let cells = scene.freeze_water();
        eprintln!(
            "startup freeze: {cells} ice cells in {:.2}s",
            t.elapsed().as_secs_f64()
        );
    }
    let spawn = scene.player_spawns()[0];
    let mut setup = scene.world_setup();
    setup.materials = game::manifest();
    setup.physics.disable_ccd = true;
    let mut config = SimulationConfig::new(setup);
    config.column_water = scene.column_water_setup().cloned();
    config.column_water_worker = true;
    let mut sim = Simulation::new(config)?;
    sim.enable_off_thread_staging();
    if with_ice && std::env::var_os("ICE_STARTUP_ONLY").is_none() {
        sim.set_ice(game::ice_settings());
    }
    sim.set_season(3)?;
    let player = EntityId::new((1 << 48) + 1).unwrap();
    sim.add_player(player, spawn);
    eprintln!("setup {:.1}s, ice {with_ice}", t.elapsed().as_secs_f64());

    let ticks = (seconds * 60.0) as u64;
    let period = Duration::from_secs_f64(1.0 / 60.0);
    let mut durations = Vec::with_capacity(ticks as usize);
    let mut stages: HashMap<&'static str, (Duration, Duration, u64)> = HashMap::new();
    let mut ice_commits = 0usize;
    let mut ice_cells = 0u64;
    let mut commit_ticks = Vec::new();
    let mut quiet_ticks = Vec::new();
    let mut next_transfer = 0u64;
    let started = Instant::now();
    for i in 0..ticks {
        // `FIRST_DIG=1`: one small player-style cut under the spawn at 10 s, so
        // a no-ice baseline pays the same first-edit cost (it detaches the
        // world's floating worldgen fragments) as a run whose ice commits do.
        if i == 600 && std::env::var_os("FIRST_DIG").is_some() {
            let cell = |v: f64| (v / 0.25).floor() as i64;
            let units = |c: i64| c * spall_core::BRUSH_UNIT + spall_core::BRUSH_UNIT / 2;
            let centre = spall_core::BrushPoint::from_units(
                units(cell(spawn[0])),
                units(cell(spawn[1]) - 3),
                units(cell(spawn[2])),
            );
            sim.submit(spall_sim::EditIntent {
                request_id: spall_protocol::RequestId(1),
                actor: player,
                target: spall_sim::EditTarget::Terrain,
                kind: spall_sim::EditKind::Cut,
                brush: spall_core::SphereBrush::new(centre, spall_core::BRUSH_UNIT)?,
                explosion: None,
            })?;
        }
        let begin = Instant::now();
        // Bulk split IDs reserved before the tick, as the server does.
        let ids = (0..sim.transfer_reservation_bound())
            .map(|_| {
                next_transfer += 1;
                spall_protocol::TransferId(
                    spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT | next_transfer,
                )
            })
            .collect();
        let report = sim.tick_with_transfer_ids(ids)?;
        let took = begin.elapsed();
        durations.push(took);
        for (stage, elapsed) in spall_sim::prof::drain() {
            let e = stages.entry(stage).or_default();
            e.0 += elapsed;
            e.1 = e.1.max(elapsed);
            e.2 += 1;
        }
        let mut ice_now = 0;
        if std::env::var_os("ICE_DEBUG").is_some() {
            for (r, why) in &report.rejected {
                eprintln!("rejected {:x} {why}", r.0);
            }
            for r in &report.retried {
                eprintln!("retried {:x}", r.0);
            }
            for r in &report.discarded_stale {
                eprintln!("stale {:x}", r.0);
            }
        }
        for (request, committed) in &report.committed {
            if std::env::var_os("ICE_DEBUG").is_some() {
                let ops = &committed.topology.ops;
                let splits = ops
                    .iter()
                    .filter(|op| !matches!(op, spall_protocol::TopologyOp::CellRun { .. }))
                    .count();
                eprintln!(
                    "commit {:x} ops {} non-run ops {splits} first {:?}",
                    request.0,
                    ops.len(),
                    ops.first()
                );
            }
            for op in &committed.topology.ops {
                if let spall_protocol::TopologyOp::CellRun { material, len, .. } = op
                    && *material == game::materials::ICE
                {
                    ice_cells += u64::from(*len);
                    ice_now += 1;
                }
            }
        }
        if ice_now > 0 {
            ice_commits += report.committed.len();
            commit_ticks.push(took);
        } else {
            quiet_ticks.push(took);
        }
        if i % 600 == 0 {
            eprintln!(
                "t={:.0}s ice cells {ice_cells} commits {ice_commits} tick {:.2} ms",
                i as f64 / 60.0,
                took.as_secs_f64() * 1e3
            );
        }
        let next = started + period * (i as u32 + 1);
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    let mut quiet = quiet_ticks;
    quiet.sort();
    eprintln!(
        "ticks without ice commits p50 {:.2} p99 {:.2} max {:.2} ms",
        percentile(&quiet, 0.5),
        percentile(&quiet, 0.99),
        percentile(&quiet, 1.0)
    );
    durations.sort();
    commit_ticks.sort();
    eprintln!(
        "ticks {} p50 {:.2} p99 {:.2} p99.9 {:.2} max {:.2} ms",
        durations.len(),
        percentile(&durations, 0.5),
        percentile(&durations, 0.99),
        percentile(&durations, 0.999),
        percentile(&durations, 1.0)
    );
    eprintln!(
        "ice: {ice_cells} cells in {ice_commits} commits over {} ticks; those ticks p50 {:.2} p99 {:.2} max {:.2} ms",
        commit_ticks.len(),
        percentile(&commit_ticks, 0.5),
        percentile(&commit_ticks, 0.99),
        percentile(&commit_ticks, 1.0)
    );
    let mut rows: Vec<_> = stages.into_iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1.0));
    for (stage, (total, max, n)) in rows.iter().take(14) {
        eprintln!(
            "  {stage:<28} mean {:.3} ms max {:.2} ms ({n})",
            total.as_secs_f64() * 1e3 / *n as f64,
            max.as_secs_f64() * 1e3
        );
    }
    Ok(())
}

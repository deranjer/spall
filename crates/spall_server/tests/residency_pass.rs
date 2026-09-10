//! T23 / G3 row 7, slice D — the serve-loop residency pass, run against a live
//! `Simulation`, must reach **the same** committed world as a fully-resident run
//! from the identical edit sequence: same `world_hash`, same conservation, same
//! per-transaction `result_hashes` — while actually evicting and reloading
//! terrain bricks.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, EntityId, SphereBrush};
use spall_protocol::{Hash32, RequestId};
use spall_server::{ResidencyLimits, ResidencyPass};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};

/// (tick, cell, radius) — a west collapse, an east collapse, two floor cuts.
const SCRIPT: &[(u64, [i64; 3], i64)] = &[
    (2, [10, 6, 3], 2),
    (30, [82, 6, 75], 2),
    (60, [1, 1, 3], 1),
    (90, [90, 1, 75], 1),
];

fn cut(req: u64, cell: [i64; 3], radius: i64) -> EditIntent {
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            cell[0] * BRUSH_UNIT + h,
            cell[1] * BRUSH_UNIT + h,
            cell[2] * BRUSH_UNIT + h,
        ),
        radius * BRUSH_UNIT,
    )
    .unwrap();
    EditIntent::cut(
        RequestId(req),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    )
}

fn sim() -> Simulation {
    let mut setup = fixtures::separated_regions_setup();
    setup.physics.disable_ccd = true;
    Simulation::new(SimulationConfig::new(setup)).unwrap()
}

/// Committed `result_hashes`, in commit order, as `(volume raw, hash)` rows.
fn result_hash_trace(sim: &Simulation) -> Vec<(u64, Hash32)> {
    (1..=SCRIPT.len() as u64)
        .filter_map(|r| sim.committed(RequestId(r)))
        .flat_map(|c| {
            c.topology
                .result_hashes
                .iter()
                .map(|vh| (vh.volume.get(), vh.hash))
        })
        .collect()
}

fn run_without_residency() -> (Hash32, u64, Vec<(u64, Hash32)>) {
    let mut sim = sim();
    let mut next = 0usize;
    for tick in 1..=180u64 {
        while next < SCRIPT.len() && SCRIPT[next].0 == tick {
            let (_, cell, r) = SCRIPT[next];
            sim.submit(cut(next as u64 + 1, cell, r)).unwrap();
            next += 1;
        }
        sim.tick().unwrap();
    }
    (
        sim.world().world_hash(),
        sim.world().total_solid_cells(),
        result_hash_trace(&sim),
    )
}

#[test]
fn residency_on_reaches_the_same_committed_world_as_residency_off() {
    let (off_hash, off_solid, off_trace) = run_without_residency();

    let mut sim = sim();
    let terrain = sim.world().terrain_volume_id();
    let mut pass = ResidencyPass::install(
        sim.world_mut(),
        ResidencyLimits {
            budget_bricks: 4,
            interest_radius_bricks: 1,
        },
    );

    // A single stationary player in the west region; the east region is out of
    // interest and gets evicted, then reloaded for the east cut.
    let player_feet = [[1.0_f64, 1.0, 1.0]];

    let initial_resident = sim.world().terrain().volume.resident_brick_count();
    let mut pipeline_retries = 0u64;
    let mut evicted_at_east_cut = false;

    let mut next = 0usize;
    for tick in 1..=180u64 {
        while next < SCRIPT.len() && SCRIPT[next].0 == tick {
            let (_, cell, r) = SCRIPT[next];
            sim.submit(cut(next as u64 + 1, cell, r)).unwrap();
            next += 1;
        }
        let report = sim.tick().unwrap();
        pipeline_retries += report.retried.len() as u64;
        for (_, done) in &report.committed {
            let touched: Vec<BrickCoord> = done
                .topology
                .after
                .iter()
                .filter(|br| br.volume == terrain)
                .map(|br| br.coord)
                .collect();
            pass.on_commit(sim.world(), touched);
        }
        pass.run(sim.world_mut(), &player_feet);
        // Just before the east cut is submitted, the east region must be
        // evicted (the player is in the west, radius 1).
        if tick == 29 {
            evicted_at_east_cut = sim.world().has_evicted();
        }
    }

    // Every scripted cut committed.
    for r in 1..=SCRIPT.len() as u64 {
        assert!(
            sim.committed(RequestId(r)).is_some(),
            "cut {r} did not commit under residency; status {:?}",
            sim.action_status(RequestId(r))
        );
    }

    // The committed world is identical to the fully-resident run.
    assert_eq!(sim.world().world_hash(), off_hash, "world_hash diverged");
    assert_eq!(
        sim.world().total_solid_cells(),
        off_solid,
        "conservation diverged"
    );
    assert_eq!(
        result_hash_trace(&sim),
        off_trace,
        "per-transaction result_hashes diverged"
    );

    // And the pass actually did work: it evicted terrain, the east region was
    // evicted when its cut arrived, and the edit pipeline reloaded it on demand
    // (retry) so the cut still committed against the same world.
    let stats = pass.stats();
    assert!(stats.evictions_total > 0, "nothing was ever evicted");
    assert!(
        stats.resident_terrain_bricks_min < initial_resident,
        "resident brick count never dropped: {stats:?} (initial {initial_resident})"
    );
    assert!(
        evicted_at_east_cut,
        "the east region was still resident when its cut arrived"
    );
    assert!(
        pipeline_retries > 0,
        "the edit pipeline never had to reload evicted geometry"
    );
}

#[test]
fn residency_default_off_is_a_no_op() {
    // No `ResidencyPass::install` — `world.evicted` stays empty, `has_backing`
    // false, and a plain run matches the reference.
    let (off_hash, _, _) = run_without_residency();
    let mut sim = sim();
    assert!(!sim.world().has_backing());
    let mut next = 0usize;
    for tick in 1..=180u64 {
        while next < SCRIPT.len() && SCRIPT[next].0 == tick {
            let (_, cell, r) = SCRIPT[next];
            sim.submit(cut(next as u64 + 1, cell, r)).unwrap();
            next += 1;
        }
        sim.tick().unwrap();
    }
    assert!(!sim.world().has_evicted());
    assert_eq!(sim.world().world_hash(), off_hash);
}

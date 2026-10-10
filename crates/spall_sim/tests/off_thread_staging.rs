//! Staging on a worker thread must commit exactly what inline staging commits, in request order,
//! and must never carry a result across a world reset.
//!
//! Which *tick* a commit lands on depends on wall-clock staging time in this mode, so these tests
//! compare end states and ordering, never tick numbers.

use std::time::{Duration, Instant};

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures;
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig};

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

/// Every cell's sample over the region the cuts touch, in a fixed order. Compares what the world
/// contains, not the revision numbers, which follow commit order.
fn cell_contents(sim: &Simulation) -> Vec<String> {
    let volume = &sim.world().terrain().volume;
    let mut out = Vec::new();
    for z in -4..48 {
        for y in -2..40 {
            for x in -4..96 {
                let sample = volume.sample(spall_core::GlobalCell::new(x, y, z)).unwrap();
                out.push(format!("{sample:?}"));
            }
        }
    }
    out
}

fn new_sim() -> Simulation {
    Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap()
}

fn submit_cut(sim: &mut Simulation, id: u64, x: i64, z: i64, radius: i64) {
    sim.submit(EditIntent::cut(
        RequestId(id),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_cell(x, 1, z, radius),
    ))
    .unwrap();
}

/// Ticks until the pipeline is idle, sleeping briefly so the worker can run.
fn tick_until_idle(sim: &mut Simulation) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !sim.is_idle() {
        assert!(Instant::now() < deadline, "the pipeline never went idle");
        sim.tick().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Cuts at spacings that make some neighbours share bricks (so a later one finds the world
/// moved on and must be staged again) and some independent.
const CUTS: [(i64, i64, i64); 8] = [
    (10, 8, 2),
    (14, 8, 2),
    (40, 8, 3),
    (18, 8, 2),
    (60, 20, 2),
    (22, 8, 2),
    (44, 8, 3),
    (64, 20, 2),
];

#[test]
fn a_burst_commits_everything_in_request_order_with_the_inline_contents() {
    let mut inline = new_sim();
    for (i, (x, z, r)) in CUTS.into_iter().enumerate() {
        submit_cut(&mut inline, i as u64 + 1, x, z, r);
    }
    inline.run_until_idle(64).unwrap();

    let mut off = new_sim();
    off.enable_off_thread_staging();
    for (i, (x, z, r)) in CUTS.into_iter().enumerate() {
        submit_cut(&mut off, i as u64 + 1, x, z, r);
    }
    tick_until_idle(&mut off);

    let mut last = 0;
    for i in 1..=CUTS.len() as u64 {
        let done = off
            .committed(RequestId(i))
            .unwrap_or_else(|| panic!("request {i} never committed"));
        let tx = done.transaction.get();
        assert!(tx > last, "request {i} committed out of request order");
        last = tx;
    }
    // The inline pipeline re-queues a conflicting edit behind independent ones, so it may commit
    // in a different order and number its bricks' revisions differently; the world's contents
    // must still be identical.
    assert_eq!(
        off.world().total_solid_cells(),
        inline.world().total_solid_cells()
    );
    assert_eq!(
        cell_contents(&off),
        cell_contents(&inline),
        "off-thread staging must leave the same cells as inline staging"
    );
}

#[test]
fn a_reset_with_a_request_in_flight_never_lands_it_on_the_new_world() {
    let mut sim = new_sim();
    sim.enable_off_thread_staging();
    submit_cut(&mut sim, 1, 10, 8, 3);
    // The first tick starts staging it on the worker.
    sim.tick().unwrap();
    assert!(!sim.is_idle(), "the request is in flight");

    let fresh = new_sim();
    let fresh_hash = fresh.world().world_hash();
    sim.replace_world(fresh).unwrap();
    assert!(
        sim.stages_off_thread(),
        "a reset keeps staging off the tick thread"
    );

    // Give the old job ample time to finish and be (wrongly) installed.
    for _ in 0..200 {
        sim.tick().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(sim.is_idle());
    assert!(sim.committed(RequestId(1)).is_none());
    assert_eq!(
        sim.world().world_hash(),
        fresh_hash,
        "the new world is untouched by the old world's request"
    );
}

#[test]
fn an_idle_pipeline_stays_idle_and_ticks_cheaply() {
    let mut sim = new_sim();
    sim.enable_off_thread_staging();
    assert!(sim.is_idle());
    for _ in 0..20 {
        let report = sim.tick().unwrap();
        assert_eq!(report.pending_after, 0);
        assert!(report.committed.is_empty());
    }
    assert!(sim.is_idle());
}

fn warm_for_current_state(sim: &Simulation) -> bool {
    let world = sim.world();
    let terrain = world.terrain_volume_id();
    let stamp = world.volume_ref(terrain).unwrap().state_stamp();
    world.warm_structure_index(terrain, stamp).is_some()
}

#[test]
fn an_idle_pipeline_prewarms_the_terrain_index_and_the_first_cut_reuses_it() {
    let mut sim = new_sim();
    sim.enable_off_thread_staging();
    assert!(!warm_for_current_state(&sim));

    let deadline = Instant::now() + Duration::from_secs(60);
    while !warm_for_current_state(&sim) {
        assert!(Instant::now() < deadline, "the index was never prewarmed");
        sim.tick().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        sim.is_idle(),
        "a prewarm is not edit work: the pipeline stays idle"
    );

    let reuses_before = spall_sim::warm_index_reuses();
    submit_cut(&mut sim, 1, 10, 8, 3);
    tick_until_idle(&mut sim);
    assert!(sim.committed(RequestId(1)).is_some());
    assert!(
        spall_sim::warm_index_reuses() > reuses_before,
        "the first cut staged against the prewarmed index"
    );
    assert!(
        warm_for_current_state(&sim),
        "a non-splitting commit leaves the index warm for the next cut"
    );
}

#[test]
fn a_cut_submitted_while_the_prewarm_runs_still_commits() {
    let mut sim = new_sim();
    sim.enable_off_thread_staging();
    // The first tick finds the pipeline idle and starts the prewarm.
    sim.tick().unwrap();
    submit_cut(&mut sim, 1, 10, 8, 3);
    tick_until_idle(&mut sim);
    assert!(sim.committed(RequestId(1)).is_some());
}

#[test]
fn an_index_lost_to_a_failed_staging_is_rebuilt_while_idle() {
    let mut sim = new_sim();
    sim.enable_off_thread_staging();
    let terrain = sim.world().terrain_volume_id();
    // The floor is one brick. With it evicted and no backing to reload it from, a cut over it
    // fails at staging for want of geometry.
    sim.world_mut()
        .evict_brick(terrain, spall_core::BrickCoord::new(0, 0, 0))
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    let wait_warm = |sim: &mut Simulation, what: &str| {
        while !warm_for_current_state(sim) {
            assert!(
                Instant::now() < deadline,
                "{what}: no index was ever prewarmed"
            );
            sim.tick().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    wait_warm(&mut sim, "before the failing edit");

    // The staging takes the warm index to edit it, then fails.
    submit_cut(&mut sim, 1, 10, 8, 3);
    tick_until_idle(&mut sim);
    assert!(
        sim.committed(RequestId(1)).is_none(),
        "the cut could not commit"
    );

    // Nothing changed the volume, but the index went with the failed staging: an idle tick
    // builds another instead of treating the state as already tried.
    wait_warm(&mut sim, "after the failing edit");
}

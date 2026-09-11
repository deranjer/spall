//! T23 / G4 eight-client workload primitives (row 12), proved on the real
//! `Simulation` in isolation — not just via the multi-process scenario
//! (`fixtures/scenarios/t23-g4-workload.json` under `cargo xtask scenario
//! --name t23-g4-workload`).
//!
//! Covers: the 256 active / 64-near-observer / 4096-sleeping body population
//! (`g4_body_population_matches_the_required_counts`), a sustained ~10
//! committed-edits/s stream (`g4_edit_stream_sustains_ten_committed_edits_per_second`),
//! a single 4 m diameter destructive edit
//! (`g4_blast_clears_a_substantial_volume`), and the one 64-brick connected
//! collapse attempted for real (`giant_collapse_attempt`, `#[ignore]`d — see
//! its doc comment and `docs/reports/G3.md` for why and for the measured
//! outcome).

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures::{
    self, G4_ACTIVE_BODY_COUNT, G4_NEAR_OBSERVER_BODY_COUNT, G4_NEAR_OBSERVER_RADIUS_M,
    G4_SLEEPING_BODY_COUNT, G4_WORKLOAD_SPAWNS,
};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig};

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn actor() -> EntityId {
    EntityId::new(1).unwrap()
}

/// Row 12: "256 active bodies (64 near one observer), 4096 sleeping
/// persistent bodies" — built for real by `spawn_g4_workload_bodies`, not
/// merely requested.
#[test]
fn g4_body_population_matches_the_required_counts() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::g4_workload_setup())).unwrap();
    let observer = G4_WORKLOAD_SPAWNS[0];
    let counts = fixtures::spawn_g4_workload_bodies(sim.world_mut(), observer);

    assert_eq!(counts.active_total, G4_ACTIVE_BODY_COUNT);
    assert_eq!(counts.active_near_observer, G4_NEAR_OBSERVER_BODY_COUNT);
    assert_eq!(counts.sleeping_total, G4_SLEEPING_BODY_COUNT);

    let total_bodies = G4_ACTIVE_BODY_COUNT + G4_SLEEPING_BODY_COUNT;
    assert_eq!(sim.world().body_count(), total_bodies);
    assert_eq!(
        sim.world().dormant_body_count(),
        G4_SLEEPING_BODY_COUNT,
        "every sleeping body is dormant (T21) -- no physics-step cost"
    );

    // Every body's record carries real, persisted multi-cell geometry, not
    // merely a count: walk the live world and re-derive both sub-counts.
    let mut awake = 0usize;
    let mut near = 0usize;
    for b in sim.world().bodies() {
        let entity = b.entity.expect("detached body carries an entity id");
        if sim.world().body_is_dormant(entity) {
            assert!(b.sleeping, "a dormant body is recorded sleeping");
            assert_eq!(b.linvel_m_s, [0.0; 3], "a dormant body has zero velocity");
        } else {
            awake += 1;
            let p = b.pose.translation_m;
            let d = ((p[0] - observer[0]).powi(2)
                + (p[1] - observer[1]).powi(2)
                + (p[2] - observer[2]).powi(2))
            .sqrt();
            if d <= G4_NEAR_OBSERVER_RADIUS_M {
                near += 1;
            }
        }
    }
    assert_eq!(awake, G4_ACTIVE_BODY_COUNT);
    assert_eq!(
        near, G4_NEAR_OBSERVER_BODY_COUNT,
        "the required active bodies sit within {G4_NEAR_OBSERVER_RADIUS_M} m of the observer"
    );
}

/// Row 12: "10 edits/s sustained" — a real edit stream against the terrain, at
/// a strict 10-committed-edits-per-second cadence (one every 6 ticks at the
/// fixed 60 Hz tick rate) for 2 seconds.
#[test]
fn g4_edit_stream_sustains_ten_committed_edits_per_second() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::g4_workload_setup())).unwrap();

    const TICKS_PER_EDIT: u64 = 6; // 60 Hz / 10 per second
    const SECONDS: u64 = 2;
    const EDIT_COUNT: u64 = SECONDS * 10;

    // West floor top layer, one cell in from every edge (a radius-1 sphere
    // must not spill past the world's `x, z >= 0` bound) and avoiding the
    // column footprint (x 10..=11, z 3..=4), so every scripted edit finds
    // fresh solid material with nothing rejected as out of bounds.
    let cells: Vec<(i64, i64)> = (1i64..23)
        .flat_map(|x| (1i64..7).map(move |z| (x, z)))
        .filter(|&(x, z)| !((10..=11).contains(&x) && (3..=4).contains(&z)))
        .collect();
    assert!(
        cells.len() as u64 >= EDIT_COUNT,
        "enough distinct floor cells"
    );

    let mut req = 1u64;
    let mut committed = 0u64;
    for tick in 0..(EDIT_COUNT * TICKS_PER_EDIT) {
        if tick % TICKS_PER_EDIT == 0 && req <= EDIT_COUNT {
            let (x, z) = cells[(req - 1) as usize];
            sim.submit(EditIntent::cut(
                RequestId(req),
                actor(),
                EditTarget::Terrain,
                brush_cell(x, 1, z, 1),
            ))
            .unwrap();
            req += 1;
        }
        let report = sim.tick().unwrap();
        committed += report.committed.len() as u64;
        assert!(
            report.rejected.is_empty(),
            "tick {tick}: unexpected rejection(s): {:?}",
            report.rejected
        );
    }
    // Drain anything still staged/queued past the last submit.
    for _ in 0..32 {
        let report = sim.tick().unwrap();
        committed += report.committed.len() as u64;
    }

    assert_eq!(
        committed, EDIT_COUNT,
        "every scripted edit at the 10 committed-edits/s cadence actually committed"
    );
}

/// Row 12: "one 4 m diameter blast every 10 s" — one real destructive edit at
/// the specified scale (radius 8 cells = 2 m = a 4 m diameter), clearing a
/// substantial solid volume in a single commit.
#[test]
fn g4_blast_clears_a_substantial_volume() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::g4_workload_setup())).unwrap();

    let before = sim.world().total_solid_cells();
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush_cell(16, 9, 8, 8), // 8 cells = 2 m radius = 4 m diameter
    ))
    .unwrap();
    let reports = sim.run_until_idle(32).unwrap();
    let committed: u64 = reports.iter().map(|r| r.committed.len() as u64).sum();
    assert!(committed >= 1, "the blast produces at least one commit");

    let after = sim.world().total_solid_cells();
    assert!(
        after < before,
        "the blast removed solid material ({before} -> {after})"
    );
    assert!(
        before - after >= 50,
        "a 4 m diameter blast should clear a substantial volume, not a graze ({before} -> {after})"
    );
}

/// Row 12's "one 64-brick connected collapse", attempted **for real** against
/// [`fixtures::giant_collapse_setup`] (see its doc comment for why this runs
/// standalone rather than inside [`fixtures::g4_workload_setup`]'s world).
///
/// Measured result (see `docs/reports/G3.md` row 12 for the full writeup):
/// this genuinely commits — the column cut detaches the whole 64-brick,
/// `2 097 152`-cell block as exactly one body in one transaction. The
/// dominant cost is the structural connectivity / support analysis and
/// collider build over ~2M live cells (several seconds of wall time, not
/// tick count: `run_until_idle` resolves it within its first few ticks) —
/// `#[ignore]`d so this one stress case does not slow every `cargo test
/// --workspace` run; it is real, reproducible evidence, not a skipped
/// requirement:
///
/// ```sh
/// cargo test -p spall_sim --test g4_workload -- --ignored --nocapture giant_collapse
/// ```
#[test]
#[ignore = "real-scale 64-brick (2M-cell) structural collapse; several seconds of structural-analysis wall time. See docs/reports/G3.md row 12."]
fn giant_collapse_attempt() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::giant_collapse_setup())).unwrap();
    assert_eq!(sim.world().body_count(), 0, "block starts attached");

    let t0 = std::time::Instant::now();
    // Column: x 3..=4, z 3..=4, y 2..=31. A radius-3 sphere centred mid-column
    // severs it completely.
    let submit = sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush_cell(3, 16, 3, 3),
    ));
    println!(
        "giant_collapse_attempt: submit = {submit:?} ({:?})",
        t0.elapsed()
    );
    let status = submit.expect("submit is only rejected for malformed input");

    let t1 = std::time::Instant::now();
    let outcome = sim.run_until_idle(120);
    let elapsed = t1.elapsed();

    let reports = outcome.unwrap_or_else(|e| {
        panic!("giant_collapse_attempt: run_until_idle failed after {elapsed:?}: {e:?}")
    });
    let committed: u64 = reports.iter().map(|r| r.committed.len() as u64).sum();
    let rejected: Vec<String> = reports
        .iter()
        .flat_map(|r| r.rejected.iter().map(|(_, reason)| reason.clone()))
        .collect();
    // Which commit path actually fired: T17 increment 1's inline
    // `SplitOffBaseline` (the compressed blob fit one control record) or
    // increment 2's `SplitOffBulkBaseline` + out-of-band `BaselineWorld` (the
    // literal ENG-64 "G1 64-brick collapse" path, for a blob that did not).
    // Evidence for which of the two this fixture actually exercises — not
    // just that *a* commit path exists.
    let used_bulk_baseline = reports
        .iter()
        .flat_map(|r| r.committed.iter())
        .any(|(_, c)| c.bulk_baseline.is_some());
    let op_kinds: Vec<&'static str> = reports
        .iter()
        .flat_map(|r| r.committed.iter())
        .flat_map(|(_, c)| c.topology.ops.iter())
        .map(|op| match op {
            spall_protocol::TopologyOp::SplitOffBaseline { .. } => "SplitOffBaseline",
            spall_protocol::TopologyOp::SourcePatchBaseline { .. } => "SourcePatchBaseline",
            spall_protocol::TopologyOp::SplitOffBulkBaseline { .. } => "SplitOffBulkBaseline",
            spall_protocol::TopologyOp::SourcePatchBulkBaseline { .. } => "SourcePatchBulkBaseline",
            spall_protocol::TopologyOp::SplitOff { .. } => "SplitOff",
            spall_protocol::TopologyOp::CellRun { .. } => "CellRun",
            spall_protocol::TopologyOp::IntegerBrush { .. } => "IntegerBrush",
        })
        .collect();
    println!(
        "giant_collapse_attempt: run_until_idle took {elapsed:?}, committed={committed} \
         rejected={rejected:?} bodies_after={} world_hash={} used_bulk_baseline={used_bulk_baseline} \
         op_kinds={op_kinds:?}",
        sim.world().body_count(),
        sim.world().world_hash()
    );
    assert_eq!(
        committed, 1,
        "the column cut should commit exactly once (status was {status:?}); rejections: {rejected:?}"
    );
    assert_eq!(
        sim.world().body_count(),
        1,
        "the whole 64-brick block detaches as exactly one body"
    );
    let child = sim
        .world()
        .bodies()
        .next()
        .expect("the detached body exists");
    let solid: u64 = child
        .volume
        .resident_brick_coords()
        .iter()
        .map(|&c| {
            let snap = child.volume.snapshot_brick(c).unwrap().unwrap();
            (0..spall_core::CELLS_PER_BRICK as u16)
                .filter(|&i| {
                    !snap
                        .get(spall_core::LocalCell::from_linear_index(i).unwrap())
                        .is_air()
                })
                .count() as u64
        })
        .sum();
    let block_cells = 4 * 4 * 4 * spall_core::CELLS_PER_BRICK as u64;
    // `>=`, not `==`: the sphere cut severs the column partway up, so the
    // stub above the cut (a handful of cells) detaches together with the
    // block it holds up.
    assert!(
        solid >= block_cells,
        "the detached body should keep at least the block's full 64 bricks worth of solid \
         cells ({solid} < {block_cells})"
    );
    assert!(
        solid - block_cells < 200,
        "the only extra cells should be the short column stub above the cut, not something \
         unexpected ({solid} vs block {block_cells})"
    );
}

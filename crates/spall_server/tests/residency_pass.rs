//! T23 / G3 row 7, slice D — the serve-loop residency pass, run against a live
//! `Simulation`, must reach **the same** committed world as a fully-resident run
//! from the identical edit sequence: same `world_hash`, same conservation, same
//! per-transaction `result_hashes` — while actually evicting and reloading
//! terrain bricks.

use std::sync::Arc;

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, EntityId, GlobalCell, SphereBrush};
use spall_protocol::{Hash32, RequestId};
use spall_server::{ResidencyLimits, ResidencyPass};
use spall_sim::{EditIntent, EditTarget, MemoryBacking, Simulation, SimulationConfig, fixtures};

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
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 1,
        },
    );

    // A single stationary player in the west region; the east region is out of
    // interest and gets evicted, then reloaded for the east cut.
    let player_feet = [(1u64, [1.0_f64, 1.0, 1.0])];

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
        pass.note_pipeline_reloads(report.reloaded_bricks.iter().copied());
        pass.run(sim.world_mut(), &player_feet, &Default::default());
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

/// T23 / G3 row 7 follow-up (post-merge review P2/P3): `ResidencyPass::capture_checkpoint`
/// must produce a checkpoint that recovers the complete world even while
/// terrain sits evicted at capture time, *and* it must do so without
/// reinstalling that evicted geometry into the live world first — the old
/// `reload_all`-before-`persist::capture` path this replaced defeated
/// residency's whole point by spiking memory back to the full world on every
/// checkpoint.
#[test]
fn capture_checkpoint_round_trips_through_real_persistence_while_bricks_stay_evicted() {
    use spall_physics::PhysicsConfig;
    use spall_server::persist::{self, PersistConfig};
    use spall_store::Writer;
    use spall_structure::AnchorPlane;

    let cfg = PersistConfig {
        world_id: 0x5A11_0000_0000_C0DE,
        seed: 11,
        generator_version: 1,
    };

    let mut sim = sim();
    let terrain = sim.world().terrain_volume_id();
    let mut pass = ResidencyPass::install(
        sim.world_mut(),
        ResidencyLimits {
            budget_bricks: 4,
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 1,
        },
    );
    let player_feet = [(1u64, [1.0_f64, 1.0, 1.0])];

    let mut next = 0usize;
    for tick in 1..=180u64 {
        while next < SCRIPT.len() && SCRIPT[next].0 == tick {
            let (_, cell, r) = SCRIPT[next];
            sim.submit(cut(next as u64 + 1, cell, r)).unwrap();
            next += 1;
        }
        let report = sim.tick().unwrap();
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
        pass.note_pipeline_reloads(report.reloaded_bricks.iter().copied());
        pass.run(sim.world_mut(), &player_feet, &Default::default());
    }

    assert!(
        sim.world().has_evicted(),
        "the run must still have evicted terrain at capture time -- otherwise this test proves nothing"
    );
    let resident_before = sim.world().terrain().volume.resident_brick_count();
    let expected_hash = sim.world().world_hash();
    let expected_solid = sim.world().total_solid_cells();

    let checkpoint = pass
        .capture_checkpoint(&sim, &cfg, sim.journal_cursor())
        .expect("bounded capture succeeds from the durable backing");

    // Capturing a checkpoint must not touch the live world's residency.
    assert_eq!(
        sim.world().terrain().volume.resident_brick_count(),
        resident_before,
        "capture_checkpoint must not reinstall evicted geometry into the live world"
    );
    assert!(
        sim.world().has_evicted(),
        "eviction must still hold after capture"
    );

    let dir = std::env::temp_dir().join(format!(
        "spall_residency_capture_checkpoint_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("world.db");
    {
        let mut w = Writer::open(&db).unwrap();
        w.publish_checkpoint(&checkpoint).unwrap();
    }
    let recovery = spall_store::recover(&db).unwrap();
    let (recovered, _) = persist::restore(
        &recovery,
        &cfg,
        persist::RecoveryChoice::RequireClean,
        fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .expect("a checkpoint captured with live evictions recovers cleanly");

    assert_eq!(
        recovered.world().world_hash(),
        expected_hash,
        "recovered world_hash must match the live logical hash despite evictions at capture time"
    );
    assert_eq!(
        recovered.world().total_solid_cells(),
        expected_solid,
        "recovered conservation must match"
    );
    // Recovery has no backing, so its brick set is exactly the checkpoint's,
    // which must be the *complete* world -- the evicted bricks really did
    // come back from the durable backing, not get silently dropped.
    assert!(
        !recovered.world().has_evicted(),
        "a freshly-recovered world starts fully resident"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A durable record lost or never captured must fail the whole checkpoint
/// capture closed, not publish one whose `world_hash` (logical, so it already
/// counts every evicted brick) claims geometry its `bricks` do not actually
/// carry.
#[test]
fn capture_checkpoint_fails_closed_on_a_missing_durable_record() {
    use spall_server::persist::{self, PersistConfig};

    let cfg = PersistConfig {
        world_id: 0x5A11_0000_0000_C0DE,
        seed: 11,
        generator_version: 1,
    };

    let mut sim = sim();
    let terrain = sim.world().terrain_volume_id();
    let backing = Arc::new(MemoryBacking::default());
    let mut pass = ResidencyPass::install_with_backing(
        sim.world_mut(),
        ResidencyLimits {
            budget_bricks: 4,
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 1,
        },
        backing.clone(),
    );
    let player_feet = [(1u64, [1.0_f64, 1.0, 1.0])];

    let mut next = 0usize;
    for tick in 1..=180u64 {
        while next < SCRIPT.len() && SCRIPT[next].0 == tick {
            let (_, cell, r) = SCRIPT[next];
            sim.submit(cut(next as u64 + 1, cell, r)).unwrap();
            next += 1;
        }
        let report = sim.tick().unwrap();
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
        pass.note_pipeline_reloads(report.reloaded_bricks.iter().copied());
        pass.run(sim.world_mut(), &player_feet, &Default::default());
    }

    let evicted_coord = sim
        .world()
        .evicted(terrain)
        .iter()
        .next()
        .map(|(c, _)| c)
        .expect("the run must still have evicted terrain, or this test proves nothing");
    backing.mark_unavailable(terrain, evicted_coord);

    let err = pass
        .capture_checkpoint(&sim, &cfg, sim.journal_cursor())
        .expect_err("a missing durable record for an evicted brick must fail capture");
    assert!(
        matches!(
            err,
            persist::PersistError::EvictedBrickUnavailable { volume, coord }
            if volume == terrain.get() && coord == [evicted_coord.x, evicted_coord.y, evicted_coord.z]
        ),
        "expected EvictedBrickUnavailable naming the exact brick, got {err:?}"
    );
}

/// T23 / G3 row 7 increment 16 (durable exact-revision backing
/// acknowledgement, audit + fix). The audit found `capture_checkpoint`
/// trusted whatever the backing offered for an evicted brick without
/// checking it against the retained `(revision, content_hash)` digest --
/// unlike `SimWorld::reload_brick`, which validates a backing candidate with
/// `EvictedBricks::verify_candidate` before ever publishing it. This is the
/// fault-injection test that falsifies durability directly: it forges a
/// backing record for an evicted coord that disagrees with the digest the
/// eviction actually retained (the exact shape a disk backing's
/// `synchronous=NORMAL` write rolled back by an OS/power crash, or any other
/// silent divergence, would produce) and confirms `capture_checkpoint` now
/// fails closed instead of writing the mismatched bytes into a checkpoint
/// that `Writer::publish_checkpoint` would then durably (`synchronous=FULL`)
/// commit under the *correct* recorded `world_hash`.
#[test]
fn capture_checkpoint_fails_closed_on_a_backing_record_that_disagrees_with_the_retained_digest() {
    use spall_server::persist::{self, PersistConfig};

    let cfg = PersistConfig {
        world_id: 0x5A11_0000_0000_C0DE,
        seed: 11,
        generator_version: 1,
    };

    let mut sim = sim();
    let terrain = sim.world().terrain_volume_id();
    let backing = Arc::new(MemoryBacking::default());
    let mut pass = ResidencyPass::install_with_backing(
        sim.world_mut(),
        ResidencyLimits {
            budget_bricks: 4,
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 1,
        },
        backing.clone(),
    );
    let player_feet = [(1u64, [1.0_f64, 1.0, 1.0])];

    let mut next = 0usize;
    for tick in 1..=180u64 {
        while next < SCRIPT.len() && SCRIPT[next].0 == tick {
            let (_, cell, r) = SCRIPT[next];
            sim.submit(cut(next as u64 + 1, cell, r)).unwrap();
            next += 1;
        }
        let report = sim.tick().unwrap();
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
        pass.note_pipeline_reloads(report.reloaded_bricks.iter().copied());
        pass.run(sim.world_mut(), &player_feet, &Default::default());
    }

    let (evicted_coord, retained_digest) = sim
        .world()
        .evicted(terrain)
        .iter()
        .next()
        .expect("the run must still have evicted terrain, or this test proves nothing");

    // Overwrite the backing's record for this exact coord with real-looking
    // geometry at a revision the retained digest never agreed to -- a stand-in
    // for a durable record that silently drifted from what eviction actually
    // captured (a lost/rolled-back disk write, or any other bug), not the
    // simpler "no record at all" case the neighbouring test already covers.
    backing.insert(
        terrain,
        evicted_coord,
        spall_voxel::Brick::uniform(
            spall_core::MaterialId(1),
            spall_core::Revision(retained_digest.revision.get() + 1000),
        ),
    );

    let err = pass
        .capture_checkpoint(&sim, &cfg, sim.journal_cursor())
        .expect_err(
            "a backing record that disagrees with the retained digest must fail capture, \
             not silently enter the durable checkpoint",
        );
    assert!(
        matches!(
            &err,
            persist::PersistError::EvictedBrickDigestMismatch { volume, coord, retained_revision, backing_revision, .. }
            if *volume == terrain.get()
                && *coord == [evicted_coord.x, evicted_coord.y, evicted_coord.z]
                && *retained_revision == retained_digest.revision.get()
                && *backing_revision == retained_digest.revision.get() + 1000
        ),
        "expected EvictedBrickDigestMismatch naming the exact brick and both revisions, got {err:?}"
    );
}

/// T23 / G3 row 7 — ack-before-evict. Closes the gap the post-merge review
/// (P2/P3) and increment 22's own "still open" note flagged against T18's
/// `ResidencyController::enforce_budget` contract ("persist dirty candidates
/// synchronously ... a backing error leaves geometry resident"): before this
/// fix, `ResidencyPass::run` evicted a brick from the live cache on interest/
/// hysteresis alone, trusting that some earlier `on_commit` capture had
/// already reached the backing — never checking at the moment it mattered.
/// This proves the pass now gates each eviction on a fresh, successful
/// capture taken immediately beforehand, and that a failed one leaves the
/// brick resident (not silently evicted with stale or absent durable
/// geometry) until a later tick's capture succeeds.
#[test]
fn a_failed_capture_leaves_the_brick_resident_instead_of_evicting_it() {
    let mut sim = sim();
    let terrain = sim.world().terrain_volume_id();
    let backing = Arc::new(MemoryBacking::default());
    let mut pass = ResidencyPass::install_with_backing(
        sim.world_mut(),
        ResidencyLimits {
            budget_bricks: 4,
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 1,
        },
        backing.clone(),
    );
    // Stationary west player; the east region (script cut cell [82, 6, 75],
    // same as the other tests here) is out of interest from tick 1.
    let player_feet = [(1u64, [1.0_f64, 1.0, 1.0])];
    let east_coord = GlobalCell::new(82, 6, 75).split().0;
    assert!(
        sim.world()
            .terrain()
            .volume
            .resident_brick_coords()
            .contains(&east_coord),
        "the east region brick must start resident, or this test proves nothing"
    );

    // Poison the one capture that would gate this brick's eviction. No cuts
    // are submitted -- residency alone drives this test.
    backing.fail_next_capture(terrain, east_coord);

    // `EVICT_SETTLE_TICKS` (4, private to `residency_pass`) consecutive
    // out-of-interest ticks before an eviction is even attempted; the brick
    // is out of interest from tick 1, so the first attempt lands on tick 4.
    for _ in 1..=4u64 {
        sim.tick().unwrap();
        pass.run(sim.world_mut(), &player_feet, &Default::default());
    }
    assert!(
        !sim.world().evicted(terrain).contains(east_coord),
        "the poisoned capture must have blocked eviction"
    );
    assert!(
        sim.world()
            .terrain()
            .volume
            .resident_brick_coords()
            .contains(&east_coord),
        "a failed backing write must leave the brick resident, not evict it"
    );

    // The poison was one-shot: the very next tick's capture succeeds, and the
    // brick evicts normally.
    sim.tick().unwrap();
    pass.run(sim.world_mut(), &player_feet, &Default::default());
    assert!(
        sim.world().evicted(terrain).contains(east_coord),
        "once the backing write succeeds, the brick must evict on the next attempt"
    );
    assert!(
        !sim.world()
            .terrain()
            .volume
            .resident_brick_coords()
            .contains(&east_coord),
        "an evicted brick must not still be resident"
    );
}

/// T23 / G3 row 7, item 1 (unification): `ResidencyPass` is written once
/// against `spall_sim::BrickBackingWriter`, so a real disk-backed
/// `DiskBrickBacking` (item 2) must reach exactly the same committed world as
/// the default in-process `MemoryBacking` -- this is the same assertion as
/// `residency_on_reaches_the_same_committed_world_as_residency_off`, just with
/// `ResidencyPass::install_with_backing` and a real SQLite file standing in
/// for `install`'s `MemoryBacking`.
#[test]
fn residency_on_a_disk_backing_reaches_the_same_committed_world_as_residency_off() {
    use spall_server::DiskBrickBacking;

    let (off_hash, off_solid, off_trace) = run_without_residency();

    let dir = std::env::temp_dir().join(format!(
        "spall_residency_pass_disk_backing_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let backing = Arc::new(DiskBrickBacking::open(dir.join("residency.db")).unwrap());

    let mut sim = sim();
    let terrain = sim.world().terrain_volume_id();
    let mut pass = ResidencyPass::install_with_backing(
        sim.world_mut(),
        ResidencyLimits {
            budget_bricks: 4,
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 1,
        },
        backing,
    );
    let player_feet = [(1u64, [1.0_f64, 1.0, 1.0])];
    let initial_resident = sim.world().terrain().volume.resident_brick_count();

    let mut next = 0usize;
    for tick in 1..=180u64 {
        while next < SCRIPT.len() && SCRIPT[next].0 == tick {
            let (_, cell, r) = SCRIPT[next];
            sim.submit(cut(next as u64 + 1, cell, r)).unwrap();
            next += 1;
        }
        let report = sim.tick().unwrap();
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
        pass.note_pipeline_reloads(report.reloaded_bricks.iter().copied());
        pass.run(sim.world_mut(), &player_feet, &Default::default());
    }

    for r in 1..=SCRIPT.len() as u64 {
        assert!(
            sim.committed(RequestId(r)).is_some(),
            "cut {r} did not commit under disk-backed residency"
        );
    }
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

    let stats = pass.stats();
    assert!(
        stats.evictions_total > 0,
        "the disk-backed pass never evicted anything"
    );
    assert!(stats.resident_terrain_bricks_min < initial_resident);
    assert!(
        pass.backing_disk_bytes().unwrap() > 0,
        "a real disk backing must report a nonzero on-disk footprint once it has captured bricks"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// T23 / G3 row 7, item 2's exact ask: "a brick captured to disk survives a
/// process restart and reloads correctly". This drives the capture through
/// the real `ResidencyPass` eviction path (not a hand-built volume, as in
/// `spall_server::disk_backing`'s own unit tests), then simulates a process
/// restart by dropping every in-process handle to the backing and reopening a
/// fresh `DiskBrickBacking` at the same path -- the only thing surviving is
/// what actually reached disk.
#[test]
fn a_brick_captured_to_disk_survives_a_process_restart_and_reloads_correctly() {
    use spall_server::DiskBrickBacking;
    use spall_sim::{BackingBrick, BrickBacking};

    let dir = std::env::temp_dir().join(format!(
        "spall_residency_pass_disk_restart_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("residency.db");

    let east_coord = GlobalCell::new(82, 6, 75).split().0;
    let terrain_id;
    let expected;
    {
        let backing = Arc::new(DiskBrickBacking::open(&db).unwrap());
        let mut sim = sim();
        let terrain = sim.world().terrain_volume_id();
        terrain_id = terrain;
        let mut pass = ResidencyPass::install_with_backing(
            sim.world_mut(),
            ResidencyLimits {
                budget_bricks: 4,
                max_dense_bytes: u64::MAX,
                interest_radius_bricks: 1,
            },
            backing,
        );
        let player_feet = [(1u64, [1.0_f64, 1.0, 1.0])];
        // No cuts submitted -- run just long enough for the stationary-west
        // player's out-of-interest east region to clear `EVICT_SETTLE_TICKS`
        // and evict, exactly as in `a_failed_capture_leaves_...` above.
        for _ in 1..=6u64 {
            sim.tick().unwrap();
            pass.run(sim.world_mut(), &player_feet, &Default::default());
        }
        assert!(
            sim.world().evicted(terrain).contains(east_coord),
            "the east brick must be evicted through the disk backing, or this test proves nothing"
        );
        expected = match pass.backing().load(terrain, east_coord) {
            BackingBrick::Loaded(brick) => brick,
            other => panic!("expected a loaded backing brick before the restart, got {other:?}"),
        };
        // `sim`, `pass`, and every `Arc<DiskBrickBacking>` clone are dropped
        // here at the end of this block -- the SQLite connection closes.
    }

    // "Process restart": a brand new `DiskBrickBacking` at the same path,
    // nothing carried over in memory.
    let reopened = DiskBrickBacking::open(&db).unwrap();
    let BackingBrick::Loaded(reloaded) = reopened.load(terrain_id, east_coord) else {
        panic!("the evicted brick's durable record must survive reopening the store");
    };
    assert_eq!(reloaded.revision(), expected.revision());
    assert_eq!(reloaded.is_edited(), expected.is_edited());
    for i in 0..spall_core::CELLS_PER_BRICK as u16 {
        let local = spall_core::LocalCell::from_linear_index(i).unwrap();
        assert_eq!(
            reloaded.get(local),
            expected.get(local),
            "cell {i} did not survive the restart"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// ENG-30 row 7 increment 13: a queued (not yet staged/committed) edit's
/// dependency footprint must stay resident for as long as it is queued, even
/// though it sits far outside every player's interest box and well past the
/// ordinary settle hysteresis. This is the "preflight, consumer-lifetime"
/// pinning the frozen contract calls for, distinct from the reactive
/// `EvictedGeometryRequired` reload/retry that already existed.
#[test]
fn a_pending_edits_dependency_bricks_stay_resident_while_queued() {
    let mut sim = sim();
    let terrain = sim.world().terrain_volume_id();
    let mut pass = ResidencyPass::install(
        sim.world_mut(),
        ResidencyLimits {
            budget_bricks: 100,
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 0,
        },
    );
    // A stationary west player; the east cut target is nowhere near it.
    let player_feet = [(1u64, [1.0_f64, 1.0, 1.0])];
    let target = GlobalCell::new(82, 6, 75).split().0;
    assert!(
        sim.world()
            .terrain()
            .volume
            .resident_brick_coords()
            .contains(&target),
        "the target brick must start resident, or this test proves nothing"
    );

    // Queue the east cut but never tick -- it stays in the pipeline's pending
    // queue, not yet staged or committed, for the whole test.
    sim.submit(cut(1, [82, 6, 75], 2)).unwrap();

    // Run the pass many times past `EVICT_SETTLE_TICKS` (4, private to this
    // module) with the pending edit's dependency bricks supplied every time.
    for _ in 0..10 {
        let pending = sim.pending_edit_bricks(terrain);
        assert!(
            pending.contains(&target),
            "the queued intent's brush footprint must name its own target brick"
        );
        pass.run(sim.world_mut(), &player_feet, &pending);
    }

    assert!(
        !sim.world().evicted(terrain).contains(target),
        "a queued edit's dependency must never be evicted while it is still pending"
    );
    assert!(
        sim.world()
            .terrain()
            .volume
            .resident_brick_coords()
            .contains(&target)
    );

    // Control: the identical setup and tick count, but the pass is never told
    // about the pending edit -- ordinary hysteresis must evict the brick,
    // proving the pin above (not some other effect) is what protected it.
    let mut sim2 = crate::sim();
    let terrain2 = sim2.world().terrain_volume_id();
    let mut pass2 = ResidencyPass::install(
        sim2.world_mut(),
        ResidencyLimits {
            budget_bricks: 100,
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 0,
        },
    );
    for _ in 0..10 {
        pass2.run(sim2.world_mut(), &player_feet, &Default::default());
    }
    assert!(
        sim2.world().evicted(terrain2).contains(target),
        "without the pin, ordinary hysteresis must evict the same out-of-interest brick"
    );
}

/// ENG-30 row 7 increment 13: a fast player movement's swept path must keep a
/// brick it crosses resident for the tick it crosses, even one that has
/// already sat out of interest long enough to be otherwise eligible for
/// eviction -- swept-collision movement must never see evicted geometry
/// sampled as air mid-sweep.
#[test]
fn a_players_swept_path_pins_a_brick_it_crosses_even_past_the_settle_window() {
    let mut sim = sim();
    let terrain = sim.world().terrain_volume_id();
    let mut pass = ResidencyPass::install(
        sim.world_mut(),
        ResidencyLimits {
            budget_bricks: 100,
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 0,
        },
    );
    let crossed = GlobalCell::new(82, 6, 75).split().0;
    assert!(
        sim.world()
            .terrain()
            .volume
            .resident_brick_coords()
            .contains(&crossed),
        "the crossed brick must start resident, or this test proves nothing"
    );

    // Stationary west player for up to (but not including) `EVICT_SETTLE_TICKS`
    // (4, private to this module) ticks: `crossed`, out of interest the whole
    // time, is not yet eligible for eviction.
    let player = 1u64;
    let west = [1.0_f64, 1.0, 1.0];
    for _ in 0..3 {
        pass.run(sim.world_mut(), &[(player, west)], &Default::default());
    }
    assert!(
        !sim.world().evicted(terrain).contains(crossed),
        "not yet past the settle window"
    );

    // Tick 4: the player instantly jumps far past the east region in one
    // step. The bounding segment from the old to the new feet must pin every
    // brick it crosses, including `crossed`, on this exact tick -- the same
    // tick its settle counter would otherwise reach the eviction threshold.
    let far_east = [30.0_f64, 2.0, 30.0];
    pass.run(sim.world_mut(), &[(player, far_east)], &Default::default());
    assert!(
        !sim.world().evicted(terrain).contains(crossed),
        "the swept path must have pinned the crossed brick on the jump tick"
    );

    // Once settled at the new position, `crossed` is simply out of interest
    // again with no more swept protection -- ordinary hysteresis evicts it
    // like any other stale brick after another `EVICT_SETTLE_TICKS` ticks.
    for _ in 0..4 {
        pass.run(sim.world_mut(), &[(player, far_east)], &Default::default());
    }
    assert!(
        sim.world().evicted(terrain).contains(crossed),
        "once no longer swept or in interest, the brick must eventually evict normally"
    );
}

/// ENG-30 row 7 increment 13: a dense-byte cap with no headroom must defer
/// (never silently admit past) reloading an evicted dense brick back into
/// interest -- the "loads happen before an `over_budget` count" gap the
/// coordinator review named. The committed world's logical hash is
/// completely unaffected by whether that brick happens to be resident.
#[test]
fn a_tight_dense_byte_cap_defers_a_desired_reload_instead_of_admitting_over_budget() {
    let (off_hash, off_solid, _) = run_without_residency();

    let mut sim = sim();
    let terrain = sim.world().terrain_volume_id();
    let mut pass = ResidencyPass::install(
        sim.world_mut(),
        ResidencyLimits {
            budget_bricks: 100,
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 1,
        },
    );
    let player_feet = [(1u64, [1.0_f64, 1.0, 1.0])]; // stationary west
    let east = GlobalCell::new(82, 6, 75).split().0;

    // Run the full script so the east cut actually materialises east's brick
    // as `Dense`, then let ordinary west-only-interest hysteresis evict it.
    let mut next = 0usize;
    for tick in 1..=180u64 {
        while next < SCRIPT.len() && SCRIPT[next].0 == tick {
            let (_, cell, r) = SCRIPT[next];
            sim.submit(cut(next as u64 + 1, cell, r)).unwrap();
            next += 1;
        }
        let report = sim.tick().unwrap();
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
        pass.note_pipeline_reloads(report.reloaded_bricks.iter().copied());
        pass.run(sim.world_mut(), &player_feet, &Default::default());
    }
    assert!(
        sim.world().evicted(terrain).contains(east),
        "east must be evicted by the end of the run, or this test proves nothing"
    );
    assert_eq!(sim.world().world_hash(), off_hash);
    assert_eq!(sim.world().total_solid_cells(), off_solid);

    // Tighten the dense-byte cap to exactly the current resident total --
    // zero headroom for east's dense brick to come back -- and give a small
    // interest radius, matching an ordinary walking approach rather than a
    // teleport (a single large jump would swept-pin the whole path as
    // *required*, which must never be admission-limited -- see
    // `a_players_swept_path_pins_a_brick_it_crosses_even_past_the_settle_window`).
    let current_dense = spall_server::total_resident_dense_bytes(sim.world());
    let mut limits = pass.limits();
    limits.max_dense_bytes = current_dense;
    limits.interest_radius_bricks = 1;
    pass.set_limits(limits);

    // Walk a second player toward (but not physically into) east's own brick
    // in small steps, stopping one brick short -- close enough for ordinary
    // proximity `interest` (radius 1) to *want* east back, but never crossing
    // into it, so the swept-collision path never itself needs to treat east
    // as *required* (that is
    // `a_players_swept_path_pins_a_brick_it_crosses_even_past_the_settle_window`'s
    // job; this test isolates the plain interest-driven admission path). A
    // first player camps at west the whole time, keeping west's own dense
    // brick resident and occupying the tight budget throughout, so the cap
    // does not simply free up on its own as the walker leaves west behind.
    let west_player = 1u64;
    let walker = 2u64;
    let west = [1.0_f64, 1.0, 1.0];
    let goal = [12.5_f64, 1.5, 18.75]; // one brick short of east, in range
    let steps = 40;
    let mut deferred = 0u64;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let feet = [
            west[0] + (goal[0] - west[0]) * t,
            west[1] + (goal[1] - west[1]) * t,
            west[2] + (goal[2] - west[2]) * t,
        ];
        let tick = pass.run(
            sim.world_mut(),
            &[(west_player, west), (walker, feet)],
            &Default::default(),
        );
        deferred += tick.admission_deferred;
    }
    assert!(
        deferred > 0,
        "the tight dense-byte cap must have deferred at least one admission"
    );
    assert!(
        sim.world().evicted(terrain).contains(east),
        "a deferred reload must leave the brick evicted, not admit it over budget"
    );
    // Logical topology is completely unaffected by residency placement.
    assert_eq!(sim.world().world_hash(), off_hash);
    assert_eq!(sim.world().total_solid_cells(), off_solid);

    // Raise the cap and confirm the same desired reload now succeeds.
    limits.max_dense_bytes = current_dense + spall_voxel::MemoryReport::DENSE_BRICK_BYTES as u64;
    pass.set_limits(limits);
    pass.run(
        sim.world_mut(),
        &[(west_player, west), (walker, goal)],
        &Default::default(),
    );
    assert!(
        !sim.world().evicted(terrain).contains(east),
        "once the cap has headroom, the previously deferred reload must succeed"
    );
}

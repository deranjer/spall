//! T16 save-integration acceptance: a real `spall_sim::Simulation` is edited,
//! settled, checkpointed, and recovered — geometry, ownership, materials, pose
//! of rotated fractured bodies, and sleeping state must survive the round trip.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use glam::{DQuat, DVec3};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{CellSizeCode, GlobalCell, SphereBrush, VolumeId};
use spall_physics::PhysicsConfig;
use spall_protocol::RequestId;
use spall_server::persist::{self, PersistConfig};
use spall_sim::{BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};
use spall_store::Writer;
use spall_structure::AnchorPlane;
use spall_voxel::{EditPlan, Volume};

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("spall_persist_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn db(&self) -> PathBuf {
        self.0.join("world.db")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cfg() -> PersistConfig {
    PersistConfig {
        world_id: 0x5A11_0000_0000_7016,
        seed: 7,
        generator_version: 1,
    }
}

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn actor() -> spall_core::EntityId {
    spall_core::EntityId::new(1).unwrap()
}

fn publish(db: &std::path::Path, cp: &spall_store::Checkpoint) {
    let mut w = Writer::open(db).unwrap();
    w.publish_checkpoint(cp).unwrap();
}

fn recover_restore(db: &std::path::Path) -> (Simulation, u64) {
    let recovery = spall_store::recover(db).unwrap();
    persist::restore(
        &recovery,
        &cfg(),
        persist::RecoveryChoice::RequireClean,
        fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap()
}

fn sleep_map(sim: &Simulation) -> Vec<(u64, bool)> {
    let mut v: Vec<(u64, bool)> = sim
        .world()
        .bodies()
        .map(|b| (b.entity.unwrap().get(), b.sleeping))
        .collect();
    v.sort();
    v
}

fn dug_is_air(sim: &Simulation, cell: GlobalCell) -> bool {
    matches!(
        sim.world()
            .volume_ref(sim.world().terrain_volume_id())
            .unwrap()
            .sample(cell)
            .unwrap(),
        spall_voxel::Sample::Empty { .. }
    )
}

// -------------------------------------------------------------------------

#[test]
fn save_restart_preserves_excavation_and_sleeping_body() {
    let s = Scratch::new("dig_sleep");

    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    // A compact stone cube resting on the slab settles and sleeps quickly.
    sim.world_mut()
        .spawn_body(
            fixtures::solid_block(4),
            BodyPose::new(DQuat::IDENTITY, [1.0, 0.5, 1.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            1,
        )
        .unwrap();
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush_cell(14, 1, 14, 2),
    ))
    .unwrap();
    sim.run_until_idle(16).unwrap();
    for _ in 0..120 {
        sim.step_physics_only();
    }

    let want_hash = sim.world().world_hash();
    let want_solid = sim.world().total_solid_cells();
    let want_sleep = sleep_map(&sim);
    let want_tick = sim.current_tick();
    assert!(
        want_sleep.iter().any(|(_, slept)| *slept),
        "the resting cube is asleep"
    );

    let dug = GlobalCell::new(14, 1, 14);
    assert!(dug_is_air(&sim, dug));

    publish(&s.db(), &persist::capture(&sim, &cfg(), 0).unwrap());
    drop(sim);

    let (restored, durable_seq) = recover_restore(&s.db());
    assert_eq!(durable_seq, 0);
    assert_eq!(restored.current_tick(), want_tick);
    assert_eq!(restored.world().total_solid_cells(), want_solid);
    assert_eq!(
        restored.world().world_hash(),
        want_hash,
        "geometry + ownership + materials recovered exactly"
    );
    assert_eq!(sleep_map(&restored), want_sleep, "sleep state recovered");
    assert!(dug_is_air(&restored, dug), "excavation persisted");
}

#[test]
fn save_restart_preserves_a_rotated_fractured_body() {
    let s = Scratch::new("rotated_split");

    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    // A dumbbell spawned rotated and airborne, cut through its bridge so it
    // fractures into two bodies that keep the parent's split-instant rotation.
    let spin = fixtures::oblique_spin();
    let body = sim
        .world_mut()
        .spawn_body(
            fixtures::dumbbell(4, 3),
            BodyPose::new(spin, [3.0, 6.0, 3.0]),
            [0.0, 0.0, 0.0],
            [0.4, 0.0, 0.0],
            2600.0,
            1,
        )
        .unwrap();
    // dumbbell(4, 3): the 1-cell bridge runs x = 4..=6 at y = z = 2 (local).
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Body(body),
        brush_cell(5, 2, 2, 1),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    assert_eq!(sim.world().body_count(), 2, "dumbbell fractured into two");
    for _ in 0..10 {
        sim.step_physics_only();
    }

    let want_hash = sim.world().world_hash();
    let want_solid = sim.world().total_solid_cells();
    let want_pose: Vec<(u64, [f64; 3], [f64; 4])> = sim
        .world()
        .bodies()
        .map(|b| {
            let q = b.pose.rotation;
            (
                b.entity.unwrap().get(),
                b.pose.translation_m,
                [q.x, q.y, q.z, q.w],
            )
        })
        .collect();

    publish(&s.db(), &persist::capture(&sim, &cfg(), 0).unwrap());
    drop(sim);

    let (restored, _) = recover_restore(&s.db());
    assert_eq!(restored.world().body_count(), 2);
    assert_eq!(restored.world().total_solid_cells(), want_solid);
    assert_eq!(
        restored.world().world_hash(),
        want_hash,
        "fractured geometry + child ownership recovered"
    );

    for (entity, trans, quat) in want_pose {
        let body = restored
            .world()
            .bodies()
            .find(|b| b.entity.unwrap().get() == entity)
            .expect("child body present after restore");
        let got_t = DVec3::from_array(body.pose.translation_m);
        assert!(
            (got_t - DVec3::from_array(trans)).length() < 1e-9,
            "body {entity} translation preserved"
        );
        let got_q = body.pose.rotation;
        let want_q = DQuat::from_xyzw(quat[0], quat[1], quat[2], quat[3]);
        // q and -q are the same rotation.
        let dot =
            (got_q.x * want_q.x + got_q.y * want_q.y + got_q.z * want_q.z + got_q.w * want_q.w)
                .abs();
        assert!(
            dot > 1.0 - 1e-9,
            "body {entity} rotation preserved (dot={dot})"
        );
    }
}

#[test]
fn checkpoint_plus_journal_suffix_recovers_topology() {
    let s = Scratch::new("journal");

    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();

        sim.submit(EditIntent::cut(
            RequestId(1),
            actor(),
            EditTarget::Terrain,
            brush_cell(10, 4, 1, 2),
        ))
        .unwrap();
        sim.run_until_idle(16).unwrap();
        assert_eq!(sim.world().body_count(), 1, "beam detached");

        let records = persist::journal_records(sim.journal().entries()).unwrap();
        assert_eq!(records.len(), 1);
        let durable = w.append_journal(&records).unwrap();
        assert_eq!(durable.journal_seq.0, records[0].seq);
    }

    let want_hash = sim.world().world_hash();
    let want_bodies = sim.world().body_count();

    let recovery = spall_store::recover(s.db()).unwrap();
    assert_eq!(recovery.journal.len(), 1);
    let (mut restored, durable_seq) = persist::restore(
        &recovery,
        &cfg(),
        persist::RecoveryChoice::RequireClean,
        fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap();

    assert_eq!(durable_seq, recovery.journal[0].seq);
    assert_eq!(restored.world().body_count(), want_bodies);
    assert_eq!(
        restored.world().world_hash(),
        want_hash,
        "replayed split reproduces server geometry + ownership"
    );

    // Ids resume past the replayed transaction: a fresh cut still commits.
    restored
        .submit(EditIntent::cut(
            RequestId(2),
            actor(),
            EditTarget::Terrain,
            brush_cell(6, 4, 1, 1),
        ))
        .unwrap();
    restored.run_until_idle(16).unwrap();
    assert!(restored.committed(RequestId(2)).is_some());
}

#[test]
fn a_manifest_hash_mismatch_is_rejected() {
    let s = Scratch::new("manifest");
    let sim = Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();
    }
    let recovery = spall_store::recover(s.db()).unwrap();

    let air_only = spall_core::MaterialManifest::validated(vec![spall_core::MaterialDef {
        id: spall_core::MaterialId::AIR,
        name: "air".into(),
        render: spall_core::RenderProps {
            albedo: [0.0; 3],
            roughness: 1.0,
            metalness: 0.0,
            emissive: [0.0; 3],
        },
        sim: spall_core::SimProps {
            density_kg_m3: 0.0,
            friction: 0.0,
            restitution: 0.0,
            hardness: 0.0,
            bond_strength: 0.0,
            flags: spall_core::MaterialFlags::NONE,
        },
    }])
    .unwrap();

    match persist::restore(
        &recovery,
        &cfg(),
        persist::RecoveryChoice::RequireClean,
        air_only,
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    ) {
        Err(persist::PersistError::ManifestMismatch { .. }) => {}
        Err(other) => panic!("expected ManifestMismatch, got {other:?}"),
        Ok(_) => panic!("expected ManifestMismatch, got a restored simulation"),
    }
}

// --- ENG-59: saved world identity + algorithm versions --------------------

/// A recovery whose single checkpoint was captured with [`cfg`].
fn recovery_for_meta_tests(s: &Scratch) -> spall_store::Recovery {
    let sim = Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    let mut w = Writer::open(s.db()).unwrap();
    w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
        .unwrap();
    drop(w);
    spall_store::recover(s.db()).unwrap()
}

/// Runs `restore` and returns the error, panicking if it unexpectedly succeeded
/// (`Simulation` has no `Debug`, so the `Ok` payload cannot be printed).
fn restore_err(recovery: &spall_store::Recovery, cfg: &PersistConfig) -> persist::PersistError {
    match persist::restore(
        recovery,
        cfg,
        persist::RecoveryChoice::RequireClean,
        fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    ) {
        Ok(_) => panic!("restore accepted a database it must have rejected"),
        Err(e) => e,
    }
}

fn restore_ok(recovery: &spall_store::Recovery, cfg: &PersistConfig) -> bool {
    persist::restore(
        recovery,
        cfg,
        persist::RecoveryChoice::RequireClean,
        fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .is_ok()
}

#[test]
fn a_mismatched_world_id_is_rejected() {
    let s = Scratch::new("world_id");
    let recovery = recovery_for_meta_tests(&s);
    let wrong = PersistConfig {
        world_id: cfg().world_id ^ 0x1,
        ..cfg()
    };
    assert!(matches!(
        restore_err(&recovery, &wrong),
        persist::PersistError::WorldIdMismatch { .. }
    ));
    // The original database is untouched: a correct config still recovers.
    assert!(restore_ok(&recovery, &cfg()));
}

#[test]
fn a_mismatched_seed_is_rejected() {
    let s = Scratch::new("seed");
    let recovery = recovery_for_meta_tests(&s);
    let wrong = PersistConfig {
        seed: cfg().seed + 1,
        ..cfg()
    };
    assert!(matches!(
        restore_err(&recovery, &wrong),
        persist::PersistError::ConfigMismatch { field: "seed", .. }
    ));
}

#[test]
fn a_mismatched_generator_version_is_rejected() {
    let s = Scratch::new("generator");
    let recovery = recovery_for_meta_tests(&s);
    let wrong = PersistConfig {
        generator_version: cfg().generator_version + 1,
        ..cfg()
    };
    assert!(matches!(
        restore_err(&recovery, &wrong),
        persist::PersistError::ConfigMismatch {
            field: "generator_version",
            ..
        }
    ));
}

#[test]
fn a_mismatched_algorithm_version_is_rejected() {
    for field in [
        "integer_brush_version",
        "structure_graph_version",
        "topology_hash_version",
    ] {
        let s = Scratch::new(&format!("algo_{field}"));
        let mut recovery = recovery_for_meta_tests(&s);
        // A checkpoint written by an incompatible build of one algorithm.
        match field {
            "integer_brush_version" => recovery.checkpoint.meta.integer_brush_version = 999,
            "structure_graph_version" => recovery.checkpoint.meta.structure_graph_version = 999,
            "topology_hash_version" => recovery.checkpoint.meta.topology_hash_version = 999,
            _ => unreachable!(),
        }
        match restore_err(&recovery, &cfg()) {
            persist::PersistError::AlgorithmVersionMismatch { field: got, .. } => {
                assert_eq!(got, field)
            }
            other => panic!("expected AlgorithmVersionMismatch({field}), got {other:?}"),
        }
    }
}

#[test]
fn a_mismatched_store_schema_version_is_rejected() {
    let s = Scratch::new("schema");
    let mut recovery = recovery_for_meta_tests(&s);
    recovery.checkpoint.meta.store_schema_version += 1;
    assert!(matches!(
        restore_err(&recovery, &cfg()),
        persist::PersistError::SchemaMismatch { .. }
    ));
}

// --- ENG-37: checkpoint + replay hash verification -----------------------

/// A recovery holding a fresh bridge checkpoint plus one durable column-cut
/// transaction in its journal suffix.
fn recovery_with_cut_journal(s: &Scratch) -> spall_store::Recovery {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    let mut w = Writer::open(s.db()).unwrap();
    w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
        .unwrap();
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush_cell(10, 4, 1, 2),
    ))
    .unwrap();
    sim.run_until_idle(16).unwrap();
    let records = persist::journal_records(sim.journal().entries()).unwrap();
    w.append_journal(&records).unwrap();
    drop(w);
    spall_store::recover(s.db()).unwrap()
}

#[test]
fn review_restore_must_check_checkpoint_hash() {
    // Decodable row corruption: flip one stored brick's tombstone bit but keep
    // the saved canonical hash. The rows still decode; recovery must notice the
    // rebuilt world no longer reproduces `checkpoint.world_hash`.
    let s = Scratch::new("cp_hash");
    let mut recovery = recovery_for_meta_tests(&s);
    recovery.checkpoint.bricks[0].edited = !recovery.checkpoint.bricks[0].edited;
    assert!(matches!(
        restore_err(&recovery, &cfg()),
        persist::PersistError::CheckpointHashMismatch { .. }
    ));
}

#[test]
fn a_swapped_checkpoint_brick_revision_is_rejected() {
    let s = Scratch::new("cp_rev");
    let mut recovery = recovery_for_meta_tests(&s);
    recovery.checkpoint.bricks[0].revision += 7;
    assert!(matches!(
        restore_err(&recovery, &cfg()),
        persist::PersistError::CheckpointHashMismatch { .. }
    ));
}

#[test]
fn a_journal_transaction_with_a_wrong_result_hash_is_rejected() {
    let s = Scratch::new("replay_hash");
    let mut recovery = recovery_with_cut_journal(&s);
    let (mut tx, parts) = recovery.journal[0]
        .payload
        .as_topology()
        .expect("suffix record is a topology transaction")
        .unwrap();
    // A replay defect / corrupt row: the transaction claims a result hash the
    // reconstructed geometry will not reproduce.
    tx.result_hashes[0].hash = spall_protocol::Hash32::ZERO;
    recovery.journal[0].payload = spall_store::JournalPayload::topology(&tx, &parts).unwrap();
    match restore_err(&recovery, &cfg()) {
        persist::PersistError::World(spall_sim::WorldError::ReplayResultHash(_)) => {}
        other => panic!("expected ReplayResultHash, got {other:?}"),
    }
}

#[test]
fn a_journal_transaction_with_a_wrong_precondition_is_rejected() {
    let s = Scratch::new("replay_pre");
    let mut recovery = recovery_with_cut_journal(&s);
    let (mut tx, parts) = recovery.journal[0]
        .payload
        .as_topology()
        .expect("suffix record is a topology transaction")
        .unwrap();
    assert!(!tx.before.is_empty(), "the column cut records a before-set");
    tx.before[0].revision = spall_core::Revision(999);
    recovery.journal[0].payload = spall_store::JournalPayload::topology(&tx, &parts).unwrap();
    match restore_err(&recovery, &cfg()) {
        persist::PersistError::World(spall_sim::WorldError::ReplayPrecondition(_)) => {}
        other => panic!("expected ReplayPrecondition, got {other:?}"),
    }
}

#[test]
fn a_journal_transaction_with_a_stale_algorithm_version_is_rejected() {
    let s = Scratch::new("replay_algo");
    let mut recovery = recovery_with_cut_journal(&s);
    let (mut tx, parts) = recovery.journal[0]
        .payload
        .as_topology()
        .expect("suffix record is a topology transaction")
        .unwrap();
    tx.algorithm_version += 100;
    recovery.journal[0].payload = spall_store::JournalPayload::topology(&tx, &parts).unwrap();
    match restore_err(&recovery, &cfg()) {
        persist::PersistError::AlgorithmVersionMismatch {
            field: "journal transaction.algorithm_version",
            ..
        } => {}
        other => panic!("expected AlgorithmVersionMismatch, got {other:?}"),
    }
}

// --- ENG-39: resume simulation time at the durable journal suffix -------

/// Highest tick any durable record in a recovery carries (checkpoint tick
/// included).
fn max_durable_tick(recovery: &spall_store::Recovery) -> u64 {
    recovery
        .journal
        .iter()
        .map(|r| r.tick)
        .max()
        .unwrap_or(0)
        .max(recovery.checkpoint.tick)
}

#[test]
fn review_replay_must_advance_tick_past_durable_suffix() {
    // The issue repro: checkpoint at tick 0, a column cut that becomes durable
    // at a much later tick, journal appended, then restore. Resuming at the old
    // checkpoint tick would let the next committed event be stamped *before* the
    // already-durable transaction.
    let s = Scratch::new("resume_tick");

    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();

        sim.submit(EditIntent::cut(
            RequestId(1),
            actor(),
            EditTarget::Terrain,
            brush_cell(10, 4, 1, 2),
        ))
        .unwrap();
        sim.run_until_idle(16).unwrap();
        assert_eq!(sim.world().body_count(), 1, "beam detached");

        let records = persist::journal_records(sim.journal().entries()).unwrap();
        assert_eq!(records.len(), 1);
        assert!(
            records[0].tick > 0,
            "the durable transaction is at a tick strictly after the checkpoint"
        );
        w.append_journal(&records).unwrap();
    }

    let recovery = spall_store::recover(s.db()).unwrap();
    let durable_tick = max_durable_tick(&recovery);
    assert!(durable_tick > 0);

    let (mut restored, _seq) = persist::restore(
        &recovery,
        &cfg(),
        persist::RecoveryChoice::RequireClean,
        fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap();

    // Simulation time resumes at the durable suffix, not the checkpoint tick.
    assert!(
        restored.current_tick().get() >= durable_tick,
        "restored at tick {} but the durable suffix reaches tick {durable_tick}",
        restored.current_tick().get()
    );

    // The first post-recovery committed event is strictly newer than every
    // durable record — tick / interpolation / checkpoint ordering hold.
    restored
        .submit(EditIntent::cut(
            RequestId(2),
            actor(),
            EditTarget::Terrain,
            brush_cell(6, 4, 1, 1),
        ))
        .unwrap();
    restored.run_until_idle(16).unwrap();
    let committed = restored
        .committed(RequestId(2))
        .expect("fresh cut commits after recovery");
    assert!(
        committed.topology.server_tick.get() > durable_tick,
        "post-recovery transaction stamped at tick {} — not strictly after the \
         durable suffix tick {durable_tick}",
        committed.topology.server_tick.get()
    );
}

#[test]
fn checkpoint_plus_topology_and_pose_suffix_survives_a_second_restart() {
    // A durable suffix that ends with a pose batch at a tick later than the
    // topology transaction: recovery must resume past the *pose* tick, and that
    // resumed time must itself survive a second save + restart.
    let s = Scratch::new("second_restart");

    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    let topo_records = {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();
        sim.submit(EditIntent::cut(
            RequestId(1),
            actor(),
            EditTarget::Terrain,
            brush_cell(10, 4, 1, 2),
        ))
        .unwrap();
        sim.run_until_idle(16).unwrap();
        let records = persist::journal_records(sim.journal().entries()).unwrap();
        assert_eq!(records.len(), 1);
        let topo_tick = records[0].tick;
        let pose_tick = topo_tick + 5;

        // topology record, then a later pose batch (empty snapshots is a valid
        // no-op payload — this test exercises the tick-ordering / resume path).
        let mut suffix = records.clone();
        suffix.push(spall_store::JournalRecord {
            seq: records[0].seq + 1,
            tick: pose_tick,
            payload: spall_store::JournalPayload::PoseBatch { snapshots: vec![] },
        });
        w.append_journal(&suffix).unwrap();
        records
    };

    let recovery = spall_store::recover(s.db()).unwrap();
    assert_eq!(recovery.journal.len(), 2);
    let durable_tick = max_durable_tick(&recovery);
    assert_eq!(
        durable_tick,
        topo_records[0].tick + 5,
        "the pose batch is newest"
    );

    let (first, _) = persist::restore(
        &recovery,
        &cfg(),
        persist::RecoveryChoice::RequireClean,
        fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap();
    assert!(
        first.current_tick().get() >= durable_tick,
        "first restart resumed at tick {}, behind the durable suffix tick {durable_tick}",
        first.current_tick().get()
    );
    let first_tick = first.current_tick();
    let first_hash = first.world().world_hash();

    // Second save + restart: checkpoint the recovered sim to a fresh database,
    // recover, restore again. The resumed tick and world must round-trip.
    let s2 = Scratch::new("second_restart_b");
    publish(&s2.db(), &persist::capture(&first, &cfg(), 0).unwrap());
    drop(first);
    let (second, second_seq) = recover_restore(&s2.db());
    assert_eq!(second_seq, 0);
    assert_eq!(
        second.current_tick(),
        first_tick,
        "the durable-suffix tick survived a second save/restart"
    );
    assert_eq!(second.world().world_hash(), first_hash);
}

#[test]
fn review_restore_rejects_a_tick_regression_in_the_durable_suffix() {
    let s = Scratch::new("tick_regression");
    let sim = Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();
        // Two pose batches whose ticks go backwards — an inconsistent suffix.
        w.append_journal(&[
            spall_store::JournalRecord {
                seq: 1,
                tick: 40,
                payload: spall_store::JournalPayload::PoseBatch { snapshots: vec![] },
            },
            spall_store::JournalRecord {
                seq: 2,
                tick: 9,
                payload: spall_store::JournalPayload::PoseBatch { snapshots: vec![] },
            },
        ])
        .unwrap();
    }
    let recovery = spall_store::recover(s.db()).unwrap();
    match restore_err(&recovery, &cfg()) {
        persist::PersistError::JournalTickRegression {
            seq: 2,
            tick: 9,
            durable: 40,
        } => {}
        other => panic!("expected JournalTickRegression, got {other:?}"),
    }
}

// --- ENG-36: fail closed on a reported-corrupt recovery -----------------

#[test]
fn review_restore_must_require_choice_after_corruption() {
    let s = Scratch::new("corruption_choice");
    let mut recovery = recovery_for_meta_tests(&s);
    recovery.corruption.push(spall_store::CorruptionReport {
        detail: "journal seq 1 failed CRC".into(),
    });

    // Default: a reported-corrupt recovery aborts before anything is rebuilt.
    assert!(matches!(
        restore_err(&recovery, &cfg()),
        persist::PersistError::UnrecoverableCorruption { .. }
    ));

    // An explicit operator choice to accept the verified durable prefix works.
    assert!(
        persist::restore(
            &recovery,
            &cfg(),
            persist::RecoveryChoice::AcceptDurablePrefix,
            fixtures::stone_manifest(),
            AnchorPlane::at(0),
            PhysicsConfig::default(),
        )
        .is_ok()
    );
}

// -------------------------------------------------------------------------
// ENG-38: a journalled split must reconstruct its children with the live
// physical mass / centre of mass / inertia, the *source* volume's cell size,
// and every participant's pose — geometry-hash parity is not enough.
// -------------------------------------------------------------------------

/// `entity id -> (mass_kg, local COM, principal inertia)` for every body.
fn mass_props_by_entity(
    sim: &Simulation,
) -> std::collections::BTreeMap<u64, (f32, [f32; 3], [f32; 3])> {
    sim.world()
        .bodies()
        .map(|b| {
            (
                b.entity.unwrap().get(),
                sim.world().physics().derived_mass_properties(b.phys),
            )
        })
        .collect()
}

/// A body-local dumbbell built at the fine `Sixteenth` cell size (the terrain
/// fixtures are all `Quarter`), so a replay that wrongly used the terrain cell
/// size would misplace ~64x the mass.
fn sixteenth_dumbbell(sz: i64, gap: i64) -> impl FnOnce(VolumeId) -> Volume {
    let stone = spall_core::MaterialId(1);
    move |id| {
        let mut v = Volume::new(id, CellSizeCode::Sixteenth);
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(sz - 1, sz - 1, sz - 1),
            stone,
        ))
        .unwrap();
        let rx = sz + gap;
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(rx, 0, 0),
            GlobalCell::new(rx + sz - 1, sz - 1, sz - 1),
            stone,
        ))
        .unwrap();
        let mid = sz / 2;
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(sz, mid, mid),
            GlobalCell::new(rx - 1, mid, mid),
            stone,
        ))
        .unwrap();
        v
    }
}

/// Probe `review_replayed_child_must_preserve_mass`: checkpoint the bridge,
/// durably journal the column cut, restore checkpoint + journal, and compare
/// the recovered beam's actual Rapier mass properties against the live ones.
#[test]
fn review_replayed_child_must_preserve_mass() {
    let s = Scratch::new("eng38_terrain_mass");

    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();

        sim.submit(EditIntent::cut(
            RequestId(1),
            actor(),
            EditTarget::Terrain,
            brush_cell(10, 4, 1, 2),
        ))
        .unwrap();
        sim.run_until_idle(16).unwrap();
        assert_eq!(sim.world().body_count(), 1, "the beam detached");

        let records = persist::journal_records(sim.journal().entries()).unwrap();
        assert_eq!(records.len(), 1, "one split transaction journalled");
        w.append_journal(&records).unwrap();
    }

    let (restored, _) = recover_restore(&s.db());

    let live = sim.world().bodies().next().unwrap();
    let got = restored.world().bodies().next().unwrap();
    let (want_mass, want_com, want_inertia) =
        sim.world().physics().derived_mass_properties(live.phys);
    let (got_mass, got_com, got_inertia) =
        restored.world().physics().derived_mass_properties(got.phys);

    assert!(
        want_mass > 1000.0,
        "sanity: a stone beam is heavy ({want_mass} kg) — not the ~1 kg the density=1.0 bug produced"
    );
    assert!(
        (want_mass - got_mass).abs() < 0.01,
        "replayed body mass: expected {want_mass} kg, got {got_mass} kg"
    );
    for i in 0..3 {
        assert!(
            (want_com[i] - got_com[i]).abs() < 1e-3,
            "COM axis {i}: expected {}, got {}",
            want_com[i],
            got_com[i]
        );
        let denom = want_inertia[i].abs().max(1e-6);
        assert!(
            (want_inertia[i] - got_inertia[i]).abs() / denom < 1e-3,
            "principal inertia axis {i}: expected {}, got {}",
            want_inertia[i],
            got_inertia[i]
        );
    }
}

/// A rotated, airborne dumbbell that keeps moving between the checkpoint and
/// the cut: the replay must fold every participant — including the surviving
/// parent — to the split frame, not leave the parent stranded at the
/// checkpoint frame while the child sits at the split frame.
#[test]
fn rotated_body_to_body_split_replay_preserves_mass_and_parent_frame() {
    let s = Scratch::new("eng38_body_split");

    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    let parent = sim
        .world_mut()
        .spawn_body(
            fixtures::dumbbell(4, 3),
            BodyPose::new(fixtures::oblique_spin(), [3.0, 8.0, 3.0]),
            [0.2, 0.0, 0.0],
            [0.0, 0.3, 0.0],
            2600.0,
            1,
        )
        .unwrap();
    let checkpoint_pose = sim.world().body(parent).unwrap().pose;

    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();

        // Fall + spin so the body's frame diverges from the checkpoint frame.
        for _ in 0..12 {
            sim.step_physics_only();
        }
        let moved = sim.world().body(parent).unwrap().pose;
        assert!(
            (DVec3::from_array(moved.translation_m)
                - DVec3::from_array(checkpoint_pose.translation_m))
            .length()
                > 1e-3,
            "the body moved between checkpoint and cut"
        );

        sim.submit(EditIntent::cut(
            RequestId(1),
            actor(),
            EditTarget::Body(parent),
            brush_cell(5, 2, 2, 1),
        ))
        .unwrap();
        sim.run_until_idle(24).unwrap();
        assert_eq!(sim.world().body_count(), 2, "the dumbbell fractured");

        let records = persist::journal_records(sim.journal().entries()).unwrap();
        assert_eq!(records.len(), 1, "one split transaction journalled");
        w.append_journal(&records).unwrap();
    }

    let live = mass_props_by_entity(&sim);
    let (restored, _) = recover_restore(&s.db());
    let got = mass_props_by_entity(&restored);

    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        live.keys().collect::<Vec<_>>(),
        "the same body ids recovered"
    );
    for (entity, (want_mass, _, _)) in &live {
        let (got_mass, _, _) = got[entity];
        assert!(
            *want_mass > 100.0,
            "body {entity} is stone, not density=1.0"
        );
        assert!(
            (want_mass - got_mass).abs() < 0.01,
            "body {entity}: mass expected {want_mass} kg, got {got_mass} kg"
        );
    }

    // The parent participant was carried to the split frame: off the
    // checkpoint pose, and sharing the split-instant rotation with the child
    // (a child reproduces the parent transform exactly at the cut).
    let rp = restored.world().body(parent).unwrap();
    assert!(
        (DVec3::from_array(rp.pose.translation_m)
            - DVec3::from_array(checkpoint_pose.translation_m))
        .length()
            > 1e-3,
        "restored parent advanced past the checkpoint frame, not stranded at it"
    );
    let child = restored
        .world()
        .bodies()
        .find(|b| b.entity.unwrap().get() != parent.get())
        .expect("a detached child exists");
    let (a, b) = (rp.pose.rotation, child.pose.rotation);
    let dot = (a.x * b.x + a.y * b.y + a.z * b.z + a.w * b.w).abs();
    assert!(
        dot > 1.0 - 1e-6,
        "restored parent + child share the split-instant rotation (dot={dot})"
    );
}

/// A body cut at a finer cell size than the terrain: the recovered children
/// must keep the source cell size and therefore the correct mass — a replay
/// that reached for `terrain_cell_size` would be ~64x off.
#[test]
fn detail_cell_body_split_replay_uses_the_source_cell_size() {
    let s = Scratch::new("eng38_detail");

    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    let parent = sim
        .world_mut()
        .spawn_body(
            sixteenth_dumbbell(4, 3),
            BodyPose::new(DQuat::IDENTITY, [2.0, 6.0, 2.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            1,
        )
        .unwrap();
    assert_ne!(
        sim.world().body(parent).unwrap().cell_size(),
        sim.world().terrain().cell_size(),
        "the body is finer than the terrain"
    );

    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();

        sim.submit(EditIntent::cut(
            RequestId(1),
            actor(),
            EditTarget::Body(parent),
            brush_cell(5, 2, 2, 1),
        ))
        .unwrap();
        sim.run_until_idle(24).unwrap();
        assert_eq!(
            sim.world().body_count(),
            2,
            "the detail-cell dumbbell fractured"
        );

        let records = persist::journal_records(sim.journal().entries()).unwrap();
        w.append_journal(&records).unwrap();
    }

    let live = mass_props_by_entity(&sim);
    let (restored, _) = recover_restore(&s.db());
    let got = mass_props_by_entity(&restored);

    for (entity, (want_mass, _, _)) in &live {
        let (got_mass, _, _) = got[entity];
        assert!(
            (want_mass - got_mass).abs() < 0.01,
            "detail-cell body {entity}: expected {want_mass} kg, got {got_mass} kg \
             (a terrain-cell-size child would be ~64x heavier)"
        );
    }
    for b in restored.world().bodies() {
        assert_eq!(
            b.cell_size(),
            CellSizeCode::Sixteenth,
            "recovered body {} kept the source cell size",
            b.entity.unwrap().get()
        );
    }
}

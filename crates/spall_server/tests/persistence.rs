//! T16 save-integration acceptance: a real `spall_sim::Simulation` is edited,
//! settled, checkpointed, and recovered — geometry, ownership, materials, pose
//! of rotated fractured bodies, and sleeping state must survive the round trip.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use glam::{DQuat, DVec3};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{GlobalCell, SphereBrush};
use spall_physics::PhysicsConfig;
use spall_protocol::RequestId;
use spall_server::persist::{self, PersistConfig};
use spall_sim::{BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};
use spall_store::Writer;
use spall_structure::AnchorPlane;

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

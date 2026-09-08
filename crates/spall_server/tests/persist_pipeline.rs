//! ENG-50 acceptance: the bounded asynchronous persistence pipeline and the
//! durable 20 Hz pose cadence.
//!
//! * a slow disk does not stall the tick loop (the sim thread only `try_send`s);
//! * the job queue is bounded — a sustained backlog is refused, not buffered;
//! * a long-running saved session keeps retained journal rows and in-memory
//!   snapshot memory bounded;
//! * a moving body that crashes between checkpoints recovers to its **latest
//!   durable pose batch**, not to the stale checkpoint pose.
//!
//! Measured retained-snapshot memory and flush / checkpoint latency are printed
//! (run with `--nocapture`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use glam::{DQuat, DVec3};
use spall_physics::PhysicsConfig;
use spall_server::persist_pipeline::{PersistPipeline, PipelineConfig, PipelineError};
use spall_server::{PersistConfig, persist};
use spall_sim::{BodyPose, MotionPublisher, Simulation, SimulationConfig, fixtures};
use spall_store::{Checkpoint, JournalPayload, JournalRecord, Writer};
use spall_structure::AnchorPlane;

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("spall_pipe_{tag}_{}_{n}", std::process::id()));
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
        world_id: 0x5A11_0000_0000_7050,
        seed: 50,
        generator_version: 1,
    }
}

fn pose_rec(seq: u64, tick: u64) -> JournalRecord {
    JournalRecord {
        seq,
        tick,
        payload: JournalPayload::PoseBatch {
            snapshots: vec![vec![0u8; 48]],
        },
    }
}

/// A slow disk (20 ms/write injected) must not slow the "tick loop": the loop
/// only queues immutable jobs and never blocks on the writer.
#[test]
fn a_slow_disk_does_not_stall_the_tick_loop() {
    let s = Scratch::new("slow_disk");
    let io_delay = Duration::from_millis(20);
    let pipe = PersistPipeline::spawn(
        Writer::open(s.db()).unwrap(),
        PipelineConfig {
            queue_capacity: 256,
            io_delay: Some(io_delay),
        },
    );

    const TICKS: u64 = 180;
    const SIM_WORK: Duration = Duration::from_millis(1);
    let mut seq = 0u64;
    let mut flushes = 0u64;

    let started = Instant::now();
    for tick in 1..=TICKS {
        std::thread::sleep(SIM_WORK); // stand in for physics + commit
        if tick % 3 == 0 {
            seq += 1;
            pipe.submit_journal(vec![pose_rec(seq, tick)])
                .expect("queue has slack");
            flushes += 1;
        }
        if tick % 60 == 0 {
            pipe.submit_checkpoint(dummy_checkpoint(tick, seq)).unwrap();
            pipe.submit_retain(2).unwrap();
        }
    }
    let loop_elapsed = started.elapsed();

    let io_lower_bound = io_delay * (flushes as u32);
    assert!(
        loop_elapsed < io_lower_bound / 3,
        "tick loop blocked on the disk: loop={loop_elapsed:?}, serial-io>={io_lower_bound:?}"
    );

    let out = pipe.shutdown();
    assert!(out.status.error.is_none(), "{:?}", out.status.error);
    assert_eq!(out.status.durable_seq, seq);

    eprintln!(
        "[slow-disk] {TICKS} ticks in {loop_elapsed:?} (serial disk time would be >= {io_lower_bound:?}); \
         max_queue_depth={}, last_flush={:?}, max_flush={:?}, max_checkpoint={:?}",
        out.status.max_queue_depth,
        out.status.last_journal_flush,
        out.status.max_journal_flush,
        out.status.max_checkpoint,
    );
}

/// The queue is bounded: a burst that outruns a slow disk is refused with
/// `Backlog` (admission control), never buffered without limit, and the
/// pipeline then stops rather than continuing an unsavable world.
#[test]
fn the_job_queue_is_bounded_and_refuses_a_sustained_backlog() {
    let s = Scratch::new("bounded");
    let pipe = PersistPipeline::spawn(
        Writer::open(s.db()).unwrap(),
        PipelineConfig {
            queue_capacity: 4,
            io_delay: Some(Duration::from_millis(40)),
        },
    );

    let mut refused_after = None;
    for i in 1..=100u64 {
        match pipe.submit_journal(vec![pose_rec(i, i)]) {
            Ok(()) => {}
            Err(PipelineError::Backlog { capacity, .. }) => {
                assert_eq!(capacity, 4);
                refused_after = Some(i);
                break;
            }
            Err(e) => panic!("unexpected {e}"),
        }
    }
    let refused_after = refused_after.expect("a bounded queue must eventually refuse");
    assert!(
        refused_after <= 4 + 3,
        "queue accepted {refused_after} jobs at capacity 4 — not bounded"
    );
    assert!(
        pipe.error().is_some(),
        "a tripped backlog stops the pipeline"
    );
    assert!(pipe.submit_checkpoint(dummy_checkpoint(1, 0)).is_err());

    let out = pipe.shutdown();
    assert!(
        out.status.durable_seq < refused_after,
        "only genuinely-committed sequences are acknowledged"
    );
    eprintln!("[bounded] refused after {refused_after} submits at capacity 4");
}

/// A long saved session: retained journal rows and retained in-memory snapshot
/// memory stay bounded no matter how many ticks run.
#[test]
fn a_long_running_session_keeps_retention_bounded() {
    let s = Scratch::new("long_session");
    let db = s.db();
    {
        // Seed an initial checkpoint at cursor 0 so recovery always has a floor.
        let mut w = Writer::open(&db).unwrap();
        w.publish_checkpoint(&dummy_checkpoint(0, 0)).unwrap();
    }
    let pipe = PersistPipeline::spawn(
        Writer::open(&db).unwrap(),
        PipelineConfig {
            queue_capacity: 32,
            io_delay: None,
        },
    );

    const ITERS: u64 = 4000;
    const CHECKPOINT_EVERY: u64 = 120;
    let mut seq = 0u64;
    // A crude upper bound on the retained snapshot memory: whatever is queued.
    let approx_job_bytes = std::mem::size_of::<JournalRecord>() + 48;

    for tick in 1..=ITERS {
        seq += 1;
        // A soak driver, not the real-time tick loop: block for queue room
        // rather than treating a transient full queue as fatal. The bounded
        // queue still caps retained memory.
        pipe.submit_journal_blocking(vec![pose_rec(seq, tick)])
            .unwrap();
        if tick % CHECKPOINT_EVERY == 0 {
            pipe.submit_checkpoint_blocking(dummy_checkpoint(tick, seq))
                .unwrap();
            pipe.submit_retain_blocking(2).unwrap();
        }
    }

    let out = pipe.shutdown();
    assert!(out.status.error.is_none(), "{:?}", out.status.error);
    assert_eq!(out.status.durable_seq, seq);

    // Retained journal rows must be bounded by ~one checkpoint interval, not by
    // the 4000 ticks that ran.
    let recovery = spall_store::recover(&db).unwrap();
    assert!(
        recovery.journal.len() <= (CHECKPOINT_EVERY as usize) * 3,
        "retained journal rows unbounded: {}",
        recovery.journal.len()
    );
    assert!(recovery.corruption.is_empty());

    let retained_mem = out.status.max_queue_depth * approx_job_bytes;
    eprintln!(
        "[long-session] {ITERS} ticks, durable_seq={}, retained journal rows after prune={}, \
         max_queue_depth={} (~{retained_mem} B retained snapshot memory), \
         max_flush={:?}, max_checkpoint={:?}",
        out.status.durable_seq,
        recovery.journal.len(),
        out.status.max_queue_depth,
        out.status.max_journal_flush,
        out.status.max_checkpoint,
    );
}

/// A moving body that crashes between checkpoints recovers to its latest durable
/// 20 Hz pose batch — not to the stale checkpoint pose.
#[test]
fn a_moving_body_crash_recovers_its_latest_durable_pose() {
    let s = Scratch::new("moving_pose");
    let db = s.db();

    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    let entity = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(4),
            BodyPose::new(DQuat::IDENTITY, [2.0, 8.0, 2.0]),
            [1.5, 0.0, 0.5], // moving in +x / +z, and falling under gravity
            [0.0; 3],
            2600.0,
            1,
        )
        .unwrap();

    // Settle a few ticks, then checkpoint at T0.
    for _ in 0..4 {
        sim.tick().unwrap();
    }
    let checkpoint_pos = body_pos(&sim, entity);
    {
        let mut w = Writer::open(&db).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();
    }

    // Keep moving. Every 3rd tick (20 Hz) journal a durable pose batch with a
    // contiguous, integrator-owned sequence.
    let mut motion = MotionPublisher::new(60, 20);
    let mut last_seq = 0u64;
    let mut last_batch_pos = checkpoint_pos;
    {
        let mut w = Writer::open(&db).unwrap();
        for _ in 0..30 {
            let report = sim.tick().unwrap();
            assert!(report.committed.is_empty());
            let tick = sim.current_tick();
            if motion.due(tick) {
                let snaps = motion.snapshots(sim.world(), tick);
                let seq = sim.reserve_journal_seq().unwrap().0;
                let rec = persist::pose_batch_record(seq, tick.get(), &snaps).unwrap();
                w.append_journal(&[rec]).unwrap();
                last_seq = seq;
                last_batch_pos = DVec3::from_array(snaps[0].pose.translation_m);
            }
        }
        // "Crash": drop the writer with no newer checkpoint.
    }

    assert!(last_seq > 0, "at least one pose batch was journalled");
    assert!(
        (last_batch_pos - checkpoint_pos).length() > 0.05,
        "the body actually moved between the checkpoint and the crash"
    );

    let recovery = spall_store::recover(&db).unwrap();
    assert_eq!(recovery.journal.len(), last_seq as usize);
    let (restored, durable_seq) = persist::restore(
        &recovery,
        &cfg(),
        persist::RecoveryChoice::RequireClean,
        fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap();
    assert_eq!(
        durable_seq, last_seq,
        "resume point is the last durable pose"
    );

    let recovered_pos = body_pos(&restored, entity);
    let drift = (recovered_pos - last_batch_pos).length();
    assert!(
        drift < 1e-6,
        "recovered pose {recovered_pos:?} != latest durable pose {last_batch_pos:?}"
    );
    assert!(
        (recovered_pos - checkpoint_pos).length() > 0.05,
        "recovery wrongly rewound motion to the checkpoint pose"
    );
    eprintln!(
        "[moving-pose] checkpoint_pos={checkpoint_pos:?}, latest durable pose={last_batch_pos:?}, \
         recovered={recovered_pos:?} (drift {drift:.2e} m), durable_seq={durable_seq}"
    );
}

fn body_pos(sim: &Simulation, entity: spall_core::EntityId) -> DVec3 {
    let b = sim
        .world()
        .bodies()
        .find(|b| b.entity == Some(entity))
        .expect("body present");
    DVec3::from_array(b.pose.translation_m)
}

fn dummy_checkpoint(tick: u64, cursor: u64) -> Checkpoint {
    use spall_store::{STORE_SCHEMA_VERSION, StoredWorldMeta};
    Checkpoint {
        tick,
        journal_cursor: cursor,
        world_hash: [0; 32],
        meta: StoredWorldMeta {
            store_schema_version: STORE_SCHEMA_VERSION,
            world_id: 1,
            seed: 0,
            generator_version: 1,
            material_manifest_hash: [0; 32],
            cell_size_codes: vec![2],
            next_entity: 1,
            next_volume: 2,
            next_transaction: 1,
            next_journal_seq: cursor + 1,
            integer_brush_version: 1,
            structure_graph_version: 1,
            topology_hash_version: 1,
        },
        bodies: vec![],
        bricks: vec![],
    }
}

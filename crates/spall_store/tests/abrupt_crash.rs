//! Abrupt process-death recovery (ENG-51).
//!
//! The in-process [`FaultPlan`] crash points model the *API-visible* effect of a
//! crash while still allowing SQLite to roll the pending transaction back
//! normally in the same process. They do **not** prove that a real, unclean
//! process kill leaves a recoverable database.
//!
//! This harness does: it re-invokes the test binary as a child, drives real
//! durable writes up to a named boundary around journal / checkpoint
//! publication, then the *parent* kills the child with no chance to run any
//! destructor or WAL checkpoint. Recovery then runs from a fresh process and the
//! durable prefix / transaction ownership is asserted.
//!
//! Bounded: each child is killed within a few seconds and always `wait()`ed;
//! only children this test spawned are touched. The child also self-exits after
//! a hard cap so a leak cannot outlive the run.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use spall_core::{EntityId, GlobalCell, MaterialId, Tick, TransactionId, VolumeId};
use spall_protocol::{ControlSeq, TopologyOp, TopologyTransaction};
use spall_store::{
    Checkpoint, JournalPayload, JournalRecord, STORE_SCHEMA_VERSION, StoredBrick, StoredWorldMeta,
    Writer, encode_cells, recover,
};

/// Journal records the child writes and the parent expects back. `split_record`
/// first so its whole-transaction ownership can be re-checked after recovery.
const RECORD_COUNT: usize = 3;
const LAST_SEQ: u64 = 3;

// --- scratch dir ---------------------------------------------------------

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "spall_store_abrupt_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.join("world.db")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Kills and reaps every child on drop — even if the test panics.
struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

// --- builders (shared by parent and child) -----------------------------

fn meta() -> StoredWorldMeta {
    StoredWorldMeta {
        store_schema_version: STORE_SCHEMA_VERSION,
        world_id: 0x5A11_0000_0000_7016,
        seed: 1,
        generator_version: 1,
        material_manifest_hash: [7; 32],
        cell_size_codes: vec![2],
        next_entity: 4,
        next_volume: 5,
        next_transaction: 10,
        next_journal_seq: 1,
        integer_brush_version: 1,
        structure_graph_version: 1,
        topology_hash_version: 1,
    }
}

fn checkpoint(tick: u64, cursor: u64) -> Checkpoint {
    let solid = vec![2u16; spall_core::CELLS_PER_BRICK];
    let mut m = meta();
    m.next_journal_seq = cursor + 1;
    Checkpoint {
        tick,
        journal_cursor: cursor,
        world_hash: [tick as u8; 32],
        meta: m,
        bodies: vec![],
        bricks: vec![StoredBrick {
            volume_id: 1,
            coord: [0, 0, 0],
            revision: 1,
            edited: true,
            payload: encode_cells(&solid).unwrap(),
        }],
    }
}

/// A split transaction: source-removal *and* child-fill in one atomic record.
fn split_record(seq: u64, tick: u64) -> JournalRecord {
    let source = VolumeId::new(1).unwrap();
    let child = VolumeId::new(4).unwrap();
    let tx = TopologyTransaction {
        transaction_id: TransactionId::new(9).unwrap(),
        server_tick: Tick(tick),
        control_seq: ControlSeq(seq),
        algorithm_version: 1,
        dependencies: vec![],
        before: vec![],
        after: vec![],
        ops: vec![
            TopologyOp::SplitOff {
                source,
                child,
                child_entity: EntityId::new(3).unwrap(),
            },
            TopologyOp::CellRun {
                volume: child,
                start: GlobalCell::new(0, 0, 0),
                len: 4,
                material: MaterialId(2),
            },
            TopologyOp::CellRun {
                volume: source,
                start: GlobalCell::new(0, 0, 0),
                len: 4,
                material: MaterialId::AIR,
            },
        ],
        result_hashes: vec![],
    };
    JournalRecord {
        seq,
        tick,
        payload: JournalPayload::topology(&tx, &[]).unwrap(),
    }
}

fn pose_batch(seq: u64, tick: u64) -> JournalRecord {
    JournalRecord {
        seq,
        tick,
        payload: JournalPayload::PoseBatch { snapshots: vec![] },
    }
}

fn records() -> Vec<JournalRecord> {
    vec![split_record(1, 1), pose_batch(2, 2), pose_batch(3, 3)]
}

// --- child entry point -------------------------------------------------

/// Invoked only by the parent test, once per boundary. Never returns on its own
/// before the parent kill (a hard cap self-exit guards against a leak).
#[test]
#[ignore = "child process entry point; driven by abrupt_process_death_recovers_the_durable_prefix"]
fn crash_child() {
    let Ok(boundary) = std::env::var("SPALL_CRASH_CHILD") else {
        return;
    };
    let db = PathBuf::from(std::env::var_os("SPALL_CRASH_DB").expect("SPALL_CRASH_DB"));
    let marker = PathBuf::from(std::env::var_os("SPALL_CRASH_MARKER").expect("SPALL_CRASH_MARKER"));

    let mut w = Writer::open(&db).expect("open child writer");
    w.publish_checkpoint(&checkpoint(0, 0)).expect("cp0");

    match boundary.as_str() {
        // Kill before any journal write: nothing after cp0 is durable.
        "pre_journal" => {}
        // Kill after the journal COMMIT returned: the suffix must survive.
        "post_journal" => {
            let d = w.append_journal(&records()).expect("append journal");
            assert_eq!(d.journal_seq.0, LAST_SEQ);
        }
        // Journal durable, killed before the checkpoint that would cover it.
        "pre_checkpoint" => {
            w.append_journal(&records()).expect("append journal");
        }
        // Killed after the checkpoint COMMIT returned: cp@tick 200 must survive.
        "post_checkpoint" => {
            w.append_journal(&records()).expect("append journal");
            w.publish_checkpoint(&checkpoint(200, LAST_SEQ))
                .expect("cp200");
        }
        other => panic!("unknown boundary {other:?}"),
    }

    // Signal "at the boundary", then wait to be killed. `Writer`/`Connection`
    // destructors never run: the parent uses an unconditional process kill.
    std::fs::write(
        marker.with_extension("pending"),
        std::process::id().to_string(),
    )
    .unwrap();
    std::fs::rename(marker.with_extension("pending"), &marker).unwrap();

    let cap = Instant::now() + Duration::from_secs(60);
    while Instant::now() < cap {
        std::thread::sleep(Duration::from_millis(50));
    }
    // Hard cap reached without a kill: exit non-zero so the parent's wait notes
    // it, rather than lingering.
    std::process::exit(3);
}

// --- parent orchestrator ---------------------------------------------

fn spawn_child(boundary: &str, db: &Path, marker: &Path, log: &Path) -> OwnedChild {
    let log = std::fs::File::create(log).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "crash_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env("SPALL_CRASH_CHILD", boundary)
        .env("SPALL_CRASH_DB", db)
        .env("SPALL_CRASH_MARKER", marker)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn crash child");
    OwnedChild(child)
}

fn wait_for_marker(marker: &Path, child: &mut OwnedChild) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if marker.exists() {
            return;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!("crash child exited before reaching the boundary: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "crash child never reached the boundary"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Opens the database from *this* (fresh) process. Retries briefly: on Windows
/// the killed child's file handles can take a moment to release.
fn recover_fresh(db: &Path) -> spall_store::Recovery {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match recover(db) {
            Ok(rec) => return rec,
            Err(e) if Instant::now() < deadline => {
                eprintln!("recover retry: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("recover from fresh process failed: {e}"),
        }
    }
}

fn assert_split_owns_whole_transaction(rec: &spall_store::Recovery) {
    let (tx, _) = rec.journal[0]
        .payload
        .as_topology()
        .expect("first suffix record is the split")
        .expect("topology decodes");
    let has_split = tx
        .ops
        .iter()
        .any(|o| matches!(o, TopologyOp::SplitOff { .. }));
    let removals = tx
        .ops
        .iter()
        .filter(|o| matches!(o, TopologyOp::CellRun { material, .. } if material.is_air()))
        .count();
    let fills = tx
        .ops
        .iter()
        .filter(|o| matches!(o, TopologyOp::CellRun { material, .. } if !material.is_air()))
        .count();
    assert!(
        has_split && removals == 1 && fills == 1,
        "recovered split transaction must own source-removal and child-fill together"
    );
}

#[test]
fn abrupt_process_death_recovers_the_durable_prefix() {
    for boundary in [
        "pre_journal",
        "post_journal",
        "pre_checkpoint",
        "post_checkpoint",
    ] {
        let s = Scratch::new(boundary);
        let marker = s.dir.join("at_boundary");
        let log = s.dir.join("child.log");

        let mut child = spawn_child(boundary, &s.db(), &marker, &log);
        wait_for_marker(&marker, &mut child);

        // Unconditional external kill: no unwinding, no `Drop`, no WAL
        // checkpoint — a genuine abrupt process death.
        child.0.kill().expect("kill crash child");
        child.0.wait().expect("reap crash child");

        let rec = recover_fresh(&s.db());
        assert!(
            rec.corruption.is_empty(),
            "[{boundary}] abrupt kill must not corrupt the database: {:?}",
            rec.corruption
        );

        match boundary {
            "pre_journal" => {
                assert_eq!(rec.checkpoint.tick, 0, "[{boundary}] only cp0 is durable");
                assert!(rec.journal.is_empty(), "[{boundary}] no journal suffix");
                assert_eq!(rec.durable_through, 0);
            }
            "post_journal" | "pre_checkpoint" => {
                assert_eq!(
                    rec.checkpoint.tick, 0,
                    "[{boundary}] the covering checkpoint never committed"
                );
                assert_eq!(
                    rec.journal.len(),
                    RECORD_COUNT,
                    "[{boundary}] the whole committed journal batch survived the kill"
                );
                assert_eq!(rec.durable_through, LAST_SEQ);
                assert_eq!(
                    rec.journal.iter().map(|r| r.seq).collect::<Vec<_>>(),
                    vec![1, 2, 3],
                    "[{boundary}] contiguous durable prefix"
                );
                assert_split_owns_whole_transaction(&rec);
            }
            "post_checkpoint" => {
                assert_eq!(
                    rec.checkpoint.tick, 200,
                    "[{boundary}] the committed checkpoint survived the kill"
                );
                assert_eq!(
                    rec.previous_checkpoint.map(|c| c.tick),
                    Some(0),
                    "[{boundary}] cp0 is the explicit fallback"
                );
                assert_eq!(rec.checkpoint.journal_cursor, LAST_SEQ);
                assert_eq!(rec.durable_through, LAST_SEQ);
                assert!(
                    rec.journal.is_empty(),
                    "[{boundary}] the checkpoint already covers the journal suffix"
                );
            }
            _ => unreachable!(),
        }

        drop(child); // reap (already waited) and drop the scratch dir
    }
}

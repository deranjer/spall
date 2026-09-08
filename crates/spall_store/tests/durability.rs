//! T16 acceptance: durable journal commit, checkpoint publication, crash-point
//! and disk-fault behaviour, and recovery of the durable prefix.
//!
//! `docs/validation.md` fixtures exercised at the storage layer:
//! * `save-air` — a mined-out (modified-air) brick stays empty across a restart.
//! * `crash-transfer` — a crash around a split transaction cannot recover
//!   partial ownership; one journal record is one atomic transaction.
//! * `malformed-input` — an oversized/garbage brick frame rejects within bounds
//!   (see the unit tests in `src/brick.rs`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use spall_core::{EntityId, GlobalCell, MaterialId, Tick, TransactionId, VolumeId};
use spall_protocol::{ControlSeq, TopologyOp, TopologyTransaction};
use spall_store::{
    BrickPayload, Checkpoint, CrashPoint, FaultPlan, JournalPayload, JournalRecord,
    STORE_SCHEMA_VERSION, StoreError, StoredBrick, StoredWorldMeta, Writer, decode_cells,
    encode_cells, recover,
};

// --- tiny scratch-dir helper ------------------------------------------------

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("spall_store_{tag}_{}_{n}", std::process::id()));
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

// --- builders -------------------------------------------------------------

fn meta() -> StoredWorldMeta {
    StoredWorldMeta {
        store_schema_version: STORE_SCHEMA_VERSION,
        world_id: 0x5A11_0000_0000_7016,
        seed: 1,
        generator_version: 1,
        material_manifest_hash: [7; 32],
        cell_size_codes: vec![2],
        next_entity: 3,
        next_volume: 4,
        next_transaction: 9,
        next_journal_seq: 1,
        integer_brush_version: 1,
        structure_graph_version: 1,
        topology_hash_version: 1,
    }
}

/// A checkpoint at `tick` consistent with journal `cursor`, holding one solid
/// brick and one mined-out (modified-air) tombstone brick.
fn checkpoint(tick: u64, cursor: u64) -> Checkpoint {
    let mut solid = vec![0u16; spall_core::CELLS_PER_BRICK];
    for (i, c) in solid.iter_mut().enumerate() {
        *c = if i % 3 == 0 { 1 } else { 2 };
    }
    let mut m = meta();
    m.next_journal_seq = cursor + 1;
    Checkpoint {
        tick,
        journal_cursor: cursor,
        world_hash: [tick as u8; 32],
        meta: m,
        bodies: vec![],
        bricks: vec![
            StoredBrick {
                volume_id: 1,
                coord: [0, 0, 0],
                revision: 5,
                edited: true,
                payload: encode_cells(&solid).unwrap(),
            },
            // A brick mined entirely to air: Uniform(0) + edited tombstone.
            StoredBrick {
                volume_id: 1,
                coord: [1, 0, 0],
                revision: 6,
                edited: true,
                payload: BrickPayload::Uniform(0),
            },
        ],
    }
}

fn pose_batch(seq: u64, tick: u64) -> JournalRecord {
    JournalRecord {
        seq,
        tick,
        payload: JournalPayload::PoseBatch { snapshots: vec![] },
    }
}

/// A journal record for a split transaction: source-removal runs *and* the
/// child-fill runs live in one atomic record.
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

fn raw_user_version_bump(path: &Path, to: u32) {
    let conn = rusqlite_open(path);
    conn.pragma_update(None, "user_version", to as i64).unwrap();
}

// The test binary does not depend on rusqlite directly; reach it through a
// helper compiled into `spall_store`'s dependency closure is not possible, so
// use a fresh connection via a minimal shim.
fn rusqlite_open(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}

// --- tests --------------------------------------------------------------

#[test]
fn open_verifies_wal_and_full_sync_and_reopen_is_clean() {
    let s = Scratch::new("open");
    {
        let w = Writer::open(s.db()).unwrap();
        assert!(!w.is_poisoned());
    }
    // Reopen an existing database: schema already at the current version.
    let w = Writer::open(s.db()).unwrap();
    assert_eq!(w.journal_max_seq().unwrap(), 0);
}

#[test]
fn schema_newer_than_supported_is_rejected_without_touching_data() {
    let s = Scratch::new("schema_new");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(10, 0)).unwrap();
    }
    raw_user_version_bump(&s.db(), STORE_SCHEMA_VERSION + 5);

    match Writer::open(s.db()) {
        Err(StoreError::SchemaTooNew { found, supported }) => {
            assert_eq!(found, STORE_SCHEMA_VERSION + 5);
            assert_eq!(supported, STORE_SCHEMA_VERSION);
        }
        Err(other) => panic!("expected SchemaTooNew, got {other:?}"),
        Ok(_) => panic!("expected SchemaTooNew, got an open writer"),
    }
    match recover(s.db()) {
        Err(StoreError::SchemaTooNew { .. }) => {}
        other => panic!("expected SchemaTooNew from recover, got {other:?}"),
    }

    // Put the version back: the checkpoint is still intact and recoverable.
    raw_user_version_bump(&s.db(), STORE_SCHEMA_VERSION);
    let rec = recover(s.db()).unwrap();
    assert_eq!(rec.checkpoint.tick, 10);
    assert_eq!(rec.checkpoint.bricks.len(), 2);
}

#[test]
fn journal_commit_is_durable_and_acknowledged() {
    let s = Scratch::new("journal_ok");
    let durable = {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
        let d = w
            .append_journal(&[pose_batch(1, 1), split_record(2, 2), pose_batch(3, 3)])
            .unwrap();
        assert_eq!(d.journal_seq.0, 3);
        assert!(w.metrics().journal_payload_bytes > 0);
        d
    };
    assert_eq!(durable.journal_seq.0, 3);

    let rec = recover(s.db()).unwrap();
    assert_eq!(rec.durable_through, 3);
    assert_eq!(rec.journal.len(), 3);
    assert!(rec.corruption.is_empty());
    // The split record recovers whole: SplitOff + both CellRuns.
    let (tx, _) = rec.journal[1].payload.as_topology().unwrap().unwrap();
    assert_eq!(tx.ops.len(), 3);
}

#[test]
fn crash_before_journal_commit_loses_the_batch_cleanly() {
    let s = Scratch::new("crash_before_j");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
        w.set_faults(FaultPlan::crash(CrashPoint::BeforeJournalCommit));
        let err = w
            .append_journal(&[pose_batch(1, 1), pose_batch(2, 2)])
            .unwrap_err();
        assert!(matches!(
            err,
            StoreError::CrashInjected(CrashPoint::BeforeJournalCommit)
        ));
        assert!(w.is_poisoned());
        // A poisoned writer refuses further durable work.
        assert!(matches!(
            w.append_journal(&[pose_batch(1, 1)]),
            Err(StoreError::Poisoned(_))
        ));
    }
    let rec = recover(s.db()).unwrap();
    assert_eq!(rec.durable_through, 0);
    assert!(rec.journal.is_empty());
    assert!(rec.corruption.is_empty());

    // A fresh writer resumes at seq 1 with no gap.
    let mut w = Writer::open(s.db()).unwrap();
    assert_eq!(w.journal_max_seq().unwrap(), 0);
    w.append_journal(&[pose_batch(1, 1)]).unwrap();
}

#[test]
fn crash_after_journal_commit_keeps_the_durable_prefix() {
    let s = Scratch::new("crash_after_j");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
        w.set_faults(FaultPlan::crash(CrashPoint::AfterJournalCommit));
        let err = w
            .append_journal(&[pose_batch(1, 1), split_record(2, 2)])
            .unwrap_err();
        assert!(matches!(
            err,
            StoreError::CrashInjected(CrashPoint::AfterJournalCommit)
        ));
    }
    // The batch committed before the (simulated) crash: it is durable even
    // though the caller never received the DurableThrough ack.
    let rec = recover(s.db()).unwrap();
    assert_eq!(rec.durable_through, 2);
    assert_eq!(rec.journal.len(), 2);
    assert!(rec.corruption.is_empty());
}

#[test]
fn crash_around_checkpoint_publication_leaves_no_partial_checkpoint() {
    for point in [
        CrashPoint::MidCheckpointRows,
        CrashPoint::BeforeCheckpointCommit,
    ] {
        let s = Scratch::new("crash_cp");
        {
            let mut w = Writer::open(s.db()).unwrap();
            w.publish_checkpoint(&checkpoint(100, 0)).unwrap();
            w.append_journal(&[pose_batch(1, 1)]).unwrap();
            w.set_faults(FaultPlan::crash(point));
            let err = w.publish_checkpoint(&checkpoint(200, 1)).unwrap_err();
            assert!(matches!(err, StoreError::CrashInjected(p) if p == point));
        }
        let rec = recover(s.db()).unwrap();
        assert_eq!(
            rec.checkpoint.tick, 100,
            "partial checkpoint {point:?} must be invisible"
        );
        assert!(rec.previous_checkpoint.is_none());
        assert_eq!(rec.durable_through, 1);
    }
}

#[test]
fn crash_after_checkpoint_commit_keeps_the_new_checkpoint() {
    let s = Scratch::new("crash_cp_after");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(100, 0)).unwrap();
        w.append_journal(&[pose_batch(1, 1)]).unwrap();
        w.set_faults(FaultPlan::crash(CrashPoint::AfterCheckpointCommit));
        let err = w.publish_checkpoint(&checkpoint(200, 1)).unwrap_err();
        assert!(matches!(
            err,
            StoreError::CrashInjected(CrashPoint::AfterCheckpointCommit)
        ));
    }
    let rec = recover(s.db()).unwrap();
    assert_eq!(rec.checkpoint.tick, 200);
    assert_eq!(rec.previous_checkpoint.map(|c| c.tick), Some(100));
}

#[test]
fn disk_fault_on_journal_prevents_false_success() {
    let s = Scratch::new("disk_j");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
        w.set_faults(FaultPlan::disk_fail_journal());
        let err = w.append_journal(&[pose_batch(1, 1)]).unwrap_err();
        assert!(matches!(err, StoreError::Disk(_)));
        assert!(w.is_poisoned());
    }
    let rec = recover(s.db()).unwrap();
    assert_eq!(rec.durable_through, 0);
    assert!(rec.journal.is_empty());
    assert!(
        rec.corruption.is_empty(),
        "rolled back cleanly, not corrupt"
    );
}

#[test]
fn disk_fault_on_checkpoint_prevents_false_success() {
    let s = Scratch::new("disk_cp");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(100, 0)).unwrap();
        w.set_faults(FaultPlan::disk_fail_checkpoint());
        let err = w.publish_checkpoint(&checkpoint(200, 0)).unwrap_err();
        assert!(matches!(err, StoreError::Disk(_)));
    }
    let rec = recover(s.db()).unwrap();
    assert_eq!(rec.checkpoint.tick, 100);
    assert!(rec.previous_checkpoint.is_none());
}

#[test]
fn save_air_brick_stays_empty_across_restart() {
    let s = Scratch::new("save_air");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(50, 0)).unwrap();
    }
    let rec = recover(s.db()).unwrap();
    let tomb = rec
        .checkpoint
        .bricks
        .iter()
        .find(|b| b.coord == [1, 0, 0])
        .expect("tombstone brick present");
    assert!(tomb.edited, "modified-air flag survives");
    assert_eq!(tomb.payload, BrickPayload::Uniform(0));
    assert!(decode_cells(&tomb.payload).unwrap().iter().all(|&c| c == 0));

    // The solid brick round-trips byte-for-byte.
    let solid = rec
        .checkpoint
        .bricks
        .iter()
        .find(|b| b.coord == [0, 0, 0])
        .unwrap();
    let cells = decode_cells(&solid.payload).unwrap();
    assert_eq!(cells.len(), spall_core::CELLS_PER_BRICK);
    assert!(cells.contains(&1) && cells.contains(&2));
}

#[test]
fn crash_transfer_recovers_whole_split_or_none_never_partial() {
    // Before-commit: the split record is absent entirely.
    {
        let s = Scratch::new("xfer_before");
        {
            let mut w = Writer::open(s.db()).unwrap();
            w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
            w.set_faults(FaultPlan::crash(CrashPoint::BeforeJournalCommit));
            let _ = w.append_journal(&[split_record(1, 1)]).unwrap_err();
        }
        let rec = recover(s.db()).unwrap();
        assert!(rec.journal.is_empty());
        assert_eq!(rec.durable_through, 0);
    }
    // After-commit: the split record is present with source-removal and
    // child-fill ops together — one atomic journal row.
    {
        let s = Scratch::new("xfer_after");
        {
            let mut w = Writer::open(s.db()).unwrap();
            w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
            w.set_faults(FaultPlan::crash(CrashPoint::AfterJournalCommit));
            let _ = w.append_journal(&[split_record(1, 1)]).unwrap_err();
        }
        let rec = recover(s.db()).unwrap();
        assert_eq!(rec.journal.len(), 1);
        let (tx, _) = rec.journal[0].payload.as_topology().unwrap().unwrap();
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
            "whole split or nothing"
        );
    }
}

#[test]
fn interior_journal_corruption_truncates_and_reports_with_fallback() {
    let s = Scratch::new("corrupt");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(10, 0)).unwrap();
        w.append_journal(&[pose_batch(1, 1), pose_batch(2, 2), pose_batch(3, 3)])
            .unwrap();
    }
    // Corrupt the payload of seq 2 without fixing its CRC.
    {
        let conn = rusqlite_open(&s.db());
        conn.execute("UPDATE journal SET payload = X'DEADBEEF' WHERE seq = 2", [])
            .unwrap();
    }
    let rec = recover(s.db()).unwrap();
    assert_eq!(
        rec.journal.len(),
        1,
        "replay stops before the corrupt record"
    );
    assert_eq!(rec.durable_through, 1);
    assert!(!rec.corruption.is_empty());
    assert_eq!(rec.checkpoint.tick, 10);
}

#[test]
fn retain_drops_old_checkpoints_and_covered_journal() {
    let s = Scratch::new("retain");
    let mut w = Writer::open(s.db()).unwrap();
    w.publish_checkpoint(&checkpoint(100, 0)).unwrap();
    w.append_journal(&[
        pose_batch(1, 1),
        pose_batch(2, 2),
        pose_batch(3, 3),
        pose_batch(4, 4),
        pose_batch(5, 5),
    ])
    .unwrap();
    w.publish_checkpoint(&checkpoint(200, 5)).unwrap();
    w.append_journal(&[pose_batch(6, 6), pose_batch(7, 7), pose_batch(8, 8)])
        .unwrap();

    let (dropped, pruned) = w.retain(1).unwrap();
    assert_eq!(dropped, 1);
    assert_eq!(pruned, 5);

    let rec = w.recover().unwrap();
    assert_eq!(rec.checkpoint.tick, 200);
    assert!(rec.previous_checkpoint.is_none());
    assert_eq!(
        rec.journal.iter().map(|j| j.seq).collect::<Vec<_>>(),
        vec![6, 7, 8]
    );
    assert_eq!(rec.durable_through, 8);
}

#[test]
fn metrics_expose_bytes_and_commit_rate() {
    let s = Scratch::new("metrics");
    let mut w = Writer::open(s.db()).unwrap();
    w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
    for i in 1..=10u64 {
        w.append_journal(&[split_record(i, i)]).unwrap();
    }
    let m = w.metrics();
    assert_eq!(m.journal_commits, 10);
    assert_eq!(m.journal_records, 10);
    assert!(m.bytes_per_journal_write() > 0.0);
    assert!(m.checkpoint_payload_bytes > 0);
    assert!(m.commit_bytes_per_sec() > 0.0);
}

// --- ENG-34: the durable journal high-water mark survives retention -------

/// Probe `review_retention_must_preserve_next_journal_sequence`: pruning every
/// journal row a checkpoint covers must not let the next sequence reset. Two
/// checkpoints at the same cursor, `retain(2)`, then the next append.
#[test]
fn review_retention_must_preserve_next_journal_sequence() {
    let s = Scratch::new("hwm_probe");
    let mut w = Writer::open(s.db()).unwrap();
    w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
    w.append_journal(&[pose_batch(1, 1)]).unwrap();
    w.publish_checkpoint(&checkpoint(10, 1)).unwrap();
    w.publish_checkpoint(&checkpoint(20, 1)).unwrap();
    w.retain(2).unwrap();

    let result = w.append_journal(&[pose_batch(2, 21)]);
    assert!(
        result.is_ok(),
        "valid append after pruning every covered journal row rejected: {result:?}"
    );
    assert_eq!(result.unwrap().journal_seq.0, 2);
}

/// Prune *every* journal row (no suffix left at all), across a writer reopen,
/// then keep editing: sequence ids continue past the pruned rows, never reuse.
#[test]
fn full_journal_prune_then_reopen_then_edit_never_reuses_sequences() {
    let s = Scratch::new("hwm_full_prune");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
        w.append_journal(&[pose_batch(1, 1), pose_batch(2, 2), pose_batch(3, 3)])
            .unwrap();
        // A checkpoint whose cursor covers the whole journal, then retain: the
        // journal table is emptied completely.
        w.publish_checkpoint(&checkpoint(30, 3)).unwrap();
        let (_, pruned) = w.retain(1).unwrap();
        assert_eq!(pruned, 3, "all three journal rows pruned");
        assert_eq!(w.journal_max_seq().unwrap(), 0, "journal table is empty");
        assert_eq!(
            w.durable_journal_high_water().unwrap(),
            3,
            "the high-water mark is preserved by the checkpoint cursor"
        );
    }

    // Idle restart: reopen the database, no new edits yet.
    {
        let w = Writer::open(s.db()).unwrap();
        assert_eq!(w.journal_max_seq().unwrap(), 0);
        assert_eq!(w.durable_journal_high_water().unwrap(), 3);
    }

    // A subsequent edit resumes at seq 4 — never 1 — with no gap error.
    let mut w = Writer::open(s.db()).unwrap();
    let d = w
        .append_journal(&[pose_batch(4, 40), pose_batch(5, 41)])
        .unwrap();
    assert_eq!(d.journal_seq.0, 5);

    // And the earlier sequences are genuinely gone, not re-handed out.
    let rec = w.recover().unwrap();
    assert_eq!(
        rec.journal.iter().map(|j| j.seq).collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert_eq!(rec.durable_through, 5);
}

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
    BrickPayload, Checkpoint, CrashPoint, FaultPlan, JournalPayload, JournalRecord, OutboxRecord,
    STORE_SCHEMA_VERSION, StoreError, StoredBrick, StoredWorldMeta, Writer, decode_cells,
    encode_cells, recover,
};

// --- tiny scratch-dir helper ------------------------------------------------

struct Scratch {
    dir: PathBuf,
}

#[test]
fn snow_geometry_journal_is_bounded_versioned_and_keeps_exact_owner_bytes() {
    let scratch = Scratch::new("snow-tile-journal");
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&checkpoint(0, 0)).unwrap();
    let mut record = JournalRecord {
        seq: 1,
        tick: 1,
        payload: JournalPayload::SnowTileState {
            world_epoch: 1,
            tile: [-1, 2, 3],
            state: vec![2, 0, 7],
        },
    };
    assert!(writer.append_journal(&[record.clone()]).is_err());
    record.payload = JournalPayload::SnowTileState {
        world_epoch: 1,
        tile: [-1, 2, 3],
        state: vec![1, 0, 0, 255, 129, 3],
    };
    writer.append_journal(&[record.clone()]).unwrap();
    drop(writer);
    let recovered = recover(scratch.db()).unwrap();
    assert_eq!(recovered.durable_through, 1);
    assert_eq!(recovered.journal, vec![record]);
    let oversized = JournalPayload::SnowTileState {
        world_epoch: 1,
        tile: [0; 3],
        state: vec![1; 64 * 1024 + 1],
    };
    assert!(oversized.validate_auxiliary_bounds().is_err());
}

#[test]
fn snow_checkpoint_round_trips_and_republication_clears_the_previous_blob() {
    let scratch = Scratch::new("snow-checkpoint");
    let mut cp = checkpoint(10, 0);
    // Storage treats owner-versioned snow as opaque bytes; simulation validates it.
    cp.snow = Some(vec![1, 0, 13, 21, 34]);
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&cp).unwrap();
    assert_eq!(writer.recover().unwrap().checkpoint, cp);
    cp.snow = None;
    writer.publish_checkpoint(&cp).unwrap();
    drop(writer);
    assert_eq!(recover(scratch.db()).unwrap().checkpoint, cp);
}

#[test]
fn current_schema_missing_auxiliary_tables_reject_without_mutation() {
    for missing in [
        "checkpoint_snow",
        "checkpoint_vegetation",
        "checkpoint_weather",
        "checkpoint_players",
        "game_outbox",
    ] {
        let scratch = Scratch::new(&format!("missing-{missing}"));
        let mut writer = Writer::open(scratch.db()).unwrap();
        writer.publish_checkpoint(&checkpoint(0, 0)).unwrap();
        drop(writer);
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        conn.execute(&format!("DROP TABLE {missing}"), []).unwrap();
        drop(conn);
        let before = std::fs::read(scratch.db()).unwrap();
        assert!(matches!(
            Writer::open(scratch.db()),
            Err(StoreError::Corrupt(_))
        ));
        assert!(matches!(recover(scratch.db()), Err(StoreError::Corrupt(_))));
        assert_eq!(
            std::fs::read(scratch.db()).unwrap(),
            before,
            "rejecting absent {missing} must not mutate the file"
        );
    }
}

#[test]
fn an_unversioned_nonempty_database_is_rejected_without_mutation() {
    let scratch = Scratch::new("unversioned-nonempty");
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    conn.execute_batch(
        "CREATE TABLE old_world(value BLOB); INSERT INTO old_world VALUES(x'0102');",
    )
    .unwrap();
    drop(conn);
    let before = std::fs::read(scratch.db()).unwrap();
    assert!(matches!(
        Writer::open(scratch.db()),
        Err(StoreError::SchemaTooOld { found: 0, .. })
    ));
    assert_eq!(std::fs::read(scratch.db()).unwrap(), before);
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
        vegetation: Vec::new(),
        tick,
        journal_cursor: cursor,
        world_hash: [tick as u8; 32],
        meta: m,
        bodies: vec![],
        column_water: None,
        weather: None,
        players: vec![spall_store::StoredPlayerHealth {
            player_id: [3; 16],
            health: 42,
        }],
        snow: None,
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
fn harvest_outbox_and_world_journal_share_one_crash_boundary() {
    let s = Scratch::new("harvest_outbox_atomic");
    let event = OutboxRecord {
        event_id: [42; 32],
        journal_seq: 1,
        payload: b"versioned harvest reward".to_vec(),
    };
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
        w.set_faults(FaultPlan::crash(CrashPoint::BeforeJournalCommit));
        assert!(matches!(
            w.append_journal_with_outbox(&[split_record(1, 1)], std::slice::from_ref(&event)),
            Err(StoreError::CrashInjected(CrashPoint::BeforeJournalCommit))
        ));
    }
    {
        let mut w = Writer::open(s.db()).unwrap();
        assert!(w.pending_outbox(8).unwrap().is_empty());
        assert_eq!(recover(s.db()).unwrap().durable_through, 0);
        w.append_journal_with_outbox(&[split_record(1, 1)], std::slice::from_ref(&event))
            .unwrap();
        assert_eq!(w.pending_outbox(8).unwrap(), vec![event.clone()]);
        // Crash after the destination transaction but before ack: delivery is
        // at-least-once and retains the same stable event identity on replay.
        assert_eq!(w.pending_outbox(8).unwrap(), vec![event.clone()]);
        w.acknowledge_outbox(&[event.event_id]).unwrap();
        assert!(w.pending_outbox(8).unwrap().is_empty());
        assert_eq!(recover(s.db()).unwrap().durable_through, 1);
    }
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
fn an_empty_database_is_distinct_from_one_whose_checkpoints_do_not_decode() {
    // A fresh database has the schema but no complete checkpoint: NoCheckpoint.
    let s = Scratch::new("empty_vs_corrupt");
    drop(Writer::open(s.db()).unwrap());
    assert!(matches!(recover(s.db()), Err(StoreError::NoCheckpoint)));

    // Publish a checkpoint, then make every complete checkpoint's metadata
    // undecodable: this is corruption, not an empty DB.
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(10, 0)).unwrap();
    }
    {
        let conn = rusqlite_open(&s.db());
        conn.execute("UPDATE checkpoints SET meta = X'DEADBEEF'", [])
            .unwrap();
    }
    match recover(s.db()) {
        Err(StoreError::CheckpointsUnrecoverable(detail)) => {
            assert!(detail.contains("checkpoint tick 10"));
        }
        other => panic!("expected CheckpointsUnrecoverable, got {other:?}"),
    }
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
fn real_sqlite_write_failure_on_journal_poisons_and_reports_no_success() {
    // A genuine engine-level write error (not a pre-`COMMIT` branch): the
    // connection is switched to `query_only`, so the staged `INSERT` returns a
    // real `rusqlite::Error` that propagates through `?`. The writer must poison
    // and no `DurableThrough` may be produced.
    let s = Scratch::new("real_io_j");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(0, 0)).unwrap();
        w.set_faults(FaultPlan::real_sqlite_write_failure());
        let err = w
            .append_journal(&[split_record(1, 1), pose_batch(2, 2)])
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Sqlite(_)),
            "expected a real rusqlite error, got {err:?}"
        );
        assert!(
            w.is_poisoned(),
            "a real write failure must poison the writer"
        );
        // Every further durable call is refused until re-open.
        assert!(matches!(
            w.append_journal(&[split_record(1, 1)]),
            Err(StoreError::Poisoned(_))
        ));
        assert!(matches!(
            w.publish_checkpoint(&checkpoint(10, 0)),
            Err(StoreError::Poisoned(_))
        ));
    }
    // Nothing was made durable; the checkpoint at tick 0 still stands alone.
    let rec = recover(s.db()).unwrap();
    assert_eq!(rec.durable_through, 0);
    assert!(rec.journal.is_empty());
    assert!(
        rec.corruption.is_empty(),
        "rolled back cleanly, not corrupt"
    );
    assert_eq!(rec.checkpoint.tick, 0);

    // A fresh writer resumes cleanly at seq 1.
    let mut w = Writer::open(s.db()).unwrap();
    assert_eq!(w.journal_max_seq().unwrap(), 0);
    w.append_journal(&[split_record(1, 1)]).unwrap();
}

#[test]
fn real_sqlite_write_failure_on_checkpoint_poisons_and_keeps_prior_checkpoint() {
    let s = Scratch::new("real_io_cp");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(100, 0)).unwrap();
        w.append_journal(&[pose_batch(1, 1)]).unwrap();
        w.set_faults(FaultPlan::real_sqlite_write_failure());
        let err = w.publish_checkpoint(&checkpoint(200, 1)).unwrap_err();
        assert!(
            matches!(err, StoreError::Sqlite(_)),
            "expected a real rusqlite error, got {err:?}"
        );
        assert!(w.is_poisoned());
    }
    let rec = recover(s.db()).unwrap();
    assert_eq!(
        rec.checkpoint.tick, 100,
        "the failed checkpoint never became visible"
    );
    assert!(rec.previous_checkpoint.is_none());
    assert_eq!(rec.durable_through, 1);
}

#[test]
fn disk_full_on_checkpoint_poisons_and_keeps_prior_checkpoint() {
    let s = Scratch::new("disk_full_cp");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(100, 0)).unwrap();
        w.append_journal(&[pose_batch(1, 1)]).unwrap();
        w.set_faults(FaultPlan::disk_full());
        let err = w.publish_checkpoint(&checkpoint(200, 1)).unwrap_err();
        assert!(
            matches!(err, StoreError::Disk(_)),
            "expected an out-of-disk error, got {err:?}"
        );
        assert!(
            w.is_poisoned(),
            "an out-of-disk write must poison the writer"
        );
    }
    let rec = recover(s.db()).unwrap();
    assert_eq!(
        rec.checkpoint.tick, 100,
        "the checkpoint that ran out of disk never became visible"
    );
    assert_eq!(rec.durable_through, 1);
    assert!(
        rec.corruption.is_empty(),
        "the failed write rolled back cleanly"
    );
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

#[test]
fn missing_or_corrupt_column_water_checkpoint_never_restores_a_silent_dry_world() {
    for (tag, sql) in [
        ("missing_water", "DELETE FROM checkpoint_column_water"),
        (
            "corrupt_water",
            "UPDATE checkpoint_column_water SET state = X'01'",
        ),
        ("missing_water_table", "DROP TABLE checkpoint_column_water"),
    ] {
        let scratch = Scratch::new(tag);
        {
            let mut writer = Writer::open(scratch.db()).unwrap();
            let mut cp = checkpoint(100, 0);
            cp.column_water = Some(spall_protocol::ColumnWaterState {
                version: spall_protocol::WIRE_SCHEMA_VERSION,
                y_lo: 0,
                y_hi: 32,
                frame_seq: 1,
                fluid_time_fixed: 0,
                spring_added_fixed: 0,
                drain_removed_fixed: 0,
                unplaced_fixed: 0,
                regions: vec![spall_protocol::column_water::ColumnRegionRecord {
                    rx: 0,
                    rz: 0,
                    spans: vec![spall_protocol::column_water::ColumnSpanRecord {
                        column: 5,
                        floor_y: 1,
                        volume: 1 << 32,
                    }],
                }],
                sources: vec![],
                gated_sources: [vec![], vec![], vec![]],
                sinks: vec![],
                gated_rate: 0,
                discharges: vec![],
            });
            writer.publish_checkpoint(&cp).unwrap();
        }
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        conn.execute_batch(sql).unwrap();
        drop(conn);
        if tag == "missing_water_table" {
            assert!(matches!(recover(scratch.db()), Err(StoreError::Corrupt(_))));
        } else {
            assert!(matches!(
                recover(scratch.db()),
                Err(StoreError::CheckpointsUnrecoverable(_))
            ));
        }
    }
}

#[test]
fn unsupported_save_versions_are_rejected_before_changing_rollback_journal_mode() {
    for version in [1, 2, 3, 4, STORE_SCHEMA_VERSION + 1] {
        let scratch = Scratch::new(&format!("rollback-schema-{version}"));
        {
            let mut writer = Writer::open(scratch.db()).unwrap();
            writer.publish_checkpoint(&checkpoint(0, 0)).unwrap();
        }
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        conn.execute_batch("PRAGMA journal_mode=DELETE").unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
        drop(conn);
        let bytes = std::fs::read(scratch.db()).unwrap();
        assert!(matches!(
            Writer::open(scratch.db()),
            Err(StoreError::SchemaTooOld { .. } | StoreError::SchemaTooNew { .. })
        ));
        assert!(
            std::fs::read(scratch.db()).unwrap() == bytes,
            "version rejection must not rewrite the SQLite journal-mode header"
        );
    }
}

#[test]
fn old_weather_checkpoint_and_journal_layouts_are_not_migrated() {
    use serde::Serialize;
    use spall_protocol::{WeatherClock, WeatherClockSource, WeatherKind, WeatherState};
    #[derive(Clone, Serialize)]
    struct HistoricalWeatherState {
        schema: u16,
        world_epoch: u64,
        world_revision: u64,
        effective_tick: u64,
        clock_ms: u64,
        clock_source: WeatherClockSource,
        start_season: u8,
        season_ms: u64,
        weather_seed: u64,
        transition_id: u64,
        transition_start_ms: u64,
        transition_duration_ms: u32,
        kind: WeatherKind,
        precipitation_q16: u16,
        wind_x_q15: i16,
        wind_z_q15: i16,
        gust_q16: u16,
        temperature_centi_c: i16,
        lightning_id: u64,
        lightning_tick: u64,
    }
    #[derive(Serialize)]
    enum OldJournal {
        WeatherState(HistoricalWeatherState),
    }
    let scratch = Scratch::new("weather-v1-rejected");
    let mut prior = checkpoint(100, 0);
    let mut current_cp = checkpoint(120, 0);
    let current = WeatherState::for_clock(
        42,
        3,
        120,
        WeatherClock {
            time_ms: 987_654_321,
            source: WeatherClockSource::ServerFallback,
            start_season: 0,
            season_ms: 900_000,
        },
        9,
    );
    prior.weather = Some(current.clone());
    current_cp.weather = Some(current.clone());
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&prior).unwrap();
    writer.publish_checkpoint(&current_cp).unwrap();
    drop(writer);
    let old = HistoricalWeatherState {
        schema: 1,
        world_epoch: current.world_epoch,
        world_revision: current.world_revision,
        effective_tick: current.effective_tick,
        clock_ms: current.clock_ms,
        clock_source: current.clock_source,
        start_season: current.start_season,
        season_ms: current.season_ms,
        weather_seed: current.weather_seed,
        transition_id: current.transition_id,
        transition_start_ms: current.transition_start_ms,
        transition_duration_ms: current.transition_duration_ms,
        kind: current.kind,
        precipitation_q16: current.precipitation_q16,
        wind_x_q15: current.wind_x_q15,
        wind_z_q15: current.wind_z_q15,
        gust_q16: current.gust_q16,
        temperature_centi_c: current.temperature_centi_c,
        lightning_id: current.lightning_id,
        lightning_tick: current.lightning_tick,
    };
    let checkpoint_blob = postcard::to_stdvec(&Some(old.clone())).unwrap();
    let journal_blob = postcard::to_stdvec(&OldJournal::WeatherState(old)).unwrap();
    let crc = |b: &[u8]| blake3::hash(b).as_bytes()[..16].to_vec();
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    conn.execute(
        "UPDATE checkpoint_weather SET state=?1,crc=?2 WHERE tick=120",
        rusqlite::params![checkpoint_blob, crc(&checkpoint_blob)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO journal(seq,tick,payload,crc) VALUES(1,121,?1,?2)",
        rusqlite::params![journal_blob, crc(&journal_blob)],
    )
    .unwrap();
    drop(conn);
    let recovered = recover(scratch.db()).unwrap();
    assert_eq!(
        recovered.checkpoint, prior,
        "invalid legacy checkpoint must fall back to the prior complete checkpoint"
    );
    assert!(
        recovered.journal.is_empty(),
        "legacy journal payload must not be converted"
    );
    assert!(
        recovered
            .corruption
            .iter()
            .any(|c| c.detail.contains("checkpoint tick 120"))
    );
    assert!(
        recovered
            .corruption
            .iter()
            .any(|c| c.detail.contains("journal seq 1 did not decode"))
    );
}
#[test]
fn weather_clock_checkpoint_and_journal_survive_restart_exactly() {
    use spall_protocol::{WeatherClock, WeatherClockSource, WeatherState};
    let scratch = Scratch::new("weather-restart");
    let mut cp = checkpoint(120, 0);
    let clock = WeatherClock {
        time_ms: 987_654_321,
        source: WeatherClockSource::ServerFallback,
        start_season: 0,
        season_ms: 900_000,
    };
    let mut state = WeatherState::for_clock(42, 3, 120, clock, 9);
    state.force_kind(Some(spall_protocol::WeatherKind::Snow), None);
    cp.weather = Some(state.clone());
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&cp).unwrap();
    let mut clock_next = WeatherState::for_clock(
        42,
        3,
        121,
        WeatherClock {
            time_ms: 987_654_337,
            source: WeatherClockSource::ServerFallback,
            start_season: 0,
            season_ms: 900_000,
        },
        9,
    );
    clock_next.force_kind(Some(spall_protocol::WeatherKind::Snow), None);
    writer
        .append_journal(&[JournalRecord {
            seq: 1,
            tick: 121,
            payload: JournalPayload::WeatherState(clock_next.clone()),
        }])
        .unwrap();
    drop(writer);
    let recovered = recover(scratch.db()).unwrap();
    assert_eq!(recovered.checkpoint.weather, Some(state));
    assert_eq!(recovered.journal.len(), 1);
    assert_eq!(
        recovered.journal[0].payload,
        JournalPayload::WeatherState(clock_next)
    );
}

#[test]
fn a_save_from_an_older_pre_release_build_is_refused_with_a_clear_error() {
    let s = Scratch::new("schema_old");
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&checkpoint(10, 0)).unwrap();
    }
    raw_user_version_bump(&s.db(), STORE_SCHEMA_VERSION - 1);
    let before = std::fs::read(s.db()).unwrap();

    let opened = Writer::open(s.db());
    let err = match opened {
        Err(err @ StoreError::SchemaTooOld { .. }) => err,
        Err(other) => panic!("expected SchemaTooOld, got {other:?}"),
        Ok(_) => panic!("an older save must not open"),
    };
    let text = err.to_string();
    assert!(
        text.contains("incompatible save") && text.contains("older pre-release build"),
        "unclear error: {text}"
    );
    assert!(matches!(
        recover(s.db()),
        Err(StoreError::SchemaTooOld { .. })
    ));
    assert_eq!(
        std::fs::read(s.db()).unwrap(),
        before,
        "refusing an old save must not modify it"
    );
}

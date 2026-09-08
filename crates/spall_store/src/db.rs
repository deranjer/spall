//! The single WAL writer.
//!
//! `docs/protocol.md`: "Use SQLite transactions through one I/O writer. […]
//! Configure and verify `journal_mode=WAL` and `synchronous=FULL` on the
//! writer. Group pending journal records into a database transaction, and
//! acknowledge durability only after its successful commit."
//!
//! [`Writer`] owns one [`rusqlite::Connection`], verifies those pragmas on
//! open, and exposes exactly two durable operations — [`Writer::append_journal`]
//! and [`Writer::publish_checkpoint`] — each a single DB transaction. A commit
//! that returns `Ok` is durable (`synchronous=FULL` + WAL); only then does
//! `append_journal` return a [`DurableThrough`]. A failed or crash-injected
//! write poisons the writer: further durable calls error until it is re-opened,
//! matching "stop accepting persistent edits and return an error".

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, params};
use spall_core::JournalSeq;
use spall_protocol::DurableThrough;

use crate::StoreError;
use crate::dto::{self, Checkpoint, JournalRecord, STORE_SCHEMA_VERSION};
use crate::fault::{CrashPoint, FaultPlan};
use crate::metrics::WriteMetrics;
use crate::recover::{Recovery, recover_conn};
use crate::schema::ensure_schema;

/// Default cap on records handed to one [`Writer::append_journal`] call. A
/// larger batch is refused rather than buffered without bound.
pub const DEFAULT_MAX_BATCH_RECORDS: usize = 4096;

/// Result of a `PRAGMA wal_checkpoint(TRUNCATE)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalCheckpoint {
    /// `1` if another connection held the WAL busy (no truncation happened).
    pub busy: i64,
    /// WAL frames written back to the database file.
    pub checkpointed: i64,
    /// WAL frames total at the time of the call.
    pub log_frames: i64,
}

/// The single durable writer for one world database.
pub struct Writer {
    conn: Connection,
    path: PathBuf,
    faults: FaultPlan,
    metrics: WriteMetrics,
    poison: Option<String>,
    max_batch_records: usize,
}

impl Writer {
    /// Opens (creating if absent) the world database at `path`, verifying the
    /// durability pragmas and the schema version.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_faults(path, FaultPlan::default())
    }

    /// [`Writer::open`] with fault injection armed (tests only).
    pub fn open_with_faults(path: impl AsRef<Path>, faults: FaultPlan) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // WAL + FULL, then read them back — a silent fallback to rollback
        // journalling or a weaker sync would break the durability contract.
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::NotWalMode(mode));
        }
        conn.execute_batch("PRAGMA synchronous=FULL")?;
        let sync: i64 = conn.pragma_query_value(None, "synchronous", |r| r.get(0))?;
        if sync < 2 {
            return Err(StoreError::NotSynchronousFull(sync));
        }

        ensure_schema(&conn)?;

        Ok(Self {
            conn,
            path,
            faults,
            metrics: WriteMetrics::default(),
            poison: None,
            max_batch_records: DEFAULT_MAX_BATCH_RECORDS,
        })
    }

    /// Replaces the armed fault plan.
    pub fn set_faults(&mut self, faults: FaultPlan) {
        self.faults = faults;
    }

    /// Cumulative durable-write counters.
    pub fn metrics(&self) -> &WriteMetrics {
        &self.metrics
    }

    /// The database file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `true` once a failed or crash-injected write has stopped this writer.
    pub fn is_poisoned(&self) -> bool {
        self.poison.is_some()
    }

    fn ensure_live(&self) -> Result<(), StoreError> {
        match &self.poison {
            Some(reason) => Err(StoreError::Poisoned(reason.clone())),
            None => Ok(()),
        }
    }

    fn poison(&mut self, reason: impl Into<String>) {
        if self.poison.is_none() {
            self.poison = Some(reason.into());
        }
    }

    /// Arms `query_only` on the connection so the next staged write fails with a
    /// genuine `rusqlite` engine error (`FaultPlan::real_write_failure`). Called
    /// just before the durable transaction opens.
    fn maybe_arm_real_write_failure(&mut self) -> Result<(), StoreError> {
        if std::mem::take(&mut self.faults.real_write_failure) {
            self.conn.pragma_update(None, "query_only", "ON")?;
        }
        Ok(())
    }

    /// Best-effort clear of an armed `query_only` after a real-write-failure
    /// injection. The writer is already poisoned by this point; this only keeps
    /// a reused connection sane for read-only [`Writer::recover`].
    fn disarm_real_write_failure(&mut self) {
        let _ = self.conn.pragma_update(None, "query_only", "OFF");
    }

    /// Highest journal sequence currently stored (`0` if the journal is empty).
    pub fn journal_max_seq(&self) -> Result<u64, StoreError> {
        let v: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM journal", [], |r| {
                r.get(0)
            })?;
        Ok(v as u64)
    }

    /// Groups `records` into one DB transaction and commits it. On success the
    /// batch is durable and the returned [`DurableThrough`] names its last
    /// sequence. `records` must be contiguous and start exactly one past the
    /// stored maximum.
    pub fn append_journal(
        &mut self,
        records: &[JournalRecord],
    ) -> Result<DurableThrough, StoreError> {
        self.ensure_live()?;
        if records.is_empty() {
            return Err(StoreError::Empty);
        }
        if records.len() > self.max_batch_records {
            self.poison("journal batch over cap");
            return Err(StoreError::QueueFull {
                pending: records.len(),
                cap: self.max_batch_records,
            });
        }

        let start = self
            .journal_max_seq()?
            .checked_add(1)
            .ok_or(StoreError::JournalGap {
                expected: u64::MAX,
                got: u64::MAX,
            })?;
        for (offset, r) in records.iter().enumerate() {
            let expect = start
                .checked_add(offset as u64)
                .ok_or(StoreError::JournalGap {
                    expected: u64::MAX,
                    got: r.seq,
                })?;
            if r.seq != expect {
                return Err(StoreError::JournalGap {
                    expected: expect,
                    got: r.seq,
                });
            }
        }

        // Arm a genuine engine write failure (if requested) before the txn opens.
        self.maybe_arm_real_write_failure()?;

        // The durable step runs against disjoint field borrows so that *any*
        // failure — a real `rusqlite` error propagated by `?` included — falls
        // through to `poison` below. Nothing on this path may return `Err`
        // without poisoning the writer (`docs/protocol.md`: "stop accepting
        // persistent edits and return an error").
        let conn = &mut self.conn;
        let faults = &mut self.faults;
        let metrics = &mut self.metrics;
        let outcome = (|| -> Result<u64, StoreError> {
            let mut payload_bytes = 0u64;
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO journal (seq, tick, payload, crc) VALUES (?, ?, ?, ?)",
                )?;
                for r in records {
                    let body = dto::encode(&r.payload)?;
                    let crc = crc16(&body);
                    stmt.execute(params![r.seq as i64, r.tick as i64, &body, &crc])?;
                    payload_bytes += body.len() as u64;
                }
            }

            if faults.take_crash(CrashPoint::BeforeJournalCommit) {
                drop(tx); // rollback
                return Err(StoreError::CrashInjected(CrashPoint::BeforeJournalCommit));
            }
            if std::mem::take(&mut faults.fail_journal_commit) {
                drop(tx); // rollback
                return Err(StoreError::Disk("journal commit failed (injected)".into()));
            }

            let started = Instant::now();
            tx.commit()?;
            metrics.record_commit(started.elapsed());
            metrics.journal_commits += 1;
            metrics.journal_records += records.len() as u64;
            metrics.journal_payload_bytes += payload_bytes;

            let last = records.last().expect("non-empty checked above").seq;

            if faults.take_crash(CrashPoint::AfterJournalCommit) {
                // The batch *is* durable, but the caller must re-open before any
                // further durable call — the ack was never delivered.
                return Err(StoreError::CrashInjected(CrashPoint::AfterJournalCommit));
            }
            Ok(last)
        })();

        self.observe_wal();
        match outcome {
            Ok(last) => Ok(DurableThrough {
                journal_seq: JournalSeq(last),
            }),
            Err(e) => {
                self.disarm_real_write_failure();
                self.poison(format!("durable journal write failed: {e}"));
                Err(e)
            }
        }
    }

    /// Writes every body/brick row of `checkpoint`, the world metadata, and the
    /// `complete = 1` marker in one DB transaction (`docs/protocol.md`:
    /// "Checkpoint publication records all brick/body data and its journal
    /// cursor in one DB transaction").
    pub fn publish_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<(), StoreError> {
        self.ensure_live()?;
        if checkpoint.meta.store_schema_version != STORE_SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: checkpoint.meta.store_schema_version,
                supported: STORE_SCHEMA_VERSION,
            });
        }

        let meta_blob = dto::encode(&checkpoint.meta)?;
        let now_ms = unix_millis();

        self.maybe_arm_real_write_failure()?;

        // Disjoint field borrows so every failure path — real `rusqlite` errors
        // propagated by `?` included — poisons the writer below.
        let conn = &mut self.conn;
        let faults = &mut self.faults;
        let metrics = &mut self.metrics;
        let outcome = (|| -> Result<(), StoreError> {
            let mut payload_bytes = 0u64;
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT OR REPLACE INTO checkpoints \
                 (tick, journal_cursor, world_hash, meta, complete, created_unix_ms) \
                 VALUES (?, ?, ?, ?, 0, ?)",
                params![
                    checkpoint.tick as i64,
                    checkpoint.journal_cursor as i64,
                    &checkpoint.world_hash[..],
                    &meta_blob,
                    now_ms,
                ],
            )?;
            // A re-published tick starts clean.
            tx.execute(
                "DELETE FROM checkpoint_bodies WHERE tick = ?",
                params![checkpoint.tick as i64],
            )?;
            tx.execute(
                "DELETE FROM checkpoint_bricks WHERE tick = ?",
                params![checkpoint.tick as i64],
            )?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO checkpoint_bodies (tick, entity_id, body) VALUES (?, ?, ?)",
                )?;
                for body in &checkpoint.bodies {
                    let blob = dto::encode(body)?;
                    payload_bytes += blob.len() as u64;
                    stmt.execute(params![
                        checkpoint.tick as i64,
                        body.entity_id as i64,
                        &blob
                    ])?;
                }
            }

            if faults.take_crash(CrashPoint::MidCheckpointRows) {
                drop(tx);
                return Err(StoreError::CrashInjected(CrashPoint::MidCheckpointRows));
            }

            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO checkpoint_bricks \
                     (tick, volume_id, bx, by, bz, revision, edited, payload) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )?;
                for brick in &checkpoint.bricks {
                    let blob = dto::encode(&brick.payload)?;
                    payload_bytes += blob.len() as u64;
                    stmt.execute(params![
                        checkpoint.tick as i64,
                        brick.volume_id as i64,
                        brick.coord[0],
                        brick.coord[1],
                        brick.coord[2],
                        brick.revision as i64,
                        brick.edited,
                        &blob,
                    ])?;
                }
            }

            tx.execute(
                "INSERT OR REPLACE INTO world (id, meta) VALUES (1, ?)",
                params![&meta_blob],
            )?;
            tx.execute(
                "UPDATE checkpoints SET complete = 1 WHERE tick = ?",
                params![checkpoint.tick as i64],
            )?;

            if faults.take_crash(CrashPoint::BeforeCheckpointCommit) {
                drop(tx);
                return Err(StoreError::CrashInjected(
                    CrashPoint::BeforeCheckpointCommit,
                ));
            }
            if std::mem::take(&mut faults.fail_checkpoint_commit) {
                drop(tx);
                return Err(StoreError::Disk(
                    "checkpoint commit failed (injected)".into(),
                ));
            }

            let started = Instant::now();
            tx.commit()?;
            metrics.record_commit(started.elapsed());
            metrics.checkpoint_publishes += 1;
            metrics.checkpoint_payload_bytes += payload_bytes;

            if faults.take_crash(CrashPoint::AfterCheckpointCommit) {
                return Err(StoreError::CrashInjected(CrashPoint::AfterCheckpointCommit));
            }
            Ok(())
        })();

        self.observe_wal();
        match outcome {
            Ok(()) => Ok(()),
            Err(e) => {
                self.disarm_real_write_failure();
                self.poison(format!("durable checkpoint write failed: {e}"));
                Err(e)
            }
        }
    }

    /// Keeps the newest `keep` complete checkpoints and drops older ones, then
    /// prunes journal rows no retained checkpoint still needs. Returns
    /// `(checkpoints_dropped, journal_rows_pruned)`.
    pub fn retain(&mut self, keep: usize) -> Result<(u64, u64), StoreError> {
        self.ensure_live()?;
        let keep = keep.max(1);
        let kept_ticks: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT tick FROM checkpoints WHERE complete = 1 ORDER BY tick DESC LIMIT ?",
            )?;
            let rows = stmt.query_map(params![keep as i64], |r| r.get::<_, i64>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        if kept_ticks.is_empty() {
            return Ok((0, 0));
        }
        let oldest_kept = *kept_ticks.iter().min().expect("non-empty");
        let oldest_kept_cursor: i64 = self.conn.query_row(
            "SELECT journal_cursor FROM checkpoints WHERE tick = ?",
            params![oldest_kept],
            |r| r.get(0),
        )?;

        let tx = self.conn.transaction()?;
        let dropped = tx.execute(
            "DELETE FROM checkpoints WHERE tick < ?",
            params![oldest_kept],
        )? as u64;
        let pruned = tx.execute(
            "DELETE FROM journal WHERE seq <= ?",
            params![oldest_kept_cursor],
        )? as u64;
        tx.commit()?;
        Ok((dropped, pruned))
    }

    /// Runs `PRAGMA wal_checkpoint(TRUNCATE)` off the hot path.
    pub fn wal_checkpoint(&mut self) -> Result<WalCheckpoint, StoreError> {
        self.ensure_live()?;
        let started = Instant::now();
        let out = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok(WalCheckpoint {
                    busy: r.get(0)?,
                    log_frames: r.get(1)?,
                    checkpointed: r.get(2)?,
                })
            })?;
        self.metrics.record_commit(started.elapsed());
        self.metrics.wal_checkpoints += 1;
        self.observe_wal();
        Ok(out)
    }

    /// Loads the latest complete checkpoint and the durable journal suffix,
    /// reusing this writer's connection.
    pub fn recover(&self) -> Result<Recovery, StoreError> {
        recover_conn(&self.conn)
    }

    fn observe_wal(&mut self) {
        // SQLite appends "-wal" to the full database filename.
        let wal = PathBuf::from(format!("{}-wal", self.path.display()));
        if let Ok(meta) = std::fs::metadata(&wal) {
            self.metrics.max_wal_bytes = self.metrics.max_wal_bytes.max(meta.len());
        }
    }
}

/// BLAKE3-16 of a journal payload — an interior-corruption check, not a
/// cryptographic MAC.
pub(crate) fn crc16(bytes: &[u8]) -> Vec<u8> {
    blake3::hash(bytes).as_bytes()[..16].to_vec()
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

//! Bounded asynchronous persistence pipeline (ENG-50).
//!
//! T16 published checkpoints and journal transactions **synchronously inside the
//! simulation tick loop** (a `spawn_blocking` thread, but still the thread that
//! owns physics). A disk stall therefore stalled physics. `docs/protocol.md`
//! Persistence requires the opposite: "schedule database checkpoint work off the
//! simulation thread", "Group disk flushes […] then emit `DurableThrough`", and
//! "If the storage queue exceeds its limit or disk writes fail, stop accepting
//! persistent edits and return an error; do not silently continue an unsavable
//! world."
//!
//! [`PersistPipeline`] owns the single [`Writer`] on a dedicated OS thread and
//! accepts **immutable** [`PersistJob`]s over a *bounded* channel. The sim
//! thread only ever `try_send`s: it never blocks on disk. When the backlog is
//! full, or the writer has already failed, submission returns an error and the
//! caller is expected to stop the run rather than continue an unsavable world.
//!
//! Retained-snapshot memory is therefore bounded by
//! `queue_capacity * max(job size)`; the queue never grows without limit.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use spall_store::{Checkpoint, JournalRecord, WriteMetrics, Writer};

/// Default bounded depth of the persistence job queue. Each slot is one journal
/// batch, one checkpoint, or one retain request. A burst past this stops the
/// run (`docs/protocol.md`: "If the storage queue exceeds its limit […] return
/// an error").
pub const DEFAULT_QUEUE_CAPACITY: usize = 64;

/// One immutable unit of durable work handed to the writer thread.
pub enum PersistJob {
    /// A contiguous journal batch (topology transactions and/or 20 Hz pose
    /// batches), ascending by `seq`, starting one past the stored maximum.
    Journal(Vec<JournalRecord>),
    /// A whole immutable engine checkpoint.
    Checkpoint(Box<Checkpoint>),
    /// Keep the newest `keep` complete checkpoints and prune journal rows no
    /// retained checkpoint still needs.
    Retain(usize),
    /// Run `PRAGMA wal_checkpoint(TRUNCATE)` off the hot path.
    WalCheckpoint,
}

/// Tunables for a [`PersistPipeline`].
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Bounded number of queued jobs before submission fails.
    pub queue_capacity: usize,
    /// Test / soak knob: sleep this long before every durable write to model a
    /// slow disk. `None` in production.
    pub io_delay: Option<Duration>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            io_delay: None,
        }
    }
}

/// Why a submission or shutdown failed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PipelineError {
    #[error("persistence backlog full: {queued} jobs queued at capacity {capacity}")]
    Backlog { queued: usize, capacity: usize },
    #[error("persistence writer stopped: {0}")]
    Failed(String),
}

/// Cumulative, lock-guarded progress the writer thread publishes back.
#[derive(Debug, Default, Clone)]
struct Shared {
    /// Highest journal `seq` acknowledged durable (`DurableThrough`).
    durable_seq: u64,
    journal_batches: u64,
    journal_records: u64,
    checkpoints_published: u64,
    retains: u64,
    /// Set once, the first time a durable write fails or a batch is refused.
    error: Option<String>,
    /// Wall time of the most recent journal flush / checkpoint publish.
    last_journal_flush: Duration,
    last_checkpoint: Duration,
    max_journal_flush: Duration,
    max_checkpoint: Duration,
    /// Largest queue depth ever observed at submission time.
    max_queue_depth: usize,
}

/// A cheap snapshot of pipeline progress for the sim loop.
#[derive(Debug, Clone)]
pub struct PipelineStatus {
    pub durable_seq: u64,
    pub journal_batches: u64,
    pub journal_records: u64,
    pub checkpoints_published: u64,
    pub error: Option<String>,
    pub queue_depth: usize,
    pub max_queue_depth: usize,
    pub last_journal_flush: Duration,
    pub last_checkpoint: Duration,
    pub max_journal_flush: Duration,
    pub max_checkpoint: Duration,
}

/// What the pipeline reports once its thread has drained and exited.
#[derive(Debug, Clone)]
pub struct PipelineOutcome {
    pub status: PipelineStatus,
    pub metrics: WriteMetrics,
    /// `true` if a durable write failed and poisoned the writer.
    pub poisoned: bool,
}

/// A single-writer, bounded, off-thread durable persistence sink.
pub struct PersistPipeline {
    tx: Option<SyncSender<PersistJob>>,
    handle: Option<JoinHandle<Writer>>,
    shared: Arc<Mutex<Shared>>,
    depth: Arc<AtomicUsize>,
    capacity: usize,
}

impl PersistPipeline {
    /// Takes ownership of `writer` and starts the writer thread.
    pub fn spawn(writer: Writer, config: PipelineConfig) -> Self {
        let capacity = config.queue_capacity.max(1);
        let (tx, rx) = sync_channel::<PersistJob>(capacity);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let depth = Arc::new(AtomicUsize::new(0));

        let worker_shared = shared.clone();
        let worker_depth = depth.clone();
        let io_delay = config.io_delay;
        let handle = std::thread::Builder::new()
            .name("spall-persist".into())
            .spawn(move || {
                let mut writer = writer;
                while let Ok(job) = rx.recv() {
                    worker_depth.fetch_sub(1, Ordering::AcqRel);
                    if let Some(d) = io_delay {
                        std::thread::sleep(d);
                    }
                    let started = Instant::now();
                    let outcome = run_job(&mut writer, job);
                    let elapsed = started.elapsed();
                    let mut s = worker_shared.lock().unwrap_or_else(|e| e.into_inner());
                    match outcome {
                        Ok(progress) => apply_progress(&mut s, progress, elapsed),
                        Err(reason) => {
                            if s.error.is_none() {
                                s.error = Some(reason);
                            }
                            // Stop touching the disk; the sim loop learns from
                            // `status().error` or the next `submit_*` and ends
                            // the run. The queue then disconnects.
                            break;
                        }
                    }
                }
                writer
            })
            .expect("spawn persistence thread");

        Self {
            tx: Some(tx),
            handle: Some(handle),
            shared,
            depth,
            capacity,
        }
    }

    /// Queues a contiguous journal batch. Non-blocking: returns
    /// [`PipelineError::Backlog`] if the queue is full and
    /// [`PipelineError::Failed`] once the writer has failed.
    pub fn submit_journal(&self, records: Vec<JournalRecord>) -> Result<(), PipelineError> {
        if records.is_empty() {
            return Ok(());
        }
        self.submit(PersistJob::Journal(records))
    }

    /// Queues a whole immutable checkpoint.
    pub fn submit_checkpoint(&self, checkpoint: Checkpoint) -> Result<(), PipelineError> {
        self.submit(PersistJob::Checkpoint(Box::new(checkpoint)))
    }

    /// Queues a retention pass (keep newest `keep` checkpoints, prune covered
    /// journal rows).
    pub fn submit_retain(&self, keep: usize) -> Result<(), PipelineError> {
        self.submit(PersistJob::Retain(keep))
    }

    /// Queues an off-thread WAL truncate.
    pub fn submit_wal_checkpoint(&self) -> Result<(), PipelineError> {
        self.submit(PersistJob::WalCheckpoint)
    }

    /// Like [`PersistPipeline::submit_journal`] but **blocks** until the bounded
    /// queue has room instead of returning [`PipelineError::Backlog`]. Only for
    /// non-real-time producers (soak drivers, clean shutdown) — never the
    /// simulation tick loop, which must never wait on the disk.
    pub fn submit_journal_blocking(
        &self,
        records: Vec<JournalRecord>,
    ) -> Result<(), PipelineError> {
        if records.is_empty() {
            return Ok(());
        }
        self.submit_blocking(PersistJob::Journal(records))
    }

    /// Blocking [`PersistPipeline::submit_checkpoint`] (see
    /// [`PersistPipeline::submit_journal_blocking`]).
    pub fn submit_checkpoint_blocking(&self, checkpoint: Checkpoint) -> Result<(), PipelineError> {
        self.submit_blocking(PersistJob::Checkpoint(Box::new(checkpoint)))
    }

    /// Blocking [`PersistPipeline::submit_retain`].
    pub fn submit_retain_blocking(&self, keep: usize) -> Result<(), PipelineError> {
        self.submit_blocking(PersistJob::Retain(keep))
    }

    fn submit_blocking(&self, job: PersistJob) -> Result<(), PipelineError> {
        if let Some(reason) = self.error() {
            return Err(PipelineError::Failed(reason));
        }
        let Some(tx) = self.tx.as_ref() else {
            return Err(PipelineError::Failed("pipeline already shut down".into()));
        };
        // Count the slot before the job is visible to the worker, so the
        // worker's decrement can never race ahead of this increment.
        let depth = self.depth.fetch_add(1, Ordering::AcqRel) + 1;
        match tx.send(job) {
            Ok(()) => {
                let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                s.max_queue_depth = s.max_queue_depth.max(depth);
                Ok(())
            }
            Err(_) => {
                self.depth.fetch_sub(1, Ordering::AcqRel);
                Err(PipelineError::Failed(
                    self.error()
                        .unwrap_or_else(|| "writer thread exited".into()),
                ))
            }
        }
    }

    fn submit(&self, job: PersistJob) -> Result<(), PipelineError> {
        if let Some(reason) = self.error() {
            return Err(PipelineError::Failed(reason));
        }
        let Some(tx) = self.tx.as_ref() else {
            return Err(PipelineError::Failed("pipeline already shut down".into()));
        };
        // Reserve the slot first (see `submit_blocking`); undo it if the send
        // does not land.
        let depth = self.depth.fetch_add(1, Ordering::AcqRel) + 1;
        match tx.try_send(job) {
            Ok(()) => {
                let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                s.max_queue_depth = s.max_queue_depth.max(depth);
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                let queued = self.depth.fetch_sub(1, Ordering::AcqRel) - 1;
                let msg = format!(
                    "storage queue exceeded its {} job limit ({queued} queued)",
                    self.capacity
                );
                let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                if s.error.is_none() {
                    s.error = Some(msg);
                }
                Err(PipelineError::Backlog {
                    queued,
                    capacity: self.capacity,
                })
            }
            Err(TrySendError::Disconnected(_)) => {
                self.depth.fetch_sub(1, Ordering::AcqRel);
                Err(PipelineError::Failed(
                    self.error()
                        .unwrap_or_else(|| "writer thread exited".to_string()),
                ))
            }
        }
    }

    /// The first recorded failure reason, if the writer has stopped.
    pub fn error(&self) -> Option<String> {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone()
    }

    /// A cheap progress snapshot.
    pub fn status(&self) -> PipelineStatus {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        PipelineStatus {
            durable_seq: s.durable_seq,
            journal_batches: s.journal_batches,
            journal_records: s.journal_records,
            checkpoints_published: s.checkpoints_published,
            error: s.error.clone(),
            queue_depth: self.depth.load(Ordering::Acquire),
            max_queue_depth: s.max_queue_depth,
            last_journal_flush: s.last_journal_flush,
            last_checkpoint: s.last_checkpoint,
            max_journal_flush: s.max_journal_flush,
            max_checkpoint: s.max_checkpoint,
        }
    }

    /// Highest journal `seq` acknowledged durable so far.
    pub fn durable_seq(&self) -> u64 {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .durable_seq
    }

    /// Blocks until every queued job has been processed and the writer thread
    /// has exited, then returns the final metrics and progress.
    pub fn shutdown(mut self) -> PipelineOutcome {
        // Drop the sender so the worker's `recv` returns and it exits.
        self.tx = None;
        let writer = self
            .handle
            .take()
            .expect("handle present until shutdown")
            .join()
            .expect("persistence thread panicked");
        PipelineOutcome {
            status: self.status(),
            metrics: writer.metrics().clone(),
            poisoned: writer.is_poisoned(),
        }
    }
}

impl Drop for PersistPipeline {
    fn drop(&mut self) {
        // If `shutdown` was not called, still join so the thread and its file
        // handles are released deterministically.
        self.tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

enum Progress {
    Journal { through: u64, records: u64 },
    Checkpoint,
    Retain,
    Wal,
}

fn run_job(writer: &mut Writer, job: PersistJob) -> Result<Progress, String> {
    match job {
        PersistJob::Journal(records) => {
            let n = records.len() as u64;
            writer
                .append_journal(&records)
                .map(|d| Progress::Journal {
                    through: d.journal_seq.0,
                    records: n,
                })
                .map_err(|e| format!("journal flush failed: {e}"))
        }
        PersistJob::Checkpoint(cp) => writer
            .publish_checkpoint(&cp)
            .map(|()| Progress::Checkpoint)
            .map_err(|e| format!("checkpoint publish failed: {e}")),
        PersistJob::Retain(keep) => writer
            .retain(keep)
            .map(|_| Progress::Retain)
            .map_err(|e| format!("journal retain failed: {e}")),
        PersistJob::WalCheckpoint => writer
            .wal_checkpoint()
            .map(|_| Progress::Wal)
            .map_err(|e| format!("wal checkpoint failed: {e}")),
    }
}

fn apply_progress(s: &mut Shared, progress: Progress, elapsed: Duration) {
    match progress {
        Progress::Journal { through, records } => {
            s.durable_seq = s.durable_seq.max(through);
            s.journal_batches += 1;
            s.journal_records += records;
            s.last_journal_flush = elapsed;
            s.max_journal_flush = s.max_journal_flush.max(elapsed);
        }
        Progress::Checkpoint => {
            s.checkpoints_published += 1;
            s.last_checkpoint = elapsed;
            s.max_checkpoint = s.max_checkpoint.max(elapsed);
        }
        Progress::Retain => s.retains += 1,
        Progress::Wal => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_store::{JournalPayload, Writer};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    fn scratch_db(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("spall_pipeline_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("world.db")
    }

    fn rec(seq: u64) -> JournalRecord {
        JournalRecord {
            seq,
            tick: seq,
            payload: JournalPayload::PoseBatch {
                snapshots: vec![vec![0u8; 32]],
            },
        }
    }

    #[test]
    fn contiguous_batches_are_acknowledged_durable_off_thread() {
        let db = scratch_db("durable");
        let pipe = PersistPipeline::spawn(Writer::open(&db).unwrap(), PipelineConfig::default());
        for s in 1..=10 {
            pipe.submit_journal(vec![rec(s)]).unwrap();
        }
        let out = pipe.shutdown();
        assert!(out.status.error.is_none(), "{:?}", out.status.error);
        assert_eq!(out.status.durable_seq, 10);
        assert_eq!(out.status.journal_records, 10);
        assert_eq!(out.metrics.journal_commits, 10);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_full_queue_is_refused_without_blocking_the_submitter() {
        let db = scratch_db("backlog");
        let pipe = PersistPipeline::spawn(
            Writer::open(&db).unwrap(),
            PipelineConfig {
                queue_capacity: 2,
                io_delay: Some(Duration::from_millis(80)),
            },
        );
        // The submitter must never block for the disk. Fill far past capacity
        // and time the loop: it stays quick and eventually returns Backlog.
        let started = Instant::now();
        let mut hit_backlog = false;
        let mut seq = 1u64;
        for _ in 0..64 {
            match pipe.submit_journal(vec![rec(seq)]) {
                Ok(()) => seq += 1,
                Err(PipelineError::Backlog { capacity, .. }) => {
                    assert_eq!(capacity, 2);
                    hit_backlog = true;
                    break;
                }
                Err(e) => panic!("unexpected {e}"),
            }
        }
        let elapsed = started.elapsed();
        assert!(hit_backlog, "a bounded queue must refuse a sustained burst");
        assert!(
            elapsed < Duration::from_millis(80) * 8,
            "submitter blocked on the disk: {elapsed:?}"
        );
        // Once the backlog tripped, the pipeline is stopped: no false acks.
        assert!(pipe.error().is_some());
        assert!(pipe.submit_journal(vec![rec(999)]).is_err());
        let out = pipe.shutdown();
        assert!(out.status.durable_seq < seq, "only real commits are acked");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_disk_failure_stops_the_pipeline_and_is_not_reported_as_success() {
        use spall_store::FaultPlan;
        let db = scratch_db("diskfail");
        let mut writer = Writer::open(&db).unwrap();
        writer.set_faults(FaultPlan::disk_fail_journal());
        let pipe = PersistPipeline::spawn(writer, PipelineConfig::default());
        pipe.submit_journal(vec![rec(1)]).unwrap();
        // Give the worker a moment to process and record the failure.
        for _ in 0..200 {
            if pipe.error().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(pipe.error().is_some(), "disk failure must surface");
        assert!(pipe.submit_checkpoint(dummy_checkpoint()).is_err());
        let out = pipe.shutdown();
        assert_eq!(out.status.durable_seq, 0, "nothing was made durable");
        assert!(out.poisoned);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    fn dummy_checkpoint() -> Checkpoint {
        use spall_store::{STORE_SCHEMA_VERSION, StoredWorldMeta};
        Checkpoint {
            tick: 1,
            journal_cursor: 0,
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
                next_journal_seq: 1,
                integer_brush_version: 1,
                structure_graph_version: 1,
                topology_hash_version: 1,
            },
            bodies: vec![],
            bricks: vec![],
        }
    }
}

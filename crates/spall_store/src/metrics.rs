//! Persistence throughput counters (`docs/validation.md`: "Include WAL size and
//! flush/checkpoint latency in persistence metrics"; T16: "Report measured
//! bytes/write rate").

use std::time::Duration;

/// Cumulative durable-write counters for one [`Writer`](crate::Writer).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WriteMetrics {
    /// Journal `COMMIT`s that succeeded.
    pub journal_commits: u64,
    /// Journal records written across all commits.
    pub journal_records: u64,
    /// Journal payload bytes written (postcard body, pre-WAL-framing).
    pub journal_payload_bytes: u64,
    /// Checkpoint publications that succeeded.
    pub checkpoint_publishes: u64,
    /// Body + brick blob bytes written across all checkpoints.
    pub checkpoint_payload_bytes: u64,
    /// `PRAGMA wal_checkpoint(TRUNCATE)` runs.
    pub wal_checkpoints: u64,
    /// Wall time of the most recent successful `COMMIT`.
    pub last_commit: Duration,
    /// Summed wall time of every successful `COMMIT`.
    pub total_commit_time: Duration,
    /// Largest `-wal` file size observed after a commit, bytes.
    pub max_wal_bytes: u64,
}

impl WriteMetrics {
    /// Mean journal payload bytes per successful journal commit.
    pub fn bytes_per_journal_write(&self) -> f64 {
        if self.journal_commits == 0 {
            0.0
        } else {
            self.journal_payload_bytes as f64 / self.journal_commits as f64
        }
    }

    /// Durable payload bytes per second of time spent in `COMMIT`. This is the
    /// throughput figure the gate report cites; it excludes simulation time.
    pub fn commit_bytes_per_sec(&self) -> f64 {
        let secs = self.total_commit_time.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            (self.journal_payload_bytes + self.checkpoint_payload_bytes) as f64 / secs
        }
    }

    pub(crate) fn record_commit(&mut self, elapsed: Duration) {
        self.last_commit = elapsed;
        self.total_commit_time += elapsed;
    }
}

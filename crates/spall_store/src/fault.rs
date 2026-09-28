//! Controlled crash points and disk-error injection for the persistence tests
//! (`docs/tasks.md` T16: "Implement controlled crash points and disk-error
//! injection").
//!
//! A [`Writer`](crate::Writer) carries an optional [`FaultPlan`].
//!
//! * A [`CrashPoint`] makes the writer return [`StoreError::CrashInjected`] at a
//!   labelled site and *poison* the writer. This models the **API-visible
//!   effect** of the process dying there while running in the same process: the
//!   pending transaction is dropped (SQLite rolls it back normally) and no
//!   durable ack is produced. It does **not** reproduce abrupt process
//!   termination with an unclean WAL — that is covered separately by the
//!   child-process harness (`tests/abrupt_crash.rs`), which kills a real child
//!   at these same boundaries and reopens from a fresh process.
//! * `fail_journal_commit` / `fail_checkpoint_commit` short-circuit *before*
//!   `COMMIT` with [`StoreError::Disk`]; the DB transaction rolls back. Useful
//!   as a fast API check but not a genuine engine failure.
//! * `real_write_failure` forces a **genuine** `rusqlite` write error from the
//!   SQLite engine itself (the connection is switched to `query_only`, so the
//!   staged `INSERT`/`COMMIT` fails inside SQLite, not at a pre-commit branch).
//!   This exercises the real error path end to end, including
//!   [`Writer::poison`](crate::Writer) via `?`.
//!
//! In every failure mode the API must never report a save it did not make
//! durable, and the writer must be poisoned/closed to further durable calls.

/// A labelled point in a durable write where a crash can be injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// After the journal rows are staged, before `COMMIT`. Nothing is durable.
    BeforeJournalCommit,
    /// Immediately after the journal `COMMIT` returns. The batch is durable but
    /// the caller never learns it (no `DurableThrough` is returned).
    AfterJournalCommit,
    /// Between writing checkpoint body/brick rows, before the cursor/`complete`
    /// row. The partial checkpoint must be invisible to recovery.
    MidCheckpointRows,
    /// After the `complete = 1` row is staged, before `COMMIT`.
    BeforeCheckpointCommit,
    /// Immediately after the checkpoint `COMMIT` returns.
    AfterCheckpointCommit,
}

/// Fault injection for one [`Writer`](crate::Writer). Default is no faults.
#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    /// Inject a crash the next time this site is reached, then clear it.
    pub crash_at: Option<CrashPoint>,
    /// Fail the next journal `COMMIT` as a disk error (transaction rolls back).
    pub fail_journal_commit: bool,
    /// Fail the next checkpoint `COMMIT` as a disk error (transaction rolls
    /// back).
    pub fail_checkpoint_commit: bool,
    /// Force the next durable write to hit a **real** SQLite engine error: the
    /// connection is switched to `query_only` just before the transaction, so
    /// the staged `INSERT`/`COMMIT` returns an actual `rusqlite::Error`. One
    /// shot; consumed when reached.
    pub real_write_failure: bool,
    /// Fail the next journal or checkpoint `COMMIT` as an **out-of-disk**
    /// (`SQLITE_FULL`) error: the transaction rolls back and the writer is
    /// poisoned, exactly as for a real ENOSPC. Modelled the same way as
    /// `fail_*_commit` (a pre-`COMMIT` short-circuit), with a disk-full label.
    /// One shot; consumed when reached.
    pub disk_full: bool,
}

impl FaultPlan {
    /// A plan that crashes once at `point`.
    pub fn crash(point: CrashPoint) -> Self {
        Self {
            crash_at: Some(point),
            ..Self::default()
        }
    }

    /// A plan that fails the next journal commit as a disk error.
    pub fn disk_fail_journal() -> Self {
        Self {
            fail_journal_commit: true,
            ..Self::default()
        }
    }

    /// A plan that fails the next checkpoint commit as a disk error.
    pub fn disk_fail_checkpoint() -> Self {
        Self {
            fail_checkpoint_commit: true,
            ..Self::default()
        }
    }

    /// A plan whose next durable write hits a genuine SQLite engine write error.
    pub fn real_sqlite_write_failure() -> Self {
        Self {
            real_write_failure: true,
            ..Self::default()
        }
    }

    /// A plan that fails the next journal or checkpoint commit as an
    /// out-of-disk (`SQLITE_FULL`) error.
    pub fn disk_full() -> Self {
        Self {
            disk_full: true,
            ..Self::default()
        }
    }

    /// `true` if `point` is armed. Consumes the arming (one-shot).
    pub(crate) fn take_crash(&mut self, point: CrashPoint) -> bool {
        if self.crash_at == Some(point) {
            self.crash_at = None;
            true
        } else {
            false
        }
    }
}

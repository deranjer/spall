//! Controlled crash points and disk-error injection for the persistence tests
//! (`docs/tasks.md` T16: "Implement controlled crash points and disk-error
//! injection").
//!
//! A [`Writer`](crate::Writer) carries an optional [`FaultPlan`]. A crash point
//! makes the writer return [`StoreError::CrashInjected`] at a labelled site,
//! modelling the process dying there; the test then drops the writer and
//! re-opens the database with [`crate::recover`] to assert the durable prefix.
//! A disk fault makes the pending DB transaction roll back and the call return
//! [`StoreError::Disk`], modelling a failed write — the API must never report a
//! save it did not make durable.

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

//! Edit-admission accounting: request **attempts**, **unique logical edits**, terminal outcomes and
//! genuinely pending work, kept apart.
//!
//! A logical edit is one `RequestId`. A client may send it several times (bounded retries after
//! `throttled` / `overloaded` answers, or a resend after a reconnect), so attempts overcount edits.
//! An edit can also be *admitted* (staged) and later **terminally rejected** by the pipeline; it is
//! then neither committed nor pending. Counting every admitted-but-not-committed edit as
//! "unresolved" conflates the two (the 30-minute soak's 532 did exactly that).

use std::collections::HashMap;
use std::time::Instant;

use serde::Serialize;
use spall_protocol::RequestId;

/// Most distinct request ids tracked individually; past it the counters keep counting but per-id
/// state stops (`ledger_saturated`), so a client flooding fresh ids cannot grow this without bound.
pub const MAX_TRACKED_REQUESTS: usize = 262_144;

/// Why an attempt was refused at admission (the request was not staged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionRefusal {
    /// The per-client per-tick quota (`throttled:`), retryable.
    Throttled,
    /// The intent queue was full (`overloaded:`), retryable with back-off.
    QueueFull,
    /// Malformed, unsupported or invalid claim; not retryable.
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Logical {
    /// Attempted; the latest attempt was refused at admission (a retry may still be admitted).
    RefusedAtAdmission(AdmissionRefusal),
    /// Admitted and awaiting a tick-resolved outcome.
    Pending,
    Committed,
    /// Admitted, then rejected by staging or commit: terminal.
    RejectedAfterAdmission,
}

/// The ledger. It also owns the admission timestamps that measure admission-to-commit latency, so
/// a request leaves them the moment it resolves either way.
#[derive(Debug, Default)]
pub struct AdmissionLedger {
    attempts: u64,
    duplicates_replayed: u64,
    admitted: u64,
    committed: u64,
    rejected_after_admission: u64,
    refused_throttled: u64,
    refused_queue_full: u64,
    refused_invalid: u64,
    pending: HashMap<RequestId, Instant>,
    logical: HashMap<u64, Logical>,
    saturated: bool,
}

/// Serialised into the server summary.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AdmissionSummary {
    /// Every `ActionRequest` received, retries and duplicates included.
    pub attempts: u64,
    /// Distinct request ids seen (unique logical edits requested).
    pub unique_requests: u64,
    /// Attempts answered from the stored status of an already-admitted request.
    pub duplicates_replayed: u64,
    /// First admissions into the pipeline.
    pub admitted: u64,
    pub committed: u64,
    /// Admitted and later rejected (terminal); **not** pending.
    pub rejected_after_admission: u64,
    /// Admitted and still awaiting an outcome at the end of the run (genuinely pending work).
    pub pending_at_end: u64,
    /// Refused attempts by class (an edit may be refused several times before it is admitted).
    pub refused_throttled: u64,
    pub refused_queue_full: u64,
    pub refused_invalid: u64,
    /// Final state of each unique logical edit.
    pub logical_committed: u64,
    pub logical_pending: u64,
    pub logical_rejected_after_admission: u64,
    /// Never admitted: the last attempt was refused (retries exhausted or none sent).
    pub logical_never_admitted: u64,
    /// Per-id tracking stopped at [`MAX_TRACKED_REQUESTS`]; the `logical_*` counts are then partial.
    pub ledger_saturated: bool,
}

impl AdmissionLedger {
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&mut self, id: RequestId, state: Logical) {
        if self.logical.contains_key(&id.0) || self.logical.len() < MAX_TRACKED_REQUESTS {
            // A committed edit never goes back.
            if self.logical.get(&id.0) != Some(&Logical::Committed) {
                self.logical.insert(id.0, state);
            }
        } else {
            self.saturated = true;
        }
    }

    /// An `ActionRequest` arrived (first send, retry or duplicate).
    pub fn attempt(&mut self) {
        self.attempts += 1;
    }

    /// The attempt was answered from the stored status of an already-admitted request.
    pub fn duplicate_replayed(&mut self) {
        self.duplicates_replayed += 1;
    }

    /// The attempt was refused before staging.
    pub fn refused(&mut self, id: RequestId, why: AdmissionRefusal) {
        match why {
            AdmissionRefusal::Throttled => self.refused_throttled += 1,
            AdmissionRefusal::QueueFull => self.refused_queue_full += 1,
            AdmissionRefusal::Invalid => self.refused_invalid += 1,
        }
        self.set(id, Logical::RefusedAtAdmission(why));
    }

    /// The request entered the pipeline (first admission).
    pub fn admitted(&mut self, id: RequestId, now: Instant) {
        if !self.pending.contains_key(&id) {
            self.admitted += 1;
        }
        self.pending.entry(id).or_insert(now);
        self.set(id, Logical::Pending);
    }

    /// A commit resolved `id`; returns when it was admitted (for latency).
    pub fn committed(&mut self, id: RequestId) -> Option<Instant> {
        self.committed += 1;
        self.set(id, Logical::Committed);
        self.pending.remove(&id)
    }

    /// The pipeline rejected an admitted request: terminal, and no longer pending.
    pub fn rejected_after_admission(&mut self, id: RequestId) {
        if self.pending.remove(&id).is_some() {
            self.rejected_after_admission += 1;
            self.set(id, Logical::RejectedAfterAdmission);
        }
    }

    /// Folds one tick-resolved status into the ledger. Commits are recorded by
    /// [`Self::committed`] (which returns the admission time for latency); a rejection of an
    /// admitted request is terminal here.
    pub fn on_status(&mut self, status: &spall_protocol::ActionStatus) {
        if matches!(
            status.outcome,
            spall_protocol::ActionOutcome::Rejected { .. }
        ) {
            self.rejected_after_admission(status.request_id);
        }
    }

    /// Genuinely pending admitted work right now.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// The end-of-run summary.
    pub fn summary(&self) -> AdmissionSummary {
        let mut s = AdmissionSummary {
            attempts: self.attempts,
            unique_requests: self.logical.len() as u64,
            duplicates_replayed: self.duplicates_replayed,
            admitted: self.admitted,
            committed: self.committed,
            rejected_after_admission: self.rejected_after_admission,
            pending_at_end: self.pending.len() as u64,
            refused_throttled: self.refused_throttled,
            refused_queue_full: self.refused_queue_full,
            refused_invalid: self.refused_invalid,
            ledger_saturated: self.saturated,
            ..AdmissionSummary::default()
        };
        for st in self.logical.values() {
            match st {
                Logical::Committed => s.logical_committed += 1,
                Logical::Pending => s.logical_pending += 1,
                Logical::RejectedAfterAdmission => s.logical_rejected_after_admission += 1,
                Logical::RefusedAtAdmission(_) => s.logical_never_admitted += 1,
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> RequestId {
        RequestId(n)
    }

    #[test]
    fn an_admitted_request_later_rejected_is_terminal_not_pending() {
        let mut l = AdmissionLedger::new();
        let t = Instant::now();
        l.attempt();
        l.admitted(id(1), t);
        assert_eq!(l.pending_len(), 1);
        // The pipeline rejects it at staging.
        l.rejected_after_admission(id(1));
        assert_eq!(
            l.pending_len(),
            0,
            "a terminally rejected request is not pending"
        );
        let s = l.summary();
        assert_eq!(s.admitted, 1);
        assert_eq!(s.rejected_after_admission, 1);
        assert_eq!(s.pending_at_end, 0);
        assert_eq!(s.logical_rejected_after_admission, 1);
        assert_eq!(s.logical_pending, 0);
        assert_eq!(s.committed, 0);
    }

    #[test]
    fn attempts_are_kept_apart_from_unique_logical_edits() {
        let mut l = AdmissionLedger::new();
        let t = Instant::now();
        // Edit 1: refused twice (queue full), then admitted and committed.
        for _ in 0..2 {
            l.attempt();
            l.refused(id(1), AdmissionRefusal::QueueFull);
        }
        l.attempt();
        l.admitted(id(1), t);
        assert!(l.committed(id(1)).is_some());
        // Edit 2: throttled once and never sent again (retries exhausted).
        l.attempt();
        l.refused(id(2), AdmissionRefusal::Throttled);
        // Edit 3: admitted, resent (duplicate replay), still pending.
        l.attempt();
        l.admitted(id(3), t);
        l.attempt();
        l.duplicate_replayed();
        // Edit 4: invalid.
        l.attempt();
        l.refused(id(4), AdmissionRefusal::Invalid);

        let s = l.summary();
        assert_eq!(s.attempts, 7);
        assert_eq!(s.unique_requests, 4);
        assert_eq!(s.duplicates_replayed, 1);
        assert_eq!(s.admitted, 2);
        assert_eq!(
            (s.refused_queue_full, s.refused_throttled, s.refused_invalid),
            (2, 1, 1)
        );
        assert_eq!(s.logical_committed, 1);
        assert_eq!(s.logical_pending, 1);
        assert_eq!(s.logical_never_admitted, 2);
        assert_eq!(s.pending_at_end, 1);
        assert_eq!(
            s.logical_committed
                + s.logical_pending
                + s.logical_never_admitted
                + s.logical_rejected_after_admission,
            s.unique_requests,
            "every logical edit is in exactly one final state"
        );
    }

    #[test]
    fn a_refused_edit_that_is_later_admitted_and_committed_ends_committed() {
        let mut l = AdmissionLedger::new();
        l.attempt();
        l.refused(id(9), AdmissionRefusal::QueueFull);
        l.attempt();
        l.admitted(id(9), Instant::now());
        l.committed(id(9));
        // A stale late refusal cannot un-commit it.
        l.refused(id(9), AdmissionRefusal::Throttled);
        let s = l.summary();
        assert_eq!(s.logical_committed, 1);
        assert_eq!(s.logical_never_admitted, 0);
    }

    #[test]
    fn per_id_tracking_is_bounded() {
        let mut l = AdmissionLedger::new();
        for n in 0..(MAX_TRACKED_REQUESTS as u64 + 10) {
            l.attempt();
            l.refused(id(n), AdmissionRefusal::Invalid);
        }
        let s = l.summary();
        assert!(s.ledger_saturated);
        assert_eq!(s.unique_requests, MAX_TRACKED_REQUESTS as u64);
        assert_eq!(
            s.attempts,
            MAX_TRACKED_REQUESTS as u64 + 10,
            "counters keep counting"
        );
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use spall_sim::{
        EditIntent, EditTarget, Simulation, SimulationConfig, action_statuses, fixtures,
    };

    /// A real pipeline: the request is admitted (staged), then the staging step rejects it. It must
    /// leave the pending set and be counted as a terminal rejection, not as unresolved work.
    #[test]
    fn a_request_admitted_by_the_real_pipeline_and_rejected_at_staging_is_not_pending() {
        use spall_core::units::{BRUSH_UNIT, BrushPoint};
        let mut sim =
            Simulation::new(SimulationConfig::new(fixtures::g4_integrated_setup())).unwrap();
        let mut ledger = AdmissionLedger::new();
        // A brush dipping below the bounded volume's floor is admitted, then rejected at staging
        // ("edit touches brick ... outside the volume bounds").
        let h = BRUSH_UNIT / 2;
        let brush = spall_core::SphereBrush::new(
            BrushPoint::from_units(80 * BRUSH_UNIT + h, BRUSH_UNIT + h, 80 * BRUSH_UNIT + h),
            3 * BRUSH_UNIT,
        )
        .unwrap();
        let rid = RequestId(7);
        let intent = EditIntent::cut(
            rid,
            spall_core::EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            brush,
        );
        ledger.attempt();
        sim.submit(intent).expect("the intent queue accepts it");
        ledger.admitted(rid, Instant::now());
        assert_eq!(
            ledger.pending_len(),
            1,
            "admitted work is pending until resolved"
        );

        let report = sim.tick().unwrap();
        assert!(report.committed.is_empty(), "it did not commit");
        assert_eq!(
            report.rejected.len(),
            1,
            "the pipeline rejected it: {:?}",
            report.rejected
        );
        for status in action_statuses(&report) {
            ledger.on_status(&status);
        }
        assert_eq!(
            ledger.pending_len(),
            0,
            "a terminally rejected request is not pending"
        );
        let s = ledger.summary();
        assert_eq!(
            (
                s.admitted,
                s.rejected_after_admission,
                s.pending_at_end,
                s.committed
            ),
            (1, 1, 0, 0)
        );
        assert_eq!(
            (s.logical_rejected_after_admission, s.logical_pending),
            (1, 0)
        );
    }
}

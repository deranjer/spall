//! T11a / ENG-62: server commit-latency measurement and admission accounting.
//!
//! The G1 gate names three commit-latency targets (`docs/validation.md` "G1"):
//! a normal single-brick edit's server commit p95 (`<= 100 ms` without network
//! delay), an ordinary structure split's p95 (`<= 500 ms`), and a designated
//! large-collapse's time before consistent activation (`<= 2 s`). It also
//! requires the gate report to distinguish **requested / rejected / queued /
//! committed** counts.
//!
//! This module owns the bucketing and the nearest-rank p95. The [`serve`] loop
//! records one wall-clock sample per committed transaction — from the moment the
//! server admitted the `ActionRequest` for staging to the moment its transaction
//! committed — and classifies it by the shape of the commit.
//!
//! [`serve`]: crate::serve

use std::time::Duration;

use spall_protocol::{TopologyOp, TopologyTransaction};

/// A committed transaction whose detached geometry is at least this many cells
/// is bucketed as a large-collapse rather than an ordinary structure split.
/// Chosen near the 4096-op inline-transaction budget: a split approaching that
/// size is the "designated large-collapse stress" the gate calls out.
pub const LARGE_COLLAPSE_CELLS: u64 = 2048;

/// How a committed transaction is bucketed for its latency target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitClass {
    /// No structural split — a normal terrain or body edit.
    SingleBrick,
    /// A split whose detached geometry is under [`LARGE_COLLAPSE_CELLS`].
    StructureSplit,
    /// A split at or over [`LARGE_COLLAPSE_CELLS`].
    LargeCollapse,
}

/// Classifies a committed transaction. `bumped_epoch` is
/// `spall_sim::Committed::bumped_epoch` — true iff the commit split a body.
pub fn classify(bumped_epoch: bool, tx: &TopologyTransaction) -> CommitClass {
    if !bumped_epoch {
        return CommitClass::SingleBrick;
    }
    // A split whose geometry travels as a compressed baseline blob (T17) was
    // too big for the inline `CellRun` encoding — that is a large collapse by
    // construction.
    if tx.ops.iter().any(|op| {
        matches!(
            op,
            TopologyOp::SplitOffBaseline { .. }
                | TopologyOp::SourcePatchBaseline { .. }
                | TopologyOp::SplitOffBulkBaseline { .. }
                | TopologyOp::SourcePatchBulkBaseline { .. }
        )
    }) {
        return CommitClass::LargeCollapse;
    }
    let detached_cells: u64 = tx
        .ops
        .iter()
        .map(|op| match op {
            TopologyOp::CellRun { len, .. } => u64::from(*len),
            TopologyOp::IntegerBrush { .. }
            | TopologyOp::SplitOff { .. }
            | TopologyOp::SplitOffBaseline { .. }
            | TopologyOp::SourcePatchBaseline { .. }
            | TopologyOp::SplitOffBulkBaseline { .. }
            | TopologyOp::SourcePatchBulkBaseline { .. } => 0,
        })
        .sum();
    if detached_cells >= LARGE_COLLAPSE_CELLS {
        CommitClass::LargeCollapse
    } else {
        CommitClass::StructureSplit
    }
}

/// Per-class commit-latency samples (milliseconds) plus the admission tally.
#[derive(Debug, Default)]
pub struct CommitLatency {
    single_brick_ms: Vec<f64>,
    structure_split_ms: Vec<f64>,
    large_collapse_ms: Vec<f64>,
}

impl CommitLatency {
    /// Adds one commit's server-side latency to its bucket.
    pub fn record(&mut self, class: CommitClass, latency: Duration) {
        let ms = latency.as_secs_f64() * 1_000.0;
        match class {
            CommitClass::SingleBrick => self.single_brick_ms.push(ms),
            CommitClass::StructureSplit => self.structure_split_ms.push(ms),
            CommitClass::LargeCollapse => self.large_collapse_ms.push(ms),
        }
    }

    /// Nearest-rank p95 (ms) and sample count for each bucket.
    pub fn report(&self) -> LatencyReport {
        LatencyReport {
            single_brick_commit_p95_ms: p95(&self.single_brick_ms),
            single_brick_commit_samples: self.single_brick_ms.len() as u64,
            structure_split_p95_ms: p95(&self.structure_split_ms),
            structure_split_samples: self.structure_split_ms.len() as u64,
            large_collapse_p95_ms: p95(&self.large_collapse_ms),
            large_collapse_samples: self.large_collapse_ms.len() as u64,
        }
    }
}

/// Flat p95 / sample-count view copied into the run summary.
#[derive(Debug, Clone, Copy, Default)]
pub struct LatencyReport {
    pub single_brick_commit_p95_ms: f64,
    pub single_brick_commit_samples: u64,
    pub structure_split_p95_ms: f64,
    pub structure_split_samples: u64,
    pub large_collapse_p95_ms: f64,
    pub large_collapse_samples: u64,
}

/// Nearest-rank p95 of `samples` (milliseconds); `0.0` for an empty set.
fn p95(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = ((0.95 * s.len() as f64).ceil() as usize).clamp(1, s.len());
    s[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{GlobalCell, MaterialId, Tick, TransactionId, VolumeId};
    use spall_protocol::{ControlSeq, TopologyOp, TopologyTransaction};

    fn tx_with(ops: Vec<TopologyOp>) -> TopologyTransaction {
        TopologyTransaction {
            transaction_id: TransactionId::new(1).unwrap(),
            server_tick: Tick::ZERO,
            control_seq: ControlSeq(0),
            algorithm_version: 1,
            dependencies: Vec::new(),
            before: Vec::new(),
            after: Vec::new(),
            ops,
            result_hashes: Vec::new(),
        }
    }

    fn run(volume: u64, len: u32) -> TopologyOp {
        TopologyOp::CellRun {
            volume: VolumeId::new(volume).unwrap(),
            start: GlobalCell::new(0, 0, 0),
            len,
            material: MaterialId(1),
        }
    }

    #[test]
    fn classify_splits_by_detached_cell_count() {
        assert_eq!(
            classify(false, &tx_with(vec![run(1, 10_000)])),
            CommitClass::SingleBrick,
            "no epoch bump is always a single-brick commit regardless of size"
        );
        assert_eq!(
            classify(true, &tx_with(vec![run(2, 64), run(2, 64)])),
            CommitClass::StructureSplit
        );
        assert_eq!(
            classify(true, &tx_with(vec![run(2, LARGE_COLLAPSE_CELLS as u32)])),
            CommitClass::LargeCollapse
        );
    }

    #[test]
    fn p95_is_nearest_rank_and_empty_is_zero() {
        let mut cl = CommitLatency::default();
        assert_eq!(cl.report().single_brick_commit_p95_ms, 0.0);
        assert_eq!(cl.report().single_brick_commit_samples, 0);

        // 20 samples 1..=20 ms → nearest-rank p95 is the 19th (ceil(0.95*20)=19).
        for i in 1..=20 {
            cl.record(CommitClass::SingleBrick, Duration::from_millis(i));
        }
        let r = cl.report();
        assert_eq!(r.single_brick_commit_samples, 20);
        assert!((r.single_brick_commit_p95_ms - 19.0).abs() < 1e-6);

        cl.record(CommitClass::LargeCollapse, Duration::from_millis(1500));
        assert!((cl.report().large_collapse_p95_ms - 1500.0).abs() < 1e-6);
        assert_eq!(cl.report().large_collapse_samples, 1);
    }
}

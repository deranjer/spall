//! Debug tracing of an incoming transaction against the replica's bricks.
use super::*;

impl ReplicaWorld {
    pub(super) fn trace_transaction(&self, tx: &TopologyTransaction) {
        if tracing::enabled!(tracing::Level::DEBUG) {
            let ops: Vec<&str> = tx
                .ops
                .iter()
                .map(|op| match op {
                    TopologyOp::IntegerBrush { .. } => "brush",
                    TopologyOp::CellRun { .. } => "cells",
                    TopologyOp::SplitOff { .. } => "split",
                    TopologyOp::SplitOffBaseline { .. } => "split-baseline",
                    TopologyOp::SourcePatchBaseline { .. } => "patch-baseline",
                    TopologyOp::SplitOffBulkBaseline { .. } => "split-bulk",
                    TopologyOp::SourcePatchBulkBaseline { .. } => "patch-bulk",
                })
                .collect();
            let before: Vec<String> = tx
                .before
                .iter()
                .map(|br| {
                    let local = match self
                        .volumes
                        .get(&br.volume.get())
                        .and_then(|v| v.brick_revision(br.coord).ok().flatten())
                    {
                        Some(rev) => format!("resident {rev:?}"),
                        None => match self
                            .evicted
                            .get(&br.volume.get())
                            .and_then(|e| e.get(br.coord))
                        {
                            Some(digest) => format!("digest {digest:?}"),
                            None => "absent".into(),
                        },
                    };
                    format!("{:?}@{:?} local={local}", br.coord, br.revision)
                })
                .collect();
            tracing::debug!(
                tx = tx.transaction_id.get(),
                tick = tx.server_tick.get(),
                ?ops,
                ?before,
                "applying topology transaction"
            );
        }
    }
}

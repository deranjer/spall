//! T17 increment 2 / ENG-64: a cut that detaches a component too large for even
//! a compressed inline op blob must still **commit**, with marker
//! `SplitOffBulkBaseline` / `SourcePatchBulkBaseline` ops plus an out-of-band
//! `BaselineWorld` on the journal entry — and that transaction must replay from
//! the tick-0 baseline (feeding the sibling `BaselineWorld`) to the identical
//! canonical hash.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush};
use spall_protocol::{RequestId, SPLIT_BULK_TRANSFER_ID_BIT, TopologyOp, TransferId};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn sim() -> Simulation {
    let mut setup = fixtures::bulk_split_setup();
    setup.physics.disable_ccd = true;
    Simulation::new(SimulationConfig::new(setup)).expect("bulk-split world stands up")
}

fn run(ticks: u64) -> Simulation {
    let mut sim = sim();
    let actor = EntityId::new(1).unwrap();
    let mut next_transfer_low = 401u64;
    for tick in 1..=ticks {
        if tick == 4 {
            sim.submit(EditIntent::cut(
                RequestId(1),
                actor,
                EditTarget::Terrain,
                // Sever the whole 8x8 x 12 column at x/z 28..=35, y 2..=13.
                brush_cell(31, 8, 31, 8),
            ))
            .expect("column cut admitted");
        }
        let count = sim.transfer_reservation_bound();
        let ids = (0..count)
            .map(|_| {
                let id = TransferId(SPLIT_BULK_TRANSFER_ID_BIT | next_transfer_low);
                next_transfer_low += 1;
                id
            })
            .collect();
        sim.tick_with_transfer_ids(ids).expect("tick");
    }
    sim
}

#[test]
fn giant_split_commits_with_bulk_baseline_ops_and_replays_exactly() {
    let live = run(40);

    let entries = live.journal().entries();
    assert_eq!(entries.len(), 1, "one committed transaction");
    assert_eq!(live.world().body_count(), 1, "the block detached");

    let e = &entries[0];
    assert!(
        e.attachments.is_some(),
        "the split's geometry travels as an out-of-band BaselineWorld"
    );
    let marker_id = e
        .transaction
        .ops
        .iter()
        .find_map(|op| match op {
            TopologyOp::SplitOffBulkBaseline { transfer_id, .. } => Some(*transfer_id),
            _ => None,
        })
        .expect("bulk child marker carries a transfer ID");
    assert_eq!(marker_id, SPLIT_BULK_TRANSFER_ID_BIT | 401);
    assert_eq!(
        live.committed(RequestId(1))
            .and_then(|commit| commit.bulk_transfer_id)
            .map(|id| id.0),
        Some(marker_id)
    );
    assert!(e.transaction.ops.iter().any(|op| matches!(
        op,
        TopologyOp::SourcePatchBulkBaseline { transfer_id, .. } if *transfer_id == marker_id
    )));
    let tx = &e.transaction;
    assert!(
        tx.ops
            .iter()
            .any(|o| matches!(o, TopologyOp::SplitOffBulkBaseline { .. })),
        "child geometry is a SplitOffBulkBaseline marker"
    );
    assert!(
        tx.ops
            .iter()
            .any(|o| matches!(o, TopologyOp::SourcePatchBulkBaseline { .. })),
        "source side is a SourcePatchBulkBaseline marker"
    );
    assert!(
        !tx.ops.iter().any(|o| matches!(
            o,
            TopologyOp::CellRun { .. }
                | TopologyOp::SplitOffBaseline { .. }
                | TopologyOp::SourcePatchBaseline { .. }
        )),
        "no inline geometry in a bulk split"
    );
    // The marker transaction itself still fits one reliable control frame.
    assert!(
        spall_protocol::encode_control(tx).is_ok(),
        "the marker transaction fits the wire"
    );

    assert_eq!(
        run(40).world().world_hash(),
        live.world().world_hash(),
        "run is deterministic"
    );

    // Exact-replay parity: fold the journalled marker transaction from a fresh
    // tick-0 baseline, feeding the sibling BaselineWorld, and reach the same
    // canonical topology hash.
    let mut replay = sim();
    for entry in entries {
        replay
            .world_mut()
            .replay_transaction(
                &entry.transaction,
                &entry.participants,
                entry.geometry_only_bulk(),
            )
            .expect("journalled bulk split replays from baseline");
    }
    assert_eq!(
        replay.world().world_hash(),
        live.world().world_hash(),
        "replayed world hash matches the live run"
    );
    assert_eq!(replay.world().body_count(), 1);
}

//! T17 / ENG-64: a cut that detaches a component too fragmented to encode as
//! inline `CellRun`s must still **commit**, carrying the geometry as compressed
//! `SplitOffBaseline` / `SourcePatchBaseline` op blobs — and the journalled
//! transaction must replay from the tick-0 baseline to the identical canonical
//! hash (exact-replay parity for the new ops).

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush};
use spall_protocol::{RequestId, TopologyOp};
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
    let mut setup = fixtures::checkerboard_split_setup();
    setup.physics.disable_ccd = true;
    Simulation::new(SimulationConfig::new(setup)).expect("checkerboard-split world stands up")
}

/// Cut the column at tick 4, run to `ticks`, return the finished sim.
fn run(ticks: u64) -> Simulation {
    let mut sim = sim();
    let actor = EntityId::new(1).unwrap();
    for tick in 1..=ticks {
        if tick == 4 {
            sim.submit(EditIntent::cut(
                RequestId(1),
                actor,
                EditTarget::Terrain,
                brush_cell(15, 4, 15, 2),
            ))
            .expect("column cut admitted");
        }
        sim.tick().expect("tick");
    }
    sim
}

#[test]
fn oversized_split_commits_via_baseline_blob_ops_and_replays_exactly() {
    let live = run(40);

    // The cut committed exactly one transaction, and it detached the block.
    let entries = live.journal().entries();
    assert_eq!(entries.len(), 1, "one committed transaction");
    assert_eq!(
        live.world().body_count(),
        1,
        "the checkerboard block detached"
    );

    let tx = &entries[0].transaction;
    assert!(
        tx.ops
            .iter()
            .any(|o| matches!(o, TopologyOp::SplitOffBaseline { .. })),
        "the split fell back to the compressed-baseline-blob op path"
    );
    assert!(
        tx.ops
            .iter()
            .any(|o| matches!(o, TopologyOp::SourcePatchBaseline { .. })),
        "the source side is a SourcePatchBaseline"
    );
    assert!(
        !tx.ops
            .iter()
            .any(|o| matches!(o, TopologyOp::CellRun { .. })),
        "no inline CellRuns in an oversized split"
    );
    // The whole record still fits one reliable control frame.
    assert!(
        spall_protocol::encode_control(tx).is_ok(),
        "transaction fits the wire"
    );

    // Deterministic.
    assert_eq!(
        run(40).world().world_hash(),
        live.world().world_hash(),
        "run is deterministic"
    );

    // Exact-replay parity: fold the journalled transaction from a fresh tick-0
    // baseline and reach the identical canonical topology hash.
    let mut replay = sim();
    for e in entries {
        replay
            .world_mut()
            .replay_transaction(&e.transaction, &e.participants, e.bulk_baseline.as_ref())
            .expect("journalled oversized split replays from baseline");
    }
    assert_eq!(
        replay.world().world_hash(),
        live.world().world_hash(),
        "replayed world hash matches the live run"
    );
    assert_eq!(replay.world().body_count(), 1);
}

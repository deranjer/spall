//! T17 increment 2 / ENG-64: a replica holds a giant-split marker transaction
//! until its out-of-band `BaselineWorld` arrives, then applies it to the same
//! canonical hash the server reached — in either arrival order.

use spall_client::{ApplyOutcome, ReplicaConfig, ReplicaWorld};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush, VolumeId};
use spall_protocol::baseline::BaselineWorld;
use spall_protocol::{RequestId, SPLIT_BULK_TRANSFER_ID_BIT, TopologyOp, TopologyTransaction};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};

/// Run the bulk-split scene through the column cut; return the live sim, its one
/// committed marker transaction, and the out-of-band `BaselineWorld`.
fn committed_giant_split() -> (Simulation, TopologyTransaction, BaselineWorld) {
    let mut setup = fixtures::bulk_split_setup();
    setup.physics.disable_ccd = true;
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    let actor = EntityId::new(1).unwrap();
    for tick in 1..=20u64 {
        if tick == 4 {
            let h = BRUSH_UNIT / 2;
            let brush = SphereBrush::new(
                BrushPoint::from_units(
                    31 * BRUSH_UNIT + h,
                    8 * BRUSH_UNIT + h,
                    31 * BRUSH_UNIT + h,
                ),
                8 * BRUSH_UNIT,
            )
            .unwrap();
            sim.submit(EditIntent::cut(
                RequestId(1),
                actor,
                EditTarget::Terrain,
                brush,
            ))
            .unwrap();
        }
        sim.tick().unwrap();
    }
    let entry = sim.journal().entries()[0].clone();
    let world = entry
        .bulk_baseline
        .clone()
        .expect("giant split has a bulk baseline");
    (sim, entry.transaction, world)
}

fn fresh_replica() -> ReplicaWorld {
    ReplicaWorld::from_baseline(
        spall_voxel::fixtures::bulk_split_scene(VolumeId::new(1).unwrap()),
        ReplicaConfig::default(),
    )
}

fn transfer_id_of(tx: &TopologyTransaction) -> u64 {
    tx.ops
        .iter()
        .find_map(|o| o.bulk_transfer_id())
        .expect("a bulk-split marker op")
}

#[test]
fn replica_holds_the_marker_then_applies_the_bulk_blob_to_the_server_hash() {
    let (sim, tx, world) = committed_giant_split();
    assert!(
        tx.ops
            .iter()
            .any(|o| matches!(o, TopologyOp::SplitOffBulkBaseline { .. })),
        "precondition: bulk marker ops"
    );
    let tid = transfer_id_of(&tx);
    assert_eq!(tid & SPLIT_BULK_TRANSFER_ID_BIT, SPLIT_BULK_TRANSFER_ID_BIT);

    // Marker first: the replica holds it.
    let mut replica = fresh_replica();
    match replica.apply_transaction(&tx) {
        ApplyOutcome::AwaitingBulkSplit { transfer_id } => assert_eq!(transfer_id, tid),
        other => panic!("expected AwaitingBulkSplit, got {other:?}"),
    }
    assert_eq!(replica.pending_bulk_split_txn_count(), 1);

    // Blob arrives: the held transaction retries and publishes.
    let retried = replica.provide_bulk_split_world(tid, world);
    assert!(
        matches!(retried.as_slice(), [(_, ApplyOutcome::Published { .. })]),
        "the held marker publishes once its blob arrives: {retried:?}"
    );
    assert_eq!(replica.pending_bulk_split_txn_count(), 0);
    assert_eq!(
        replica.world_hash(),
        sim.world().world_hash(),
        "replica reaches the server's canonical topology hash"
    );
}

#[test]
fn the_bulk_blob_may_arrive_before_the_marker_transaction() {
    let (sim, tx, world) = committed_giant_split();
    let tid = transfer_id_of(&tx);

    let mut replica = fresh_replica();
    // Blob first: stashed, nothing to retry yet.
    assert!(replica.provide_bulk_split_world(tid, world).is_empty());
    // Marker arrives: applies straight through.
    assert!(matches!(
        replica.apply_transaction(&tx),
        ApplyOutcome::Published { .. }
    ));
    assert_eq!(replica.world_hash(), sim.world().world_hash());
}

#[test]
fn a_corrupted_bulk_blob_volume_is_rejected_and_leaves_the_replica_untouched() {
    let (_sim, tx, mut world) = committed_giant_split();
    let tid = transfer_id_of(&tx);
    // Corrupt a child brick's revision so its canonical hash no longer matches
    // the transaction's `result_hashes`.
    if let Some(v) = world.volumes.iter_mut().find(|v| v.volume_id.get() != 1) {
        v.bricks[0].revision ^= 0xFFFF;
    }

    let mut replica = fresh_replica();
    let before = replica.world_hash();
    replica.provide_bulk_split_world(tid, world);
    assert!(
        matches!(
            replica.apply_transaction(&tx),
            ApplyOutcome::Rejected { .. }
        ),
        "a mismatched bulk blob rejects the whole transaction"
    );
    assert_eq!(
        replica.world_hash(),
        before,
        "live replica state is untouched"
    );
}

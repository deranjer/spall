//! T17 / ENG-64: a replica applies a compressed-baseline-blob split
//! (`SplitOffBaseline` / `SourcePatchBaseline`) to the same canonical hash the
//! server reached, and rejects a corrupted blob without touching live state.

use spall_client::{ApplyOutcome, ReplicaConfig, ReplicaWorld};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush, VolumeId};
use spall_protocol::{RequestId, TopologyOp, TopologyTransaction};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};

/// Run the checkerboard-split scene through the column cut and return the live
/// sim plus its one committed transaction and participant keyframes.
fn committed_split() -> (
    Simulation,
    TopologyTransaction,
    Vec<spall_protocol::MotionSnapshot>,
) {
    let mut setup = fixtures::checkerboard_split_setup();
    setup.physics.disable_ccd = true;
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    let actor = EntityId::new(1).unwrap();
    for tick in 1..=20u64 {
        if tick == 4 {
            let h = BRUSH_UNIT / 2;
            let brush = SphereBrush::new(
                BrushPoint::from_units(
                    15 * BRUSH_UNIT + h,
                    4 * BRUSH_UNIT + h,
                    15 * BRUSH_UNIT + h,
                ),
                2 * BRUSH_UNIT,
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
    (sim, entry.transaction, entry.participants)
}

fn fresh_replica() -> ReplicaWorld {
    ReplicaWorld::from_baseline(
        spall_voxel::fixtures::checkerboard_split_scene(VolumeId::new(1).unwrap()),
        ReplicaConfig::default(),
    )
}

#[test]
fn replica_applies_a_baseline_blob_split_to_the_server_hash() {
    let (sim, tx, _participants) = committed_split();
    assert!(
        tx.ops
            .iter()
            .any(|o| matches!(o, TopologyOp::SplitOffBaseline { .. })),
        "precondition: the split used the blob op path"
    );

    let mut replica = fresh_replica();
    match replica.apply_transaction(&tx) {
        ApplyOutcome::Published { new_bodies, .. } => {
            assert_eq!(
                new_bodies.len(),
                1,
                "the detached block is created on the replica"
            );
        }
        other => panic!("expected Published, got {other:?}"),
    }
    assert_eq!(
        replica.world_hash(),
        sim.world().world_hash(),
        "replica reaches the server's canonical topology hash"
    );
}

#[test]
fn a_corrupted_split_blob_is_rejected_and_leaves_the_replica_untouched() {
    let (_sim, mut tx, _participants) = committed_split();
    for op in &mut tx.ops {
        if let TopologyOp::SplitOffBaseline { blob, .. } = op {
            // Flip a byte in the middle of the compressed payload.
            let mid = blob.len() / 2;
            blob[mid] ^= 0xFF;
        }
    }

    let mut replica = fresh_replica();
    let before = replica.world_hash();
    assert!(
        matches!(
            replica.apply_transaction(&tx),
            ApplyOutcome::Rejected { .. }
        ),
        "a corrupted blob rejects the whole transaction"
    );
    assert_eq!(
        replica.world_hash(),
        before,
        "live replica state is untouched"
    );
}

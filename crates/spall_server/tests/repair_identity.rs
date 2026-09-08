//! ENG-49: a brick repair is a *versioned baseline patch*, never a synthetic
//! `TopologyTransaction` with a reused id. This pins the three cases the ticket
//! calls out:
//!
//! * a real committed transaction followed by two distinct brick repairs — all
//!   apply, none is dropped as a duplicate, and the replica reaches exact
//!   revision/hash parity;
//! * a `before`-gapped transaction is *retained* and retried once its missing
//!   predecessor lands (its ops are never silently dropped);
//! * a repair patch that arrives *before* the transaction it precedes still
//!   converges, with no corruption and no infinite re-request loop.
//!
//! `spall_client` + `spall_sim` are dev-dependencies here (this is a test, not a
//! runtime edge in the `docs/architecture.md` graph).

use spall_client::{ApplyOutcome, ReplicaConfig, ReplicaWorld};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, EntityId, JournalSeq, Revision, SphereBrush};
use spall_protocol::{
    BaselineBrick, BaselineCells, BaselineOwner, BaselineVolume, BaselineWorld, Hash32,
    InterestEpoch, RepairKey, RepairRequest, RequestId, TopologyTransaction, TransferId,
};
use spall_server::{brick_repair_patch, capture_transfer};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, TickReport, fixtures};

fn fresh_sim() -> Simulation {
    Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).expect("bridge scene")
}

fn assemble(parts: &[spall_protocol::BaselinePart]) -> BaselineWorld {
    let mut bytes = Vec::new();
    for p in parts {
        bytes.extend_from_slice(&p.payload);
    }
    BaselineWorld::decode(&bytes).expect("assembled baseline decodes")
}

fn committed_in_order(reports: &[TickReport]) -> Vec<TopologyTransaction> {
    let mut txs: Vec<TopologyTransaction> = reports
        .iter()
        .flat_map(|r| r.committed.iter().map(|(_, c)| c.topology.clone()))
        .collect();
    txs.sort_by_key(|t| t.control_seq.0);
    txs
}

fn cut(req: u64, x: i64, y: i64, z: i64, r: i64) -> EditIntent {
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        r * BRUSH_UNIT,
    )
    .expect("valid brush");
    EditIntent::cut(
        RequestId(req),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    )
}

/// Install a late-join baseline captured from `sim` at the current tick.
fn replica_from(sim: &Simulation, id: u64) -> (BaselineWorld, ReplicaWorld) {
    let transfer = capture_transfer(sim, TransferId(id), InterestEpoch(1), JournalSeq(0))
        .expect("capture baseline");
    let world = assemble(&transfer.parts);
    let replica = ReplicaWorld::from_baseline_world(&world, ReplicaConfig::default())
        .expect("install baseline");
    (world, replica)
}

/// Corrupt one replica brick: wrong revision, wrong material — exactly what a
/// `before`-revision check catches.
fn diverge_brick(replica: &mut ReplicaWorld, world: &BaselineWorld, brick_index: usize) {
    let terrain = &world.volumes[0];
    assert!(matches!(terrain.owner, BaselineOwner::Terrain));
    let b = &terrain.bricks[brick_index];
    let corrupt = BaselineWorld {
        schema: spall_protocol::BASELINE_WORLD_SCHEMA,
        checkpoint_tick: world.checkpoint_tick,
        volumes: vec![BaselineVolume {
            volume_id: terrain.volume_id,
            cell_size_code: terrain.cell_size_code,
            owner: BaselineOwner::Terrain,
            bounds: terrain.bounds,
            bricks: vec![BaselineBrick {
                coord: b.coord,
                revision: b.revision + 9_000,
                edited: true,
                cells: BaselineCells::Uniform(0),
            }],
        }],
    };
    replica
        .apply_baseline_patch(&corrupt)
        .expect("apply divergence");
}

fn repair_brick(sim: &Simulation, replica: &mut ReplicaWorld, world: &BaselineWorld, i: usize) {
    let terrain = &world.volumes[0];
    let b = &terrain.bricks[i];
    let coord = BrickCoord::new(b.coord[0], b.coord[1], b.coord[2]);
    let request = RepairRequest {
        key: RepairKey::Brick {
            volume: terrain.volume_id,
            coord,
        },
        expected_revision: Revision(b.revision),
        current_revision: Revision(b.revision + 9_000),
        expected_hash: Hash32::ZERO,
        current_hash: Hash32::ZERO,
    };
    let patch = brick_repair_patch(sim, &request).expect("server produces a brick patch");
    replica.apply_baseline_patch(&patch).expect("apply repair");
}

#[test]
fn real_transaction_then_two_distinct_brick_repairs_all_apply_and_converge() {
    let mut sim = fresh_sim();
    let (_, mut replica) = replica_from(&sim, 1);
    assert_eq!(replica.world_hash(), sim.world().world_hash());

    // Real transaction 1: cut the column so the beam detaches.
    sim.submit(cut(1, 10, 4, 1, 2)).expect("submit tx1");
    let reports = sim.run_until_idle(24).expect("run to idle");
    let txs = committed_in_order(&reports);
    assert!(!txs.is_empty(), "the cut committed a transaction");
    let tx1 = txs[0].clone();
    let tx1_id = tx1.transaction_id;

    assert!(matches!(
        replica.apply_transaction(&tx1),
        ApplyOutcome::Published { .. }
    ));
    assert!(replica.has_applied(tx1_id));
    assert_eq!(replica.world_hash(), sim.world().world_hash());

    // A post-cut snapshot just to enumerate resident terrain brick coords —
    // the cut made several bricks resident.
    let post = {
        let t = capture_transfer(&sim, TransferId(11), InterestEpoch(1), JournalSeq(0))
            .expect("capture");
        assemble(&t.parts)
    };
    assert!(
        post.volumes[0].bricks.len() >= 2,
        "the post-cut terrain has at least two bricks to diverge"
    );

    // Diverge two *distinct* terrain bricks, then heal each with the server's
    // versioned one-brick baseline patch. Neither collides with the other or
    // with the real committed transaction id.
    diverge_brick(&mut replica, &post, 0);
    diverge_brick(&mut replica, &post, 1);
    assert_ne!(
        replica.world_hash(),
        sim.world().world_hash(),
        "the two divergences actually broke parity"
    );

    repair_brick(&sim, &mut replica, &post, 0);
    repair_brick(&sim, &mut replica, &post, 1);

    assert_eq!(
        replica.world_hash(),
        sim.world().world_hash(),
        "both distinct repairs restored exact parity, revision included"
    );
    assert!(
        replica.has_applied(tx1_id),
        "the repairs are versioned patches — real transaction 1 stays applied, \
         not dropped as a duplicate and not overwritten"
    );
    assert_eq!(replica.pending_repair_txn_count(), 0);
}

#[test]
fn a_before_gapped_transaction_is_retained_and_retried_after_its_predecessor() {
    let mut sim = fresh_sim();
    let (_, mut replica) = replica_from(&sim, 2);

    // Two sequential cuts on the same strip of floor: tx2 depends on tx1.
    sim.submit(cut(1, 3, 1, 1, 1)).expect("submit tx1");
    let r1 = sim.run_until_idle(16).expect("run tx1");
    sim.submit(cut(2, 4, 1, 1, 1)).expect("submit tx2");
    let r2 = sim.run_until_idle(16).expect("run tx2");
    let tx1 = committed_in_order(&r1).remove(0);
    let tx2 = committed_in_order(&r2).remove(0);

    // tx2 arrives first: `before` gap, transaction retained, repair requested.
    match replica.apply_transaction(&tx2) {
        ApplyOutcome::NeedsRepair(reqs) => assert!(!reqs.is_empty(), "asked for repair"),
        other => panic!("expected NeedsRepair, got {other:?}"),
    }
    assert_eq!(
        replica.pending_repair_txn_count(),
        1,
        "tx2's ops are held for retry, not dropped"
    );

    // The missing predecessor lands; retrying the pending set publishes tx2.
    assert!(matches!(
        replica.apply_transaction(&tx1),
        ApplyOutcome::Published { .. }
    ));
    let retried = replica.retry_pending_repair_txns();
    assert!(
        retried
            .iter()
            .any(|(id, o)| *id == tx2.transaction_id && matches!(o, ApplyOutcome::Published { .. })),
        "the retained transaction was retried and published"
    );
    assert_eq!(
        replica.pending_repair_txn_count(),
        0,
        "nothing left pending"
    );
    assert_eq!(
        replica.world_hash(),
        sim.world().world_hash(),
        "end-to-end revision/hash convergence"
    );
}

#[test]
fn a_repair_patch_that_arrives_before_its_transaction_still_converges() {
    let mut sim = fresh_sim();
    let (_, mut replica) = replica_from(&sim, 3);

    // A single-brick cut so the transaction's `before` set is exactly one brick.
    sim.submit(cut(1, 3, 1, 1, 0)).expect("submit tx1");
    let reports = sim.run_until_idle(16).expect("run tx1");
    let tx1 = committed_in_order(&reports).remove(0);
    let before = tx1
        .before
        .first()
        .copied()
        .expect("the cut references a brick");

    // The repair patch (authoritative post-cut state of that brick) arrives
    // FIRST — as if a `RepairRequest` from another gap was answered before this
    // transaction reached the replica.
    let request = RepairRequest {
        key: RepairKey::Brick {
            volume: before.volume,
            coord: before.coord,
        },
        expected_revision: before.revision,
        current_revision: Revision::ZERO,
        expected_hash: Hash32::ZERO,
        current_hash: Hash32::ZERO,
    };
    let patch = brick_repair_patch(&sim, &request).expect("server brick patch");
    replica
        .apply_baseline_patch(&patch)
        .expect("apply early repair");

    // Now the transaction itself arrives. The replica is already strictly past
    // it on the only brick it touches → treated as a duplicate: no corruption,
    // no dangling pending entry, no repair loop.
    let outcome = replica.apply_transaction(&tx1);
    assert!(
        matches!(
            outcome,
            ApplyOutcome::Duplicate | ApplyOutcome::Published { .. }
        ),
        "got {outcome:?}"
    );
    assert_eq!(
        replica.pending_repair_txn_count(),
        0,
        "the transaction is not left dangling"
    );
    assert_eq!(
        replica.world_hash(),
        sim.world().world_hash(),
        "converges regardless of repair / transaction ordering"
    );
}

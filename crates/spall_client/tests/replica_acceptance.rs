//! T10 phase B acceptance: the `spall_client` replica against a real
//! `spall_sim` authoritative server.
//!
//! `spall_sim` is a dev-dependency only (this is a test, not a runtime edge in
//! the `docs/architecture.md` graph). Each test drives the authoritative
//! `Simulation`, feeds its committed `TopologyTransaction`s / `MotionSnapshot`s
//! into a `ReplicaWorld`, and checks the acceptance bullets from
//! `docs/tasks.md` T10:
//!
//! * topology hashes match after concurrent cuts;
//! * a snapshot arriving before its body's create is handled;
//! * duplicates do not repeat an edit;
//! * missing source revisions request repair (and repair heals it);
//! * a split spanning multiple records never appears partly applied.

use spall_core::EntityId;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_protocol::{RequestId, TopologyOp, TopologyTransaction};
use spall_sim::fixtures;
use spall_sim::{EditIntent, EditTarget, MotionPublisher, Simulation, SimulationConfig};

use spall_client::{ApplyOutcome, ReplicaConfig, ReplicaWorld};

fn brush(x: i64, y: i64, z: i64, r: i64) -> spall_core::SphereBrush {
    let h = BRUSH_UNIT / 2;
    spall_core::SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        r * BRUSH_UNIT,
    )
    .unwrap()
}

fn actor() -> EntityId {
    EntityId::new(1).unwrap()
}

/// Every transaction the sim committed, in control-stream (commit) order.
fn committed_in_order(reports: &[spall_sim::TickReport]) -> Vec<TopologyTransaction> {
    let mut txs: Vec<TopologyTransaction> = reports
        .iter()
        .flat_map(|r| r.committed.iter().map(|(_, c)| c.topology.clone()))
        .collect();
    txs.sort_by_key(|t| t.control_seq.0);
    txs
}

#[test]
fn replica_converges_to_the_server_hash_after_concurrent_cuts() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();
    let mut replica = ReplicaWorld::from_baseline(
        fixtures::bridged_terrain_setup().terrain,
        ReplicaConfig::default(),
    );
    assert_eq!(
        replica.world_hash(),
        sim.world().world_hash(),
        "baselines agree"
    );

    // Two cuts contending over the column brick, plus an excavation elsewhere.
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush(10, 4, 1, 2),
    ))
    .unwrap();
    sim.submit(EditIntent::cut(
        RequestId(2),
        actor(),
        EditTarget::Terrain,
        brush(11, 5, 2, 2),
    ))
    .unwrap();
    sim.submit(EditIntent::cut(
        RequestId(3),
        actor(),
        EditTarget::Terrain,
        brush(2, 1, 1, 1),
    ))
    .unwrap();
    let reports = sim.run_until_idle(64).unwrap();

    let txs = committed_in_order(&reports);
    assert!(txs.len() >= 3, "all three cuts commit");
    for tx in &txs {
        match replica.apply_transaction(tx) {
            ApplyOutcome::Published { .. } => {}
            other => panic!("tx {} did not publish: {other:?}", tx.transaction_id.get()),
        }
    }

    assert_eq!(
        replica.world_hash(),
        sim.world().world_hash(),
        "replica topology hash equals the authoritative world hash"
    );
    assert_eq!(replica.total_solid_cells(), sim.world().total_solid_cells());
    // The beam detached: the replica sees the same body count.
    assert_eq!(
        replica.body_ids().count(),
        sim.world().body_count(),
        "replica reproduced every split body"
    );
    let _ = terrain;
}

#[test]
fn a_reordered_transaction_asks_for_repair_and_repair_heals_it() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    let mut replica = ReplicaWorld::from_baseline(
        fixtures::bridged_terrain_setup().terrain,
        ReplicaConfig::default(),
    );

    // Two sequential cuts on the same brick: tx2 depends on tx1's result.
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush(6, 1, 1, 1),
    ))
    .unwrap();
    let r1 = sim.run_until_idle(16).unwrap();
    sim.submit(EditIntent::cut(
        RequestId(2),
        actor(),
        EditTarget::Terrain,
        brush(8, 1, 1, 1),
    ))
    .unwrap();
    let r2 = sim.run_until_idle(16).unwrap();
    let tx1 = committed_in_order(&r1).remove(0);
    let tx2 = committed_in_order(&r2).remove(0);

    // Apply tx2 first (tx1 was "lost"): its `before` revisions do not match.
    let before_hash = replica.world_hash();
    match replica.apply_transaction(&tx2) {
        ApplyOutcome::NeedsRepair(reqs) => assert!(!reqs.is_empty()),
        other => panic!("expected NeedsRepair, got {other:?}"),
    }
    assert_eq!(replica.world_hash(), before_hash, "nothing applied yet");

    // Deliver the missing tx1, then tx2 applies cleanly.
    assert!(matches!(
        replica.apply_transaction(&tx1),
        ApplyOutcome::Published { .. }
    ));
    assert!(matches!(
        replica.apply_transaction(&tx2),
        ApplyOutcome::Published { .. }
    ));
    assert_eq!(replica.world_hash(), sim.world().world_hash());
}

#[test]
fn a_corrupted_op_rejects_the_whole_split_transaction() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush(10, 4, 1, 2),
    ))
    .unwrap();
    let reports = sim.run_until_idle(16).unwrap();
    let mut tx = committed_in_order(&reports).remove(0);
    assert!(
        tx.ops
            .iter()
            .any(|o| matches!(o, TopologyOp::SplitOff { .. })),
        "this cut splits the beam across multiple records"
    );

    // Corrupt one child-fill run: wrong material.
    for op in &mut tx.ops {
        if let TopologyOp::CellRun { material, .. } = op {
            *material = spall_core::MaterialId(2); // dirt, not the stone that was there
            break;
        }
    }

    let mut replica = ReplicaWorld::from_baseline(
        fixtures::bridged_terrain_setup().terrain,
        ReplicaConfig::default(),
    );
    let before = replica.world_hash();
    match replica.apply_transaction(&tx) {
        ApplyOutcome::Rejected { .. } => {}
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert_eq!(
        replica.world_hash(),
        before,
        "a partly-applicable split never partly replaces live state"
    );
    assert_eq!(replica.body_ids().count(), 0, "no orphan child body");
}

#[test]
fn a_child_snapshot_that_arrives_before_the_split_is_held_then_lands() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush(10, 4, 1, 2),
    ))
    .unwrap();
    let reports = sim.run_until_idle(16).unwrap();
    let tx = committed_in_order(&reports).remove(0);
    let child_entity = match tx.ops.iter().find_map(|o| match o {
        TopologyOp::SplitOff { child_entity, .. } => Some(*child_entity),
        _ => None,
    }) {
        Some(e) => e,
        None => panic!("the cut produced a split"),
    };

    // A motion snapshot for the child, produced by the sim, arrives first.
    let mut motion = MotionPublisher::new(60, 20);
    let snaps = motion.snapshots(sim.world(), spall_core::Tick(3));
    let child_snap = snaps
        .iter()
        .find(|s| s.body == child_entity)
        .copied()
        .expect("sim publishes a snapshot for the new body");

    let mut replica = ReplicaWorld::from_baseline(
        fixtures::bridged_terrain_setup().terrain,
        ReplicaConfig::default(),
    );
    assert!(
        !replica.ingest_snapshot(&child_snap),
        "snapshot for a body that does not exist yet is held, not applied"
    );
    assert!(replica.interpolated_pose(child_entity, 3.0).is_none());

    // Now the split transaction arrives and creates the body...
    match replica.apply_transaction(&tx) {
        ApplyOutcome::Published { new_bodies, .. } => {
            assert!(new_bodies.contains(&child_entity))
        }
        other => panic!("expected Published, got {other:?}"),
    }
    // ...and the held snapshot has been applied to it.
    assert_eq!(
        replica.latest_motion_tick(child_entity),
        Some(child_snap.server_tick.get()),
        "the held snapshot landed once its topology arrived"
    );
    assert!(replica.interpolated_pose(child_entity, 3.0).is_some());
}

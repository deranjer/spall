//! Spawn-area baselines preserve exact logical topology while omitting distant geometry.
use spall_client::{ApplyOutcome, ClientResidencyPass, ReplicaConfig, ReplicaWorld};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, EntityId, Revision, SphereBrush, VolumeId};
use spall_protocol::{Hash32, InterestEpoch, RepairKey, RepairRequest, RequestId, TransferId};
use spall_server::baseline::{snapshot_world, transfer_from_snapshot_supported};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};

fn server() -> Simulation {
    let mut setup = fixtures::separated_regions_setup();
    setup.physics.disable_ccd = true;
    Simulation::new(SimulationConfig::new(setup)).unwrap()
}

fn regional(sim: &Simulation, center: [f64; 3]) -> ReplicaWorld {
    let transfer = transfer_from_snapshot_supported(
        snapshot_world(sim, None).with_region(center, 1),
        TransferId(1),
        InterestEpoch(1),
        true,
    )
    .unwrap();
    assert_eq!(
        transfer.begin.world_version,
        spall_protocol::segment::BASELINE_REGIONAL_WORLD_VERSION
    );
    let mut receiver = spall_client::segmented::SegmentedReceiver::with_limits(
        transfer.begin.checkpoint_tick.get(),
        None,
        transfer.begin.total_bytes,
    );
    for part in transfer.parts.iter() {
        receiver.push(&part.payload).unwrap();
    }
    let receipt = receiver.finish().unwrap();
    assert_eq!(receipt.chain_hash, transfer.end.assembled_hash);
    let mut replica = ReplicaWorld::empty(ReplicaConfig::default());
    replica.install_staged(receipt.staged).unwrap();
    replica
}

/// A deferred join: the replica installed from the core baseline, still waiting for its
/// catalogue, plus the catalogue's chunks.
fn deferred(
    sim: &Simulation,
    center: [f64; 3],
) -> (
    ReplicaWorld,
    spall_server::baseline::BaselineTransfer,
    Vec<spall_protocol::CatalogueChunk>,
) {
    let transfer = spall_server::baseline::transfer_from_snapshot_deferred(
        snapshot_world(sim, None).with_region(center, 1),
        TransferId(7),
        InterestEpoch(1),
        None,
    )
    .unwrap();
    assert_eq!(
        transfer.begin.world_version,
        spall_protocol::segment::BASELINE_DEFERRED_REGIONAL_WORLD_VERSION
    );
    let mut receiver = spall_client::segmented::SegmentedReceiver::with_limits(
        transfer.begin.checkpoint_tick.get(),
        None,
        transfer.begin.total_bytes,
    );
    for part in transfer.parts.iter() {
        receiver.push(&part.payload).unwrap();
    }
    let receipt = receiver.finish().unwrap();
    assert_eq!(receipt.chain_hash, transfer.end.assembled_hash);
    let mut replica = ReplicaWorld::empty(ReplicaConfig::default());
    replica.install_staged(receipt.staged).unwrap();
    replica.expect_catalogue(transfer.begin.transfer_id);
    let catalogue = transfer
        .catalogue
        .clone()
        .expect("a regional world has distant digests");
    assert_eq!(catalogue.transfer_id, transfer.begin.transfer_id);
    (replica, transfer, catalogue.chunks.to_vec())
}

fn finish_catalogue(
    replica: &mut ReplicaWorld,
    transfer: &spall_server::baseline::BaselineTransfer,
    chunks: &[spall_protocol::CatalogueChunk],
) -> Vec<(spall_core::TransactionId, ApplyOutcome)> {
    let mut receiver = spall_client::CatalogueReceiver::new(
        transfer.begin.transfer_id,
        transfer.begin.checkpoint_tick.get(),
        u64::MAX,
    );
    let mut staged = None;
    for chunk in chunks {
        assert!(staged.is_none(), "chunks after the last one");
        staged = receiver.push(chunk).unwrap();
    }
    replica
        .complete_catalogue(staged.expect("the last chunk completes the catalogue"))
        .unwrap()
}

fn request(volume: VolumeId, coord: BrickCoord) -> RepairRequest {
    RepairRequest {
        key: RepairKey::Brick { volume, coord },
        expected_revision: Revision::ZERO,
        current_revision: Revision::ZERO,
        expected_hash: Hash32::ZERO,
        current_hash: Hash32::ZERO,
    }
}

#[test]
fn regional_snapshot_omits_geometry_and_reentry_reloads_exactly() {
    let sim = server();
    let mut replica = regional(&sim, [0.0; 3]);
    let terrain = replica.terrain_volume_id();
    assert_eq!(replica.world_hash(), sim.world().world_hash());
    assert!(
        replica.terrain_volume().unwrap().resident_brick_count()
            < sim.world().terrain().volume.resident_brick_count()
    );
    assert!(!replica.evicted(terrain).is_empty());
    let mut residency = ClientResidencyPass::new(256, 1, 64 * 1024 * 1024);
    let requests = residency.step(&mut replica, [19.0, 0.5, 19.0]);
    assert!(!requests.is_empty());
    assert!(requests.len() <= spall_client::MAX_RELOAD_REQUESTS_PER_STEP);
    for req in requests {
        let RepairKey::Brick { coord, .. } = req.key else {
            panic!("brick request required");
        };
        assert!(replica.evicted(terrain).contains(coord));
        let patch = spall_server::brick_repair_patch(&sim, &req).unwrap();
        replica.apply_baseline_patch(&patch).unwrap();
        assert!(!replica.evicted(terrain).contains(coord));
    }
    assert_eq!(replica.world_hash(), sim.world().world_hash());
    // Return and reload every original logical key, including air and revisions.
    let far: Vec<_> = replica.evicted(terrain).iter().map(|(c, _)| c).collect();
    for c in far {
        let patch = spall_server::brick_repair_patch(&sim, &request(terrain, c)).unwrap();
        replica.apply_baseline_patch(&patch).unwrap();
    }
    assert_eq!(
        replica.terrain_volume().unwrap().resident_brick_count(),
        sim.world().terrain().volume.resident_brick_count()
    );
    assert_eq!(replica.world_hash(), sim.world().world_hash());
}

#[test]
fn a_distant_edit_repairs_a_never_loaded_brick_and_converges() {
    let mut sim = server();
    let mut replica = regional(&sim, [0.0; 3]);
    let h = BRUSH_UNIT / 2;
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        SphereBrush::new(
            BrushPoint::from_units(73 * BRUSH_UNIT + h, BRUSH_UNIT + h, 75 * BRUSH_UNIT + h),
            BRUSH_UNIT,
        )
        .unwrap(),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    let tx = &sim.committed(RequestId(1)).unwrap().topology;
    let mut pending = match replica.apply_transaction(tx) {
        ApplyOutcome::NeedsRepair(reqs) => reqs,
        other => panic!("expected geometry repair, got {other:?}"),
    };
    for _ in 0..16 {
        if pending.is_empty() {
            break;
        }
        for req in std::mem::take(&mut pending) {
            let patch = spall_server::brick_repair_patch(&sim, &req).unwrap();
            replica.apply_baseline_patch(&patch).unwrap();
        }
        for (_, result) in replica.retry_pending_repair_txns() {
            if let ApplyOutcome::NeedsRepair(reqs) = result {
                pending.extend(reqs);
            }
        }
    }
    assert!(pending.is_empty());
    assert_eq!(replica.pending_repair_txn_count(), 0);
    assert_eq!(replica.world_hash(), sim.world().world_hash());
    let replacement = regional(&sim, [19.0, 0.5, 19.0]);
    assert_eq!(replacement.world_hash(), sim.world().world_hash());
    assert!(
        !replacement
            .evicted(replacement.terrain_volume_id())
            .is_empty()
    );
}

#[test]
fn a_distant_collapse_splits_a_digest_only_region_and_converges() {
    let mut sim = server();
    let mut replica = regional(&sim, [0.0; 3]);
    let u = BRUSH_UNIT;
    // Sever the east column: its beam detaches into a body. The replica holds
    // that whole region only as digests.
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        SphereBrush::new(BrushPoint::from_units(83 * u, 7 * u, 76 * u), 2 * u).unwrap(),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    let tx = &sim.committed(RequestId(1)).unwrap().topology;
    assert!(
        tx.ops
            .iter()
            .any(|op| !matches!(op, spall_protocol::TopologyOp::IntegerBrush { .. })),
        "the cut must detach a body: {:?}",
        tx.ops.len()
    );
    let mut pending = match replica.apply_transaction(tx) {
        ApplyOutcome::NeedsRepair(reqs) => reqs,
        other => panic!("expected geometry repair, got {other:?}"),
    };
    for _ in 0..16 {
        if pending.is_empty() {
            break;
        }
        for req in std::mem::take(&mut pending) {
            let patch = spall_server::brick_repair_patch(&sim, &req).unwrap();
            replica.apply_baseline_patch(&patch).unwrap();
        }
        for (_, result) in replica.retry_pending_repair_txns() {
            if let ApplyOutcome::NeedsRepair(reqs) = result {
                pending.extend(reqs);
            }
        }
    }
    assert!(pending.is_empty());
    assert_eq!(replica.pending_repair_txn_count(), 0);
    assert_eq!(replica.world_hash(), sim.world().world_hash());
}

#[test]
fn a_transaction_waits_behind_a_held_predecessor_on_the_same_volume() {
    let mut sim = server();
    let mut replica = regional(&sim, [0.0; 3]);
    let u = BRUSH_UNIT;
    let cut = |sim: &mut Simulation, id: u64, x: i64, z: i64| {
        sim.submit(EditIntent::cut(
            RequestId(id),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            SphereBrush::new(BrushPoint::from_units(x * u, 7 * u, z * u), 2 * u).unwrap(),
        ))
        .unwrap();
        sim.run_until_idle(24).unwrap();
    };
    // First the distant column, whose bricks this replica holds only as
    // digests, then the near one it holds resident.
    cut(&mut sim, 1, 83, 76);
    cut(&mut sim, 2, 11, 4);
    let first = sim.committed(RequestId(1)).unwrap().topology.clone();
    let second = sim.committed(RequestId(2)).unwrap().topology.clone();

    let mut pending = match replica.apply_transaction(&first) {
        ApplyOutcome::NeedsRepair(reqs) => reqs,
        other => panic!("expected geometry repair, got {other:?}"),
    };
    // The second result hash includes the first's effect, so the replica must
    // hold it behind the first instead of rejecting it.
    match replica.apply_transaction(&second) {
        ApplyOutcome::NeedsRepair(reqs) => assert!(reqs.is_empty()),
        other => panic!("expected the second to wait, got {other:?}"),
    }
    assert_eq!(replica.pending_repair_txn_count(), 2);

    for _ in 0..16 {
        if pending.is_empty() {
            break;
        }
        for req in std::mem::take(&mut pending) {
            let patch = spall_server::brick_repair_patch(&sim, &req).unwrap();
            replica.apply_baseline_patch(&patch).unwrap();
        }
        for (_, result) in replica.retry_pending_repair_txns() {
            match result {
                ApplyOutcome::NeedsRepair(reqs) => pending.extend(reqs),
                ApplyOutcome::Rejected { reason } => panic!("rejected: {reason}"),
                _ => {}
            }
        }
    }
    assert_eq!(replica.pending_repair_txn_count(), 0);
    assert!(replica.has_applied(first.transaction_id));
    assert!(replica.has_applied(second.transaction_id));
    assert_eq!(replica.world_hash(), sim.world().world_hash());
}

#[test]
fn a_deferred_catalogue_completes_the_replica_to_the_server_hash() {
    let sim = server();
    let (mut replica, transfer, chunks) = deferred(&sim, [0.0; 3]);
    let terrain = replica.terrain_volume_id();
    assert!(
        replica.evicted(terrain).is_empty(),
        "the baseline itself carries no digests"
    );
    assert_ne!(
        replica.world_hash(),
        sim.world().world_hash(),
        "the hash is not comparable until the catalogue lands"
    );
    assert_eq!(
        replica.catalogue_pending(),
        Some(transfer.begin.transfer_id)
    );
    assert!(finish_catalogue(&mut replica, &transfer, &chunks).is_empty());
    assert_eq!(replica.catalogue_pending(), None);
    assert_eq!(replica.world_hash(), sim.world().world_hash());
    // The deferred form is much smaller than the regional baseline it replaces.
    let regional = transfer_from_snapshot_supported(
        snapshot_world(&sim, None).with_region([0.0; 3], 1),
        TransferId(1),
        InterestEpoch(1),
        true,
    )
    .unwrap();
    assert!(transfer.payload_bytes() < regional.payload_bytes());
}

/// Installs `transfer`'s core into `replica`, replacing its world, and returns the staged result
/// the way a reset does (the caller captured any basis first).
fn install_core(replica: &mut ReplicaWorld, transfer: &spall_server::baseline::BaselineTransfer) {
    let mut receiver = spall_client::segmented::SegmentedReceiver::with_limits(
        transfer.begin.checkpoint_tick.get(),
        None,
        transfer.begin.total_bytes,
    );
    for part in transfer.parts.iter() {
        receiver.push(&part.payload).unwrap();
    }
    let receipt = receiver.finish().unwrap();
    replica.install_staged(receipt.staged).unwrap();
    replica.expect_catalogue(transfer.begin.transfer_id);
}

fn feed_catalogue(
    transfer: &spall_server::baseline::BaselineTransfer,
) -> spall_client::CatalogueReceiver {
    spall_client::CatalogueReceiver::new(
        transfer.begin.transfer_id,
        transfer.begin.checkpoint_tick.get(),
        u64::MAX,
    )
}

#[test]
fn a_reset_catalogue_lists_only_what_changed_and_converges() {
    let mut sim = server();
    let (mut replica, first, chunks) = deferred(&sim, [0.0; 3]);
    finish_catalogue(&mut replica, &first, &chunks);

    // The world drifts from pristine: a distant column is cut, and the replica follows it.
    let u = BRUSH_UNIT;
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        SphereBrush::new(BrushPoint::from_units(83 * u, 7 * u, 76 * u), 2 * u).unwrap(),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    let tx = sim.committed(RequestId(1)).unwrap().topology.clone();
    let mut pending = match replica.apply_transaction(&tx) {
        ApplyOutcome::NeedsRepair(reqs) => reqs,
        other => panic!("expected geometry repair, got {other:?}"),
    };
    for _ in 0..16 {
        if pending.is_empty() {
            break;
        }
        for req in std::mem::take(&mut pending) {
            let patch = spall_server::brick_repair_patch(&sim, &req).unwrap();
            replica.apply_baseline_patch(&patch).unwrap();
        }
        for (_, result) in replica.retry_pending_repair_txns() {
            if let ApplyOutcome::NeedsRepair(reqs) = result {
                pending.extend(reqs);
            }
        }
    }
    assert_eq!(replica.world_hash(), sim.world().world_hash());

    // A reset replaces the world with a fresh one. The server captures what the client holds
    // first; the client keeps its own copy of it through the install.
    let basis = spall_server::baseline::CatalogueBasis::capture(&sim);
    let held = replica
        .logical_terrain_digests()
        .expect("a consistent replica");
    let mut setup = fixtures::separated_regions_setup();
    setup.physics.disable_ccd = true;
    sim.replace_world(Simulation::new(SimulationConfig::new(setup)).unwrap())
        .unwrap();
    let expected = sim.world().world_hash();

    let full = transfer_from_snapshot_deferred_for_test(&sim, TransferId(8), None);
    let delta =
        transfer_from_snapshot_deferred_for_test(&sim, TransferId(9), Some((&basis, expected)));
    let full_catalogue = full.catalogue.as_ref().expect("catalogue");
    let delta_catalogue = delta.catalogue.as_ref().expect("catalogue");
    assert!(!full_catalogue.delta);
    assert!(delta_catalogue.delta);
    assert!(
        delta_catalogue.bricks < full_catalogue.bricks,
        "only the bricks that differ from the held catalogue travel ({} of {})",
        delta_catalogue.bricks,
        full_catalogue.bricks
    );
    assert!(delta_catalogue.payload_bytes() < full_catalogue.payload_bytes());

    // The client installs the new core and merges the delta onto what it held.
    install_core(&mut replica, &delta);
    let mut receiver = feed_catalogue(&delta);
    let mut staged = None;
    for chunk in delta_catalogue.chunks.iter() {
        staged = receiver.push(chunk).unwrap();
    }
    assert!(receiver.delta());
    assert_eq!(receiver.expected_world_hash(), Some(expected));
    let outcomes = replica
        .complete_delta_catalogue(staged.unwrap(), held.clone(), expected)
        .unwrap();
    assert!(outcomes.is_empty());
    assert_eq!(replica.world_hash(), sim.world().world_hash());

    // A catalogue that does not reproduce the declared hash is refused, and nothing replays.
    install_core(&mut replica, &delta);
    let mut receiver = feed_catalogue(&delta);
    let mut staged = None;
    for chunk in delta_catalogue.chunks.iter() {
        staged = receiver.push(chunk).unwrap();
    }
    let wrong = spall_protocol::Hash32::of(b"not the world");
    assert!(
        replica
            .complete_delta_catalogue(staged.unwrap(), held, wrong)
            .is_err()
    );
    assert_eq!(replica.catalogue_pending(), Some(delta.begin.transfer_id));
}

#[test]
fn a_reset_delta_merges_even_when_an_edit_is_still_held_for_repair() {
    let mut sim = server();
    let (mut replica, first, chunks) = deferred(&sim, [0.0; 3]);
    finish_catalogue(&mut replica, &first, &chunks);

    // A distant edit commits, but its repair has not reached the client: the transaction is
    // held and the client's digests for those bricks are the older ones.
    let u = BRUSH_UNIT;
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        SphereBrush::new(BrushPoint::from_units(83 * u, 7 * u, 76 * u), 2 * u).unwrap(),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    let tx = sim.committed(RequestId(1)).unwrap().topology.clone();
    assert!(matches!(
        replica.apply_transaction(&tx),
        ApplyOutcome::NeedsRepair(reqs) if !reqs.is_empty()
    ));
    assert_eq!(replica.pending_repair_txn_count(), 1);
    assert_ne!(replica.world_hash(), sim.world().world_hash());

    let basis = spall_server::baseline::CatalogueBasis::capture(&sim);
    let held = replica
        .logical_terrain_digests()
        .expect("held repairs do not prevent a basis");
    let mut setup = fixtures::separated_regions_setup();
    setup.physics.disable_ccd = true;
    sim.replace_world(Simulation::new(SimulationConfig::new(setup)).unwrap())
        .unwrap();
    let expected = sim.world().world_hash();
    let delta =
        transfer_from_snapshot_deferred_for_test(&sim, TransferId(9), Some((&basis, expected)));
    let catalogue = delta.catalogue.as_ref().expect("catalogue");
    assert!(catalogue.delta);

    install_core(&mut replica, &delta);
    let mut receiver = feed_catalogue(&delta);
    let mut staged = None;
    for chunk in catalogue.chunks.iter() {
        staged = receiver.push(chunk).unwrap();
    }
    assert!(
        replica
            .complete_delta_catalogue(staged.unwrap(), held, expected)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        replica.pending_repair_txn_count(),
        0,
        "the reset discarded it"
    );
    assert_eq!(replica.world_hash(), sim.world().world_hash());
}

fn transfer_from_snapshot_deferred_for_test(
    sim: &Simulation,
    id: TransferId,
    basis: Option<(
        &spall_server::baseline::CatalogueBasis,
        spall_protocol::Hash32,
    )>,
) -> spall_server::baseline::BaselineTransfer {
    spall_server::baseline::transfer_from_snapshot_deferred(
        snapshot_world(sim, None).with_region([0.0; 3], 1),
        id,
        InterestEpoch(1),
        basis,
    )
    .unwrap()
}

#[test]
fn a_corrupted_or_misordered_catalogue_is_refused() {
    let sim = server();
    let (replica, transfer, chunks) = deferred(&sim, [0.0; 3]);
    drop(replica);
    let new_receiver = || {
        spall_client::CatalogueReceiver::new(
            transfer.begin.transfer_id,
            transfer.begin.checkpoint_tick.get(),
            u64::MAX,
        )
    };
    if chunks.len() > 1 {
        assert!(new_receiver().push(&chunks[1]).is_err(), "out of order");
    }
    let mut wrong_id = chunks[0].clone();
    wrong_id.transfer_id = TransferId(99);
    assert!(new_receiver().push(&wrong_id).is_err(), "stale transfer id");
    let mut tampered = new_receiver();
    let mut result = Ok(None);
    for chunk in &chunks {
        let mut chunk = chunk.clone();
        if chunk.index == 0 {
            let last = chunk.payload.len() - 1;
            chunk.payload[last] ^= 0x55;
        }
        result = tampered.push(&chunk);
        if result.is_err() {
            break;
        }
    }
    assert!(result.is_err(), "a flipped byte must not assemble");
}

#[test]
fn transactions_wait_for_the_catalogue_then_apply_in_order() {
    let mut sim = server();
    let (mut replica, transfer, chunks) = deferred(&sim, [0.0; 3]);
    let h = BRUSH_UNIT / 2;
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        SphereBrush::new(
            BrushPoint::from_units(73 * BRUSH_UNIT + h, BRUSH_UNIT + h, 75 * BRUSH_UNIT + h),
            BRUSH_UNIT,
        )
        .unwrap(),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    let tx = sim.committed(RequestId(1)).unwrap().topology.clone();

    // Before the catalogue the replica cannot tell which bricks the cut touches.
    let before = replica.world_hash();
    match replica.apply_transaction(&tx) {
        ApplyOutcome::NeedsRepair(reqs) => assert!(reqs.is_empty()),
        other => panic!("expected the transaction to wait, got {other:?}"),
    }
    assert_eq!(replica.catalogue_deferred_count(), 1);
    assert_eq!(
        replica.apply_transaction(&tx),
        ApplyOutcome::Duplicate,
        "a redelivery does not queue twice"
    );
    assert_eq!(replica.world_hash(), before);

    // The catalogue is the checkpoint's, taken before the cut; the wait ends with the replay.
    let outcomes = finish_catalogue(&mut replica, &transfer, &chunks);
    assert_eq!(outcomes.len(), 1);
    let mut pending = match &outcomes[0].1 {
        ApplyOutcome::NeedsRepair(reqs) => reqs.clone(),
        other => panic!("expected geometry repair, got {other:?}"),
    };
    for _ in 0..16 {
        if pending.is_empty() {
            break;
        }
        for req in std::mem::take(&mut pending) {
            let patch = spall_server::brick_repair_patch(&sim, &req).unwrap();
            replica.apply_baseline_patch(&patch).unwrap();
        }
        for (_, result) in replica.retry_pending_repair_txns() {
            if let ApplyOutcome::NeedsRepair(reqs) = result {
                pending.extend(reqs);
            }
        }
    }
    assert_eq!(replica.pending_repair_txn_count(), 0);
    assert_eq!(replica.catalogue_deferred_count(), 0);
    assert_eq!(replica.world_hash(), sim.world().world_hash());
}

#[test]
fn a_brick_reloaded_before_the_catalogue_lands_is_not_replaced_by_its_digest() {
    let sim = server();
    let (mut replica, transfer, chunks) = deferred(&sim, [0.0; 3]);
    let terrain = replica.terrain_volume_id();
    // A distant brick the baseline did not carry, fetched by an ordinary repair.
    let coord = BrickCoord::new(2, 0, 2);
    assert!(
        replica
            .volume(terrain)
            .unwrap()
            .snapshot_brick(coord)
            .unwrap()
            .is_none()
    );
    let patch = spall_server::brick_repair_patch(&sim, &request(terrain, coord)).unwrap();
    replica.apply_baseline_patch(&patch).unwrap();
    assert!(finish_catalogue(&mut replica, &transfer, &chunks).is_empty());
    assert!(
        !replica.evicted(terrain).contains(coord),
        "a resident brick keeps no digest"
    );
    assert_eq!(replica.world_hash(), sim.world().world_hash());
}

#[test]
fn invalid_interest_and_legacy_digest_encoding_are_rejected() {
    let sim = server();
    for (center, radius, segmented) in [
        ([f64::NAN, 0.0, 0.0], 1, true),
        ([0.0; 3], 0, true),
        ([0.0; 3], 17, true),
        ([0.0; 3], 1, false),
    ] {
        assert!(
            transfer_from_snapshot_supported(
                snapshot_world(&sim, None).with_region(center, radius),
                TransferId(1),
                InterestEpoch(1),
                segmented
            )
            .is_err()
        );
    }
}

#[test]
fn distant_dynamic_bodies_keep_complete_geometry_and_ownership() {
    let mut sim = server();
    let entity = spall_sim::fixtures::spawn_g1_hollow_test_volume(sim.world_mut()).unwrap();
    let replica = regional(&sim, [-100.0; 3]);
    assert_eq!(replica.world_hash(), sim.world().world_hash());
    assert!(replica.body_ids().any(|id| id == entity));
    for body in sim.world().bodies() {
        let remote = replica.volume(body.volume_id).unwrap();
        assert_eq!(
            remote.resident_brick_count(),
            body.volume.resident_brick_count()
        );
        assert!(replica.evicted(body.volume_id).is_empty());
    }
    assert_eq!(replica.terrain_volume().unwrap().resident_brick_count(), 0);
}

fn run_quic_regional_session(defer: bool) {
    use spall_client::{
        BaselineScene, ClientNetConfig, ClientResidencyLimits, MovementStep, ScriptTarget,
        ScriptedAction, run_replication_client,
    };
    use spall_core::{CellSizeCode, GlobalCell, MaterialId};
    use spall_net::{Fingerprint, JoinToken, TransportConfig};
    use spall_server::{CustomWorld, Scene, ServeConfig, serve};
    use spall_voxel::{EditPlan, Volume};
    use std::time::{Duration, Instant};
    let dir = std::env::temp_dir().join(format!(
        "spall-regional-quic-{}-{}",
        std::process::id(),
        defer
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let token = JoinToken::generate().unwrap();
    let fp = dir.join("fingerprint");
    let addr = dir.join("addr");
    let mut config = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
    config.custom_world = Some(CustomWorld::new(vec![[4.0, 0.5, 4.0]], || {
        let mut setup = fixtures::flat_terrain_setup();
        let id = VolumeId::new(1).unwrap();
        let mut volume = Volume::new(id, CellSizeCode::Quarter);
        let mut plan = EditPlan::new(id);
        for z in 0..32 {
            for y in 0..2 {
                for x in 0..256 {
                    plan.set(GlobalCell::new(x, y, z), MaterialId(1));
                }
            }
        }
        volume.apply_edit(&plan).unwrap();
        setup.terrain = volume;
        setup.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(255, 31, 31));
        setup
    }));
    config.max_ticks = 840;
    config.quiescence_ticks = 0;
    config.paced = true;
    config.dev_unvalidated_actions = true;
    config.admin_commands = true;
    config.log_json = dir.join("server.jsonl");
    config.fingerprint_out = Some(fp.clone());
    config.addr_out = Some(addr.clone());
    let thread = std::thread::spawn(move || serve(config));
    let start = Instant::now();
    while !(fp.exists() && addr.exists()) {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    let initial_counts = std::sync::Arc::new(std::sync::Mutex::new(None));
    let observed = initial_counts.clone();
    let client = run_replication_client(ClientNetConfig {
        connect_addr: std::fs::read_to_string(&addr)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
        server_fingerprint: Fingerprint::from_hex(std::fs::read_to_string(&fp).unwrap().trim())
            .unwrap(),
        join_token: token,
        script: vec![ScriptedAction {
            at_tick: 60,
            request: spall_client::cut_request(1, 0, [190, 1, 16], 0),
            target: ScriptTarget::Terrain,
        }],
        movement_script: vec![
            MovementStep {
                from_tick: 0,
                to_tick: 240,
                movement: [1.0, 0.0, 0.0],
                view_dir: [0.0, 0.0, -1.0],
                buttons: 0,
            },
            MovementStep {
                from_tick: 240,
                to_tick: 480,
                movement: [-1.0, 0.0, 0.0],
                view_dir: [0.0, 0.0, -1.0],
                buttons: 0,
            },
        ],
        late_join: true,
        baseline_scene: BaselineScene::Walk,
        run_ticks: 840,
        idle_grace: Duration::from_secs(2),
        overall_timeout: Duration::from_secs(30),
        log_json: dir.join("client.jsonl"),
        summary_json: Some(dir.join("client.summary.json")),
        transport: TransportConfig::for_tests(),
        client_residency: Some(ClientResidencyLimits {
            stream_initial: true,
            defer_catalogue: defer,
            budget_bricks: 32,
            interest_radius_bricks: 1,
            max_dense_bytes: 4 * 1024 * 1024,
        }),
        baseline_budget_bytes: 64 * 1024 * 1024,
        on_replica_ready: Some(std::sync::Arc::new(move |replica| {
            let replica = replica.lock().unwrap();
            *observed.lock().unwrap() = Some((
                replica.terrain_volume().unwrap().resident_brick_count(),
                replica.evicted(replica.terrain_volume_id()).len(),
            ));
        })),
        interactive: None,
        client_authoritative: false,
        admin_script: vec![(660, spall_protocol::AdminCommand::ResetWorld)],
    })
    .unwrap();
    let server = thread.join().unwrap().unwrap();
    if defer {
        assert_eq!(
            *initial_counts.lock().unwrap(),
            Some((2, 0)),
            "a deferred baseline carries no digests"
        );
        assert!(client.catalogue_complete_ms > 0, "the catalogue completed");
        assert!(!client.catalogue_pending_at_end);
    } else {
        assert_eq!(*initial_counts.lock().unwrap(), Some((2, 6)));
    }
    assert_eq!(client.final_world_hash, server.final_world_hash);
    assert!(client.client_residency_reloads_completed >= 2);
    assert!(client.client_residency_evictions >= 1);
    // An authoritative post-edit repair can satisfy a held transaction directly.
    assert!(server.transactions_committed >= 1);
    assert!(client.client_residency_evicted_transaction_gaps >= 1);
    assert!(client.repairs_applied >= 1);
    assert_eq!(client.transactions_rejected, 0);
    assert_eq!(client.world_resets_installed, 1);
    assert_eq!(client.baseline_transfer_failures, 0);
    assert_eq!(server.actions_rejected, 0);
    let trace = client.movement_trace.as_ref().expect("movement trace");
    assert!(
        trace.ticks.iter().any(|r| r.predicted_pos_m[0] > 17.0),
        "player crossed into initially unloaded terrain"
    );
    assert!(
        trace.ticks.iter().all(|r| r.predicted_pos_m[1] > 0.3),
        "unknown collision must not become air"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn quic_regional_join_traversal_distant_edit_and_reset_converge() {
    run_quic_regional_session(false);
}

#[test]
fn quic_deferred_catalogue_join_traversal_distant_edit_and_reset_converge() {
    run_quic_regional_session(true);
}

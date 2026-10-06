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

#[test]
fn quic_regional_join_traversal_distant_edit_and_reset_converge() {
    use spall_client::{
        BaselineScene, ClientNetConfig, ClientResidencyLimits, MovementStep, ScriptTarget,
        ScriptedAction, run_replication_client,
    };
    use spall_core::{CellSizeCode, GlobalCell, MaterialId};
    use spall_net::{Fingerprint, JoinToken, TransportConfig};
    use spall_server::{CustomWorld, Scene, ServeConfig, serve};
    use spall_voxel::{EditPlan, Volume};
    use std::time::{Duration, Instant};
    let dir = std::env::temp_dir().join(format!("spall-regional-quic-{}", std::process::id()));
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
    assert_eq!(*initial_counts.lock().unwrap(), Some((2, 6)));
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

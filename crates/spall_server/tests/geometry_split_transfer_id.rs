//! ENG-218: a real oversized geometry split uses one allocator ID across the
//! QUIC marker/bulk stream and the durable topology replay record.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spall_client::{ApplyOutcome, ReplicaConfig, ReplicaWorld, cut_request};
use spall_core::WorldId;
use spall_net::{Fingerprint, JoinToken, TransportConfig, WireRecord, connect};
use spall_protocol::{
    AlgorithmVersions, BaselineAck, Handshake, Hash32, NegotiatedLimits, PROTOCOL_VERSION,
    SessionId, SlotId, TopologyOp, TransferId,
};
use spall_server::serve::{
    DEFAULT_CATCH_UP_CAP, DEFAULT_MAX_JOIN_RETRIES, default_capture_workers,
};
use spall_server::{HarnessConfig, HostConfig};
use spall_server::{Scene, ServeConfig, ServeSummary, serve};
use spall_store::{FaultPlan, Recovery};

const WORLD_ID: u128 = 0x5A11_0000_0000_7010;

fn unique_dir(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("spall-eng218-{tag}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn wait_for_file(path: &Path, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        if let Ok(value) = std::fs::read_to_string(path)
            && !value.trim().is_empty()
        {
            return value.trim().to_owned();
        }
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn config(
    dir: &Path,
    tag: &str,
    token: JoinToken,
    save: &Path,
    max_ticks: u64,
    save_faults: Option<FaultPlan>,
) -> ServeConfig {
    ServeConfig {
        server: HostConfig {
            listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            scene: Scene::BulkSplit,
            terrain_collider_mode: spall_sim::world::TerrainColliderMode::PerBrick,
            join_token: token,
            max_clients: 2,
            paced: true,
            log_json: dir.join(format!("{tag}.jsonl")),
            transport: TransportConfig::for_tests(),
            save: Some(save.to_owned()),
            checkpoint_interval_ticks: 0,
            seed: 0,
            catch_up_cap: DEFAULT_CATCH_UP_CAP,
            max_join_retries: DEFAULT_MAX_JOIN_RETRIES,
            capture_workers: default_capture_workers(),
            admin_commands: false,
            motion_interest: None,
            residency: None,
            residency_disk_path: None,
            creatures: None,
            creature_shape: None,
            item_pickup: None,
            contact_damage: None,
            dormancy: None,
            credential_registry_file: None,
            day_clock: Default::default(),
            custom_world: None,
        },
        harness: HarnessConfig {
            max_ticks,
            quiescence_ticks: 180,
            drain_timeout: spall_server::serve::DEFAULT_DRAIN_TIMEOUT,
            min_clients: 1,
            startup_timeout: Duration::from_secs(20),
            summary_json: Some(dir.join(format!("{tag}.summary.json"))),
            fingerprint_out: Some(dir.join(format!("{tag}.fingerprint"))),
            addr_out: Some(dir.join(format!("{tag}.addr"))),
            dev_unvalidated_actions: true,
            save_faults,
            await_body_settle: false,
            timing_window: None,
        },
    }
}

fn client_handshake() -> Handshake {
    let materials = spall_sim::fixtures::stone_manifest();
    Handshake {
        protocol_version: PROTOCOL_VERSION,
        content_manifest_hash: spall_protocol::content_manifest_hash(&materials),
        world_id: WorldId::from_u128(WORLD_ID),
        generator_version: 1,
        algorithms: AlgorithmVersions {
            integer_brush: 1,
            structure_graph: 1,
            topology_hash: 1,
        },
        server_tick_hz: 60,
        motion_snapshot_hz: 20,
        cell_size_codes: vec![spall_core::CellSizeCode::Quarter],
        session: SessionId::from_parts(SlotId(0), 1),
        limits: NegotiatedLimits::DEFAULT,
        day_clock: spall_core::time_of_day::DayClock::default(),
    }
}

async fn request_full_baseline(conn: &spall_net::Connection) -> (ReplicaWorld, u64) {
    conn.send_record(WireRecord::BaselineAck(BaselineAck {
        transfer_id: TransferId(0),
        verified_manifest_hash: spall_protocol::segment::baseline_cap_streamed(),
        installed_cursor: spall_core::JournalSeq(0),
    }))
    .await
    .unwrap();

    let begin = loop {
        if let WireRecord::BaselineBegin(begin) = conn
            .recv_record()
            .await
            .unwrap()
            .expect("server remains connected")
        {
            break begin;
        }
    };
    assert_eq!(
        begin.world_version,
        spall_protocol::segment::BASELINE_STREAMED_WORLD_VERSION,
        "the test client negotiated a bounded streamed baseline"
    );
    let mut receiver = spall_client::segmented::SegmentedReceiver::with_limits(
        begin.checkpoint_tick.get(),
        None,
        begin.total_bytes,
    );
    let mut stream = conn.accept_streamed_baseline(&begin).await.unwrap();
    let mut part_ids = Vec::new();
    let mut bytes = 0_u64;
    while let Some(part) = stream.next_part().await.unwrap() {
        part_ids.push(part.transfer_id);
        bytes += part.payload.len() as u64;
        receiver.push(&part.payload).unwrap();
    }
    assert_eq!(bytes, begin.total_bytes);
    assert_eq!(part_ids.len(), begin.part_count as usize);
    assert!(part_ids.iter().all(|id| *id == begin.transfer_id));
    let end = loop {
        if let WireRecord::BaselineEnd(end) = conn
            .recv_record()
            .await
            .unwrap()
            .expect("baseline end arrives")
        {
            break end;
        }
    };
    let receipt = receiver.finish().unwrap();
    assert_eq!(end.transfer_id, begin.transfer_id);
    assert_eq!(end.journal_cursor, begin.journal_cursor);
    assert_eq!(end.assembled_hash, receipt.chain_hash);
    let hash = receipt.chain_hash;
    let mut replica = ReplicaWorld::empty(ReplicaConfig::default());
    replica.install_staged(receipt.staged).unwrap();
    conn.send_record(WireRecord::BaselineAck(BaselineAck {
        transfer_id: begin.transfer_id,
        verified_manifest_hash: hash,
        installed_cursor: begin.journal_cursor,
    }))
    .await
    .unwrap();
    (replica, begin.transfer_id.0)
}

async fn apply_one_bulk_split(
    address: SocketAddr,
    fingerprint: Fingerprint,
    token: JoinToken,
    request_split: bool,
) -> (u64, u64, Hash32, Hash32) {
    let conn = connect(
        address,
        fingerprint,
        token,
        client_handshake(),
        TransportConfig::for_tests(),
    )
    .await
    .unwrap();
    let conn = std::sync::Arc::new(conn);
    let (stop_liveness, liveness_rx) = tokio::sync::watch::channel(false);
    let liveness = tokio::spawn(conn.clone().run_liveness(liveness_rx));
    let (mut replica, initial_transfer_id) = request_full_baseline(&conn).await;

    if !request_split {
        // The baseline requested above is the first post-recovery allocation.
        // Its low sequence shares the allocator with Split namespace IDs.
        let id = initial_transfer_id;
        let _ = stop_liveness.send(true);
        conn.close("transfer ID observed");
        let _ = liveness.await;
        return (id, 0, Hash32::of(&[]), replica.world_hash());
    }

    conn.send_record(WireRecord::ActionRequest(cut_request(1, 0, [31, 8, 31], 8)))
        .await
        .unwrap();
    let transaction = loop {
        match conn
            .recv_record()
            .await
            .unwrap()
            .expect("split transaction arrives")
        {
            WireRecord::TopologyTransaction(tx)
                if tx
                    .ops
                    .iter()
                    .any(|op| matches!(op, TopologyOp::SplitOffBulkBaseline { .. })) =>
            {
                break tx;
            }
            _ => {}
        }
    };
    let marker_id = transaction
        .ops
        .iter()
        .find_map(|op| match op {
            TopologyOp::SplitOffBulkBaseline { transfer_id, .. } => Some(*transfer_id),
            _ => None,
        })
        .expect("the accepted noisy split is marked for bulk geometry");
    let begin = loop {
        if let WireRecord::BaselineBegin(begin) = conn
            .recv_record()
            .await
            .unwrap()
            .expect("bulk begin follows marker")
        {
            break begin;
        }
    };
    assert_eq!(begin.transfer_id.0, marker_id);
    let mut stream = conn.accept_streamed_baseline(&begin).await.unwrap();
    let mut compressed = Vec::with_capacity(begin.total_bytes as usize);
    let mut part_ids = Vec::new();
    while let Some(part) = stream.next_part().await.unwrap() {
        part_ids.push(part.transfer_id.0);
        compressed.extend_from_slice(&part.payload);
    }
    assert_eq!(part_ids.len(), begin.part_count as usize);
    assert!(part_ids.iter().all(|id| *id == marker_id));
    assert_eq!(compressed.len() as u64, begin.total_bytes);
    let end = loop {
        if let WireRecord::BaselineEnd(end) = conn
            .recv_record()
            .await
            .unwrap()
            .expect("bulk end follows parts")
        {
            break end;
        }
    };
    assert_eq!(end.transfer_id.0, marker_id);
    assert_eq!(end.journal_cursor, begin.journal_cursor);
    let baseline = spall_protocol::BaselineWorld::decode_compressed(&compressed).unwrap();
    assert_eq!(end.assembled_hash, Hash32::of(&baseline.encode()));
    assert!(matches!(
        replica.apply_transaction(&transaction),
        ApplyOutcome::AwaitingBulkSplit { transfer_id } if transfer_id == marker_id
    ));
    let applied = replica.provide_bulk_split_world(marker_id, baseline);
    assert!(matches!(
        applied.as_slice(),
        [(_, ApplyOutcome::Published { .. })]
    ));
    let client_hash = replica.world_hash();
    let received_baseline_hash = end.assembled_hash;
    let _ = stop_liveness.send(true);
    conn.close("bulk split applied");
    let _ = liveness.await;
    (
        marker_id,
        begin.transfer_id.0,
        received_baseline_hash,
        client_hash,
    )
}

fn start_server(
    dir: &Path,
    tag: &str,
    save: &Path,
    token: JoinToken,
    max_ticks: u64,
    faults: Option<FaultPlan>,
) -> (
    std::thread::JoinHandle<Result<ServeSummary, spall_server::serve::ServeError>>,
    SocketAddr,
    Fingerprint,
) {
    let cfg = config(dir, tag, token, save, max_ticks, faults);
    let addr_path = cfg.harness.addr_out.clone().unwrap();
    let fingerprint_path = cfg.harness.fingerprint_out.clone().unwrap();
    let handle = std::thread::spawn(move || serve(cfg));
    let addr: SocketAddr = wait_for_file(&addr_path, Duration::from_secs(20))
        .parse()
        .unwrap();
    let fingerprint =
        Fingerprint::from_hex(&wait_for_file(&fingerprint_path, Duration::from_secs(20))).unwrap();
    (handle, addr, fingerprint)
}

fn retained_bulk_record(recovery: &Recovery) -> Option<(u64, Hash32)> {
    recovery.journal.iter().find_map(|row| {
        let saved = row.payload.as_topology_with_attachments()?;
        let decoded = saved.as_ref().ok()?;
        let (tx, _, attachments) = decoded;
        let transfer_id = tx.ops.iter().find_map(|op| match op {
            TopologyOp::SplitOffBulkBaseline { transfer_id, .. } => Some(*transfer_id),
            _ => None,
        })?;
        let spall_protocol::ValidatedTransactionAttachmentV1::Geometry(world) =
            attachments.get(transfer_id)?;
        Some((transfer_id, Hash32::of(&world.encode())))
    })
}

#[test]
fn oversized_split_shares_one_wire_id_and_durable_replay_then_seeds_restart() {
    let dir = unique_dir("geometry-split-transfer");
    let save = dir.join("world.db");
    let token = JoinToken::generate().unwrap();

    // The noisy full-size fixture is intentional: it exceeds the inline split
    // payload cap and exercises the same workload as the Sim acceptance test.
    let (server_thread, address, fingerprint) = start_server(
        &dir,
        "first",
        &save,
        token,
        900,
        Some(FaultPlan::disk_fail_checkpoint()),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (marker_id, stream_id, received_baseline_hash, client_hash) =
        runtime.block_on(apply_one_bulk_split(address, fingerprint, token, true));
    assert_eq!(
        marker_id, stream_id,
        "marker and received begin/parts/end agree"
    );

    let capture_deadline = Instant::now() + Duration::from_secs(10);
    let (recovery, durable_id, attached_hash) = loop {
        let recovery = spall_store::recover(&save).expect("SQLite recovery while server is live");
        if let Some((id, hash)) = retained_bulk_record(&recovery) {
            break (recovery, id, hash);
        }
        assert!(
            Instant::now() < capture_deadline,
            "bulk split journal row was not durable"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        durable_id, marker_id,
        "durable transaction preserves the wire transfer ID"
    );
    assert_eq!(
        attached_hash, received_baseline_hash,
        "durable baseline content matches the baseline assembled from QUIC parts"
    );

    let persist_cfg = spall_server::PersistConfig {
        world_id: WORLD_ID,
        seed: 0,
        generator_version: 1,
    };
    let (restored, _) = spall_server::restore(
        &recovery,
        &persist_cfg,
        spall_server::RecoveryChoice::RequireClean,
        spall_sim::fixtures::stone_manifest(),
        spall_structure::AnchorPlane::at(0),
        spall_physics::PhysicsConfig::default(),
    )
    .expect("the SQLite checkpoint plus durable split suffix restores");
    assert_eq!(restored.world().world_hash(), client_hash);
    let durable_baseline = recovery
        .journal
        .iter()
        .find_map(|row| {
            let (tx, _, set) = row.payload.as_topology_with_attachments()?.ok()?;
            let id = tx.ops.iter().find_map(TopologyOp::bulk_transfer_id)?;
            let spall_protocol::ValidatedTransactionAttachmentV1::Geometry(world) = set.get(id)?;
            Some(world.clone())
        })
        .unwrap();
    assert_eq!(Hash32::of(&durable_baseline.encode()), attached_hash);

    // The injected failure is limited to the final checkpoint. The initial
    // checkpoint and split journal row are durable and should remain readable.
    let first_server = server_thread.join().unwrap();
    assert!(
        first_server.is_err(),
        "the armed final-checkpoint failure is surfaced"
    );
    let after_shutdown =
        spall_store::recover(&save).expect("durable suffix survives failed checkpoint");
    assert_eq!(retained_bulk_record(&after_shutdown).unwrap().0, marker_id);

    // Restart the same saved world and observe its next allocator-issued
    // baseline transfer. The allocator is shared across namespaces, so this
    // low sequence must exceed the retained Split ID's low sequence.
    let (restart_thread, restart_addr, restart_fingerprint) =
        start_server(&dir, "restart", &save, token, 120, None);
    let restart_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (restart_baseline_id, _, _, _) = restart_runtime.block_on(apply_one_bulk_split(
        restart_addr,
        restart_fingerprint,
        token,
        false,
    ));
    let low_mask = (1_u64 << 61) - 1;
    assert!(
        restart_baseline_id & low_mask > marker_id & low_mask,
        "restarted allocator must seed above retained split ID: new {restart_baseline_id:#x}, old {marker_id:#x}"
    );
    restart_thread
        .join()
        .unwrap()
        .expect("recovered server shuts down cleanly");
}

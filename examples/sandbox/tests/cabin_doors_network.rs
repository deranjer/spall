#![cfg(feature = "client")]
//! Real QUIC door interaction, late join and duplicate request regression.
use sandbox::{editor_scene, game};
use spall_client::replica::ReplicaWorld;
use spall_client::{
    BaselineScene, ClientNetConfig, InteractiveSession, run_replication_client_with_manifest,
};
use spall_core::{BrushPoint, GlobalCell, MaterialId, SphereBrush};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_protocol::{ActionKind, ActionRequest, ClaimedTarget, InputSeq, RequestId};
use spall_server::{CustomWorld, Scene, ServeConfig};
use spall_voxel::Sample;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn until<T>(mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "door network wait timed out"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn hinge(replica: &Arc<Mutex<ReplicaWorld>>) -> Option<MaterialId> {
    let r = replica.lock().unwrap();
    match r
        .terrain_volume()?
        .sample(GlobalCell::new(37, 4, 24))
        .ok()?
    {
        Sample::Filled(m) => Some(m),
        _ => None,
    }
}
#[test]
fn door_open_late_join_close_and_duplicate_request_converge() {
    let dir = std::env::temp_dir().join(format!("spall-cabin-door-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let scene = Arc::new(
        editor_scene::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cabin"))
            .unwrap(),
    );
    let token = JoinToken::generate().unwrap();
    let mut cfg = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
    cfg.server.custom_world = Some(CustomWorld::new(
        vec![[10.125, 1.0, 8.5], [12.0, 1.0, 9.0]],
        move || scene.world_setup(),
    ));
    cfg.server.paced = true;
    cfg.server.max_clients = 2;
    cfg.server.log_json = dir.join("server.jsonl");
    cfg.server.transport = TransportConfig::for_tests();
    cfg.harness.max_ticks = 900;
    cfg.harness.min_clients = 1;
    cfg.harness.quiescence_ticks = 0;
    cfg.harness.startup_timeout = Duration::from_secs(20);
    cfg.harness.fingerprint_out = Some(dir.join("fingerprint"));
    cfg.harness.addr_out = Some(dir.join("addr"));
    let server = std::thread::spawn(move || {
        spall_server::serve_with_game_content(cfg, game::tool_catalog(), game::manifest())
            .expect("door server")
    });
    let fingerprint = Fingerprint::from_hex(
        until(|| std::fs::read_to_string(dir.join("fingerprint")).ok()).trim(),
    )
    .unwrap();
    let addr = until(|| std::fs::read_to_string(dir.join("addr")).ok())
        .trim()
        .parse()
        .unwrap();
    let connect = |name: &str| {
        let session = InteractiveSession::new();
        let slot = Arc::new(Mutex::new(None));
        let ready = slot.clone();
        let cfg = ClientNetConfig {
            connect_addr: addr,
            server_fingerprint: fingerprint,
            join_token: token,
            script: Vec::new(),
            movement_script: Vec::new(),
            late_join: true,
            baseline_scene: BaselineScene::BridgeCut,
            run_ticks: 880,
            idle_grace: Duration::from_secs(20),
            overall_timeout: Duration::from_secs(30),
            log_json: dir.join(format!("{name}.jsonl")),
            summary_json: None,
            transport: TransportConfig::for_tests(),
            client_residency: None,
            baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
            on_replica_ready: Some(Arc::new(move |r| *ready.lock().unwrap() = Some(r))),
            interactive: Some(session.clone()),
            client_authoritative: false,
            admin_script: Vec::new(),
        };
        let client = std::thread::spawn(move || {
            run_replication_client_with_manifest(cfg, game::manifest()).expect("door client")
        });
        let replica = until(|| slot.lock().unwrap().clone());
        (session, client, replica)
    };
    let (first, client1, replica1) = connect("first");
    until(|| (hinge(&replica1) == Some(game::CABIN_DOOR.closed_hinge)).then_some(()));
    let request = |id| ActionRequest {
        request_id: RequestId(id),
        input_seq: InputSeq(id),
        action: ActionKind::Interact,
        tool: 5,
        // A bogus claim intentionally proves the server selects from its player.
        aim_origin_m: [999.0, 999.0, 999.0],
        aim_dir: [0.0, 0.0, -1.0],
        claimed_target: ClaimedTarget::Terrain,
        claimed_brush: SphereBrush::new(BrushPoint::from_units(0, 0, 0), 0).unwrap(),
    };
    first.push_action(request(2_000_000));
    until(|| (hinge(&replica1) == Some(game::CABIN_DOOR.open_hinge)).then_some(()));
    let (second, client2, replica2) = connect("late");
    until(|| (hinge(&replica2) == Some(game::CABIN_DOOR.open_hinge)).then_some(()));
    first.push_action(request(2_000_000));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(hinge(&replica1), Some(game::CABIN_DOOR.open_hinge));
    first.push_action(request(2_000_001));
    until(|| {
        (hinge(&replica1) == Some(game::CABIN_DOOR.closed_hinge)
            && hinge(&replica2) == Some(game::CABIN_DOOR.closed_hinge))
        .then_some(())
    });
    assert_eq!(
        replica1.lock().unwrap().world_hash(),
        replica2.lock().unwrap().world_hash()
    );
    first.request_stop();
    second.request_stop();
    client1.join().unwrap();
    client2.join().unwrap();
    server.join().unwrap();
}

#![cfg(feature = "client")]
//! The torch, end to end: a real server hosts a generated world, a client
//! session queues the exact `ActionRequest` the `T` key builds, the server
//! validates the aim against its own terrain and places the emissive lamp in
//! the empty cell against the struck face, and the client's replica sees it.
//!
//! Run: `cargo test -p sandbox --features client --test worldgen_torch`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glam::Vec3;
use sandbox::game;
use sandbox::worldgen_scene;
use spall_client::replica::ReplicaWorld;
use spall_client::window::torch_request;
use spall_client::{
    BaselineScene, ClientNetConfig, InteractiveSession, run_replication_client_with_manifest,
};
use spall_core::GlobalCell;
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_server::{Scene, ServeConfig};
use spall_voxel::Sample;

fn patient_transport() -> TransportConfig {
    let mut transport = TransportConfig::for_tests();
    transport.idle_timeout = Duration::from_secs(120);
    transport
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spall-worldgen-torch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(path: &Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return text.trim().to_owned();
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "waiting for {path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sample(replica: &Arc<Mutex<ReplicaWorld>>, cell: GlobalCell) -> Option<Sample> {
    let guard = replica.lock().unwrap();
    guard.terrain_volume().and_then(|v| v.sample(cell).ok())
}

#[test]
fn a_torch_places_a_lamp_the_client_replica_sees() {
    let scene = worldgen_scene::generate("showcase", 1, 256).expect("generate");
    let spawn = scene.player_spawns()[0];
    // Aim straight down at the ground 6 m under a point above the first spawn.
    let (x, z) = (
        (spawn[0] / 0.25).floor() as i64,
        (spawn[2] / 0.25).floor() as i64,
    );
    let ground_y = (spawn[1] / 0.25).round() as i64 - 1;
    let eye = Vec3::new(spawn[0] as f32, spawn[1] as f32 + 6.0, spawn[2] as f32);
    let ground = GlobalCell::new(x, ground_y, z);

    let dir = scratch();
    let token = JoinToken::generate().unwrap();
    let mut config = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
    config.custom_world = Some(scene.into_custom_world());
    // The server stops after this many ticks (25 s paced). The client joins and
    // baselines at a variable pace, so a short budget can end before the swing
    // is even sent; this leaves a wide margin.
    config.max_ticks = 1500;
    config.quiescence_ticks = 0;
    config.min_clients = 1;
    config.max_clients = 1;
    config.paced = true;
    config.startup_timeout = Duration::from_secs(60);
    config.log_json = dir.join("server.jsonl");
    config.fingerprint_out = Some(dir.join("server.fingerprint"));
    config.addr_out = Some(dir.join("server.addr"));
    config.transport = patient_transport();
    let server = std::thread::spawn(move || {
        spall_server::serve_with_game_content(config, game::tool_catalog(), game::manifest())
    });
    let fingerprint = Fingerprint::from_hex(&wait_for(&dir.join("server.fingerprint"))).unwrap();
    let addr: SocketAddr = wait_for(&dir.join("server.addr")).parse().unwrap();

    let session = InteractiveSession::new();
    let slot: Arc<Mutex<Option<Arc<Mutex<ReplicaWorld>>>>> = Arc::new(Mutex::new(None));
    let slot_for_client = slot.clone();
    let cfg = ClientNetConfig {
        connect_addr: addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::BridgeCut,
        run_ticks: 1450,
        idle_grace: Duration::from_secs(40),
        overall_timeout: Duration::from_secs(90),
        log_json: dir.join("client.jsonl"),
        summary_json: None,
        transport: patient_transport(),
        client_residency: None,
        baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
        on_replica_ready: Some(Arc::new(move |replica| {
            *slot_for_client.lock().unwrap() = Some(replica);
        })),
        interactive: Some(session.clone()),
        client_authoritative: false,
        admin_script: Vec::new(),
    };
    let client =
        std::thread::spawn(move || run_replication_client_with_manifest(cfg, game::manifest()));

    let replica = loop {
        if let Some(r) = slot.lock().unwrap().clone() {
            break r;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // The ground is solid on the client before the torch goes down.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !matches!(sample(&replica, ground), Some(Sample::Filled(_))) {
        assert!(
            Instant::now() < deadline,
            "the client never received the terrain"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // One torch, aimed straight down: the lamp lands on top of the ground cell.
    session.push_action(torch_request(2_000_000, eye, Vec3::NEG_Y));
    let lamp_cell = GlobalCell::new(x, ground_y + 1, z);

    let lamp = Some(Sample::Filled(game::materials::LAMP));
    let deadline = Instant::now() + Duration::from_secs(30);
    while sample(&replica, lamp_cell) != lamp {
        if Instant::now() >= deadline {
            session.request_stop();
            let summary = client.join().unwrap().expect("client run");
            let server_summary = server.join().unwrap().expect("server run");
            panic!(
                "the torch never appeared at {lamp_cell:?}: {:?}; client sent {}, rejected {} {:?}; \
                 server committed {} transactions, rejected {} actions",
                sample(&replica, lamp_cell),
                summary.actions_sent,
                summary.action_requests_rejected,
                summary.action_reject_reasons,
                server_summary.transactions_committed,
                server_summary.actions_rejected,
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // One voxel: neighbours, the air above and the ground below are untouched.
    let at = |dx: i64, dy: i64, dz: i64| {
        sample(&replica, GlobalCell::new(x + dx, ground_y + 1 + dy, z + dz))
    };
    assert_ne!(at(1, 0, 0), lamp, "a torch is one voxel");
    assert!(
        matches!(at(0, 1, 0), Some(Sample::Empty { .. })),
        "air above the torch"
    );
    assert!(
        matches!(sample(&replica, ground), Some(Sample::Filled(m)) if m != game::materials::LAMP),
        "the ground under the torch is still ground"
    );

    session.request_stop();
    let summary = client.join().unwrap().expect("client run");
    assert!(summary.connected, "{summary:?}");
    assert!(
        summary.actions_sent >= 1,
        "the queued action was never sent: {summary:?}"
    );
    let _ = server.join().unwrap().expect("server run");
}

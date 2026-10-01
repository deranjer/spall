#![cfg(feature = "client")]
//! The hammer, end to end: a real server hosts a generated world, a client
//! session queues the exact `ActionRequest` the window's left click builds
//! (through `InteractiveSession::push_action`, the path the network thread
//! drains), the server validates the aim against its own terrain, and the
//! crater shows up in the client's replica.
//!
//! Run: `cargo test -p sandbox --features client --test worldgen_hammer`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glam::Vec3;
use sandbox::game;
use sandbox::worldgen_scene;
use spall_client::replica::ReplicaWorld;
use spall_client::window::hammer_request;
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
    let dir = std::env::temp_dir().join(format!("spall-worldgen-hammer-{}", std::process::id()));
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
fn a_hammer_swing_digs_a_crater_the_client_replica_sees() {
    let scene = worldgen_scene::generate(
        "showcase",
        1,
        std::env::var("HAMMER_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256),
    )
    .expect("generate");
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
    // Timestamp the server's commit of the cut, to split server time from the
    // network and the client's apply.
    let committed_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let commit_slot = committed_at.clone();
    let commit_handler: spall_server::CommittedEditHandler = Box::new(move |_, _, _, removed| {
        if !removed.is_empty() {
            commit_slot.lock().unwrap().get_or_insert_with(Instant::now);
        }
    });
    let server = std::thread::spawn(move || {
        spall_server::serve_with_game_content_and_commit_handler(
            config,
            game::tool_catalog(),
            game::manifest(),
            game::contact_damage_profiles(),
            None,
            None,
            commit_handler,
        )
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
    // The ground is solid on the client before the swing.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !matches!(sample(&replica, ground), Some(Sample::Filled(_))) {
        assert!(
            Instant::now() < deadline,
            "the client never received the terrain"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // One swing, radius 3 cells.
    let swung = Instant::now();
    session.push_action(hammer_request(2_000_000, eye, Vec3::NEG_Y, 3));

    let deadline = Instant::now() + Duration::from_secs(30);
    while !matches!(sample(&replica, ground), Some(Sample::Empty { .. })) {
        if Instant::now() >= deadline {
            session.request_stop();
            let summary = client.join().unwrap().expect("client run");
            let server_summary = server.join().unwrap().expect("server run");
            panic!(
                "the swing never dug the ground at {ground:?}: {:?}; client sent {}, rejected {} {:?},                  applied {} transactions, repairs {}, hash {}; server committed {} transactions,                  rejected {} actions, ran {} ticks, hash {}",
                sample(&replica, ground),
                summary.actions_sent,
                summary.action_requests_rejected,
                summary.action_reject_reasons,
                summary.transactions_applied,
                summary.repairs_applied,
                summary.final_world_hash,
                server_summary.transactions_committed,
                server_summary.actions_rejected,
                server_summary.ticks_run,
                server_summary.final_world_hash,
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!(
        "click to replica change: {:.0} ms",
        swung.elapsed().as_secs_f64() * 1e3
    );
    if let Some(at) = *committed_at.lock().unwrap() {
        println!(
            "server committed the cut {:.0} ms after the click; the replica changed {:.0} ms after that",
            at.saturating_duration_since(swung).as_secs_f64() * 1e3,
            at.elapsed().as_secs_f64() * 1e3
        );
    }
    // A crater of the brush's size: centre column down to the radius is gone,
    // well below it and well to the side are untouched.
    let at = |dx: i64, dy: i64, dz: i64| {
        sample(&replica, GlobalCell::new(x + dx, ground_y + dy, z + dz))
    };
    assert!(
        matches!(at(0, -2, 0), Some(Sample::Empty { .. })),
        "2 cells down"
    );
    assert!(
        matches!(at(2, 0, 0), Some(Sample::Empty { .. })),
        "2 cells aside"
    );
    // The generator keeps 6 solid cells under the surface (its cave roof).
    assert!(
        matches!(at(0, -5, 0), Some(Sample::Filled(_))),
        "5 cells down is rock"
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

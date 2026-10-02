#![cfg(feature = "client")]
//! Does the dam-gate admin edit actually reach a *client's* replica, not just
//! the server's authoritative terrain? The admin panel already confirms the
//! server-side commit (`AdminStatus` follow-up in `serve.rs`); this is the
//! missing other half — a real QUIC client, exactly like a player's game
//! window, receiving and applying the resulting `TopologyTransaction`.
//!
//! Run: `cargo test -p sandbox --features client --test dam_gate_replication`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sandbox::editor_scene;
use sandbox::game;
use spall_client::replica::ReplicaWorld;
use spall_client::{BaselineScene, ClientNetConfig, run_replication_client_with_manifest};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_protocol::AdminCommand;
use spall_server::{Scene, ServeConfig};
use spall_voxel::Sample;

fn valley() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/valley-showcase"
    ))
}

fn patient_transport() -> TransportConfig {
    let mut transport = TransportConfig::for_tests();
    transport.idle_timeout = Duration::from_secs(120);
    transport
}

fn scratch() -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("spall-dam-gate-replication-{}", std::process::id()));
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

#[test]
fn a_real_client_sees_the_dam_gate_open_after_the_admin_command() {
    let scene = editor_scene::load(valley()).expect("valley project loads");
    let gate_cells = scene.dam_gate_cells().to_vec();
    assert!(!gate_cells.is_empty(), "the scene authors a dam gate");

    let dir = scratch();
    let token = JoinToken::generate().unwrap();
    let mut config = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
    config.custom_world = Some(scene.into_custom_world());
    config.max_ticks = 300;
    config.quiescence_ticks = 0;
    config.min_clients = 1;
    config.max_clients = 1;
    config.paced = true;
    config.admin_commands = true;
    config.startup_timeout = Duration::from_secs(30);
    config.log_json = dir.join("server.jsonl");
    config.fingerprint_out = Some(dir.join("server.fingerprint"));
    config.addr_out = Some(dir.join("server.addr"));
    config.transport = patient_transport();
    let server_thread = std::thread::spawn(move || {
        spall_server::serve_with_game_content(config, game::tool_catalog(), game::manifest())
    });

    let fingerprint = Fingerprint::from_hex(&wait_for(&dir.join("server.fingerprint"))).unwrap();
    let addr: SocketAddr = wait_for(&dir.join("server.addr")).parse().unwrap();

    let replica_slot: Arc<Mutex<Option<Arc<Mutex<ReplicaWorld>>>>> = Arc::new(Mutex::new(None));
    let slot_for_client = replica_slot.clone();
    let client_cfg = ClientNetConfig {
        connect_addr: addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::BridgeCut,
        // Final hashes must describe the same end of the run. Stopping at
        // tick 200 while the server continues to 300 races later topology
        // commits (including detached-body damage). Wait for server shutdown;
        // its max_ticks and our overall_timeout still bound the test.
        run_ticks: 0,
        idle_grace: Duration::from_secs(25),
        overall_timeout: Duration::from_secs(60),
        log_json: dir.join("client.jsonl"),
        summary_json: None,
        transport: patient_transport(),
        client_residency: None,
        on_replica_ready: Some(Arc::new(move |replica| {
            *slot_for_client.lock().unwrap() = Some(replica);
        })),
        interactive: None,
        client_authoritative: false,
        // Give the client a couple dozen ticks to finish baselining before
        // the admin command lands.
        admin_script: vec![(30, AdminCommand::SetDamGate { open: true })],
    };
    let client = std::thread::spawn(move || {
        run_replication_client_with_manifest(client_cfg, game::manifest())
    });

    let client_summary = client.join().unwrap().expect("client run");
    let server_summary = server_thread.join().unwrap().expect("server run");

    assert!(client_summary.connected, "{client_summary:?}");
    // The immediate "queued" ack, plus one delayed commit/reject follow-up
    // per piece the gate's notch was covered by (`covering_spheres`) — a
    // wide, flat notch needs more than one sphere.
    assert!(
        client_summary.admin_statuses.len() >= 2,
        "expected the immediate \"queued\" ack plus at least one delayed \
         commit/reject follow-up: {:?}",
        client_summary.admin_statuses
    );
    for status in &client_summary.admin_statuses {
        assert!(status.accepted, "admin command refused: {status:?}");
    }

    let replica = replica_slot.lock().unwrap().clone().expect("replica ready");
    let replica = replica.lock().unwrap();
    let terrain = replica.terrain_volume().expect("terrain resident");
    let open_on_client = gate_cells
        .iter()
        .filter(|&&cell| matches!(terrain.sample(cell), Ok(Sample::Empty { .. })))
        .count();
    eprintln!(
        "gate cells: {}, open on the client replica: {open_on_client}, \
         server final hash {}, client final hash {}",
        gate_cells.len(),
        server_summary.final_world_hash,
        client_summary.final_world_hash
    );
    assert_eq!(
        client_summary.final_world_hash, server_summary.final_world_hash,
        "client replica must match the server exactly"
    );
    assert!(
        open_on_client > 0,
        "the client's own replica must show the gate open, not just the server's \
         authoritative terrain"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#![cfg(feature = "client")]
//! Walking on a generated world, end to end: a real server hosts the world, a
//! real client predicts and sends forward input, and the player must actually
//! travel (server-authoritative character physics on the generated terrain).
//!
//! Run: `cargo test -p sandbox --features client --test worldgen_walk -- --nocapture`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sandbox::game;
use sandbox::worldgen_scene;
use spall_client::{
    BaselineScene, ClientNetConfig, MovementStep, run_replication_client_with_manifest,
};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_server::{Scene, ServeConfig};

fn patient_transport() -> TransportConfig {
    let mut transport = TransportConfig::for_tests();
    transport.idle_timeout = Duration::from_secs(120);
    transport
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spall-worldgen-walk-{}", std::process::id()));
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
fn a_walking_player_travels_across_the_generated_meadow() {
    // 256 cells keeps the debug-build CI run quick; set WALK_SIZE=1024 and run
    // with --release to walk the full 256 m arena.
    let size = std::env::var("WALK_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let scene = worldgen_scene::generate("showcase", 1, size).expect("generate");
    let spawn = scene.player_spawns()[0];
    let dir = scratch();
    let token = JoinToken::generate().unwrap();
    let mut config = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
    config.custom_world = Some(scene.into_custom_world());
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

    let cfg = ClientNetConfig {
        connect_addr: addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: Vec::new(),
        // Forward (+z in local input space) toward -z, 8 s after the world is up.
        movement_script: vec![MovementStep {
            from_tick: 200,
            to_tick: 700,
            movement: [0.0, 0.0, 1.0],
            view_dir: [0.0, 0.0, -1.0],
            buttons: 0,
        }],
        late_join: true,
        baseline_scene: BaselineScene::BridgeCut,
        run_ticks: 1000,
        idle_grace: Duration::from_secs(40),
        overall_timeout: Duration::from_secs(90),
        log_json: dir.join("client.jsonl"),
        summary_json: None,
        transport: patient_transport(),
        client_residency: None,
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
        admin_script: Vec::new(),
    };
    let summary = run_replication_client_with_manifest(cfg, game::manifest()).expect("client run");
    let _ = server.join().unwrap().expect("server run");

    let movement = summary
        .movement
        .expect("a movement script produces a summary");
    println!("spawn {spawn:?}\n{movement:#?}");
    assert!(
        movement.distance_travelled_m > 5.0,
        "the player barely moved: {movement:?}"
    );
    // Walked, not fell: still near spawn height and grounded most of the time.
    let end = movement.final_authoritative_pos_m;
    assert!((end[1] - spawn[1]).abs() < 8.0, "left the ground: {end:?}");
    assert!(movement.ground_contact_ratio > 0.5, "{movement:?}");
}

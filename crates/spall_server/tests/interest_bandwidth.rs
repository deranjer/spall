//! T20: per-client interest relevance and motion egress budget, end to end
//! over a real QUIC session.
//!
//! Both tests run one authoritative `serve` host with `motion_interest`
//! configured and one headless `spall_client` replica that cuts the bridge
//! column. They assert the T20 contract's integration edges: an interest set
//! wide enough to cover the scene changes nothing; a static anchor placed away
//! from the world drops the detached body's motion **without** dropping any
//! committed geometry; and the run always reports real application- and
//! transport-layer egress.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spall_client::{
    BaselineScene, ClientNetConfig, ScriptedAction, cut_request, run_replication_client,
};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_server::{MotionInterest, Scene, ServeConfig, serve};

fn unique_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "spall-t20-{}-{}-{}",
        tag,
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn wait_for_file(path: &PathBuf, deadline: Duration) -> String {
    let start = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && !s.trim().is_empty()
        {
            return s.trim().to_string();
        }
        assert!(
            start.elapsed() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn base_config(
    dir: &std::path::Path,
    token: JoinToken,
    motion_interest: MotionInterest,
) -> ServeConfig {
    ServeConfig {
        listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        scene: Scene::BridgeCut,
        join_token: token,
        max_ticks: 300,
        quiescence_ticks: 30,
        min_clients: 1,
        max_clients: 4,
        startup_timeout: Duration::from_secs(20),
        paced: true,
        log_json: dir.join("server.jsonl"),
        summary_json: Some(dir.join("server.summary.json")),
        fingerprint_out: Some(dir.join("server.fingerprint")),
        addr_out: Some(dir.join("server.addr")),
        transport: TransportConfig::for_tests(),
        save: None,
        checkpoint_interval_ticks: 0,
        seed: 0,
        catch_up_cap: spall_server::serve::DEFAULT_CATCH_UP_CAP,
        max_join_retries: spall_server::serve::DEFAULT_MAX_JOIN_RETRIES,
        dev_unvalidated_actions: true,
        save_faults: None,
        await_body_settle: false,
        motion_interest: Some(motion_interest),
        residency: None,
        contact_damage: None,
        dormancy: None,
    }
}

fn column_cut_client(
    dir: &std::path::Path,
    token: JoinToken,
    connect: SocketAddr,
    fp: Fingerprint,
) -> ClientNetConfig {
    ClientNetConfig {
        connect_addr: connect,
        server_fingerprint: fp,
        join_token: token,
        script: vec![ScriptedAction {
            at_tick: 4,
            request: cut_request(1, 0, [10, 4, 1], 2),
            target: spall_client::ScriptTarget::Terrain,
        }],
        movement_script: Vec::new(),
        late_join: false,
        baseline_scene: BaselineScene::BridgeCut,
        run_ticks: 0,
        idle_grace: Duration::from_millis(500),
        overall_timeout: Duration::from_secs(25),
        log_json: dir.join("client.jsonl"),
        summary_json: Some(dir.join("client.summary.json")),
        transport: TransportConfig::for_tests(),
        client_residency: None,
        on_replica_ready: None,
        interactive: None,
    }
}

/// An interest set anchored at the scene with radii larger than the whole world
/// must not filter anything: the client converges exactly as it does with
/// `motion_interest` unset, and the run reports real egress.
#[test]
fn a_scene_covering_interest_set_changes_nothing_and_reports_egress() {
    let dir = unique_dir("noop");
    let token = JoinToken::generate().unwrap();
    let cfg = base_config(
        &dir,
        token,
        MotionInterest {
            near_radius_m: 10_000.0,
            far_radius_m: 20_000.0,
            far_interval: 1,
            per_client_budget_bytes: 0,
            static_anchor_m: Some([0.0, 0.0, 0.0]),
        },
    );
    let fp_path = cfg.fingerprint_out.clone().unwrap();
    let addr_path = cfg.addr_out.clone().unwrap();
    let server_thread = std::thread::spawn(move || serve(cfg));

    let fp = Fingerprint::from_hex(&wait_for_file(&fp_path, Duration::from_secs(15))).unwrap();
    let connect: SocketAddr = wait_for_file(&addr_path, Duration::from_secs(15))
        .parse()
        .unwrap();

    let client =
        run_replication_client(column_cut_client(&dir, token, connect, fp)).expect("client");
    let server = server_thread
        .join()
        .expect("server thread")
        .expect("server run");

    assert!(client.connected);
    assert!(
        client.transactions_applied >= 1,
        "client applied the column cut"
    );
    assert_eq!(client.transactions_rejected, 0);
    assert_eq!(
        client.final_world_hash, server.final_world_hash,
        "replica converges with a scene-covering interest set"
    );
    assert_eq!(
        server.motion_snapshots_interest_culled, 0,
        "nothing is outside a 10 km interest radius"
    );
    assert_eq!(server.motion_snapshots_budget_deferred, 0);
    assert!(
        server.motion_snapshots_sent >= 1,
        "the detached body's motion was still sent"
    );
    assert!(
        client.motion_snapshots >= 1,
        "the client received motion for the detached body"
    );
    assert!(
        server.app_egress_bytes > 0 && server.transport_egress_bytes > 0,
        "run reports both application ({}) and transport ({}) egress",
        server.app_egress_bytes,
        server.transport_egress_bytes
    );
    assert!(
        server.transport_egress_bytes >= server.app_egress_bytes,
        "wire bytes ({}) include framing on top of application bytes ({})",
        server.transport_egress_bytes,
        server.app_egress_bytes
    );
}

/// A static interest anchor 1 km from the world: the detached body is outside
/// every client's interest, so its motion is culled entirely — yet every
/// committed topology transaction still reaches the client and the hashes
/// agree. "Snapshots do not consume all capacity needed by controls or joins;
/// no dropped committed geometry events."
#[test]
fn a_far_interest_anchor_culls_body_motion_but_never_geometry() {
    let dir = unique_dir("cull");
    let token = JoinToken::generate().unwrap();
    let cfg = base_config(
        &dir,
        token,
        MotionInterest {
            near_radius_m: 12.0,
            far_radius_m: 24.0,
            far_interval: 4,
            per_client_budget_bytes: 0,
            static_anchor_m: Some([1000.0, 0.0, 0.0]),
        },
    );
    let fp_path = cfg.fingerprint_out.clone().unwrap();
    let addr_path = cfg.addr_out.clone().unwrap();
    let server_thread = std::thread::spawn(move || serve(cfg));

    let fp = Fingerprint::from_hex(&wait_for_file(&fp_path, Duration::from_secs(15))).unwrap();
    let connect: SocketAddr = wait_for_file(&addr_path, Duration::from_secs(15))
        .parse()
        .unwrap();

    let client =
        run_replication_client(column_cut_client(&dir, token, connect, fp)).expect("client");
    let server = server_thread
        .join()
        .expect("server thread")
        .expect("server run");

    assert!(client.connected);
    assert_eq!(server.result, "passed");
    assert!(
        client.transactions_applied >= 1,
        "the committed column cut still reached the client ({} applied)",
        client.transactions_applied
    );
    assert_eq!(
        client.transactions_rejected, 0,
        "no committed geometry was dropped or corrupted by interest filtering"
    );
    assert_eq!(
        client.final_world_hash, server.final_world_hash,
        "replica topology still converges with the body outside interest"
    );
    assert!(
        server.motion_snapshots_interest_culled >= 1,
        "the detached body's motion was culled by the far anchor"
    );
    assert_eq!(
        server.motion_snapshots_sent, 0,
        "nothing is within 24 m of an anchor 1 km away"
    );
    assert!(
        server.transport_egress_bytes > 0,
        "the run still measured transport egress"
    );
}

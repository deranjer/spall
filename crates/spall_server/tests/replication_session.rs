//! T10 phase C: a real QUIC session between the `spall_server` host and the
//! `spall_client` headless replica.
//!
//! One authoritative server and one client run in separate threads, each with
//! its own Tokio runtime, talking over a loopback QUIC connection. The client
//! scripts a cut that detaches the bridge beam; the test asserts the client's
//! replicated topology hash equals the server's authoritative world hash.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spall_client::{ClientNetConfig, ScriptedAction, cut_request, run_replication_client};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_server::{Scene, ServeConfig, serve};

fn unique_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "spall-t10c-{}-{}-{}",
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

#[test]
fn client_replica_matches_the_server_hash_over_real_quic() {
    let dir = unique_dir("session");
    let token = JoinToken::generate().unwrap();
    let fp_path = dir.join("server.fingerprint");
    let addr_path = dir.join("server.addr");

    let server_cfg = ServeConfig {
        listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        scene: Scene::BridgeCut,
        join_token: token,
        max_ticks: 300,
        quiescence_ticks: 30,
        min_clients: 1,
        max_clients: 4,
        startup_timeout: Duration::from_secs(20),
        // Real-time pacing so a scripted client can interact with the tick loop.
        paced: true,
        log_json: dir.join("server.jsonl"),
        summary_json: Some(dir.join("server.summary.json")),
        fingerprint_out: Some(fp_path.clone()),
        addr_out: Some(addr_path.clone()),
        transport: TransportConfig::for_tests(),
        save: None,
        checkpoint_interval_ticks: 0,
        seed: 0,
    };

    let server_thread = std::thread::spawn(move || serve(server_cfg));

    let fp_hex = wait_for_file(&fp_path, Duration::from_secs(15));
    let addr_str = wait_for_file(&addr_path, Duration::from_secs(15));
    let fingerprint = Fingerprint::from_hex(&fp_hex).expect("valid fingerprint hex");
    let connect_addr: SocketAddr = addr_str.parse().expect("valid bound addr");

    let client_cfg = ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        // Cut the column (x 10..=11, z 1..=2, y 2..=7) so the beam detaches.
        script: vec![ScriptedAction {
            at_tick: 4,
            request: cut_request(1, 0, [10, 4, 1], 2),
        }],
        run_ticks: 0,
        idle_grace: Duration::from_millis(500),
        overall_timeout: Duration::from_secs(25),
        log_json: dir.join("client.jsonl"),
        summary_json: Some(dir.join("client.summary.json")),
        transport: TransportConfig::for_tests(),
    };

    let client = run_replication_client(client_cfg).expect("client run");
    let server = server_thread
        .join()
        .expect("server thread")
        .expect("server run");

    assert!(client.connected);
    assert!(
        client.transactions_applied >= 1,
        "client applied at least the column cut ({} applied)",
        client.transactions_applied
    );
    assert_eq!(
        client.transactions_rejected, 0,
        "no transaction was rejected on the client"
    );
    assert_eq!(
        server.transactions_committed, client.transactions_applied,
        "client applied every committed transaction"
    );
    assert_eq!(
        client.final_world_hash, server.final_world_hash,
        "client replica topology hash equals the authoritative world hash"
    );
    assert_eq!(client.total_solid_cells, server.total_solid_cells);
    assert_eq!(client.body_count, server.body_count);
    assert!(
        server.body_count >= 1,
        "the beam detached into at least one body"
    );
    assert!(
        client.motion_snapshots >= 1,
        "the client received motion for the detached body"
    );
}

/// Run the host once with `--save`, cut the column, let it checkpoint on
/// shutdown; then run it again against the same database with a client that
/// does nothing. The second host must recover the post-cut world (the detached
/// body and its geometry), not restart the fresh bridge scene.
///
/// Client-side convergence after a restart needs a late-join baseline (T17);
/// this asserts only the authoritative server recovery.
#[test]
fn server_persists_and_recovers_across_a_restart() {
    let dir = unique_dir("persist");
    let token = JoinToken::generate().unwrap();
    let save = dir.join("world.db");

    let base_cfg = |tag: &str| ServeConfig {
        listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        scene: Scene::BridgeCut,
        join_token: token,
        max_ticks: 200,
        quiescence_ticks: 20,
        min_clients: 1,
        max_clients: 2,
        startup_timeout: Duration::from_secs(20),
        paced: true,
        log_json: dir.join(format!("server-{tag}.jsonl")),
        summary_json: None,
        fingerprint_out: Some(dir.join(format!("fp-{tag}"))),
        addr_out: Some(dir.join(format!("addr-{tag}"))),
        transport: TransportConfig::for_tests(),
        save: Some(save.clone()),
        checkpoint_interval_ticks: 0,
        seed: 9,
    };

    let run_once = |tag: &'static str, script: Vec<ScriptedAction>| {
        let cfg = base_cfg(tag);
        let fp_path = cfg.fingerprint_out.clone().unwrap();
        let addr_path = cfg.addr_out.clone().unwrap();
        let dir = dir.clone();
        let server_thread = std::thread::spawn(move || serve(cfg));

        let fp_hex = wait_for_file(&fp_path, Duration::from_secs(15));
        let addr_str = wait_for_file(&addr_path, Duration::from_secs(15));
        let client_cfg = ClientNetConfig {
            connect_addr: addr_str.parse().unwrap(),
            server_fingerprint: Fingerprint::from_hex(&fp_hex).unwrap(),
            join_token: token,
            script,
            run_ticks: 0,
            idle_grace: Duration::from_millis(500),
            overall_timeout: Duration::from_secs(25),
            log_json: dir.join(format!("client-{tag}.jsonl")),
            summary_json: None,
            transport: TransportConfig::for_tests(),
        };
        let _ = run_replication_client(client_cfg).expect("client run");
        server_thread
            .join()
            .expect("server thread")
            .expect("server run")
    };

    let first = run_once(
        "1",
        vec![ScriptedAction {
            at_tick: 4,
            request: cut_request(1, 0, [10, 4, 1], 2),
        }],
    );
    assert!(first.body_count >= 1, "the beam detached in run 1");
    assert!(
        first.checkpoints_published >= 1,
        "run 1 published a shutdown checkpoint"
    );
    assert!(
        first.journal_records_written >= 1,
        "run 1 journalled the split transaction"
    );

    let second = run_once("2", vec![]);
    assert_eq!(
        second.body_count, first.body_count,
        "run 2 recovered the detached body"
    );
    assert_eq!(
        second.total_solid_cells, first.total_solid_cells,
        "run 2 recovered the same solid-cell total"
    );
    assert_eq!(
        second.final_world_hash, first.final_world_hash,
        "run 2 recovered the post-cut authoritative geometry"
    );
    assert_eq!(
        second.transactions_committed, 0,
        "run 2 did not replay or re-run the cut"
    );
}

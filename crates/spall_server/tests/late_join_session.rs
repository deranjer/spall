//! T17 acceptance: a third client joins **during repeated destruction**, pulls a
//! dependency-complete baseline over a real QUIC bulk transfer, drains the
//! server's catch-up queue, and ends at the exact authoritative topology hash —
//! with no replay of edits from world creation.
//!
//! One authoritative `serve` host, an "early" client that keeps cutting, and a
//! `--late-join` client that connects part-way through. All three must agree on
//! the final canonical world hash, and the server must report at least one
//! completed late join.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spall_client::{ClientNetConfig, ScriptedAction, cut_request, run_replication_client};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_server::serve::{DEFAULT_CATCH_UP_CAP, DEFAULT_MAX_JOIN_RETRIES};
use spall_server::{Scene, ServeConfig, serve};

fn unique_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "spall-t17-{}-{}-{}",
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
fn a_third_client_late_joins_during_destruction_and_matches_the_server_hash() {
    let dir = unique_dir("late-join");
    let token = JoinToken::generate().unwrap();
    let fp_path = dir.join("server.fingerprint");
    let addr_path = dir.join("server.addr");

    let server_cfg = ServeConfig {
        listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        scene: Scene::BridgeCut,
        join_token: token,
        max_ticks: 1_500,
        quiescence_ticks: 60,
        min_clients: 1,
        max_clients: 3,
        startup_timeout: Duration::from_secs(20),
        paced: true,
        log_json: dir.join("server.jsonl"),
        summary_json: Some(dir.join("server.summary.json")),
        fingerprint_out: Some(fp_path.clone()),
        addr_out: Some(addr_path.clone()),
        transport: TransportConfig::for_tests(),
        save: None,
        checkpoint_interval_ticks: 0,
        seed: 0,
        catch_up_cap: DEFAULT_CATCH_UP_CAP,
        max_join_retries: DEFAULT_MAX_JOIN_RETRIES,
        // Scripted fixture cuts hit arbitrary cells; use the ENG-47
        // dev-scenario path so this late-join plumbing test still runs.
        dev_unvalidated_actions: true,
    };
    let server_thread = std::thread::spawn(move || serve(server_cfg));

    let fp_hex = wait_for_file(&fp_path, Duration::from_secs(15));
    let addr_str = wait_for_file(&addr_path, Duration::from_secs(15));
    let fingerprint = Fingerprint::from_hex(&fp_hex).expect("valid fingerprint hex");
    let connect_addr: SocketAddr = addr_str.parse().expect("valid bound addr");

    // The early client: detach the beam, then keep excavating the floor so the
    // late joiner really does arrive mid-destruction.
    let early_cfg = ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: vec![
            ScriptedAction {
                at_tick: 4,
                request: cut_request(1, 0, [10, 4, 1], 2),
            },
            ScriptedAction {
                at_tick: 20,
                request: cut_request(2, 1, [3, 1, 1], 1),
            },
            ScriptedAction {
                at_tick: 40,
                request: cut_request(3, 2, [5, 1, 1], 1),
            },
            ScriptedAction {
                at_tick: 60,
                request: cut_request(4, 3, [7, 1, 1], 1),
            },
            ScriptedAction {
                at_tick: 80,
                request: cut_request(5, 4, [9, 1, 1], 1),
            },
        ],
        late_join: false,
        run_ticks: 0,
        idle_grace: Duration::from_millis(800),
        overall_timeout: Duration::from_secs(35),
        log_json: dir.join("early.jsonl"),
        summary_json: Some(dir.join("early.summary.json")),
        transport: TransportConfig::for_tests(),
    };
    let early_thread = std::thread::spawn(move || run_replication_client(early_cfg));

    // Give the early client time to connect and land its first few cuts, then
    // bring up the late joiner.
    std::thread::sleep(Duration::from_millis(900));

    let late_cfg = ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        // One cut after the join too, so it also proves it can act post-barrier.
        script: vec![ScriptedAction {
            at_tick: 70,
            request: cut_request(1_000, 0, [12, 1, 1], 1),
        }],
        late_join: true,
        run_ticks: 0,
        idle_grace: Duration::from_millis(800),
        overall_timeout: Duration::from_secs(35),
        log_json: dir.join("late.jsonl"),
        summary_json: Some(dir.join("late.summary.json")),
        transport: TransportConfig::for_tests(),
    };
    let late = run_replication_client(late_cfg).expect("late-join client run");
    let early = early_thread
        .join()
        .expect("early client thread")
        .expect("early client run");
    let server = server_thread
        .join()
        .expect("server thread")
        .expect("server run");

    assert!(late.connected && late.late_join);
    assert!(
        late.baseline_bricks > 0,
        "the late joiner installed a non-empty baseline"
    );
    assert_eq!(
        server.late_joins_completed, 1,
        "the server completed exactly one late-join baseline"
    );
    assert_eq!(
        late.transactions_rejected, 0,
        "no transaction rejected on the late joiner"
    );
    assert!(
        late.transactions_applied >= 1,
        "the late joiner drained at least one catch-up transaction ({} applied)",
        late.transactions_applied
    );

    // The whole point: identical authoritative topology on all three.
    assert_eq!(
        late.final_world_hash, server.final_world_hash,
        "late-join replica hash == authoritative world hash"
    );
    assert_eq!(
        early.final_world_hash, server.final_world_hash,
        "the already-connected client also matches (it never paused)"
    );
    assert_eq!(late.total_solid_cells, server.total_solid_cells);
    assert_eq!(late.body_count, server.body_count);
    assert!(server.body_count >= 1, "the beam detached");
}

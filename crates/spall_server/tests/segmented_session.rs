//! Segmented late-join baselines over a real QUIC session: capture on the server, paced bulk
//! transfer, client validation and atomic install, and catch-up of edits committed while the
//! transfer was in flight. A low segment budget and a rate limit make the transfer span many
//! segments and several seconds on a small world.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spall_client::{
    BaselineScene, ClientNetConfig, ClientSummary, ScriptTarget, ScriptedAction, cut_request,
    run_replication_client,
};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_protocol::segment::DENSE_BRICK_DECODED_COST;
use spall_server::serve::{
    DEFAULT_CATCH_UP_CAP, DEFAULT_MAX_JOIN_RETRIES, default_capture_workers,
};
use spall_server::{Scene, ServeConfig, serve};

const SEGMENT_CAP: usize = 2 * DENSE_BRICK_DECODED_COST;

fn unique_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "spall-seg-{tag}-{}-{}",
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

#[allow(clippy::too_many_arguments)]
fn client(
    addr: SocketAddr,
    fp: Fingerprint,
    token: JoinToken,
    dir: &std::path::Path,
    name: &str,
    late: bool,
    script: Vec<ScriptedAction>,
    staging_budget: Option<u64>,
) -> ClientNetConfig {
    ClientNetConfig {
        connect_addr: addr,
        server_fingerprint: fp,
        join_token: token,
        script,
        movement_script: Vec::new(),
        late_join: late,
        baseline_scene: BaselineScene::BulkSplit,
        run_ticks: 0,
        idle_grace: Duration::from_millis(800),
        overall_timeout: Duration::from_secs(40),
        log_json: dir.join(format!("{name}.jsonl")),
        summary_json: Some(dir.join(format!("{name}.summary.json"))),
        transport: TransportConfig::for_tests(),
        client_residency: None,
        baseline_staging_budget_bytes: staging_budget,
        on_replica_ready: None,
        interactive: None,
    }
}

struct Outcome {
    early: ClientSummary,
    late: Result<ClientSummary, String>,
    server: spall_server::serve::ServeSummary,
}

/// One server, an early client that cuts the column and then keeps excavating (edits land while
/// the late joiner's baseline is in flight), and a late joiner.
fn run(tag: &str, staging_budget: Option<u64>, rate: Option<u64>) -> Outcome {
    let dir = unique_dir(tag);
    let token = JoinToken::generate().unwrap();
    let fp_path = dir.join("server.fingerprint");
    let addr_path = dir.join("server.addr");
    let server_cfg = ServeConfig {
        listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        scene: Scene::BulkSplit,
        join_token: token,
        max_ticks: 1_800,
        quiescence_ticks: 90,
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
        capture_workers: default_capture_workers(),
        dev_unvalidated_actions: true,
        save_faults: None,
        await_body_settle: false,
        motion_interest: None,
        residency: None,
        residency_disk_path: None,
        terrain_brick_colliders: false,
        baseline_segment_bytes: Some(SEGMENT_CAP),
        contact_damage: None,
        dormancy: None,
        debris_lifetime: None,
        expendable_debris: Vec::new(),
        timing_window: None,
        baseline_rate_limit_bytes_per_sec: rate,
        wake_audit: false,
    };
    let server_thread = std::thread::spawn(move || serve(server_cfg));
    let fp = Fingerprint::from_hex(&wait_for_file(&fp_path, Duration::from_secs(15))).unwrap();
    let addr: SocketAddr = wait_for_file(&addr_path, Duration::from_secs(15))
        .parse()
        .unwrap();

    let cut = |at_tick, id, cell, r| ScriptedAction {
        at_tick,
        request: cut_request(id, id - 1, cell, r),
        target: ScriptTarget::Terrain,
    };
    // Detach the block (column x 28..=35, y 2..=13), then keep cutting the floor.
    let mut script = vec![cut(4, 1, [31, 7, 31], 6)];
    for (i, x) in [4i64, 10, 16, 40, 46, 52].into_iter().enumerate() {
        script.push(cut(40 + 25 * i as u64, 2 + i as u64, [x, 1, 10], 1));
    }
    let early_cfg = client(addr, fp, token, &dir, "early", false, script, None);
    let early_thread = std::thread::spawn(move || run_replication_client(early_cfg));
    std::thread::sleep(Duration::from_millis(900));
    let late_cfg = client(
        addr,
        fp,
        token,
        &dir,
        "late",
        true,
        Vec::new(),
        staging_budget,
    );
    let late = run_replication_client(late_cfg).map_err(|e| e.to_string());
    let early = early_thread.join().unwrap().expect("early client run");
    let server = server_thread.join().unwrap().expect("server run");
    Outcome {
        early,
        late,
        server,
    }
}

#[test]
fn a_slow_late_joiner_receives_a_segmented_baseline_while_edits_land_and_converges() {
    // ~120 KB/s: the transfer takes seconds, during which the early client's cuts commit and queue
    // for catch-up behind the baseline.
    let o = run("converge", None, Some(120_000));
    let late = o.late.expect("the late joiner joins");
    assert_eq!(
        late.segmented_transfers, 1,
        "the baseline arrived segmented"
    );
    assert!(late.segments_received >= 3, "{}", late.segments_received);
    assert!(
        late.max_segment_decoded_bytes as usize <= SEGMENT_CAP,
        "the client never decoded more than one segment budget at once: {}",
        late.max_segment_decoded_bytes
    );
    assert!(late.staged_decoded_bytes > 0);
    assert!(late.baseline_bricks > 0);
    assert_eq!(o.server.late_joins_completed, 1);
    assert_eq!(late.transactions_rejected, 0);
    assert!(
        late.transactions_applied >= 1,
        "edits committed during the transfer arrived by catch-up ({})",
        late.transactions_applied
    );
    assert_eq!(
        late.final_world_hash, o.server.final_world_hash,
        "segmented install + catch-up reaches the authoritative hash"
    );
    assert_eq!(o.early.final_world_hash, o.server.final_world_hash);
    assert_eq!(late.total_solid_cells, o.server.total_solid_cells);
    assert_eq!(late.body_count, o.server.body_count);
    assert!(o.server.body_count >= 1, "the block detached");
}

#[test]
fn a_client_whose_staging_budget_is_too_small_fails_the_join_explicitly_and_the_server_carries_on()
{
    let o = run("budget", Some(10_000), None);
    let err = o.late.expect_err("the late join must fail");
    assert!(err.contains("budget"), "{err}");
    assert_eq!(o.server.late_joins_completed, 0);
    // The other client is unaffected and converged.
    assert_eq!(o.early.final_world_hash, o.server.final_world_hash);
}

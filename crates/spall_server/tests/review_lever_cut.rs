//! The reviewer's "kick off the test" path end to end: a real server hosts the
//! `review-lever` scene, an interactive-session client (no window) late-joins, sends the
//! review-cut request the window's `1` key builds, and the server — with aim validation
//! ON — commits it and the beam tips on the client's replica.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spall_client::interactive::{InteractiveSession, ReviewCut, review_cut_request};
use spall_client::{BaselineScene, ClientNetConfig, run_replication_client};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_server::{Scene, ServeConfig, serve};

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
fn the_review_cut_key_is_accepted_and_the_beam_tips() {
    let dir = std::env::temp_dir().join(format!("spall-review-lever-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let token = JoinToken::generate().unwrap();
    let fp_path = dir.join("fp");
    let addr_path = dir.join("addr");
    let server_cfg = ServeConfig {
        listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        scene: Scene::ReviewLever,
        join_token: token,
        max_ticks: 1200,
        quiescence_ticks: 0,
        min_clients: 1,
        max_clients: 2,
        startup_timeout: Duration::from_secs(20),
        paced: true,
        log_json: dir.join("server.jsonl"),
        summary_json: None,
        fingerprint_out: Some(fp_path.clone()),
        addr_out: Some(addr_path.clone()),
        transport: TransportConfig::for_tests(),
        save: None,
        checkpoint_interval_ticks: 0,
        seed: 0,
        catch_up_cap: spall_server::serve::DEFAULT_CATCH_UP_CAP,
        max_join_retries: spall_server::serve::DEFAULT_MAX_JOIN_RETRIES,
        capture_workers: spall_server::serve::default_capture_workers(),
        // Aim validation stays ON: this is the path a real player's request takes.
        dev_unvalidated_actions: false,
        save_faults: None,
        await_body_settle: false,
        motion_interest: None,
        residency: None,
        residency_disk_path: None,
        contact_damage: None,
        dormancy: None,
        timing_window: None,
        baseline_rate_limit_bytes_per_sec: None,
        wake_audit: false,
    };
    let server = std::thread::spawn(move || serve(server_cfg));

    let fp = Fingerprint::from_hex(&wait_for_file(&fp_path, Duration::from_secs(15))).unwrap();
    let addr: SocketAddr = wait_for_file(&addr_path, Duration::from_secs(15))
        .parse()
        .unwrap();

    let session = InteractiveSession::new();
    let client_cfg = ClientNetConfig {
        connect_addr: addr,
        server_fingerprint: fp,
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::default(),
        run_ticks: 0,
        idle_grace: Duration::from_millis(500),
        overall_timeout: Duration::from_secs(60),
        log_json: dir.join("client.jsonl"),
        summary_json: None,
        transport: TransportConfig::for_tests(),
        client_residency: None,
        on_replica_ready: None,
        interactive: Some(session.clone()),
    };
    let client = std::thread::spawn(move || run_replication_client(client_cfg));

    // Wait for the replica to hold both bodies with motion, then send the review cut
    // exactly as the `1` key does.
    let start = Instant::now();
    let (lever, tilt_of) = loop {
        assert!(
            start.elapsed() < Duration::from_secs(40),
            "replica never got the bodies"
        );
        if let Some(replica) = session.replica.get() {
            let g = replica.lock().unwrap();
            let ids: Vec<_> = g.body_ids().collect();
            if ids.len() == 2 && ids.iter().all(|e| g.latest_motion_tick(*e).is_some()) {
                let counts: Vec<_> = ids
                    .iter()
                    .map(|e| (*e, g.body_solid_cells(*e).unwrap_or(0)))
                    .collect();
                let e = spall_client::interactive::pick_review_lever(&counts).unwrap();
                let tick = g.latest_motion_tick(e).unwrap() as f64;
                let pose = g.interpolated_pose(e, tick).unwrap();
                break (
                    (e, pose.translation_m, pose.rotation.to_unit().unwrap()),
                    pose.rotation.to_unit().unwrap(),
                );
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = tilt_of;
    // Let the beam settle to sleep first (as a person would), then cut.
    std::thread::sleep(Duration::from_secs(3));
    let (entity, t, q) = lever;
    let request = review_cut_request(entity, t, q, ReviewCut::LeftEnd, 6, 1).unwrap();
    {
        let g = session.replica.get().unwrap().lock().unwrap();
        for e in g.body_ids() {
            let tick = g.latest_motion_tick(e).unwrap() as f64;
            eprintln!(
                "body {} cells {:?} pose {:?}",
                e.get(),
                g.body_solid_cells(e),
                g.interpolated_pose(e, tick).map(|p| p.translation_m)
            );
        }
    }
    eprintln!("picked {} request {:?}", entity.get(), request);
    session.push_action(request);

    // The beam must tip on the client's replica.
    let start = Instant::now();
    let mut max_tilt = 0.0f32;
    while start.elapsed() < Duration::from_secs(12) {
        if let Some(replica) = session.replica.get() {
            let g = replica.lock().unwrap();
            let tick = g.latest_motion_tick(entity).unwrap() as f64;
            let qn = g
                .interpolated_pose(entity, tick)
                .unwrap()
                .rotation
                .to_unit()
                .unwrap();
            // Rotation about z: 2*asin(qz).
            max_tilt = max_tilt.max(2.0 * qn[2].abs().min(1.0).asin());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    session.request_stop();
    let client = client.join().unwrap().expect("client run");
    let server = server.join().unwrap().expect("server run");
    assert_eq!(
        server.transactions_committed, 1,
        "the review cut committed (rejected {}; reasons {:?})",
        server.actions_rejected, client.action_reject_reasons
    );
    assert_eq!(server.actions_rejected, 0);
    assert!(client.connected);
    assert!(
        max_tilt > 0.05,
        "the beam tipped (peak tilt {max_tilt} rad)"
    );
}

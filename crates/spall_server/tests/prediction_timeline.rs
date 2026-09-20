//! Headless prediction-timeline reproduction (ENG-69 follow-up): the real paced
//! server and the real scripted mover over a loopback QUIC session, walking a
//! flat lane (no moving bodies) through walk / release / idle / reverse cycles.
//!
//! Two failures a hands-on run showed:
//! * the prediction lead (`records_replayed`) grew ~1 tick per 0.85 s for the
//!   whole session because the server measured ~58.8 Hz against a 60 Hz client;
//! * releasing the keys produced backward / sideways corrections.
//!
//! The run length is `SPALL_REPRO_SECONDS` (default 25 s; use 60-120 for the
//! full soak). The measured trace is written to the temp dir and its path is
//! printed, so a failing run can be inspected.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spall_client::net::{MovementStep, MovementTrace, ReconcileTraceRow};
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

/// Server-tick script: walk forward 1.2 s, release + idle 2 s, walk back 1.2 s,
/// release + idle 2 s, repeated to `total_ticks`.
fn cycle_script(total_ticks: u64) -> Vec<MovementStep> {
    const WALK: u64 = 72;
    const IDLE: u64 = 120;
    let step = |from, to, forward: f32| MovementStep {
        from_tick: from,
        to_tick: to,
        movement: [0.0, 0.0, forward],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    };
    let mut steps = Vec::new();
    let mut t = 60;
    let mut dir = 1.0;
    while t + WALK < total_ticks {
        steps.push(step(t, t + WALK, dir));
        t += WALK + IDLE;
        dir = -dir;
    }
    steps
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Runs one paced server and one scripted client for `seconds`; returns the
/// client's summary and the directory holding the logs.
fn run_session(
    scene: Scene,
    seconds: u64,
    script: Vec<MovementStep>,
    late_join: bool,
) -> (spall_client::ClientSummary, PathBuf) {
    let total_ticks = seconds * 60;
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("spall-timeline-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let token = JoinToken::generate().unwrap();
    let fp_path = dir.join("server.fingerprint");
    let addr_path = dir.join("server.addr");

    let server_cfg = ServeConfig {
        listen: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        scene,
        join_token: token,
        max_ticks: total_ticks + 600,
        quiescence_ticks: 0,
        min_clients: 1,
        max_clients: 1,
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
        catch_up_cap: spall_server::serve::DEFAULT_CATCH_UP_CAP,
        max_join_retries: spall_server::serve::DEFAULT_MAX_JOIN_RETRIES,
        capture_workers: spall_server::serve::default_capture_workers(),
        dev_unvalidated_actions: false,
        save_faults: None,
        await_body_settle: false,
        motion_interest: None,
        residency: None,
        residency_disk_path: None,
        contact_damage: None,
        dormancy: None,
    };
    let server_thread = std::thread::spawn(move || serve(server_cfg));
    let fp_hex = wait_for_file(&fp_path, Duration::from_secs(20));
    let addr_str = wait_for_file(&addr_path, Duration::from_secs(20));

    let client_cfg = ClientNetConfig {
        connect_addr: addr_str.parse().unwrap(),
        server_fingerprint: Fingerprint::from_hex(&fp_hex).unwrap(),
        join_token: token,
        script: Vec::new(),
        movement_script: script,
        late_join,
        baseline_scene: BaselineScene::Walk,
        run_ticks: total_ticks,
        idle_grace: Duration::from_millis(500),
        overall_timeout: Duration::from_secs(seconds + 40),
        log_json: dir.join("client.jsonl"),
        summary_json: Some(dir.join("client.summary.json")),
        transport: TransportConfig::for_tests(),
        client_residency: None,
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
    };
    let summary = run_replication_client(client_cfg).expect("client run");
    let _ = server_thread.join();
    (summary, dir)
}

#[test]
fn prediction_lead_stays_bounded_and_release_does_not_reverse() {
    let seconds: u64 = std::env::var("SPALL_REPRO_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);
    let total_ticks = seconds * 60;
    let wall = Instant::now();
    let (summary, dir) = run_session(Scene::Walk, seconds, cycle_script(total_ticks), false);
    let wall_s = wall.elapsed().as_secs_f64();
    let trace: MovementTrace = summary
        .movement_trace
        .clone()
        .expect("scripted run records a trace");
    let trace_path = dir.join("movement_trace.json");
    std::fs::write(&trace_path, serde_json::to_string(&trace).unwrap()).unwrap();
    assert!(
        trace.reconciles.len() > (seconds as usize) * 10,
        "expected ~20 reconciles/s, got {} over {seconds}s ({})",
        trace.reconciles.len(),
        trace_path.display()
    );

    // --- server rate ------------------------------------------------------
    let first = trace.reconciles.first().unwrap();
    let last = trace.reconciles.last().unwrap();
    let server_hz = (last.server_tick - first.server_tick) as f64
        / ((last.wall_ms - first.wall_ms) as f64 / 1e3);

    // --- lead ---------------------------------------------------------------
    let settled: Vec<&ReconcileTraceRow> = trace
        .reconciles
        .iter()
        .filter(|r| r.wall_ms > 4_000)
        .collect();
    let mut leads: Vec<f64> = settled.iter().map(|r| r.lead_ticks as f64).collect();
    leads.sort_by(f64::total_cmp);
    let half = settled.len() / 2;
    let mean = |rows: &[&ReconcileTraceRow]| {
        rows.iter().map(|r| r.lead_ticks as f64).sum::<f64>() / rows.len().max(1) as f64
    };
    let (early, late) = (mean(&settled[..half]), mean(&settled[half..]));
    let target = settled.last().map_or(0.0, |r| r.target_lead_ticks);

    // --- release behaviour ------------------------------------------------
    // Two separate questions. (1) Edge snap: how far the reconcile moves the
    // predicted position horizontally at a key edge (the server applies an
    // arriving input on whichever of its ticks comes next, the client applied
    // it on its own tick, so an edge is off by whole ticks: 7.5 cm each at walk
    // speed). (2) Sustained drift: once the player has been stationary for
    // `SETTLE_MS`, horizontal corrections must be ~zero and must never pull the
    // player back along the direction it walked.
    const SETTLE_MS: u64 = 400;
    let horizontal = |r: &ReconcileTraceRow| {
        let (b, a) = (r.predicted_before_pos_m, r.predicted_after_pos_m);
        (a[0] - b[0]).hypot(a[2] - b[2])
    };
    let mut worst_edge_snap_m = 0.0f64;
    let mut worst_settled_horizontal_m = 0.0f64;
    let mut settled_backslide_m = 0.0f64;
    let mut release_windows = 0;
    for change in trace.input_changes.iter().skip(1) {
        for r in trace.reconciles.iter().filter(|r| {
            r.wall_ms >= change.wall_ms.saturating_sub(100)
                && r.wall_ms < change.wall_ms + SETTLE_MS
        }) {
            worst_edge_snap_m = worst_edge_snap_m.max(horizontal(r));
        }
    }
    for pair in trace.input_changes.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.movement[2].abs() < 1e-6 || b.movement[2].abs() > 1e-6 {
            continue; // want walking -> released
        }
        release_windows += 1;
        let walked = f64::from(a.movement[2].signum());
        for r in trace
            .reconciles
            .iter()
            .filter(|r| r.wall_ms >= b.wall_ms + SETTLE_MS && r.wall_ms < b.wall_ms + 1_800)
        {
            worst_settled_horizontal_m = worst_settled_horizontal_m.max(horizontal(r));
            let along = (r.predicted_after_pos_m[0] - r.predicted_before_pos_m[0]) * walked;
            settled_backslide_m = settled_backslide_m.min(along);
        }
    }

    println!(
        "REPRO seconds={seconds} wall_s={wall_s:.1} server_hz={server_hz:.2} \
         reconciles={} lead_p50={:.1} lead_p95={:.1} lead_max={:.0} early_mean={early:.2} \
         late_mean={late:.2} target={target:.1} release_windows={release_windows} \
         worst_edge_snap_m={worst_edge_snap_m:.3} worst_settled_horizontal_m={worst_settled_horizontal_m:.4} \
         settled_backslide_m={settled_backslide_m:.4} \
         corrections={} max_correction_m={:.3} trace={}",
        trace.reconciles.len(),
        percentile(&leads, 0.5),
        percentile(&leads, 0.95),
        leads.last().copied().unwrap_or(0.0),
        summary.movement.as_ref().map_or(0, |m| m.corrections),
        summary
            .movement
            .as_ref()
            .map_or(0.0, |m| m.max_correction_m),
        trace_path.display()
    );

    assert!(release_windows >= 2, "script must contain release events");
    assert!(
        server_hz > 59.0 && server_hz < 61.0,
        "server ran at {server_hz:.2} Hz, expected ~60"
    );
    assert!(
        leads.last().copied().unwrap_or(0.0) <= target + 12.0,
        "lead exceeded target+12: {:?} (target {target})",
        leads.last()
    );
    assert!(
        late <= early + 2.0,
        "lead grows with session length: early mean {early:.2}, late mean {late:.2}"
    );
    // An edge may land a couple of ticks (7.5 cm each) from the server's; more
    // than that is a real divergence.
    assert!(
        worst_edge_snap_m <= 0.16,
        "a key edge snapped the predicted position {worst_edge_snap_m:.3} m"
    );
    assert!(
        worst_settled_horizontal_m < 0.01,
        "horizontal corrections continue while stationary: {worst_settled_horizontal_m:.4} m"
    );
    assert!(
        settled_backslide_m > -0.005,
        "stationary player pulled backwards {settled_backslide_m:.4} m"
    );
}

/// Server-authoritative playground with real falling / pushed bodies: the
/// player leaves the plaza spawn, walks into the showcase drop zone (boxes land
/// every 5 s), pushes through it, releases, backs out, and repeats. Not a pass
/// / fail gate on feel -- it prints the correction statistics and asserts only
/// that the prediction lead stays bounded and no correction is a teleport.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "216-body playground needs a release build to hold 60 Hz on the client mover"
)]
fn playground_push_run_reports_lead_and_correction_sizes() {
    let seconds: u64 = std::env::var("SPALL_PLAYGROUND_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let step = |from, to, forward: f32| MovementStep {
        from_tick: from,
        to_tick: to,
        movement: [0.0, 0.0, forward],
        view_dir: [-1.0, 0.0, 0.0], // walk toward -x, the drop zone
        buttons: 0,
    };
    let mut script = Vec::new();
    let mut t = 120;
    while t + 420 < seconds * 60 {
        script.push(step(t, t + 66, 1.0)); // in to the boxes (1.1 s, ~5 m)
        script.push(step(t + 66 + 90, t + 66 + 90 + 66, -1.0)); // back out
        t += 66 + 90 + 66 + 120;
    }
    let (summary, dir) = run_session(Scene::Playground, seconds, script, true);
    let trace = summary.movement_trace.expect("trace");
    std::fs::write(
        dir.join("movement_trace.json"),
        serde_json::to_string(&trace).unwrap(),
    )
    .unwrap();
    let rows: Vec<&ReconcileTraceRow> = trace
        .reconciles
        .iter()
        .filter(|r| r.wall_ms > 4_000)
        .collect();
    assert!(rows.len() > 100, "only {} reconciles", rows.len());
    let mut leads: Vec<f64> = rows.iter().map(|r| r.lead_ticks as f64).collect();
    leads.sort_by(f64::total_cmp);
    let mut shifts: Vec<f64> = rows.iter().map(|r| r.shift_m).collect();
    shifts.sort_by(f64::total_cmp);
    let big = shifts.iter().filter(|s| **s > 0.01).count();
    println!(
        "PLAYGROUND seconds={seconds} reconciles={} lead_p50={:.0} lead_p95={:.0} lead_max={:.0} \
         shift_p50={:.4} shift_p95={:.3} shift_max={:.3} shifts_over_1cm={big} \
         unmatched_reconciles={} trace={}",
        rows.len(),
        percentile(&leads, 0.5),
        percentile(&leads, 0.95),
        leads.last().copied().unwrap_or(0.0),
        percentile(&shifts, 0.5),
        percentile(&shifts, 0.95),
        shifts.last().copied().unwrap_or(0.0),
        summary.movement.as_ref().map_or(0, |_| 0),
        dir.join("movement_trace.json").display()
    );
    let target = rows.last().map_or(0.0, |r| r.target_lead_ticks);
    assert!(
        leads.last().copied().unwrap_or(0.0) <= target + 12.0,
        "lead unbounded"
    );
    assert!(
        shifts.last().copied().unwrap_or(0.0) < 2.0,
        "a correction teleported the player"
    );
}

/// One dynamic box in the lane (`Scene::PushTest`): the player walks +x into it
/// and pushes it ahead for 2.5 s, releases, backs out, and repeats. This is the
/// deterministic version of "one-box pushing": the box really is contacted and
/// moved by the server's character push / carry code, so the correction sizes
/// here are what prediction against a pushed body costs.
#[test]
fn one_box_push_run_reports_correction_sizes_while_pushing() {
    let seconds: u64 = std::env::var("SPALL_PUSH_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let step = |from, to, forward: f32| MovementStep {
        from_tick: from,
        to_tick: to,
        movement: [0.0, 0.0, forward],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    };
    let mut script = Vec::new();
    let mut t = 90;
    while t + 600 < seconds * 60 {
        script.push(step(t, t + 120, 1.0)); // 2 s forward: reaches and pushes the box
        script.push(step(t + 120 + 120, t + 120 + 120 + 120, -1.0)); // 2 s back
        t += 120 + 120 + 120 + 120;
    }
    let (summary, dir) = run_session(Scene::PushTest, seconds, script, false);
    let trace = summary.movement_trace.expect("trace");
    std::fs::write(
        dir.join("movement_trace.json"),
        serde_json::to_string(&trace).unwrap(),
    )
    .unwrap();
    let rows: Vec<&ReconcileTraceRow> = trace
        .reconciles
        .iter()
        .filter(|r| r.wall_ms > 3_000)
        .collect();
    assert!(rows.len() > 100, "only {} reconciles", rows.len());
    let mut leads: Vec<f64> = rows.iter().map(|r| r.lead_ticks as f64).collect();
    leads.sort_by(f64::total_cmp);
    // Horizontal shift while a movement key is held (pushing) vs at rest.
    let horiz = |r: &ReconcileTraceRow| {
        let (b, a) = (r.predicted_before_pos_m, r.predicted_after_pos_m);
        (a[0] - b[0]).hypot(a[2] - b[2])
    };
    let mut pushing: Vec<f64> = rows
        .iter()
        .filter(|r| r.current_movement[2] != 0.0)
        .map(|r| horiz(r))
        .collect();
    pushing.sort_by(f64::total_cmp);
    let mut resting: Vec<f64> = rows
        .iter()
        .filter(|r| r.current_movement[2] == 0.0 && r.authoritative_vel_m_s[0].abs() < 0.01)
        .map(|r| horiz(r))
        .collect();
    resting.sort_by(f64::total_cmp);
    let min_x = rows
        .iter()
        .map(|r| r.authoritative_pos_m[0])
        .fold(f64::MAX, f64::min);
    let max_x = rows
        .iter()
        .map(|r| r.authoritative_pos_m[0])
        .fold(f64::MIN, f64::max);
    println!(
        "PUSH seconds={seconds} reconciles={} lead_p50={:.0} lead_max={:.0} auth_x=[{min_x:.2},{max_x:.2}] \
         held_key_horizontal_shift_p50={:.4} p95={:.3} max={:.3} (n={}) \
         resting_horizontal_shift_p95={:.4} max={:.3} (n={}) trace={}",
        rows.len(),
        percentile(&leads, 0.5),
        leads.last().copied().unwrap_or(0.0),
        percentile(&pushing, 0.5),
        percentile(&pushing, 0.95),
        pushing.last().copied().unwrap_or(0.0),
        pushing.len(),
        percentile(&resting, 0.95),
        resting.last().copied().unwrap_or(0.0),
        resting.len(),
        dir.join("movement_trace.json").display()
    );
    let target = rows.last().map_or(0.0, |r| r.target_lead_ticks);
    assert!(
        leads.last().copied().unwrap_or(0.0) <= target + 12.0,
        "lead unbounded"
    );
    assert!(
        pushing.last().copied().unwrap_or(0.0) < 2.0,
        "a correction teleported the player"
    );
}

/// `(label, ticks, movement)` phases of the controlled single-box scenario. The
/// player starts 3 m behind a 0.75 m box: pushes it ahead continuously,
/// releases, backs out, sidesteps past it and pushes it sideways.
const BOX_PHASES: &[(&str, u64, [f32; 3])] = &[
    ("push_forward", 100, [0.0, 0.0, 1.0]),
    ("release", 90, [0.0, 0.0, 0.0]),
    ("sidestep", 15, [-1.0, 0.0, 0.0]),
    ("pass_beside", 40, [0.0, 0.0, 1.0]),
    ("push_sideways", 60, [1.0, 0.0, 0.0]),
    ("release_2", 90, [0.0, 0.0, 0.0]),
];

fn stats(mut v: Vec<f64>) -> String {
    v.sort_by(f64::total_cmp);
    format!(
        "n={} p50={:.4} p95={:.4} max={:.4}",
        v.len(),
        percentile(&v, 0.5),
        percentile(&v, 0.95),
        v.last().copied().unwrap_or(0.0)
    )
}

/// Controlled one-box reproduction that separates *visible box stepping* from
/// *player correction jumps*. For each phase it prints, from synchronized
/// traces: reconcile shifts (player corrections), the box's snapshot-arrival
/// extrapolation error (what the collision mirror jumps by), the drawn box's
/// per-tick step, repeated drawn poses while the box is moving, and grounded
/// flips. Full per-tick rows are in the written trace for inspection.
#[test]
fn one_box_phases_separate_box_stepping_from_player_corrections() {
    let mut script = Vec::new();
    let mut t = 90;
    for (_, ticks, movement) in BOX_PHASES {
        if *movement != [0.0; 3] {
            script.push(MovementStep {
                from_tick: t,
                to_tick: t + ticks,
                movement: *movement,
                view_dir: [1.0, 0.0, 0.0],
                buttons: 0,
            });
        }
        t += ticks;
    }
    let seconds = t / 60 + 6;
    let (summary, dir) = run_session(Scene::PushTest, seconds, script, false);
    let trace = summary.movement_trace.expect("trace");
    std::fs::write(
        dir.join("movement_trace.json"),
        serde_json::to_string(&trace).unwrap(),
    )
    .unwrap();

    // Phases are sliced by server tick: the tick a row simulated is exact,
    // the mover's pass rate is not. Script tick 90 is where phase 1 starts.
    let rows: Vec<_> = trace.ticks.iter().collect();
    println!(
        "BOXPHASES ticks_traced={} trace={}",
        rows.len(),
        dir.join("movement_trace.json").display()
    );
    let first_move_tick = rows
        .iter()
        .find(|r| r.movement != [0.0; 3])
        .map_or(0, |r| r.tick);
    let mut phase_start_tick = first_move_tick;
    for (label, ticks, movement) in BOX_PHASES {
        let (start_tick, end_tick) = (phase_start_tick, phase_start_tick + ticks);
        phase_start_tick = end_tick;
        let _ = movement;
        let first = rows.iter().position(|r| r.tick >= start_tick);
        let Some(first) = first else {
            println!("BOX phase={label}: never reached");
            continue;
        };
        let last = rows
            .iter()
            .position(|r| r.tick >= end_tick)
            .unwrap_or(rows.len());
        let first_ms = rows[first].wall_ms;
        let end_ms = rows.get(last).map_or(u64::MAX, |r| r.wall_ms);
        let phase = &rows[first..last];
        // One row per mover pass (a burst shares a wall_ms and a drawn pose).
        let mut passes: Vec<&&spall_client::net::TickTraceRow> = Vec::new();
        for r in phase {
            if passes.last().is_none_or(|l| l.wall_ms != r.wall_ms) {
                passes.push(r);
            }
        }
        let mut steps = Vec::new();
        let mut repeats = 0;
        let mut moving = 0;
        for w in passes.windows(2) {
            let (Some(a), Some(b)) = (w[0].bodies.first(), w[1].bodies.first()) else {
                continue;
            };
            let d = (b.displayed_pos_m[0] - a.displayed_pos_m[0])
                .hypot(b.displayed_pos_m[2] - a.displayed_pos_m[2]);
            let v = b.snapshot_vel_m_s;
            if v[0].hypot(v[2]) > 0.05 {
                moving += 1;
                steps.push(d);
                if d == 0.0 {
                    repeats += 1;
                }
            }
        }
        let grounded_false = phase.iter().filter(|r| !r.grounded).count();
        let shifts: Vec<f64> = trace
            .reconciles
            .iter()
            .filter(|r| r.wall_ms >= first_ms && r.wall_ms < end_ms)
            .map(|r| r.shift_m)
            .collect();
        let steady_shifts: Vec<f64> = trace
            .reconciles
            .iter()
            .filter(|r| r.wall_ms >= first_ms + 300 && r.wall_ms < end_ms)
            .map(|r| r.shift_m)
            .collect();
        let jumps: Vec<f64> = trace
            .body_jumps
            .iter()
            .filter(|j| j.wall_ms >= first_ms && j.wall_ms < end_ms && j.speed_m_s > 0.05)
            .map(|j| j.extrapolation_error_m)
            .collect();
        // Collision mirror vs the newest snapshot it was extrapolated from.
        let gaps: Vec<f64> = phase
            .iter()
            .filter_map(|r| r.bodies.first())
            .map(|b| {
                (b.collision_pos_m[0] - b.displayed_pos_m[0])
                    .hypot(b.collision_pos_m[2] - b.displayed_pos_m[2])
            })
            .collect();
        println!(
            "BOX phase={label} ticks_rows={} player x=[{:.2}..{:.2}] z_end={:.2} box_end x={:.2} y={:.2} z={:.2}\n  \
             player correction shift_m: {} | steady (skip 0.3 s): {}\n  \
             box snapshot-arrival extrapolation error_m: {}\n  \
             drawn box step/pass_m (moving passes={moving}, repeated={repeats}): {}\n  \
             collision-vs-drawn gap_m: {}\n  grounded=false rows: {grounded_false}",
            phase.len(),
            phase.first().map_or(0.0, |r| r.predicted_pos_m[0]),
            phase.last().map_or(0.0, |r| r.predicted_pos_m[0]),
            phase.last().map_or(0.0, |r| r.predicted_pos_m[2]),
            phase
                .last()
                .and_then(|r| r.bodies.first())
                .map_or(0.0, |b| b.displayed_pos_m[0]),
            phase
                .last()
                .and_then(|r| r.bodies.first())
                .map_or(0.0, |b| b.displayed_pos_m[1]),
            phase
                .last()
                .and_then(|r| r.bodies.first())
                .map_or(0.0, |b| b.displayed_pos_m[2]),
            stats(shifts),
            stats(steady_shifts),
            stats(jumps),
            stats(steps),
            stats(gaps),
        );
    }
    assert!(!rows.is_empty(), "no tick rows traced");
}

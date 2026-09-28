//! ENG-69 live-equivalent prediction clock acceptance.

use std::time::Duration;

use spall_client::predict::{
    ClientPhysics, FixedStepClock, MAX_PREDICTION_CATCHUP_STEPS, PredictedPlayer,
};
use spall_core::{PlayerInput, player_entity_for};
use spall_physics::CharacterParams;
use spall_protocol::InputSeq;
use spall_sim::fixtures::{WALK_ARENA_SPAWNS, walk_arena_setup};
use spall_sim::{Simulation, SimulationConfig};

fn input_at_tick(tick: u64) -> PlayerInput {
    let movement = match tick {
        0..=179 | 360..=539 => [0.0, 0.0, 1.0],
        180..=359 | 540..=719 => [0.0, 0.0, -1.0],
        _ => [0.0; 3],
    };
    PlayerInput {
        movement,
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    }
}

#[test]
fn deadline_clock_sustains_server_rate_under_realistic_work_and_stalls() {
    // The captured live failure advanced the server 825 ticks while the old
    // sleep-after-work mover produced only 318 local records. Model that same
    // 13.75 s window. A representative 28 ms of hot-loop work plus the old
    // fixed 16 ms sleep cannot exceed 313 predictions here.
    let total = Duration::from_millis(13_750);
    let old_steps = total.as_millis() / (28 + 16);
    assert!(
        old_steps < 318,
        "fixture no longer represents the live failure"
    );

    let mut clock = FixedStepClock::new(Duration::from_secs_f64(1.0 / 60.0));
    let mut now = Duration::ZERO;
    let mut steps = 0_u64;
    let mut dropped = 0_u64;
    let mut max_due = 0_u32;
    while now < total {
        now += clock.wait_duration(now);
        let batch = clock.poll(now);
        steps += u64::from(batch.steps);
        dropped += u64::from(batch.dropped);
        max_due = max_due.max(batch.due);

        // Ordinary cached prediction work is 1 ms. Six deterministic 100 ms
        // stalls stand in for scheduling/CPU hiccups while snapshots continue.
        let work = if steps > 0 && steps.is_multiple_of(120) {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(1)
        };
        now += work;
    }

    let hz = steps as f64 / total.as_secs_f64();
    eprintln!(
        "deadline prediction: {steps} steps in {:.3}s = {hz:.2} Hz, dropped={dropped}, max_due={max_due}",
        total.as_secs_f64()
    );
    assert!(steps >= 805, "prediction under-produced at {hz:.2} Hz");
    assert!(
        hz >= 58.5,
        "prediction throughput {hz:.2} Hz is below target"
    );
    assert!(
        max_due <= 6,
        "fixture produced an unexpected backlog {max_due}"
    );
    assert!(
        dropped <= 15,
        "bounded stalls discarded too many steps: {dropped}"
    );
}

#[test]
fn catchup_is_bounded_and_hold_reset_discards_debt() {
    let step = Duration::from_secs_f64(1.0 / 60.0);
    let mut clock = FixedStepClock::new(step);
    let stalled = clock.poll(Duration::from_secs(1));
    assert_eq!(stalled.steps, MAX_PREDICTION_CATCHUP_STEPS);
    assert!(stalled.dropped >= 55);

    // A deliberate no-player / unknown-collision hold is not overload. Reset
    // makes the next prediction one full period later with no catch-up burst.
    clock.reset(Duration::from_secs(10));
    assert_eq!(clock.poll(Duration::from_secs(10)).steps, 0);
    assert_eq!(clock.poll(Duration::from_secs(10) + step).steps, 1);
}

#[test]
fn delayed_snapshots_and_cpu_stalls_keep_recurring_pose_jumps_small() {
    let mut sim = Simulation::new(SimulationConfig::new(walk_arena_setup())).unwrap();
    let entity = player_entity_for(0);
    sim.add_player(entity, WALK_ARENA_SPAWNS[0]);
    let volume = sim.world().terrain().volume.clone();
    let mut phys = ClientPhysics::new();
    phys.set_terrain(&volume);
    let mut predictor = PredictedPlayer::new(
        CharacterParams::DEFAULT,
        sim.player_state(entity).unwrap(),
        sim.current_tick(),
    );
    let mut clock = FixedStepClock::new(Duration::from_secs_f64(1.0 / 60.0));
    let mut client_seq = 0_u64;
    let mut server_seq = 0_u64;
    let mut client_steps = 0_u64;
    let mut snapshots = std::collections::VecDeque::new();
    let mut displacements = Vec::new();

    // 825 authoritative ticks in 13.75 s, matching the real capture. Player
    // snapshots publish every three ticks and arrive 50 ms later. Six 100 ms
    // mover stalls occur while server simulation and delivery continue.
    for ms in 1_u64..=13_750 {
        if ms * 60 / 1_000 > sim.current_tick().get() {
            server_seq += 1;
            let next_tick = sim.current_tick().get() + 1;
            sim.set_player_input(entity, input_at_tick(next_tick), InputSeq(server_seq));
            sim.tick().unwrap();
            if sim.current_tick().get().is_multiple_of(3) {
                snapshots.push_back((
                    ms + 50,
                    sim.current_tick(),
                    sim.player_state(entity).unwrap(),
                    sim.player_acked_input(entity).unwrap(),
                ));
            }
        }

        let inside_stall = [2_000_u64, 4_000, 6_000, 8_000, 10_000, 12_000]
            .iter()
            .any(|start| ms >= *start && ms < *start + 100);
        if !inside_stall {
            let elapsed = Duration::from_millis(ms);
            let batch = clock.poll(elapsed);
            for _ in 0..batch.steps {
                client_seq += 1;
                predictor.tick(
                    &mut phys,
                    &volume,
                    input_at_tick(ms * 60 / 1_000),
                    InputSeq(client_seq),
                    1.0 / 60.0,
                );
                client_steps += 1;
            }
        }

        while snapshots.front().is_some_and(|s| s.0 <= ms) {
            let (_, tick, state, acked) = snapshots.pop_front().unwrap();
            let outcome = predictor.reconcile(&mut phys, &volume, state, acked, tick);
            displacements.push(
                outcome
                    .predicted_before
                    .distance_m(&outcome.predicted_after),
            );
        }
    }
    // Deliver the final 50 ms of already-published snapshots without adding
    // new local predictions.
    while let Some((_, tick, state, acked)) = snapshots.pop_front() {
        let outcome = predictor.reconcile(&mut phys, &volume, state, acked, tick);
        displacements.push(
            outcome
                .predicted_before
                .distance_m(&outcome.predicted_after),
        );
    }

    displacements.sort_by(f64::total_cmp);
    let median = displacements[displacements.len() / 2];
    let p95 = displacements[displacements.len() * 95 / 100];
    let max = *displacements.last().unwrap();
    let hz = client_steps as f64 / 13.75;
    eprintln!(
        "825-tick delayed-snapshot trace: {client_steps} local steps ({hz:.2} Hz), {} reconciles, displacement median={median:.6}m p95={p95:.6}m max={max:.6}m",
        displacements.len()
    );
    assert!(
        client_steps >= 805,
        "local prediction under-produced: {hz:.2} Hz"
    );
    assert!(
        median < 0.03,
        "median recurring reconcile displacement {median:.4} m remains visibly large"
    );
    assert!(
        p95 < 0.08,
        "p95 recurring reconcile displacement {p95:.4} m exceeds one prediction step"
    );
    assert!(
        max < 0.31,
        "bounded 100 ms stall caused {max:.4} m displacement"
    );
}

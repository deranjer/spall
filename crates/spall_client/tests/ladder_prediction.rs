//! ENG-263: climbing a ladder predicts the same motion the authoritative
//! [`spall_sim::Simulation`] produces (same binary, no transport). Both sides
//! probe their own copy of the terrain for the ladder and step the one
//! character kernel, so a reconciling predictor never has to correct.

use spall_client::predict::{ClientPhysics, PredictedPlayer};
use spall_core::{BUTTON_JUMP, PlayerInput, player_entity_for};
use spall_physics::fixtures::ladder::{FLOOR_TOP_M, LADDER_FACE_X_M, PLATFORM_TOP_M, SPAWN_M};
use spall_physics::{CharacterParams, CharacterState, Climbable};
use spall_protocol::InputSeq;
use spall_sim::fixtures::ladder_setup;
use spall_sim::{Simulation, SimulationConfig, TICK_DT_S};

fn input(forward: f32, buttons: u32) -> PlayerInput {
    PlayerInput {
        movement: [0.0, 0.0, forward],
        view_dir: [1.0, 0.0, 0.0], // facing the ladder (+x)
        buttons,
    }
}

#[test]
fn ladder_climb_descend_slide_and_mount_predict_like_the_server() {
    let mut sim = Simulation::new(SimulationConfig::new(ladder_setup())).expect("ladder scene");
    let player = player_entity_for(0);
    sim.add_player(player, SPAWN_M);
    let volume = sim.world().terrain().volume.clone();
    let mut phys = ClientPhysics::new();
    phys.set_terrain(&volume);
    let mut predictor = PredictedPlayer::new(
        CharacterParams::DEFAULT,
        CharacterState::at(SPAWN_M),
        sim.current_tick(),
    );
    predictor.set_climbable(Climbable::from_manifest(sim.world().materials()));

    // Settle, walk in and climb part way, hold, climb down with S, climb again,
    // slide down with SPACE, then climb all the way and step onto the platform.
    let script: Vec<(usize, PlayerInput)> = vec![
        (15, PlayerInput::NEUTRAL),
        (60, input(1.0, 0)),
        (30, PlayerInput::NEUTRAL),
        (80, input(-1.0, 0)),
        (70, input(1.0, 0)),
        (60, input(0.0, BUTTON_JUMP)),
        (150, input(1.0, 0)),
    ];
    let ack_delay = 1;
    let mut server_log = Vec::new();
    let (mut seq, mut climbing_ticks, mut max_ladder_error_m) = (0u64, 0usize, 0.0_f64);
    for (ticks, inp) in script {
        for _ in 0..ticks {
            // Once on the platform, stop before walking off its far side.
            let now = sim.player_state(player).unwrap();
            let on_platform = !now.climbing && now.grounded && now.position_m[1] > 4.9;
            let inp = if on_platform {
                PlayerInput::NEUTRAL
            } else {
                inp
            };
            seq += 1;
            sim.set_player_input(player, inp, InputSeq(seq));
            sim.tick().unwrap();
            let server = sim.player_state(player).unwrap();
            server_log.push((server, sim.current_tick()));
            let predicted = predictor.tick(&mut phys, &volume, inp, InputSeq(seq), TICK_DT_S);
            assert_eq!(
                predicted.climbing, server.climbing,
                "tick {seq}: predicted {predicted:?} server {server:?}"
            );
            climbing_ticks += usize::from(server.climbing);
            if server_log.len() > ack_delay {
                let (auth, tick) = server_log[server_log.len() - 1 - ack_delay];
                let acked = InputSeq(seq - ack_delay as u64);
                let outcome = predictor.reconcile(&mut phys, &volume, auth, acked, tick);
                // The predicted step of a tick the server spent on the ladder,
                // replayed from the previous authoritative state, must land
                // where the server did. Walking on the floor carries the known
                // few-centimetre ground-snap residual between the server's and
                // the client's terrain colliders (ENG-69), so it is not held to
                // this bound.
                if auth.climbing
                    && let Some(c) = outcome.comparison
                {
                    max_ladder_error_m = max_ladder_error_m.max(c.error_m);
                }
            }
        }
    }
    let end = sim.player_state(player).unwrap();
    assert!(climbing_ticks > 200, "climbed for {climbing_ticks} ticks");
    assert!(!end.climbing && end.grounded, "{end:?}");
    assert!((end.position_m[1] - PLATFORM_TOP_M).abs() < 0.05, "{end:?}");
    assert!(end.position_m[0] > LADDER_FACE_X_M, "{end:?}");
    assert!(end.position_m[1] > FLOOR_TOP_M + 3.0);
    assert!(
        max_ladder_error_m < 1e-3,
        "ladder prediction corrected by {max_ladder_error_m} m"
    );
}

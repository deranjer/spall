//! T19 authoritative player-movement acceptance.
//!
//! Covers, on the CPU: a capsule walks the arena and stays grounded; a lost
//! "button up" cannot leave it walking forever (held input clears after the
//! 250 ms timeout); a committed cut under the player bumps its prediction epoch
//! and does not leave it hovering; and the player's motion snapshot reports the
//! acknowledged input sequence the client needs for reconciliation.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, PlayerInput, SphereBrush, player_entity_for};
use spall_protocol::{InputSeq, RequestId};
use spall_sim::fixtures::{WALK_ARENA_SPAWNS, walk_arena_setup};
use spall_sim::{
    EditIntent, EditTarget, HELD_INPUT_TIMEOUT_TICKS, MotionPublisher, Simulation, SimulationConfig,
};

const FLOOR_TOP_M: f64 = 1.0;

fn sim() -> Simulation {
    Simulation::new(SimulationConfig::new(walk_arena_setup())).expect("arena is valid")
}

fn forward() -> PlayerInput {
    PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0], // walk toward +X
        buttons: 0,
    }
}

fn brush_at_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

#[test]
fn a_player_walks_the_arena_and_stays_grounded() {
    let mut sim = sim();
    let player = player_entity_for(0);
    sim.add_player(player, WALK_ARENA_SPAWNS[0]);
    let start = sim.player_state(player).unwrap().position_m;

    let mut seq = 0u64;
    for _ in 0..120 {
        seq += 1;
        assert!(sim.set_player_input(player, forward(), InputSeq(seq)));
        sim.tick().unwrap();
    }

    let end = sim.player_state(player).unwrap();
    let dx = end.position_m[0] - start[0];
    assert!(dx > 3.0, "player only advanced {dx} m in ~2 s of walking");
    assert!(end.grounded, "player should be grounded on the flat floor");
    assert!(
        (end.position_m[1] - FLOOR_TOP_M).abs() < 0.1,
        "player drifted off the floor: y = {}",
        end.position_m[1]
    );
}

#[test]
fn a_lost_button_release_stops_the_player_after_the_timeout() {
    let mut sim = sim();
    let player = player_entity_for(0);
    sim.add_player(player, WALK_ARENA_SPAWNS[0]);

    // One held-forward frame, then silence — as if every later datagram (and the
    // eventual "stopped walking" frame) was lost.
    assert!(sim.set_player_input(player, forward(), InputSeq(1)));

    let mut last_x = sim.player_state(player).unwrap().position_m[0];
    let mut moved_ticks = 0;
    for _ in 0..(HELD_INPUT_TIMEOUT_TICKS as usize + 40) {
        sim.tick().unwrap();
        let x = sim.player_state(player).unwrap().position_m[0];
        if x - last_x > 1e-3 {
            moved_ticks += 1;
        }
        last_x = x;
    }

    // It coasted on the reused input for about the timeout window, then stopped.
    assert!(
        (HELD_INPUT_TIMEOUT_TICKS as i32 - moved_ticks).abs() <= 3,
        "expected ~{HELD_INPUT_TIMEOUT_TICKS} ticks of coasting, saw {moved_ticks}"
    );
    let settled_x = last_x;
    for _ in 0..30 {
        sim.tick().unwrap();
    }
    assert!(
        (sim.player_state(player).unwrap().position_m[0] - settled_x).abs() < 1e-2,
        "player kept walking after the held input timed out"
    );
}

#[test]
fn a_cut_under_the_player_bumps_the_epoch_and_leaves_no_hover() {
    let mut sim = sim();
    let player = player_entity_for(0);
    sim.add_player(player, WALK_ARENA_SPAWNS[0]);
    // Settle onto the floor.
    for _ in 0..20 {
        sim.tick().unwrap();
    }
    let grounded = sim.player_state(player).unwrap();
    assert!(grounded.grounded);
    let epoch_before = sim.player_movement_epoch(player).unwrap();

    // Cut the floor out from under the spawn: feet at x≈1 m (cell 4),
    // z≈1.5 m (cell 6); remove a 4-cell-radius sphere through the slab.
    let intent = EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_at_cell(4, 2, 6, 4),
    );
    sim.submit(intent).unwrap();
    sim.run_until_idle(60).unwrap();
    // A few more ticks for the capsule to react to the hole.
    for _ in 0..40 {
        sim.tick().unwrap();
    }

    let after = sim.player_state(player).unwrap();
    assert!(
        sim.player_movement_epoch(player).unwrap() > epoch_before,
        "a cut next to the player should bump the movement epoch"
    );
    assert!(
        !after.grounded && after.position_m[1] < grounded.position_m[1] - 0.3,
        "player hovered over the removed floor: y {} -> {}",
        grounded.position_m[1],
        after.position_m[1]
    );
}

#[test]
fn the_player_motion_snapshot_reports_the_acked_input() {
    let mut sim = sim();
    let player = player_entity_for(2);
    sim.add_player(player, WALK_ARENA_SPAWNS[2]);

    assert!(sim.set_player_input(player, forward(), InputSeq(7)));
    let report = sim.tick().unwrap();
    let _ = report;

    let mut publisher = MotionPublisher::new(60, 20);
    let snaps = publisher.snapshots(sim.world(), sim.current_tick());
    let mine = snaps
        .iter()
        .find(|s| s.body == player)
        .expect("a snapshot for the player entity");
    assert_eq!(mine.acked_input, InputSeq(7));
    assert!(!mine.sleeping);
}

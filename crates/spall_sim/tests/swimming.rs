//! A player swims in the column water and can leap out onto a raft.

use glam::DQuat;
use spall_core::{
    BrickCoord, CellSizeCode, GlobalCell, MaterialId, PlayerInput, Revision, VolumeId,
    player_entity_for,
};
use spall_protocol::InputSeq;
use spall_sim::column_water::ColumnWaterSetup;
use spall_sim::playground::spawn_box_body;
use spall_sim::{BodyPose, Simulation, SimulationConfig, fixtures};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, EditPlan, Volume};

const WOOD: MaterialId = MaterialId(3);
const STONE: MaterialId = MaterialId(1);
/// Water surface height in the pool, m.
const SURFACE_M: f64 = 3.0;

/// 16 m x 8 m x 4 m basin with 2.5 m of water over a 0.5 m floor.
fn pool_sim() -> Simulation {
    pool_sim_with(|_| {})
}

/// The pool with `tune` applied to its water setup (springs and drains).
fn pool_sim_with(tune: impl FnOnce(&mut ColumnWaterSetup)) -> Simulation {
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    for bx in 0..2 {
        terrain
            .insert_brick(
                BrickCoord::new(bx, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
    }
    let mut plan = EditPlan::new(terrain.id());
    let mut water = Vec::new();
    for z in 0..16 {
        for y in 0..32 {
            for x in 0..64 {
                let wall = x == 0 || x == 63 || z == 0 || z == 15;
                if y < 2 || (wall && y < 24) {
                    plan.set(GlobalCell::new(x, y, z), STONE);
                } else if y < 12 {
                    water.push(GlobalCell::new(x, y, z));
                }
            }
        }
    }
    terrain.apply_edit(&plan).unwrap();
    let mut world = fixtures::flat_terrain_setup();
    world.terrain = terrain;
    world.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 31, 15));
    world.anchor = AnchorPlane::at(0);
    let mut config = SimulationConfig::new(world);
    let mut setup = ColumnWaterSetup::new(0, 32, water);
    tune(&mut setup);
    config.column_water = Some(setup);
    Simulation::new(config).unwrap()
}

/// Faces +x: the view direction decides where "forward" goes.
fn swim_east(forward: f32, jump: bool) -> PlayerInput {
    PlayerInput {
        movement: [0.0, 0.0, forward],
        view_dir: [1.0, 0.0, 0.0],
        buttons: if jump { spall_core::BUTTON_JUMP } else { 0 },
    }
}

#[test]
fn a_player_who_drops_into_the_pool_treads_water_and_can_swim() {
    let mut sim = pool_sim();
    let player = player_entity_for(0);
    sim.add_player(player, [1.5, 4.5, 2.0]);
    let mut seq = 0u64;
    let mut tick = |sim: &mut Simulation, input: PlayerInput| {
        seq += 1;
        assert!(sim.set_player_input(player, input, InputSeq(seq)));
        sim.tick().unwrap();
    };
    for _ in 0..180 {
        tick(&mut sim, swim_east(0.0, false));
    }
    let treading = sim.player_state(player).unwrap();
    let depth = SURFACE_M - treading.position_m[1];
    assert!(
        (depth - 1.25).abs() < 0.1,
        "treads {depth} m deep, expected about 1.25"
    );
    let x0 = treading.position_m[0];
    for _ in 0..60 {
        tick(&mut sim, swim_east(1.0, false));
    }
    let swum = sim.player_state(player).unwrap().position_m[0] - x0;
    assert!(swum > 2.5, "swam only {swum} m in a second");
}

#[test]
fn a_swimmer_can_leap_from_the_water_onto_a_raft() {
    let mut sim = pool_sim();
    // A 4 m x 0.5 m x 3 m wood raft; its deck rides about 0.15 m above the water.
    let raft = spawn_box_body(
        sim.world_mut(),
        [16, 2, 12],
        WOOD,
        700.0,
        BodyPose::new(DQuat::IDENTITY, [5.0, 3.2, 0.5]),
    )
    .unwrap();
    let player = player_entity_for(0);
    sim.add_player(player, [2.0, 4.5, 2.0]);
    let mut seq = 0u64;
    let mut step = |sim: &mut Simulation, input: PlayerInput| {
        seq += 1;
        assert!(sim.set_player_input(player, input, InputSeq(seq)));
        sim.tick().unwrap();
    };
    for _ in 0..240 {
        step(&mut sim, swim_east(0.0, false));
    }
    let raft_deck = |sim: &Simulation| {
        sim.world()
            .bodies()
            .find(|b| b.entity == Some(raft))
            .unwrap()
            .pose
            .translation_m
    };
    println!("raft at {:?}", raft_deck(&sim));
    // Swim to the raft's edge, then jump while still pushing forward; once
    // aboard, stop and stand there.
    let mut jumped = false;
    let mut landed_at = None;
    for tick in 0..150 {
        let p = sim.player_state(player).unwrap();
        let near = p.position_m[0] > raft_deck(&sim)[0] - 1.2;
        let jump = near && !jumped;
        jumped |= jump;
        let aboard = landed_at.is_some();
        step(&mut sim, swim_east(if aboard { 0.0 } else { 1.0 }, jump));
        if jumped && landed_at.is_none() && sim.player_state(player).unwrap().grounded {
            landed_at = Some(tick);
        }
    }
    assert!(
        landed_at.is_some(),
        "the player never landed after the leap"
    );
    let end = sim.player_state(player).unwrap();
    let raft_at = raft_deck(&sim);
    let deck_y = raft_at[1] + 0.5;
    assert!(
        end.position_m[1] > deck_y - 0.2 && end.grounded,
        "the player should stand on the deck: feet {:.2} grounded {} (deck {:.2})",
        end.position_m[1],
        end.grounded,
        deck_y
    );
    assert!(
        end.position_m[0] > raft_at[0] && end.position_m[0] < raft_at[0] + 4.0,
        "and be over the raft: x {:.2} vs raft x {:.2}..{:.2}",
        end.position_m[0],
        raft_at[0],
        raft_at[0] + 4.0
    );
}

/// A current carries a player treading water downstream, as it does a raft; the
/// same player in still water stays put.
#[test]
fn a_current_carries_an_idle_swimmer_downstream() {
    let drift = |current: bool| {
        let mut sim = if current {
            pool_sim_with(|setup| {
                for z in 1..15 {
                    for y in 2..12 {
                        setup.sources.push(GlobalCell::new(2, y, z));
                        setup.sinks.push(GlobalCell::new(61, y, z));
                    }
                }
            })
        } else {
            pool_sim()
        };
        let player = player_entity_for(0);
        sim.add_player(player, [8.0, 4.5, 2.0]);
        let mut seq = 0u64;
        // Settle onto the surface first, then let go of the controls.
        for tick in 0..(60 * 10) {
            seq += 1;
            assert!(sim.set_player_input(player, swim_east(0.0, false), InputSeq(seq)));
            sim.tick().unwrap();
            if tick == 59 {
                assert!(sim.player_state(player).unwrap().position_m[1] < 4.0);
            }
        }
        let end = sim.player_state(player).unwrap();
        end.position_m[0] - 8.0
    };
    let still_x = drift(false);
    let river_x = drift(true);
    println!("still moved {still_x:.2} m, river moved {river_x:.2} m");
    assert!(
        still_x.abs() < 0.3,
        "still water holds the swimmer: {still_x}"
    );
    assert!(river_x > 1.5, "the current carries the swimmer: {river_x}");
}

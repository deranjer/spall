//! ENG-223 player health: damage, death and respawn on the authoritative
//! simulation, and per-account health across reconnects.

use spall_core::{PlayerInput, player_entity_for};
use spall_protocol::{InputSeq, PlayerId};
use spall_sim::fixtures::{WALK_ARENA_SPAWNS, walk_arena_setup};
use spall_sim::{PLAYER_MAX_HEALTH, PlayerDamage, Simulation, SimulationConfig};

fn sim() -> Simulation {
    Simulation::new(SimulationConfig::new(walk_arena_setup())).expect("arena is valid")
}

#[test]
fn damage_below_health_hurts_and_lethal_damage_respawns_at_full_health() {
    let mut sim = sim();
    let player = sim.add_player(player_entity_for(0), WALK_ARENA_SPAWNS[0]);
    assert_eq!(sim.player_health(player), Some(PLAYER_MAX_HEALTH));

    // Walk away from the spawn so the respawn is observable.
    let walk = PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    };
    for seq in 1..=60 {
        sim.set_player_input(player, walk, InputSeq(seq));
        sim.tick().unwrap();
    }
    let walked = sim.player_state(player).unwrap().position_m;
    assert!(
        walked[0] - WALK_ARENA_SPAWNS[0][0] > 1.0,
        "player walked: {walked:?}"
    );

    assert_eq!(
        sim.damage_player(player, 30),
        Some(PlayerDamage::Hurt { health: 70 })
    );
    assert_eq!(
        sim.damage_player(player, 0),
        Some(PlayerDamage::Hurt { health: 70 })
    );
    let epoch = sim.player_movement_epoch(player).unwrap();

    // Exactly the remaining health is lethal: health never rests at zero.
    assert_eq!(sim.damage_player(player, 70), Some(PlayerDamage::Killed));
    assert_eq!(sim.player_health(player), Some(PLAYER_MAX_HEALTH));
    let respawned = sim.player_state(player).unwrap();
    assert!(
        (respawned.position_m[0] - WALK_ARENA_SPAWNS[0][0]).abs() < 0.5
            && (respawned.position_m[2] - WALK_ARENA_SPAWNS[0][2]).abs() < 0.5,
        "respawned at {:?}, spawn {:?}",
        respawned.position_m,
        WALK_ARENA_SPAWNS[0]
    );
    assert_eq!(respawned.velocity_m_s, [0.0; 3]);
    assert_ne!(
        sim.player_movement_epoch(player).unwrap(),
        epoch,
        "the owning client must rebuild prediction after a respawn"
    );

    // Overkill is still one death.
    assert_eq!(
        sim.damage_player(player, u16::MAX),
        Some(PlayerDamage::Killed)
    );
    assert_eq!(sim.player_health(player), Some(PLAYER_MAX_HEALTH));
    // Each death is counted, for the game's rules to answer (ENG-251).
    assert_eq!(sim.world().player(player).unwrap().deaths, 2);
    assert_eq!(sim.damage_player(player_entity_for(7), 10), None);
}

#[test]
fn account_health_follows_the_account_across_reconnects_and_restore() {
    let account = PlayerId([7; 16]);
    let mut sim = sim();
    let first = sim.add_player(player_entity_for(0), WALK_ARENA_SPAWNS[0]);
    assert!(sim.bind_player_principal(first, account));
    sim.damage_player(first, 45).unwrap();

    // Reconnect into a different slot while the old capsule lingers.
    let second = sim.add_player(player_entity_for(1), WALK_ARENA_SPAWNS[1]);
    assert_eq!(sim.player_health(second), Some(PLAYER_MAX_HEALTH));
    assert!(sim.bind_player_principal(second, account));
    assert_eq!(sim.player_health(second), Some(55));
    assert_eq!(sim.player_health_records(), vec![(account, 55)]);

    // Reconnect into the same slot: the replaced capsule hands health back.
    sim.damage_player(second, 5).unwrap();
    let again = sim.add_player(player_entity_for(1), WALK_ARENA_SPAWNS[1]);
    assert_eq!(sim.player_health_records(), vec![(account, 50)]);
    assert!(sim.bind_player_principal(again, account));
    assert_eq!(sim.player_health(again), Some(50));

    // A restored simulation hands the account its saved health on join.
    let mut restored = self::sim();
    restored
        .restore_player_health(sim.player_health_records())
        .unwrap();
    let player = restored.add_player(player_entity_for(3), WALK_ARENA_SPAWNS[0]);
    assert!(restored.bind_player_principal(player, account));
    assert_eq!(restored.player_health(player), Some(50));

    assert_eq!(restored.restore_player_health(vec![(account, 0)]), Err(0));
    assert_eq!(
        restored.restore_player_health(vec![(account, PLAYER_MAX_HEALTH + 1)]),
        Err(PLAYER_MAX_HEALTH + 1)
    );
}

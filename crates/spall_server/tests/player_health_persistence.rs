//! ENG-223: player health survives simulation -> SQLite checkpoint ->
//! recovered simulation, keyed by the stable account.

use spall_core::player_entity_for;
use spall_physics::PhysicsConfig;
use spall_protocol::PlayerId;
use spall_server::{PersistConfig, persist};
use spall_sim::{PLAYER_MAX_HEALTH, Simulation, SimulationConfig};
use spall_store::Writer;
use spall_structure::AnchorPlane;

#[test]
fn health_survives_save_and_load_for_online_and_offline_accounts() {
    let dir = std::env::temp_dir().join(format!(
        "spall-player-health-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = PersistConfig {
        world_id: 223,
        seed: 0,
        generator_version: 1,
    };
    let (online, offline, fresh) = (PlayerId([1; 16]), PlayerId([2; 16]), PlayerId([3; 16]));
    let mut sim = Simulation::new(SimulationConfig::new(
        spall_sim::fixtures::flat_terrain_setup(),
    ))
    .unwrap();
    let a = sim.add_player(player_entity_for(0), [2.0, 1.0, 2.0]);
    sim.bind_player_principal(a, online);
    sim.damage_player(a, 40).unwrap();
    // `offline` was hurt, then its slot was reused by someone else.
    let b = sim.add_player(player_entity_for(1), [3.0, 1.0, 2.0]);
    sim.bind_player_principal(b, offline);
    sim.damage_player(b, 75).unwrap();
    sim.add_player(player_entity_for(1), [3.0, 1.0, 2.0]);

    let checkpoint = persist::capture(&sim, &cfg, 0).unwrap();
    let mut writer = Writer::open(dir.join("world.db")).unwrap();
    writer.publish_checkpoint(&checkpoint).unwrap();
    let (mut restored, _) = persist::restore(
        &writer.recover().unwrap(),
        &cfg,
        persist::RecoveryChoice::RequireClean,
        sim.world().materials().clone(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap();
    drop(writer);

    // Players are not restored as capsules; each account gets its health back
    // when it joins again, whichever slot it lands in.
    assert_eq!(restored.player_count(), 0);
    for (slot, account, health) in [
        (5, online, 60),
        (6, offline, 25),
        (7, fresh, PLAYER_MAX_HEALTH),
    ] {
        let entity = restored.add_player(player_entity_for(slot), [2.0, 1.0, 2.0]);
        assert!(restored.bind_player_principal(entity, account));
        assert_eq!(restored.player_health(entity), Some(health), "{account:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

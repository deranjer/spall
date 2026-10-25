//! ENG-257: world items survive simulation -> SQLite checkpoint -> recovered
//! simulation, and the recovered world never reuses a taken item's id. A
//! fire (ENG-266) keeps its fuel.

use spall_physics::PhysicsConfig;
use spall_server::{PersistConfig, persist};
use spall_sim::{Simulation, SimulationConfig};
use spall_store::Writer;
use spall_structure::AnchorPlane;

#[test]
fn items_and_their_id_counter_survive_save_and_load() {
    let dir = std::env::temp_dir().join(format!(
        "spall-world-items-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = PersistConfig {
        world_id: 257,
        seed: 0,
        generator_version: 1,
    };
    let mut sim = Simulation::new(SimulationConfig::new(
        spall_sim::fixtures::flat_terrain_setup(),
    ))
    .unwrap();
    let kept = sim.spawn_world_item(40, [2.0, 1.0, 2.5], 0.5).unwrap();
    let taken = sim.spawn_world_item(43, [3.0, 1.0, 2.5], 0.0).unwrap();
    sim.remove_world_item(taken.item_id).unwrap();
    let fire = sim.place_fire(6, [4.0, 1.0, 2.5], 0.0, 95.5).unwrap();

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

    let mut items: Vec<_> = restored.world_items().copied().collect();
    items.sort_unstable_by_key(|item| item.item_id);
    assert_eq!(items, [kept, fire]);
    assert!(items[1].is_lit());
    assert!(
        restored.take_world_item_changes().is_empty(),
        "recovered items reach clients with their baseline, not as spawns"
    );
    let next = restored.spawn_world_item(40, [0.0; 3], 0.0).unwrap();
    assert!(next.item_id > taken.item_id, "ids are never reused");
    let _ = std::fs::remove_dir_all(&dir);
}

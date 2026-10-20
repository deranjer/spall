//! Public API reset/restore identity checks, independent of host internals.
use spall_core::{CreatureAllocatorSnapshotV1, STORE_SCHEMA_VERSION_U16, WorldId};
use spall_sim::{Simulation, SimulationConfig, fixtures};

fn simulation() -> Simulation {
    Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap()
}

#[test]
fn replacement_never_discards_a_fresh_worlds_later_actor_counters() {
    let world = WorldId::from_u128(901);
    let mut active = simulation();
    active.initialize_creature_host(world).unwrap();
    let before = active.creature_allocator_snapshot().unwrap();
    let mut fresh = simulation();
    fresh
        .restore_creature_allocator_snapshot(CreatureAllocatorSnapshotV1 {
            schema: STORE_SCHEMA_VERSION_U16,
            world_id: world,
            world_epoch: 5,
            next_creature_id: 100,
        })
        .unwrap();
    match active.replace_world(fresh) {
        Ok(()) => {
            let after = active
                .creature_allocator_snapshot()
                .expect("restored identity was dropped");
            assert!(
                after.next_creature_id >= 100,
                "fresh-world creature ID counter rewound"
            );
            assert!(after.world_epoch > 5, "fresh-world epoch rewound");
        }
        Err(_) => assert_eq!(active.creature_allocator_snapshot(), Some(before)),
    }
}

#[test]
fn exhausted_epoch_replacement_leaves_allocator_unchanged() {
    let mut active = simulation();
    let saved = CreatureAllocatorSnapshotV1 {
        schema: STORE_SCHEMA_VERSION_U16,
        world_id: WorldId::from_u128(902),
        world_epoch: u64::MAX,
        next_creature_id: 123,
    };
    active.restore_creature_allocator_snapshot(saved).unwrap();
    assert!(active.replace_world(simulation()).is_err());
    assert_eq!(active.creature_allocator_snapshot(), Some(saved));
}

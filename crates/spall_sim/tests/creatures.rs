//! Creature entities: spawn and despawn bookkeeping, join list, motion frames.
use spall_protocol::CreatureRecord;
use spall_sim::{
    CreatureSpawnError, CreatureWalkRequest, Simulation, SimulationConfig, fixtures,
};

fn record(id: u64, x: f64) -> CreatureRecord {
    CreatureRecord {
        creature_id: id,
        species: "deer".into(),
        translation_m: [x, 1.0, 3.0],
        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        locomotion_tag: 0,
        clip_phase_tick: 0,
    }
}

fn sim() -> Simulation {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    sim.initialize_creature_host().unwrap();
    sim
}

#[test]
fn spawns_are_atomic_and_ids_are_never_reused() {
    let mut sim = sim();
    assert_eq!(sim.next_creature_id(), Some(1));
    sim.spawn_creatures(vec![record(1, 2.0), record(2, 4.0)])
        .unwrap();
    assert_eq!(sim.next_creature_id(), Some(3));

    // A batch with one bad member spawns nothing.
    assert_eq!(
        sim.spawn_creatures(vec![record(5, 1.0), record(4, 1.0)]),
        Err(CreatureSpawnError::IdUsed)
    );
    assert_eq!(sim.next_creature_id(), Some(3));
    assert_eq!(sim.creature_records().len(), 2);

    // A despawned id stays burned.
    assert!(sim.despawn_creature(2));
    assert!(!sim.despawn_creature(2));
    assert_eq!(
        sim.spawn_creatures(vec![record(2, 4.0)]),
        Err(CreatureSpawnError::IdUsed)
    );
    sim.spawn_creatures(vec![record(3, 6.0)]).unwrap();

    let mut bad = record(9, 1.0);
    bad.species.clear();
    assert_eq!(
        sim.spawn_creatures(vec![bad]),
        Err(CreatureSpawnError::Invalid)
    );
}

#[test]
fn join_list_and_motion_frames_follow_the_current_creatures() {
    let mut sim = sim();
    sim.spawn_creatures(vec![record(1, 2.0), record(2, 8.0)])
        .unwrap();
    sim.tick().unwrap();

    // Move creature 1 along +x; the join list carries its new pose and clip.
    let outcome = sim
        .walk_creature(CreatureWalkRequest {
            creature_id: 1,
            desired_step_m: [0.5, 0.0],
            target_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            locomotion_tag: 1,
            clip_phase_tick: 8,
            feet_offset_m: -0.5,
            capsule_radius_m: 0.28,
            capsule_half_height_m: 0.25,
        })
        .unwrap();
    assert!(outcome.moved_m[0] > 0.4, "{outcome:?}");
    let join = sim.creature_records();
    assert_eq!(
        join.iter().map(|c| c.creature_id).collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(join[0].translation_m[0] > 2.4);
    assert_eq!((join[0].locomotion_tag, join[0].clip_phase_tick), (1, 8));
    assert_eq!(join[1], record(2, 8.0));

    // Motion frames list only creatures inside the filter, sorted by id.
    let all = sim.creature_motion_frame(&|_| true).unwrap();
    assert_eq!(
        all.entries.iter().map(|e| e.creature_id).collect::<Vec<_>>(),
        [1, 2]
    );
    let near = sim.creature_motion_frame(&|p| p[0] < 5.0).unwrap();
    assert_eq!(near.entries.len(), 1);

    // A despawned creature is gone from both.
    assert!(sim.despawn_creature(1));
    assert_eq!(sim.creature_records().len(), 1);
    assert_eq!(sim.creature_motion_frame(&|_| true).unwrap().entries.len(), 1);
    assert!(sim.creature_motion_frame(&|_| false).is_none());

    // Moving a creature that is not hosted is refused.
    assert!(
        sim.walk_creature(CreatureWalkRequest {
            creature_id: 1,
            desired_step_m: [0.1, 0.0],
            target_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            locomotion_tag: 1,
            clip_phase_tick: 0,
            feet_offset_m: -0.5,
            capsule_radius_m: 0.28,
            capsule_half_height_m: 0.25,
        })
        .is_err()
    );
}

#[test]
fn a_world_reset_clears_creatures_but_keeps_the_id_counter() {
    let mut sim = sim();
    sim.spawn_creatures(vec![record(1, 2.0), record(2, 4.0)])
        .unwrap();
    let fresh = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    sim.replace_world(fresh).unwrap();
    assert!(sim.creature_records().is_empty());
    assert_eq!(sim.next_creature_id(), Some(3));
}

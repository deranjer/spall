#![cfg(feature = "client")]
//! The sandbox director spawns the authored species as plain creature records,
//! walks and flies them against terrain, and a replica converges on them.

use sandbox::animals::AnimalDirector;
use spall_server::creature_service::CreatureService;
use spall_sim::{Simulation, SimulationConfig, fixtures};

fn simulation() -> Simulation {
    let mut setup = fixtures::flat_terrain_setup();
    setup.materials = sandbox::game::manifest();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    sim.initialize_creature_host().unwrap();
    sim
}

fn replica() -> spall_client::replica::ReplicaWorld {
    let mut setup = fixtures::flat_terrain_setup();
    setup.materials = sandbox::game::manifest();
    spall_client::replica::ReplicaWorld::from_baseline(
        setup.terrain,
        spall_client::replica::ReplicaConfig::default(),
    )
}

/// Runs the server tick and announces the roster change to `replica`, as the
/// serve loop does.
fn tick_into(
    sim: &mut Simulation,
    service: &CreatureService,
    replica: &mut spall_client::replica::ReplicaWorld,
) {
    sim.tick().unwrap();
    let roster = service.run_tick(sim).expect("spawns commit");
    for creature in roster.spawned {
        replica
            .apply_creature_spawn(&spall_protocol::CreatureSpawn {
                schema: spall_protocol::WIRE_SCHEMA_VERSION,
                creature,
            })
            .expect("replica accepts a server spawn");
    }
}

#[test]
fn director_spawns_deer_and_crows_that_replicate() {
    let mut sim = simulation();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], &[2, 2, 0], 7)
        .unwrap()
        .with_radius_m(0.0, 1.0);
    let service = CreatureService::new(Box::new(director));
    let mut replica = replica();
    for _ in 0..400 {
        tick_into(&mut sim, &service, &mut replica);
    }
    let server = sim.creature_records();
    assert_eq!(server.len(), 4, "2 deer + 2 crows");
    assert_eq!(server.iter().filter(|c| c.species == "deer").count(), 2);
    assert_eq!(server.iter().filter(|c| c.species == "crow").count(), 2);
    let replicated: Vec<_> = replica.creatures().records().cloned().collect();
    assert_eq!(replicated, server);
}

#[test]
fn a_walking_deer_and_a_flying_crow_are_swept_against_terrain_and_published_as_motion() {
    use spall_sim::{CreatureMoveOutcome, CreatureMoveRequest, CreatureWalkRequest};

    let mut sim = simulation();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], &[1, 1, 0], 3)
        .unwrap()
        .with_radius_m(0.0, 1.0);
    let service = CreatureService::new(Box::new(director));
    for _ in 0..60 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
    }
    let records = sim.creature_records();
    let deer = records
        .iter()
        .find(|c| c.species == "deer")
        .unwrap()
        .clone();
    let crow = records
        .iter()
        .find(|c| c.species == "crow")
        .unwrap()
        .clone();

    // A short step along open ground moves the deer and shows up in the published frame.
    let outcome = sim
        .walk_creature(CreatureWalkRequest {
            creature_id: deer.creature_id,
            desired_step_m: [0.05, 0.0],
            target_rotation_xyzw: deer.rotation_xyzw,
            locomotion_tag: 1,
            clip_phase_tick: 1,
            feet_offset_m: -0.5,
            capsule_radius_m: 0.28,
            capsule_half_height_m: 0.25,
        })
        .unwrap();
    assert!((outcome.moved_m[0] - 0.05).abs() < 1e-3, "{outcome:?}");
    let moved = sim
        .creature_poses()
        .into_iter()
        .find(|p| p.creature_id == deer.creature_id)
        .unwrap();
    assert!((moved.translation_m[0] - (deer.translation_m[0] + outcome.moved_m[0])).abs() < 1e-9);
    let frame = sim
        .creature_motion_frame(&|_| true)
        .expect("creatures are published");
    let entry = frame
        .entries
        .iter()
        .find(|e| e.creature_id == deer.creature_id)
        .unwrap();
    assert_eq!((entry.locomotion_tag, entry.clip_phase_tick), (1, 1));
    // Interest filtering drops far animals.
    assert!(sim.creature_motion_frame(&|_| false).is_none());

    // A crow told to fly into the floor is blocked and stays where it was.
    let fly = |dx: f64, dy: f64| CreatureMoveRequest {
        creature_id: crow.creature_id,
        target_translation_m: [
            crow.translation_m[0] + dx,
            crow.translation_m[1] + dy,
            crow.translation_m[2],
        ],
        target_rotation_xyzw: crow.rotation_xyzw,
        locomotion_tag: 1,
        clip_phase_tick: 3,
        feet_offset_m: -0.2,
        capsule_radius_m: 0.25,
        capsule_half_height_m: 0.1,
    };
    assert_eq!(
        sim.move_creature(fly(0.0, -30.0)).unwrap(),
        CreatureMoveOutcome::Blocked
    );
    assert_eq!(
        sim.creature_poses()
            .into_iter()
            .find(|p| p.creature_id == crow.creature_id)
            .unwrap()
            .translation_m,
        crow.translation_m
    );
    assert_eq!(
        sim.move_creature(fly(0.3, 0.0)).unwrap(),
        CreatureMoveOutcome::Moved
    );
}

#[test]
fn deer_wander_and_crows_circle_under_the_swept_motion_path() {
    let mut sim = simulation();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], &[3, 2, 0], 11)
        .unwrap()
        .with_radius_m(0.0, 1.5);
    let service = CreatureService::new(Box::new(director));
    let mut start = std::collections::BTreeMap::new();
    let mut frames = 0;
    let mut motion_time = std::time::Duration::ZERO;
    for tick in 0..1500 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
        let started = std::time::Instant::now();
        service.run_motion(&mut sim).unwrap();
        motion_time += started.elapsed();
        if tick == 60 {
            for alive in sim.creature_poses() {
                start.insert(alive.creature_id, alive.translation_m);
            }
        }
        if tick % 3 == 0 && sim.creature_motion_frame(&|_| true).is_some() {
            frames += 1;
        }
    }
    assert_eq!(start.len(), 5, "3 deer and 2 crows alive");
    assert!(frames > 100);
    let moved: Vec<f64> = sim
        .creature_poses()
        .iter()
        .map(|alive| {
            let from = start[&alive.creature_id];
            (alive.translation_m[0] - from[0]).hypot(alive.translation_m[2] - from[2])
        })
        .collect();
    eprintln!(
        "moved metres: {moved:?}; motion hook {:?} total over 1500 ticks",
        motion_time
    );
    assert!(
        moved.iter().filter(|d| **d > 0.5).count() >= 2,
        "animals barely moved: {moved:?}"
    );
}

#[test]
fn bears_and_snow_leopards_spawn_and_walk_on_their_own_capsules() {
    let mut sim = simulation();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], &[0, 0, 0, 2, 2], 13)
        .unwrap()
        .with_radius_m(0.0, 3.0);
    let service = CreatureService::new(Box::new(director));
    let mut start = std::collections::BTreeMap::new();
    for tick in 0..1500 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
        service.run_motion(&mut sim).unwrap();
        if tick == 60 {
            for alive in sim.creature_poses() {
                start.insert(alive.creature_id, alive.translation_m);
            }
        }
    }
    assert_eq!(start.len(), 4, "2 bears and 2 snow leopards alive");
    let moved: Vec<f64> = sim
        .creature_poses()
        .iter()
        .map(|alive| {
            let from = start[&alive.creature_id];
            (alive.translation_m[0] - from[0]).hypot(alive.translation_m[2] - from[2])
        })
        .collect();
    assert!(
        moved.iter().filter(|d| **d > 0.5).count() >= 2,
        "bears and leopards barely moved: {moved:?}"
    );
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "timing budget is only meaningful in release"
)]
fn drawing_animals_each_frame_is_cheap_enough_for_an_uncapped_window() {
    use sandbox::animal_view::AnimalView;
    use spall_client::window::CreatureView;

    let mut sim = simulation();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], &[4, 2, 0], 5)
        .unwrap()
        .with_radius_m(0.0, 1.5);
    let service = CreatureService::new(Box::new(director));
    let mut replica = replica();
    for _ in 0..200 {
        tick_into(&mut sim, &service, &mut replica);
    }
    let creatures = replica.creatures();
    assert_eq!(creatures.len(), 6);
    let view = AnimalView::new().unwrap();
    let started = std::time::Instant::now();
    let mut cubes = 0;
    for _ in 0..200 {
        cubes = view
            .instances(creatures, 0.0, std::time::Instant::now())
            .len();
    }
    let per_call = started.elapsed() / 200;
    eprintln!("AnimalView: {cubes} cubes for 6 animals, {per_call:?} per frame");
    // 36 animals scale linearly; budget well under a 60 Hz frame even uncapped.
    assert!(
        per_call * 6 < std::time::Duration::from_millis(2),
        "{per_call:?} per 6 animals"
    );
}

#[test]
fn a_player_walking_into_moving_animals_never_stalls_the_tick() {
    use spall_core::{EntityId, PLAYER_ENTITY_BASE, PlayerInput};
    use spall_protocol::InputSeq;

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sim = simulation();
        let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], &[3, 2, 0], 21)
            .unwrap()
            .with_radius_m(0.0, 1.0);
        let service = CreatureService::new(Box::new(director));
        let player = EntityId::new(PLAYER_ENTITY_BASE + 1).unwrap();
        sim.add_player(player, [1.0, 0.6, 3.0]);
        let mut worst = std::time::Duration::ZERO;
        for tick in 0..2400_u64 {
            // Walk +X into the herd, then turn around and come back, repeatedly.
            let heading = if (tick / 300) % 2 == 0 {
                [1.0, 0.0, 0.0]
            } else {
                [-1.0, 0.0, 0.0]
            };
            sim.set_player_input(
                player,
                PlayerInput {
                    movement: [0.0, 0.0, 1.0],
                    view_dir: heading,
                    buttons: 0,
                },
                InputSeq(tick + 1),
            );
            let started = std::time::Instant::now();
            sim.tick().unwrap();
            service.run_tick(&mut sim).unwrap();
            service.run_motion(&mut sim).unwrap();
            worst = worst.max(started.elapsed());
        }
        done_tx.send(worst).unwrap();
    });
    let worst = done_rx
        .recv_timeout(std::time::Duration::from_secs(90))
        .expect("tick loop stalled with a player among moving animals");
    eprintln!("worst tick with player + animals: {worst:?}");
}

#[test]
fn players_walk_through_creatures_instead_of_being_blocked_by_them() {
    use spall_core::{EntityId, PLAYER_ENTITY_BASE, PlayerInput};
    use spall_protocol::InputSeq;

    let mut sim = simulation();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], &[1, 0, 0], 4)
        .unwrap()
        .with_radius_m(0.0, 0.5);
    let service = CreatureService::new(Box::new(director));
    for _ in 0..30 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
    }
    let deer = sim.creature_poses()[0].translation_m;
    let player = EntityId::new(PLAYER_ENTITY_BASE + 1).unwrap();
    // Start well clear of the deer, in line with it, and walk straight through.
    sim.add_player(player, [deer[0] - 2.5, 0.6, deer[2]]);
    let start = sim.player_state(player).unwrap().position_m;
    for tick in 0..90_u64 {
        sim.set_player_input(
            player,
            PlayerInput {
                movement: [0.0, 0.0, 1.0],
                view_dir: [1.0, 0.0, 0.0],
                buttons: 0,
            },
            InputSeq(tick + 1),
        );
        sim.tick().unwrap();
    }
    let end = sim.player_state(player).unwrap().position_m;
    let travelled = end[0] - start[0];
    assert!(
        end[0] > deer[0] + 0.5,
        "player stopped at x={:.2} short of the deer at x={:.2} (travelled {travelled:.2} m)",
        end[0],
        deer[0]
    );
}

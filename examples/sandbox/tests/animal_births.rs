#![cfg(feature = "client")]
//! The sandbox director spawns the authored species through the engine's
//! ordered lifecycle path, and the resulting records round-trip through a
//! replica unchanged.

use sandbox::animals::AnimalDirector;
use spall_core::WorldId;
use spall_server::creature_service::CreatureService;
use spall_sim::{Simulation, SimulationConfig, fixtures};

fn simulation() -> Simulation {
    let mut setup = fixtures::flat_terrain_setup();
    setup.materials = sandbox::game::manifest();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    sim.initialize_creature_host(WorldId::from_u128(77))
        .unwrap();
    sim
}

#[test]
fn director_births_deer_and_crows_that_validate_and_replicate() {
    let mut sim = simulation();
    let anchor = [3.0, 1.0, 3.0];
    let director = AnimalDirector::new(vec![anchor], [2, 2, 0], 7)
        .unwrap()
        .with_radius_m(0.0, 1.0);
    let service = CreatureService::new(Box::new(director));

    let mut replica = spall_client::replica::ReplicaWorld::from_baseline(
        {
            let mut setup = fixtures::flat_terrain_setup();
            setup.materials = sandbox::game::manifest();
            setup.terrain
        },
        spall_client::replica::ReplicaConfig::default(),
    );
    replica.install_creature_catalog(service.catalog().clone());

    let mut committed = 0;
    for _ in 0..400 {
        sim.tick().unwrap();
        if let Some(birth) = service.run_tick(&mut sim).expect("birth commits") {
            committed += 1;
            let outcome = replica.apply_transaction(&birth.topology);
            assert!(
                matches!(
                    outcome,
                    spall_client::replica::ApplyOutcome::Published { .. }
                ),
                "replica rejected a server birth: {outcome:?}"
            );
        }
    }
    assert!(committed > 0, "director never spawned anything");
    let snapshot = service
        .catalog()
        .with_view(|view| sim.capture_creature_snapshot(view))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.owners.records.len(), 4, "2 deer + 2 crows");
    let replicated: Vec<_> = replica.creatures().unwrap().records().cloned().collect();
    assert_eq!(replicated.len(), snapshot.owners.records.len());
    for server in &snapshot.owners.records {
        let client = replicated
            .iter()
            .find(|r| r.state.key == server.state.key)
            .expect("replica holds every server creature");
        // Topology is exact. Motion is captured live on the server, so the
        // at-birth record may differ from it only by physics rounding.
        assert_eq!(client.topology_hash(), server.topology_hash());
        assert_eq!(client.parts, server.parts);
        for axis in 0..3 {
            assert!(
                (client.state.root_translation_m[axis] - server.state.root_translation_m[axis])
                    .abs()
                    < 1e-3,
                "creature {} root drifted on axis {axis}",
                server.state.key.creature_id.get()
            );
        }
    }
}

#[test]
fn a_walking_deer_is_swept_against_terrain_and_published_as_motion() {
    use sandbox::animals::{pose_parts, species};
    use spall_sim::{CreatureMoveOutcome, CreatureMoveRequest};

    let mut sim = simulation();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], [1, 0, 0], 3)
        .unwrap()
        .with_radius_m(0.0, 1.0);
    let service = CreatureService::new(Box::new(director));
    for _ in 0..60 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
    }
    let deer = species()
        .unwrap()
        .into_iter()
        .find(|s| s.key == "deer")
        .unwrap();
    let alive = sim.alive_creatures();
    assert_eq!(alive.len(), 1);
    let start = alive[0];

    let request = |dx: f64, dy: f64, phase: u64| CreatureMoveRequest {
        key: start.key,
        target_translation_m: [
            start.translation_m[0] + dx,
            start.translation_m[1] + dy,
            start.translation_m[2],
        ],
        target_rotation_xyzw: start.rotation_xyzw,
        parts: pose_parts(&deer.asset, "walk", phase as f64).unwrap(),
        locomotion_tag: 1,
        clip_phase_tick: phase,
    };

    // A short step along open ground is staged, installs on the next step, and
    // shows up in the applied root and the published frame.
    assert_eq!(
        sim.move_creature(request(0.05, 0.0, 1)).unwrap(),
        CreatureMoveOutcome::Staged
    );
    assert_eq!(
        sim.move_creature(request(0.05, 0.0, 2)).unwrap(),
        CreatureMoveOutcome::Busy,
        "a second pose before the first installs must wait"
    );
    sim.tick().unwrap();
    let moved = sim.alive_creatures()[0];
    assert!(
        (moved.translation_m[0] - (start.translation_m[0] + 0.05)).abs() < 1e-3,
        "root moved {:?} -> {:?}",
        start.translation_m,
        moved.translation_m
    );
    let frame = sim
        .creature_motion_frame(&|_| true)
        .expect("one creature moves");
    assert_eq!(frame.entries.len(), 1);
    assert_eq!(frame.entries[0].locomotion_tag, 1);
    assert_eq!(frame.entries[0].clip_phase_tick, 1);
    assert!((frame.entries[0].translation_m[0] - moved.translation_m[0]).abs() < 1e-9);
    // Interest filtering drops far animals.
    assert!(sim.creature_motion_frame(&|_| false).is_none());

    // Driving the root down into the floor is blocked and changes nothing.
    let before = sim.alive_creatures()[0];
    let blocked = sim.move_creature(request(0.0, -3.0, 3)).unwrap();
    assert!(matches!(blocked, CreatureMoveOutcome::Blocked { .. }));
    sim.tick().unwrap();
    assert_eq!(sim.alive_creatures()[0], before);
}

#[test]
fn deer_wander_and_crows_circle_under_the_swept_motion_path() {
    let mut sim = simulation();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], [3, 2, 0], 11)
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
            for alive in sim.alive_creatures() {
                start.insert(alive.key.creature_id.get(), alive.translation_m);
            }
        }
        if tick % 3 == 0 && sim.creature_motion_frame(&|_| true).is_some() {
            frames += 1;
        }
    }
    assert_eq!(start.len(), 5, "3 deer and 2 crows alive");
    assert!(frames > 100);
    let moved: Vec<f64> = sim
        .alive_creatures()
        .iter()
        .map(|alive| {
            let from = start[&alive.key.creature_id.get()];
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

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
/// Bears walk the meadow around their anchor; snow leopards are born on the
/// mountain beside it (a 10 m plateau with a cliff) and their wandering keeps
/// them on it, both on their own capsules.
fn bears_walk_the_lowland_and_snow_leopards_stay_on_the_mountain() {
    // 32 m x 12 m: lowland (top face 0.5 m) for x < 16 m, plateau (10.25 m) beyond.
    let mut sim = terrain_simulation(128, 48, 72, 64, 40);
    let director = AnimalDirector::new(vec![[6.0, 0.5, 6.0]], &[0, 0, 0, 2, 2], 13)
        .unwrap()
        .with_radius_m(0.0, 5.0);
    let service = CreatureService::new(Box::new(director));
    let mut start = std::collections::BTreeMap::new();
    let mut leopard_low = Vec::new();
    for tick in 0..3000 {
        step(&mut sim, &service);
        if tick == 60 {
            for alive in sim.creature_records() {
                start.insert(alive.creature_id, alive.translation_m);
            }
        }
        for leopard in sim.creature_records() {
            let at = leopard.translation_m;
            // Within a body length of the cliff counts as on the mountain.
            if leopard.species == "snow_leopard" && at[0] < 16.0 - 1.5 {
                leopard_low.push((tick, at));
            }
        }
    }
    let records = sim.creature_records();
    assert_eq!(start.len(), 4, "2 bears and 2 snow leopards alive");
    for creature in &records {
        let born = start[&creature.creature_id];
        if creature.species == "snow_leopard" {
            assert!(born[1] > 10.0, "a snow leopard was born low: {born:?}");
        } else {
            assert!(born[1] < 2.0, "a bear was born on the mountain: {born:?}");
        }
    }
    assert!(
        leopard_low.is_empty(),
        "snow leopards left the mountain: {:?}",
        &leopard_low[..leopard_low.len().min(4)]
    );
    let moved: Vec<(String, f64)> = records
        .iter()
        .map(|alive| {
            let from = start[&alive.creature_id];
            (
                alive.species.clone(),
                (alive.translation_m[0] - from[0]).hypot(alive.translation_m[2] - from[2]),
            )
        })
        .collect();
    for species in ["bear", "snow_leopard"] {
        assert!(
            moved.iter().any(|(s, d)| s == species && *d > 0.5),
            "{species} barely moved: {moved:?}"
        );
    }
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
            .instances(creatures, 0.0, 0, std::time::Instant::now(), None)
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

/// The 1 km world's herd (24 deer, 12 crows, 3 bears, 3 snow leopards,
/// 6 trout) scattered over a 60 m square around the origin, half of them
/// moving, at varied headings and clip phases.
fn one_km_herd() -> spall_client::replica_creatures::ReplicaCreatures {
    let mut creatures = spall_client::replica_creatures::ReplicaCreatures::new();
    let roster = [
        ("deer", 24),
        ("crow", 12),
        ("bear", 3),
        ("snow_leopard", 3),
        ("trout", 6),
    ];
    let mut id = 1_u64;
    for (species, count) in roster {
        for i in 0..count {
            let a = id as f64 * 2.399;
            let r = 30.0 * ((id as f64 * 0.618).fract());
            let yaw = (id as f32 * 1.7).rem_euclid(std::f32::consts::TAU);
            let (s, c) = (yaw * 0.5).sin_cos();
            creatures
                .spawn(&spall_protocol::CreatureSpawn {
                    schema: spall_protocol::WIRE_SCHEMA_VERSION,
                    creature: spall_protocol::CreatureRecord {
                        creature_id: id,
                        species: species.to_owned(),
                        translation_m: [r * a.cos(), 1.0, r * a.sin()],
                        rotation_xyzw: [0.0, s, 0.0, c],
                        locomotion_tag: (i % 2) as u8,
                        clip_phase_tick: id * 7,
                    },
                })
                .unwrap();
            id += 1;
        }
    }
    creatures
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "timing budget is only meaningful in release"
)]
fn drawing_the_one_km_herd_each_frame() {
    use sandbox::animal_view::AnimalView;
    use spall_client::window::CreatureView;

    use spall_client::window::{CreatureCull, DEFAULT_VIEW_DISTANCE_M};

    let creatures = one_km_herd();
    let view = AnimalView::new().unwrap();
    // The game camera's cone (75 degree vertical FOV at 16:9) from `eye`.
    let camera = |eye: [f64; 3], forward: [f32; 3]| CreatureCull {
        apex_m: [eye[0] - 3.0 * f64::from(forward[0]), eye[1], eye[2]],
        forward,
        half_angle_rad: ((37.5_f32).to_radians().tan()
            * (1.0_f32 + (16.0 / 9.0_f32).powi(2)).sqrt())
        .atan(),
        eye_m: eye,
        max_distance_m: DEFAULT_VIEW_DISTANCE_M + 3.0,
    };
    let cases = [
        ("no cull", None),
        (
            "inside the herd",
            Some(camera([0.0, 2.0, 0.0], [1.0, 0.0, 0.0])),
        ),
        (
            "herd edge, looking away",
            Some(camera([35.0, 2.0, 0.0], [1.0, 0.0, 0.0])),
        ),
        (
            "200 m away, looking at it",
            Some(camera([200.0, 2.0, 0.0], [-1.0, 0.0, 0.0])),
        ),
    ];
    let frames = 120;
    let mut drawn = Vec::new();
    for (name, cull) in cases {
        let started = std::time::Instant::now();
        let mut cubes = 0;
        for _ in 0..frames {
            cubes = view
                .instances(&creatures, 0.0, 0, std::time::Instant::now(), cull.as_ref())
                .len();
        }
        let per_call = started.elapsed() / frames;
        eprintln!("1 km herd, {name}: {cubes} cubes, {per_call:?} per frame");
        drawn.push(cubes);
    }
    assert!(drawn[1] > 0 && drawn[1] < drawn[0], "{drawn:?}");
    assert_eq!(drawn[2], 0, "{drawn:?}");
    assert_eq!(drawn[3], 0, "{drawn:?}");
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

/// One deer alone near the floor's centre, settled, with a player standing
/// `offset_x` metres from it along X. Returns the deer's id and position.
fn deer_with_player_at(offset_x: f64) -> (Simulation, CreatureService, u64, [f64; 3], [f64; 3]) {
    use spall_core::{EntityId, PLAYER_ENTITY_BASE};

    let mut sim = simulation();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], &[1], 9)
        .unwrap()
        .with_radius_m(0.0, 0.5);
    let service = CreatureService::new(Box::new(director));
    for _ in 0..30 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
    }
    let deer = sim.creature_poses()[0];
    let player_at = [deer.translation_m[0] - offset_x, 0.6, deer.translation_m[2]];
    sim.add_player(EntityId::new(PLAYER_ENTITY_BASE + 1).unwrap(), player_at);
    (
        sim,
        service,
        deer.creature_id,
        deer.translation_m,
        player_at,
    )
}

fn deer_record(sim: &Simulation, id: u64) -> spall_protocol::CreatureRecord {
    sim.creature_records()
        .into_iter()
        .find(|c| c.creature_id == id)
        .unwrap()
}

fn horizontal(a: [f64; 3], b: [f64; 3]) -> f64 {
    (a[0] - b[0]).hypot(a[2] - b[2])
}

#[test]
fn a_deer_bolts_at_a_run_from_a_player_close_by() {
    let (mut sim, service, id, start, player) = deer_with_player_at(3.0);
    for _ in 0..24 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
        service.run_motion(&mut sim).unwrap();
    }
    let deer = deer_record(&sim, id);
    assert_eq!(deer.locomotion_tag, sandbox::animals::LOCOMOTION_RUN);
    let gained = horizontal(deer.translation_m, player) - horizontal(start, player);
    // Three moves at 7.5 m/s are 3 m; turning away from the player costs some.
    assert!(gained > 1.5, "deer only gained {gained:.2} m on the player");
}

#[test]
fn a_deer_watches_a_player_at_a_distance_then_leaves() {
    let (mut sim, service, id, start, _) = deer_with_player_at(20.0);
    for _ in 0..16 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
        service.run_motion(&mut sim).unwrap();
    }
    let deer = deer_record(&sim, id);
    assert_eq!(deer.locomotion_tag, sandbox::animals::LOCOMOTION_ALERT);
    assert!(
        horizontal(deer.translation_m, start) < 0.05,
        "an alert deer stands still"
    );
    // A player who stays inside the notice distance is fled from once the deer
    // has watched for a few seconds.
    let ran = (0..240).any(|_| {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
        service.run_motion(&mut sim).unwrap();
        deer_record(&sim, id).locomotion_tag == sandbox::animals::LOCOMOTION_RUN
    });
    assert!(ran, "the deer never fled the watching player");
}

#[test]
fn a_deer_ignores_a_far_player() {
    let (mut sim, service, id, _, _) = deer_with_player_at(60.0);
    for _ in 0..600 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
        service.run_motion(&mut sim).unwrap();
        let tag = deer_record(&sim, id).locomotion_tag;
        assert!(
            tag != sandbox::animals::LOCOMOTION_ALERT && tag != sandbox::animals::LOCOMOTION_RUN,
            "deer reacted to a player 60 m away"
        );
    }
}

/// A deer resting on grass in a generated world sometimes stops to graze, and
/// each bite clears the grass under its muzzle through the server's
/// destroyed-cover record: the same record a dig makes, saved and sent to
/// clients in the vegetation frame. A player close by ends the meal at a run.
#[test]
fn a_grazing_deer_eats_the_grass_under_its_muzzle() {
    use sandbox::animals::{LOCOMOTION_GRAZE, LOCOMOTION_RUN};
    use spall_core::{EntityId, PLAYER_ENTITY_BASE};
    use spall_voxel::Sample;

    let scene = sandbox::worldgen_scene::generate("showcase", 4, 256).unwrap();
    let mut config = SimulationConfig::new(scene.world_setup());
    config.vegetation = Some(scene.vegetation().clone());
    let mut sim = Simulation::new(config).unwrap();
    sim.initialize_creature_host().unwrap();
    let grass = |sim: &Simulation, x: i64, z: i64| {
        (0..200_i64)
            .rev()
            .find_map(|y| {
                match sim
                    .world()
                    .terrain()
                    .volume
                    .sample(spall_core::GlobalCell::new(x, y, z))
                {
                    Ok(Sample::Filled(material)) => Some((y, material)),
                    _ => None,
                }
            })
            .filter(|&(_, material)| {
                sandbox::appearance::base_material(material) == spall_core::MaterialId(10)
            })
    };
    // Flat open grass, the same height all around, for the herd to start on.
    let (gx, gy, gz) = (32..224_i64)
        .flat_map(|x| (32..224_i64).map(move |z| (x, z)))
        .find_map(|(x, z)| {
            let (y, _) = grass(&sim, x, z)?;
            (-4..=4_i64)
                .flat_map(|dx| (-4..=4_i64).map(move |dz| (dx, dz)))
                .all(|(dx, dz)| grass(&sim, x + dx, z + dz).is_some_and(|(h, _)| h == y))
                .then_some((x, y, z))
        })
        .expect("the showcase world has a flat meadow");
    let anchor = [
        gx as f64 * 0.25 + 0.125,
        (gy + 1) as f64 * 0.25,
        gz as f64 * 0.25 + 0.125,
    ];
    let director = AnimalDirector::new(vec![anchor], &[3], 17)
        .unwrap()
        .with_radius_m(0.0, 2.0);
    let service = CreatureService::new(Box::new(director));
    assert!(sim.vegetation_state().unwrap().cleared.is_empty());
    // (column centre x, z, the grazing deer's root and rotation at the bite)
    let mut bites: Vec<(f64, f64, [f64; 3], [f32; 4])> = Vec::new();
    for _ in 0..3600 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
        service.run_motion(&mut sim).unwrap();
        for c in &sim.vegetation_state().unwrap().cleared {
            let (cx, cz) = (f64::from(c.x) * 0.25 + 0.125, f64::from(c.z) * 0.25 + 0.125);
            if bites.iter().any(|&(x, z, _, _)| (x, z) == (cx, cz)) {
                continue;
            }
            // Only a grazing deer eats; the nearest one took this bite.
            let deer = sim
                .creature_records()
                .into_iter()
                .filter(|d| d.locomotion_tag == LOCOMOTION_GRAZE)
                .min_by(|a, b| {
                    let d = |r: &spall_protocol::CreatureRecord| {
                        (r.translation_m[0] - cx).hypot(r.translation_m[2] - cz)
                    };
                    d(a).total_cmp(&d(b))
                })
                .expect("grass was cleared with no deer grazing");
            bites.push((cx, cz, deer.translation_m, deer.rotation_xyzw));
        }
    }
    assert!(bites.len() >= 2, "deer hardly grazed: {bites:?}");
    for &(cx, cz, root, [_, sin, _, cos]) in &bites {
        let yaw = 2.0 * f64::from(sin).atan2(f64::from(cos));
        let (dx, dz) = (cx - root[0], cz - root[2]);
        let ahead = dx * yaw.cos() - dz * yaw.sin();
        assert!(
            dx.hypot(dz) < 1.5 && ahead > 0.3,
            "bite at ({cx:.2}, {cz:.2}) is not under the muzzle of a deer at {root:?} facing {yaw:.2}"
        );
    }
    // Clients draw cover from the vegetation frame, which carries the record.
    let state = sim.vegetation_state().unwrap();
    assert_eq!(sim.vegetation_visual().unwrap().cleared, state.cleared);

    // Wait for a deer to graze again, then walk a player up to it: it runs.
    let grazing = (0..3600)
        .find_map(|_| {
            sim.tick().unwrap();
            service.run_tick(&mut sim).unwrap();
            service.run_motion(&mut sim).unwrap();
            sim.creature_records()
                .into_iter()
                .find(|d| d.locomotion_tag == LOCOMOTION_GRAZE)
        })
        .expect("a deer grazes again");
    let at = grazing.translation_m;
    sim.add_player(
        EntityId::new(PLAYER_ENTITY_BASE + 1).unwrap(),
        [at[0] + 3.0, at[1] + 0.5, at[2]],
    );
    let ran = (0..24).any(|_| {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
        service.run_motion(&mut sim).unwrap();
        sim.creature_records()
            .iter()
            .any(|d| d.creature_id == grazing.creature_id && d.locomotion_tag == LOCOMOTION_RUN)
    });
    assert!(ran, "a player beside a grazing deer did not make it flee");
}

/// A stone world `width` x `depth` cells: a 2-cell floor, raised to
/// `plateau_top` (inclusive cell) from `plateau_x` on in x, with resident air
/// above both up to `height` so downward ground queries find the surface.
fn terrain_simulation(
    width: i64,
    depth: i64,
    height: i64,
    plateau_x: i64,
    plateau_top: i64,
) -> Simulation {
    use spall_core::{GlobalCell, MaterialId};
    use spall_voxel::EditPlan;

    let mut setup = fixtures::flat_terrain_setup();
    let id = setup.terrain.id();
    let mut volume = spall_voxel::Volume::new(id, spall_core::CellSizeCode::Quarter);
    let mut fill = |a: [i64; 3], b: [i64; 3], material: MaterialId| {
        volume
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(a[0], a[1], a[2]),
                GlobalCell::new(b[0], b[1], b[2]),
                material,
            ))
            .unwrap();
    };
    fill([0, 0, 0], [width - 1, 1, depth - 1], fixtures::STONE);
    if plateau_x < width {
        fill(
            [plateau_x, 0, 0],
            [width - 1, plateau_top, depth - 1],
            fixtures::STONE,
        );
        fill(
            [plateau_x, plateau_top + 1, 0],
            [width - 1, height - 1, depth - 1],
            MaterialId::AIR,
        );
    }
    fill(
        [0, 2, 0],
        [plateau_x.min(width) - 1, height - 1, depth - 1],
        MaterialId::AIR,
    );
    setup.terrain = volume;
    setup.terrain_collider_region = (
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(width - 1, height - 1, depth - 1),
    );
    setup.materials = sandbox::game::manifest();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    sim.initialize_creature_host().unwrap();
    sim
}

fn step(sim: &mut Simulation, service: &CreatureService) {
    sim.tick().unwrap();
    service.run_tick(sim).unwrap();
    service.run_motion(sim).unwrap();
}

/// Circling crows over a wide flat field now and then come down, hop and peck
/// on the ground, and climb back to their circle; a player walking up to a
/// grounded crow sends it up early.
#[test]
fn crows_land_hop_and_peck_then_take_off_and_flee_a_near_player() {
    use sandbox::animals::{
        LOCOMOTION_HOP, LOCOMOTION_LAND, LOCOMOTION_MOVE, LOCOMOTION_PECK, LOCOMOTION_TAKEOFF,
    };
    use spall_core::{EntityId, PLAYER_ENTITY_BASE};

    // 48 m square, 16 m of air over a floor whose top face is at 0.5 m.
    let mut sim = terrain_simulation(192, 192, 64, 192, 0);
    let director = AnimalDirector::new(vec![[24.0, 0.5, 24.0]], &[0, 3], 23)
        .unwrap()
        .with_radius_m(0.0, 2.0);
    let service = CreatureService::new(Box::new(director));
    let grounded = |y: f64| y < 0.5 + 0.6;
    // Per crow: the stages seen in order (landing, on the ground, taking off,
    // circling high again).
    let mut stage: std::collections::BTreeMap<u64, u8> = Default::default();
    let mut full_cycle = None;
    for _ in 0..30_000 {
        step(&mut sim, &service);
        for crow in sim.creature_records() {
            let s = stage.entry(crow.creature_id).or_default();
            let y = crow.translation_m[1];
            *s = match (*s, crow.locomotion_tag) {
                (0, LOCOMOTION_LAND) => 1,
                (1, LOCOMOTION_HOP | LOCOMOTION_PECK) if grounded(y) => 2,
                (2, LOCOMOTION_TAKEOFF) => 3,
                (3, LOCOMOTION_MOVE) if y > 2.5 => 4,
                (s, _) => s,
            };
            if *s == 4 {
                full_cycle = Some(crow.creature_id);
            }
        }
        if full_cycle.is_some() {
            break;
        }
    }
    assert!(
        full_cycle.is_some(),
        "no crow landed, pecked and flew again: stages {stage:?}"
    );

    // Wait until a crow is on the ground, then put a player 4 m from it.
    let crow = (0..30_000)
        .find_map(|_| {
            step(&mut sim, &service);
            sim.creature_records().into_iter().find(|c| {
                matches!(c.locomotion_tag, LOCOMOTION_HOP | LOCOMOTION_PECK)
                    && grounded(c.translation_m[1])
            })
        })
        .expect("a crow lands again");
    let at = crow.translation_m;
    sim.add_player(
        EntityId::new(PLAYER_ENTITY_BASE + 1).unwrap(),
        [at[0] + 4.0, 0.6, at[2]],
    );
    for _ in 0..60 {
        step(&mut sim, &service);
    }
    let now = sim
        .creature_records()
        .into_iter()
        .find(|c| c.creature_id == crow.creature_id)
        .unwrap();
    assert_eq!(now.locomotion_tag, LOCOMOTION_TAKEOFF, "{now:?}");
    assert!(
        now.translation_m[1] > at[1] + 1.0,
        "the spooked crow did not climb: {now:?}"
    );
}

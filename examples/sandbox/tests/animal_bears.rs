#![cfg(feature = "client")]
//! ENG-238: a hungry bear stalks a deer, charges, kills it, and eats the
//! carcass a cell at a time; the deer runs from the charge, and its despawn
//! reaches a replica the way the serve loop announces it.
//!
//! Run: `cargo test --release -p sandbox --features client --test animal_bears`.

use sandbox::animals::{
    AnimalDirector, LOCOMOTION_EAT, LOCOMOTION_MOVE, LOCOMOTION_POUNCE, LOCOMOTION_REAR,
    LOCOMOTION_RUN, LOCOMOTION_STALK,
};
use spall_client::replica::{ReplicaConfig, ReplicaWorld};
use spall_core::{CellSizeCode, GlobalCell, MaterialId};
use spall_net::WireRecord;
use spall_server::creature_service::CreatureService;
use spall_sim::{Simulation, SimulationConfig, fixtures};
use spall_voxel::{EditPlan, Sample, Volume};

/// Flat world size in cells (0.25 m): a 2-cell stone floor, air above.
const SIDE: i64 = 640;
const HEIGHT: i64 = 16;

fn flat_volume() -> (Volume, (GlobalCell, GlobalCell)) {
    let setup = fixtures::flat_terrain_setup();
    let id = setup.terrain.id();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
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
    fill([0, 0, 0], [SIDE - 1, 1, SIDE - 1], fixtures::STONE);
    fill([0, 2, 0], [SIDE - 1, HEIGHT - 1, SIDE - 1], MaterialId::AIR);
    let region = (
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(SIDE - 1, HEIGHT - 1, SIDE - 1),
    );
    (volume, region)
}

fn simulation() -> Simulation {
    let mut setup = fixtures::flat_terrain_setup();
    let (volume, region) = flat_volume();
    setup.terrain = volume;
    setup.terrain_collider_region = region;
    setup.materials = sandbox::game::manifest();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    sim.initialize_creature_host().unwrap();
    sim.set_creature_shape(sandbox::animal_visual::creature_shape().unwrap())
        .unwrap();
    sim
}

fn replica() -> ReplicaWorld {
    ReplicaWorld::from_baseline(flat_volume().0, ReplicaConfig::default())
}

/// Applies roster records to the replica as a client does.
fn apply(replica: &mut ReplicaWorld, records: Vec<WireRecord>) {
    for record in records {
        match record {
            WireRecord::CreatureSpawn(spawn) => replica.apply_creature_spawn(&spawn).unwrap(),
            WireRecord::CreatureDespawn(despawn) => replica.apply_creature_despawn(&despawn),
            other => panic!("unexpected roster record {other:?}"),
        }
    }
}

/// Solid cells above the floor within `radius` cells of `at` (x, z): carcass.
fn carcass_cells(sim: &Simulation, at: [f64; 3], radius: i64) -> usize {
    let (cx, cz) = ((at[0] / 0.25) as i64, (at[2] / 0.25) as i64);
    let volume = &sim.world().terrain().volume;
    let mut count = 0;
    for x in cx - radius..=cx + radius {
        for z in cz - radius..=cz + radius {
            for y in 2..HEIGHT {
                if matches!(
                    volume.sample(GlobalCell::new(x, y, z)),
                    Ok(Sample::Filled(_))
                ) {
                    count += 1;
                }
            }
        }
    }
    count
}

#[test]
fn a_hungry_bear_stalks_catches_kills_and_eats_a_deer() {
    let mut sim = simulation();
    let mut replica = replica();
    // A deer and a bear born at two anchors 30 m apart in the middle of an
    // open 160 m field: out of the deer's notice, within the bear's.
    let mid = SIDE as f64 * 0.125;
    let anchors = vec![[mid - 15.0, 0.5, mid], [mid + 15.0, 0.5, mid]];
    // A seed that puts them at different anchors (seeds 1-12 all end in a
    // kill; most put both at one anchor).
    let seed = 5;
    let director = AnimalDirector::new(anchors, &[1, 0, 0, 1], seed)
        .unwrap()
        .with_radius_m(0.0, 0.5);
    let service = CreatureService::new(Box::new(director));

    let (mut deer_id, mut bear_id) = (None, None);
    let mut bear_tags = Vec::new();
    let mut deer_ran_from_charge = false;
    let mut kill: Option<(u64, [f64; 3])> = None;
    let mut deer_at = [0.0; 3];
    let mut start_gap = None;
    let mut most_carcass = 0;
    let mut eaten = 0;
    for tick in 0..12_000_u64 {
        sim.tick().unwrap();
        let roster = service.run_tick(&mut sim).unwrap();
        apply(&mut replica, roster.into_wire_records());
        let killed = service.run_motion(&mut sim).unwrap();
        let killed_ids = killed.despawned.clone();
        apply(&mut replica, killed.into_wire_records());

        let records = sim.creature_records();
        for record in &records {
            match record.species.as_str() {
                "deer" => deer_id = Some(record.creature_id),
                "bear" => bear_id = Some(record.creature_id),
                _ => {}
            }
        }
        let (Some(deer), Some(bear)) = (deer_id, bear_id) else {
            continue;
        };
        let bear_record = records.iter().find(|r| r.creature_id == bear).unwrap();
        if bear_tags.last() != Some(&bear_record.locomotion_tag) {
            bear_tags.push(bear_record.locomotion_tag);
        }
        if let Some(deer_record) = records.iter().find(|r| r.creature_id == deer) {
            deer_at = deer_record.translation_m;
            let gap = (deer_at[0] - bear_record.translation_m[0])
                .hypot(deer_at[2] - bear_record.translation_m[2]);
            start_gap.get_or_insert(gap);
            if bear_record.locomotion_tag == LOCOMOTION_RUN
                && deer_record.locomotion_tag == LOCOMOTION_RUN
            {
                deer_ran_from_charge = true;
            }
        }
        if killed_ids.contains(&deer) {
            assert!(kill.is_none(), "the deer died twice");
            kill = Some((tick, deer_at));
            // The kill's despawn reached the replica with the server's roster.
            assert!(
                replica.creatures().records().all(|r| r.creature_id != deer),
                "the replica still holds the killed deer"
            );
            let held: Vec<u64> = replica
                .creatures()
                .records()
                .map(|r| r.creature_id)
                .collect();
            assert_eq!(held, vec![bear], "the replica holds just the bear");
        }
        if let Some((_, at)) = kill {
            // The carcass commits a cell at a time while the bear eats it.
            let now = carcass_cells(&sim, at, 14);
            most_carcass = most_carcass.max(now);
            eaten = eaten.max(most_carcass - now);
            if eaten >= 10 {
                break;
            }
        }
    }
    eprintln!(
        "seed {seed}: start gap {start_gap:?} m, kill {kill:?}, bear tags {bear_tags:?}, \
         carcass of {most_carcass} cells, {eaten} eaten"
    );
    let (Some(_), Some(_)) = (deer_id, bear_id) else {
        panic!("a deer and a bear were not both born");
    };
    assert!(
        start_gap.is_some_and(|gap| gap > 20.0),
        "the bear was born too near the deer to stalk it: {start_gap:?}"
    );
    assert!(kill.is_some(), "the bear never caught the deer");
    let order = |tag| bear_tags.iter().position(|&t| t == tag);
    let (stalk, run, pounce, eat) = (
        order(LOCOMOTION_STALK).expect("the bear never stalked"),
        order(LOCOMOTION_RUN).expect("the bear never charged"),
        order(LOCOMOTION_POUNCE).expect("the bear never pounced"),
        order(LOCOMOTION_EAT).expect("the bear never ate"),
    );
    assert!(stalk < run && run < pounce && pounce < eat, "{bear_tags:?}");
    assert!(deer_ran_from_charge, "the deer never ran from the charge");
    assert!(
        most_carcass >= 10,
        "the kill left {most_carcass} carcass cells"
    );
    assert!(eaten >= 10, "the bear ate only {eaten} carcass cells");
}

/// One bear alone in the middle of the field, settled, and a player standing
/// `offset_x` metres from it. Returns the bear's id and the player's position.
fn bear_with_player_at(offset_x: f64) -> (Simulation, CreatureService, u64, [f64; 3]) {
    use spall_core::{EntityId, PLAYER_ENTITY_BASE};

    let mut sim = simulation();
    let middle = SIDE as f64 * 0.125;
    let director = AnimalDirector::new(vec![[middle, 0.5, middle]], &[0, 0, 0, 1, 0], 3)
        .unwrap()
        .with_radius_m(0.0, 1.0);
    let service = CreatureService::new(Box::new(director));
    for _ in 0..30 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
    }
    let bear = sim.creature_poses()[0];
    let player = [bear.translation_m[0] - offset_x, 0.6, bear.translation_m[2]];
    sim.add_player(EntityId::new(PLAYER_ENTITY_BASE + 1).unwrap(), player);
    (sim, service, bear.creature_id, player)
}

fn bear_record(sim: &Simulation, id: u64) -> spall_protocol::CreatureRecord {
    sim.creature_records()
        .into_iter()
        .find(|c| c.creature_id == id)
        .unwrap()
}

#[test]
fn a_bear_rears_bluffs_and_backs_off_from_a_player_who_comes_close() {
    let (mut sim, service, id, player) = bear_with_player_at(6.0);
    let mut tags = Vec::new();
    for _ in 0..900 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
        service.run_motion(&mut sim).unwrap();
        let tag = bear_record(&sim, id).locomotion_tag;
        if tags.last() != Some(&tag) {
            tags.push(tag);
        }
    }
    let at = |tag: u8| tags.iter().position(|t| *t == tag);
    let (Some(rear), Some(cuff)) = (at(LOCOMOTION_REAR), at(LOCOMOTION_POUNCE)) else {
        panic!("bear never reared and cuffed: tags {tags:?}");
    };
    assert!(rear < cuff, "cuffed before rearing: {tags:?}");
    assert!(
        tags[cuff..].contains(&LOCOMOTION_MOVE),
        "never backed off after the cuff: {tags:?}"
    );
    let end = bear_record(&sim, id).translation_m;
    let distance = (end[0] - player[0]).hypot(end[2] - player[2]);
    assert!(
        distance > 8.0,
        "bear ended only {distance:.1} m from the player"
    );
}

#[test]
fn a_bear_ignores_a_player_standing_well_off() {
    let (mut sim, service, id, _) = bear_with_player_at(30.0);
    for _ in 0..600 {
        sim.tick().unwrap();
        service.run_tick(&mut sim).unwrap();
        service.run_motion(&mut sim).unwrap();
        let tag = bear_record(&sim, id).locomotion_tag;
        assert!(
            tag != LOCOMOTION_REAR && tag != LOCOMOTION_POUNCE,
            "bear reacted to a player 30 m off"
        );
    }
}

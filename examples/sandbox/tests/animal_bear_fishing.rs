#![cfg(feature = "client")]
//! ENG-239: a hungry bear with no deer walks to the water beside the trout,
//! waits, swipes one out, and eats it. The caught trout despawns on every
//! client without leaving a carcass, and the fed bear does not fish again
//! right away.
//!
//! Run: `cargo test --release -p sandbox --features client --test animal_bear_fishing`.

use sandbox::animals::{AnimalDirector, LOCOMOTION_EAT, LOCOMOTION_POUNCE};
use spall_client::replica::{ReplicaConfig, ReplicaWorld};
use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_net::WireRecord;
use spall_server::creature_service::CreatureService;
use spall_sim::column_water::ColumnWaterSetup;
use spall_sim::{Simulation, SimulationConfig, fixtures};
use spall_voxel::{EditPlan, Sample, Volume};

/// Slab size in cells (0.25 m): 20 m east-west, 12 m north-south, 4 m tall.
const SIDE_X: i64 = 80;
const SIDE_Z: i64 = 48;
const HEIGHT: i64 = 16;
/// East edge of the pool's shallow half, metres; dry bank beyond.
const POOL_EAST_M: f64 = 9.0;

/// A stone slab with a 6 m pool sunk in its west part, as in `animal_trout`:
/// 1.5 m of water in the west half (x 3-6 m), 0.25 m in the east half (x 6-9
/// m), surface level with the bank. Dry bank runs east to 20 m.
fn pool_volume() -> Volume {
    let id = VolumeId::new(1).unwrap();
    let mut terrain = Volume::new(id, CellSizeCode::Quarter);
    let mut fill = |a: [i64; 3], b: [i64; 3], m: MaterialId| {
        terrain
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(a[0], a[1], a[2]),
                GlobalCell::new(b[0], b[1], b[2]),
                m,
            ))
            .unwrap();
    };
    fill([0, 0, 0], [SIDE_X - 1, 7, SIDE_Z - 1], fixtures::STONE);
    fill(
        [0, 8, 0],
        [SIDE_X - 1, HEIGHT - 1, SIDE_Z - 1],
        MaterialId::AIR,
    );
    fill([12, 2, 12], [23, 7, 35], MaterialId::AIR);
    fill([24, 7, 12], [35, 7, 35], MaterialId::AIR);
    terrain
}

fn simulation() -> Simulation {
    let mut water = Vec::new();
    for x in 12..36 {
        for z in 12..36 {
            let bed = if x < 24 { 2 } else { 7 };
            water.extend((bed..8).map(|y| GlobalCell::new(x, y, z)));
        }
    }
    let mut setup = fixtures::flat_terrain_setup();
    setup.terrain = pool_volume();
    setup.terrain_collider_region = (
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(SIDE_X - 1, HEIGHT - 1, SIDE_Z - 1),
    );
    setup.materials = sandbox::game::manifest();
    let mut config = SimulationConfig::new(setup);
    config.column_water = Some(ColumnWaterSetup::new(0, 32, water));
    let mut sim = Simulation::new(config).unwrap();
    sim.initialize_creature_host().unwrap();
    sim
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

/// Every solid terrain cell, to show no carcass was written.
fn solid_cells(sim: &Simulation) -> Vec<GlobalCell> {
    let volume = &sim.world().terrain().volume;
    let mut cells = Vec::new();
    for x in 0..SIDE_X {
        for z in 0..SIDE_Z {
            for y in 0..HEIGHT {
                let cell = GlobalCell::new(x, y, z);
                if matches!(volume.sample(cell), Ok(Sample::Filled(_))) {
                    cells.push(cell);
                }
            }
        }
    }
    cells
}

#[test]
fn a_hungry_bear_without_deer_catches_and_eats_a_trout() {
    for seed in 1..=8 {
        fishing(seed);
    }
}

fn fishing(seed: u64) {
    let mut sim = simulation();
    let mut replica = ReplicaWorld::from_baseline(pool_volume(), ReplicaConfig::default());
    let terrain_before = solid_cells(&sim);
    // One anchor on the east bank: the bear is born there, 3-7 m from the
    // shallow water; trout search six times as far and find the deep half.
    let director = AnimalDirector::new(vec![[12.0, 2.0, 6.0]], &[0, 0, 3, 1], seed)
        .unwrap()
        .with_radius_m(0.0, 1.5);
    let service = CreatureService::new(Box::new(director));

    let mut bear_id = None;
    let mut bear_tags = Vec::new();
    let mut caught: Option<(u64, u64, [f64; 3])> = None;
    let mut ate_until = None;
    for tick in 0..30_000_u64 {
        sim.tick().unwrap();
        let roster = service.run_tick(&mut sim).unwrap();
        let despawned = roster.despawned.clone();
        apply(&mut replica, roster.into_wire_records());
        let killed = service.run_motion(&mut sim).unwrap();
        assert!(killed.is_empty(), "fishing never goes through a kill");

        let records = sim.creature_records();
        bear_id = bear_id.or(records
            .iter()
            .find(|r| r.species == "bear")
            .map(|r| r.creature_id));
        let Some(bear) = bear_id else { continue };
        let bear_record = records.iter().find(|r| r.creature_id == bear).unwrap();
        let tag = bear_record.locomotion_tag;
        if bear_tags.last() != Some(&tag) {
            bear_tags.push(tag);
        }
        if let Some(&trout) = despawned.first() {
            assert!(caught.is_none(), "a second trout was caught while fed");
            assert_eq!(despawned.len(), 1);
            caught = Some((tick, trout, bear_record.translation_m));
            // The despawn reached the replica with the server's roster.
            assert!(
                replica
                    .creatures()
                    .records()
                    .all(|r| r.creature_id != trout),
                "the replica still holds the caught trout"
            );
            assert!(
                sim.creature_records()
                    .iter()
                    .all(|r| r.creature_id != trout)
            );
        }
        if caught.is_some()
            && ate_until.is_none()
            && tag != LOCOMOTION_EAT
            && bear_tags.contains(&LOCOMOTION_EAT)
        {
            ate_until = Some(tick);
        }
        // Thirty seconds after the meal, the bear has stayed fed.
        if ate_until.is_some_and(|done| tick > done + 60 * 30) {
            break;
        }
    }
    eprintln!("seed {seed}: caught {caught:?}, done eating {ate_until:?}, bear tags {bear_tags:?}");
    let (_, trout, bear_at) = caught.expect("the bear never caught a trout");
    assert!(
        bear_at[0] < POOL_EAST_M + 1.0,
        "the bear caught trout {trout} from {bear_at:?}, away from the water"
    );
    let pounce = bear_tags.iter().position(|&t| t == LOCOMOTION_POUNCE);
    let eat = bear_tags.iter().rposition(|&t| t == LOCOMOTION_EAT);
    assert!(
        pounce.zip(eat).is_some_and(|(p, e)| p < e),
        "the bear did not swipe, then eat: {bear_tags:?}"
    );
    assert!(ate_until.is_some(), "the bear never finished eating");
    assert_eq!(
        replica.creatures().records().count(),
        3,
        "the replica holds the bear and the two trout left"
    );
    // Nothing was written into the terrain: no carcass, no bites.
    assert!(sim.is_idle());
    assert_eq!(
        solid_cells(&sim),
        terrain_before,
        "the catch changed terrain"
    );
}

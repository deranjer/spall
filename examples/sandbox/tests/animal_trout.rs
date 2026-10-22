#![cfg(feature = "client")]
//! Trout are born only in live water deep enough for their bodies, swim inside
//! it, and never swim out of it, even when it drains away under them.

use sandbox::animals::AnimalDirector;
use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_server::creature_service::CreatureService;
use spall_sim::column_water::{AuthoritativeColumnWater, ColumnWaterSetup};
use spall_sim::{Simulation, SimulationConfig, fixtures};
use spall_voxel::{EditPlan, Volume};

/// Pool bed top in its deep west half (x < 6 m) and shallow east half, metres.
const DEEP_BED_M: f64 = 0.5;
const SHALLOW_BED_M: f64 = 1.75;
/// The level surface of the pool, metres.
const SURFACE_M: f64 = 2.0;
/// Middle of the pool, where the director's anchor sits.
const ANCHOR: [f64; 3] = [6.0, 2.0, 6.0];

fn bed_m(x: f64) -> f64 {
    if x < 6.0 { DEEP_BED_M } else { SHALLOW_BED_M }
}

/// A 12 m stone slab with a 6 m pool sunk in it: 1.5 m of water in the west
/// half, 0.25 m over a raised bed in the east half, surface level with the bank.
fn pool() -> (Simulation, Vec<GlobalCell>) {
    let id = VolumeId::new(1).unwrap();
    let mut terrain = Volume::new(id, CellSizeCode::Quarter);
    let fill = |terrain: &mut Volume, a: [i64; 3], b: [i64; 3], m: MaterialId| {
        terrain
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(a[0], a[1], a[2]),
                GlobalCell::new(b[0], b[1], b[2]),
                m,
            ))
            .unwrap();
    };
    fill(&mut terrain, [0, 0, 0], [47, 7, 47], fixtures::STONE);
    fill(&mut terrain, [0, 8, 0], [47, 15, 47], MaterialId::AIR);
    // Deep half: bed top at y = 2 cells; shallow half: bed top at y = 7 cells.
    fill(&mut terrain, [12, 2, 12], [23, 7, 35], MaterialId::AIR);
    fill(&mut terrain, [24, 7, 12], [35, 7, 35], MaterialId::AIR);
    let mut water = Vec::new();
    for x in 12..36 {
        for z in 12..36 {
            let bed = if x < 24 { 2 } else { 7 };
            water.extend((bed..8).map(|y| GlobalCell::new(x, y, z)));
        }
    }
    let mut setup = fixtures::flat_terrain_setup();
    setup.terrain = terrain;
    setup.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(47, 15, 47));
    setup.materials = sandbox::game::manifest();
    let mut config = SimulationConfig::new(setup);
    config.column_water = Some(ColumnWaterSetup::new(0, 32, water.clone()));
    let mut sim = Simulation::new(config).unwrap();
    sim.initialize_creature_host().unwrap();
    (sim, water)
}

/// Runs the director until `count` trout are placed, with motion if `swim`.
fn run(sim: &mut Simulation, service: &CreatureService, ticks: u32, swim: bool) {
    for _ in 0..ticks {
        sim.tick().unwrap();
        service.run_tick(sim).unwrap();
        if swim {
            service.run_motion(sim).unwrap();
        }
    }
}

fn trout_director(count: u32, seed: u64) -> CreatureService {
    let director = AnimalDirector::new(vec![ANCHOR], &[0, 0, count], seed)
        .unwrap()
        .with_radius_m(0.0, 1.0);
    CreatureService::new(Box::new(director))
}

fn trout(sim: &Simulation) -> Vec<(u64, [f64; 3])> {
    sim.creature_records()
        .into_iter()
        .filter(|c| c.species == "trout")
        .map(|c| (c.creature_id, c.translation_m))
        .collect()
}

/// The trout's root is in water, below the surface and above the bed under it.
fn assert_in_water(sim: &Simulation, root: [f64; 3], when: &str) {
    let surface = sim
        .column_water()
        .unwrap()
        .water_sample_at_m(root[0], root[1], root[2])
        .map(|w| w.surface_y);
    assert!(
        surface.is_some_and(|s| s > root[1]) && root[1] > bed_m(root[0]),
        "{when}: trout at {root:?} out of water (surface {surface:?})"
    );
}

#[test]
fn trout_are_born_only_in_water_deep_enough_for_their_bodies() {
    for seed in [1, 2, 3] {
        let (mut sim, _) = pool();
        let service = trout_director(6, seed);
        run(&mut sim, &service, 40, false);
        let placed = trout(&sim);
        assert_eq!(placed.len(), 6, "seed {seed}: every trout finds water");
        for (_, root) in placed {
            assert!(
                (3.0..6.0).contains(&root[0]) && (3.0..9.0).contains(&root[2]),
                "seed {seed}: trout at {root:?} is outside the deep half"
            );
            assert!(
                root[1] > DEEP_BED_M + 0.25 && root[1] < SURFACE_M,
                "seed {seed}: trout at {root:?} not under water above the bed"
            );
            assert_in_water(&sim, root, "birth");
        }
    }
}

/// Without water, trout are given up at once and the species after them still
/// spawn from the shared attempt budget.
#[test]
fn a_dry_world_gives_up_trout_without_starving_the_next_species() {
    let mut setup = fixtures::flat_terrain_setup();
    setup.materials = sandbox::game::manifest();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    sim.initialize_creature_host().unwrap();
    let director = AnimalDirector::new(vec![[3.0, 1.0, 3.0]], &[0, 0, 5, 1], 4)
        .unwrap()
        .with_radius_m(0.0, 1.0);
    let service = CreatureService::new(Box::new(director));
    run(&mut sim, &service, 60, false);
    let records = sim.creature_records();
    assert!(records.iter().all(|c| c.species != "trout"));
    assert_eq!(records.iter().filter(|c| c.species == "bear").count(), 1);
}

#[test]
fn trout_swim_without_leaving_the_water() {
    let (mut sim, _) = pool();
    let service = trout_director(4, 9);
    run(&mut sim, &service, 20, false);
    let start = trout(&sim);
    assert_eq!(start.len(), 4);
    let mut swam = false;
    for tick in 0..1800 {
        run(&mut sim, &service, 1, true);
        for (_, root) in trout(&sim) {
            assert_in_water(&sim, root, &format!("tick {tick}"));
        }
        swam |= sim
            .creature_motion_frame(&|_| true)
            .is_some_and(|f| f.entries.iter().any(|e| e.locomotion_tag == 1));
    }
    assert!(swam, "no trout ever swam (swim clip)");
    let moved = trout(&sim)
        .iter()
        .zip(&start)
        .map(|((_, a), (_, b))| (a[0] - b[0]).hypot(a[2] - b[2]))
        .fold(0.0, f64::max);
    assert!(moved > 1.0, "trout barely moved: {moved} m");
}

#[test]
fn a_trout_whose_water_drains_does_not_swim_into_air() {
    let (mut sim, _) = pool();
    let service = trout_director(4, 5);
    run(&mut sim, &service, 300, true);
    let before = trout(&sim);
    assert_eq!(before.len(), 4);
    // Drain the pool at once: the live water becomes an empty column world.
    let dry = AuthoritativeColumnWater::new(
        &sim.world().terrain().volume,
        ColumnWaterSetup::new(0, 32, Vec::new()),
    )
    .unwrap()
    .canonical_state();
    sim.restore_column_water(&dry, spall_fluid::columns::ColumnConfig::default())
        .unwrap();
    let mut last = before.clone();
    for tick in 0..600 {
        run(&mut sim, &service, 1, true);
        let now = trout(&sim);
        for ((_, was), (_, root)) in last.iter().zip(&now) {
            assert!(
                (root[0] - was[0]).abs() < 1e-9 && (root[2] - was[2]).abs() < 1e-9,
                "tick {tick}: stranded trout moved sideways {was:?} -> {root:?}"
            );
            assert!(
                root[1] <= was[1] + 1e-9,
                "tick {tick}: stranded trout rose {was:?} -> {root:?}"
            );
            assert!(root[1] > bed_m(root[0]) - 0.5, "trout sank into the bed");
        }
        last = now;
    }
    // Each ends lying on the bed under where it was, not hanging in the air.
    for (_, root) in &last {
        assert!(
            root[1] - bed_m(root[0]) < 0.3,
            "trout hangs in the air at {root:?}"
        );
    }
}

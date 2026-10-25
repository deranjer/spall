//! ENG-268: winter ice on settled shallow column water, walking on it, and
//! cracking through it.

use spall_core::{BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, Revision, VolumeId};
use spall_sim::column_water::ColumnWaterSetup;
use spall_sim::{IceSettings, Simulation, SimulationConfig, fixtures};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, EditPlan, Sample, Volume};

const STONE: MaterialId = MaterialId(1);
const ICE: MaterialId = MaterialId(2);
const CRACKED: MaterialId = MaterialId(3);
/// Water surface (cells): the top of cell 6, so ice goes in cell 7.
const SURFACE: i64 = 7;

/// 64 x 32 x 16 basin with walls; the west half (x < 32) has its floor at
/// y = 4 (0.75 m of water), the east half at y = 1 (1.5 m). One level pool.
fn simulation(ice: IceSettings) -> Simulation {
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    for bx in 0..2 {
        terrain
            .insert_brick(
                BrickCoord::new(bx, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
    }
    let mut plan = EditPlan::new(terrain.id());
    let mut water = Vec::new();
    for z in 0..16 {
        for x in 0..64 {
            let wall = x == 0 || x == 63 || z == 0 || z == 15;
            let floor = if x < 32 { 4 } else { 1 };
            for y in 0..32 {
                if y < floor || (wall && y < 12) {
                    plan.set(GlobalCell::new(x, y, z), STONE);
                } else if !wall && y < SURFACE {
                    water.push(GlobalCell::new(x, y, z));
                }
            }
        }
    }
    terrain.apply_edit(&plan).unwrap();
    let mut world = fixtures::flat_terrain_setup();
    let mut materials = world.materials.entries().to_vec();
    let mut cracked = materials[2].clone();
    cracked.id = CRACKED;
    cracked.name = "ice.cracked".into();
    materials.push(cracked);
    world.materials = spall_core::MaterialManifest::validated(materials).unwrap();
    world.terrain = terrain;
    world.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 31, 15));
    world.anchor = AnchorPlane::at(0);
    let mut config = SimulationConfig::new(world);
    config.column_water = Some(ColumnWaterSetup::new(0, 32, water));
    let mut sim = Simulation::new(config).unwrap();
    sim.set_weather_seed(7);
    sim.set_ice(ice);
    sim
}

fn settings() -> IceSettings {
    let mut s = IceSettings::new(ICE, CRACKED);
    s.settle_s = 0.2;
    s.crack_thin_per_s = 0.0;
    s.crack_thick_per_s = 0.0;
    s
}

fn material(sim: &Simulation, x: i64, y: i64, z: i64) -> Option<MaterialId> {
    match sim
        .world()
        .terrain()
        .volume
        .sample(GlobalCell::new(x, y, z))
    {
        Ok(Sample::Filled(m)) => Some(m),
        _ => None,
    }
}

fn ice_cells(sim: &Simulation) -> usize {
    (1..63)
        .flat_map(|x| (1..15).map(move |z| (x, z)))
        .flat_map(|(x, z)| (SURFACE..SURFACE + 3).map(move |y| (x, y, z)))
        .filter(|&(x, y, z)| material(sim, x, y, z) == Some(ICE))
        .count()
}

/// Ticks until the shallow half has frozen and the edits are done.
fn freeze(sim: &mut Simulation) {
    for _ in 0..600 {
        sim.tick().unwrap();
        if ice_cells(sim) >= 30 * 14 && sim.is_idle() {
            return;
        }
    }
    panic!("the pool did not freeze: {} ice cells", ice_cells(sim));
}

#[test]
fn only_settled_shallow_water_freezes_in_winter_and_no_water_moves() {
    let mut sim = simulation(settings());
    let volume = sim.column_water().unwrap().frame().volume_m3;
    for _ in 0..120 {
        sim.tick().unwrap();
    }
    assert_eq!(ice_cells(&sim), 0, "no ice outside winter");
    sim.set_season(3).unwrap();
    freeze(&mut sim);
    assert_eq!(
        sim.world().body_count(),
        0,
        "no ice split off as a loose body"
    );
    for x in 1..32 {
        for z in 1..15 {
            assert_eq!(
                material(&sim, x, SURFACE, z),
                Some(ICE),
                "shallow ({x},{z})"
            );
            assert_eq!(material(&sim, x, SURFACE - 1, z), None, "water stays below");
        }
    }
    for x in 33..63 {
        for z in 1..15 {
            assert_eq!(material(&sim, x, SURFACE, z), None, "deep ({x},{z})");
        }
    }
    // Freezing moves no water and the pool falls asleep again.
    let mut active = usize::MAX;
    for _ in 0..240 {
        let report = sim.tick().unwrap();
        active = report.column_water.unwrap().active_regions;
    }
    let after = sim.column_water().unwrap().frame().volume_m3;
    assert!((after - volume).abs() < 1e-9, "volume {volume} -> {after}");
    assert_eq!(active, 0, "the frozen pool sleeps");
    // Spring melts it.
    sim.set_season(0).unwrap();
    for _ in 0..240 {
        sim.tick().unwrap();
    }
    assert_eq!(ice_cells(&sim), 0, "the ice melted in spring");
}

const PLAYER: u64 = (1 << 48) + 1;

fn stand_on_ice(sim: &mut Simulation) -> EntityId {
    let entity = EntityId::new(PLAYER).unwrap();
    sim.add_player(entity, [4.0, (SURFACE + 1) as f64 * 0.25 + 0.2, 2.0]);
    entity
}

#[test]
fn a_player_stands_on_the_ice() {
    let mut sim = simulation(settings());
    sim.set_season(3).unwrap();
    freeze(&mut sim);
    let entity = stand_on_ice(&mut sim);
    for _ in 0..120 {
        sim.tick().unwrap();
    }
    let state = sim.player_state(entity).unwrap();
    assert!(state.grounded, "{state:?}");
    let top = (SURFACE + 1) as f64 * 0.25;
    assert!(
        (state.position_m[1] - top).abs() < 0.05,
        "feet at {:?}",
        state.position_m
    );
}

/// Freezes, puts a player on the ice with `ice` rates, and returns the ticks
/// the crack and the break committed at (relative to the player's arrival)
/// and the player's final feet height.
fn crack_through(ice: IceSettings) -> (Vec<u64>, Simulation, EntityId) {
    let quiet = settings();
    let mut sim = simulation(quiet);
    sim.set_season(3).unwrap();
    freeze(&mut sim);
    sim.set_ice(ice);
    let entity = stand_on_ice(&mut sim);
    let start = sim.current_tick().get();
    let mut events = Vec::new();
    for _ in 0..900 {
        let report = sim.tick().unwrap();
        for (_, committed) in &report.committed {
            let ops = &committed.topology.ops;
            if ops.iter().any(|op| {
                matches!(op, spall_protocol::TopologyOp::CellRun { material, .. }
                    if *material == CRACKED || material.is_air())
            }) {
                events.push(sim.current_tick().get() - start);
            }
        }
        if events.len() == 2 {
            break;
        }
    }
    for _ in 0..120 {
        sim.tick().unwrap();
    }
    (events, sim, entity)
}

fn breaking() -> IceSettings {
    let mut s = settings();
    s.crack_thin_per_s = 1.5;
    s.break_per_s = 1.5;
    s
}

#[test]
fn a_crack_then_a_break_drops_the_player_into_the_water() {
    let (events, sim, entity) = crack_through(breaking());
    assert_eq!(sim.world().body_count(), 0, "the hole detached nothing");
    assert_eq!(events.len(), 2, "a warning crack, then a break: {events:?}");
    let feet = sim.player_state(entity).unwrap().position_m;
    assert!(feet[1] < SURFACE as f64 * 0.25, "fell through: {feet:?}");
    let water = sim
        .column_water()
        .unwrap()
        .frame()
        .sample_at_m(feet[0], feet[1] + 0.1, feet[2])
        .expect("the player is in the water");
    assert!(water.surface_y > feet[1] + 0.3, "{water:?} over {feet:?}");
}

#[test]
fn cracking_replays_identically_in_the_same_binary() {
    let (a, _, _) = crack_through(breaking());
    let (b, _, _) = crack_through(breaking());
    assert_eq!(a, b);
    assert_eq!(a.len(), 2);
}

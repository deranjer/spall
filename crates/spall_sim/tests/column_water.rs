//! ENG-167 tests for the stacked-column authoritative water, ported from
//! `authoritative_water.rs` (breach, placement, recovery) plus sources.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, Revision, SphereBrush, VolumeId,
};
use spall_protocol::RequestId;
use spall_sim::column_water::{AuthoritativeColumnWater, ColumnWaterSetup};
use spall_sim::fixtures;
use spall_sim::{EditIntent, EditKind, EditTarget, Simulation, SimulationConfig};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, EditPlan, Sample, Volume};

const STONE: MaterialId = MaterialId(1);
const DAM_X: i64 = 20;

/// 64 x 32 x 16 basin: floor y<2, walls on every side, a dam at x=20.
fn basin() -> Volume {
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
    for z in 0..16 {
        for y in 0..32 {
            for x in 0..64 {
                let wall = x == 0 || x == 63 || z == 0 || z == 15;
                if y < 2 || (wall && y < 20) || (x == DAM_X && y < 10) {
                    plan.set(GlobalCell::new(x, y, z), STONE);
                }
            }
        }
    }
    terrain.apply_edit(&plan).unwrap();
    terrain
}

fn pool_cells() -> Vec<GlobalCell> {
    let mut cells = Vec::new();
    for z in 1..15 {
        for y in 2..9 {
            for x in 1..DAM_X {
                cells.push(GlobalCell::new(x, y, z));
            }
        }
    }
    cells
}

fn simulation(setup: ColumnWaterSetup) -> Simulation {
    let mut world = fixtures::flat_terrain_setup();
    world.terrain = basin();
    world.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 31, 15));
    world.anchor = AnchorPlane::at(0);
    let mut config = SimulationConfig::new(world);
    config.column_water = Some(setup);
    Simulation::new(config).unwrap()
}

fn brush_at(cell: GlobalCell, radius_cells: i64) -> SphereBrush {
    let half = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(
            cell.x * BRUSH_UNIT + half,
            cell.y * BRUSH_UNIT + half,
            cell.z * BRUSH_UNIT + half,
        ),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn volume_m3(sim: &Simulation) -> f64 {
    sim.column_water().unwrap().frame().volume_m3
}

fn cells_right_of_dam(sim: &Simulation) -> f64 {
    let w = sim.column_water().unwrap().world();
    (DAM_X + 1..63)
        .flat_map(|x| (1..15).map(move |z| (x, z)))
        .map(|(x, z)| w.column_volume(x, z))
        .sum()
}

fn commit(sim: &mut Simulation, id: u64, ticks: u32) -> spall_sim::TickReport {
    for _ in 0..ticks {
        let report = sim.tick().unwrap();
        if report.committed.iter().any(|(r, _)| *r == RequestId(id)) {
            return report;
        }
    }
    panic!(
        "edit {id} did not commit: {:?}",
        sim.action_status(RequestId(id))
    );
}

fn edit(sim: &mut Simulation, id: u64, kind: EditKind, at: GlobalCell, radius: i64) {
    sim.submit(EditIntent {
        request_id: RequestId(id),
        actor: EntityId::new(1).unwrap(),
        target: EditTarget::Terrain,
        kind,
        brush: brush_at(at, radius),
        explosion: None,
    })
    .unwrap();
}

#[test]
fn water_is_owned_seeded_at_rest_and_costs_nothing_while_idle() {
    let mut sim = simulation(ColumnWaterSetup::new(0, 32, pool_cells()));
    let water = sim.column_water().unwrap();
    assert_eq!(water.seed_report().rejected_cells, 0);
    let seeded = water.frame().volume_m3;
    assert!((seeded - pool_cells().len() as f64 * 0.25f64.powi(3)).abs() < 1e-9);
    assert!(water.frame().regions.len() >= 2 || !water.frame().regions.is_empty());
    for _ in 0..180 {
        let report = sim.tick().unwrap();
        let m = report.column_water.expect("column water reports each tick");
        assert_eq!(m.active_regions, 0, "a held pool is at rest and asleep");
        assert!((m.volume_m3 - seeded).abs() < 1e-9);
    }
    assert_eq!(cells_right_of_dam(&sim), 0.0);
}

#[test]
fn breaching_the_dam_refreshes_terrain_then_flows_conservatively() {
    let mut sim = simulation(ColumnWaterSetup::new(0, 32, pool_cells()));
    let start = volume_m3(&sim);
    edit(&mut sim, 2, EditKind::Cut, GlobalCell::new(DAM_X, 5, 7), 3);
    let report = commit(&mut sim, 2, 60);
    let m = report.column_water.unwrap();
    assert!(m.columns_remapped > 0, "the cut remapped columns: {m:?}");
    assert!(
        matches!(
            sim.world()
                .terrain()
                .volume
                .sample(GlobalCell::new(DAM_X, 5, 7)),
            Ok(Sample::Empty { .. })
        ),
        "the dam cell is open"
    );
    let mut stepped = 0;
    let mut ticks = 0;
    // Equalising through the breach takes minutes of simulated time; run until
    // every region sleeps again.
    while ticks < 60 * 600 {
        let report = sim.tick().unwrap();
        let m = report.column_water.unwrap();
        stepped += u32::from(m.stepped);
        ticks += 1;
        assert!(
            (m.volume_m3 - start).abs() <= 1e-9 * start,
            "volume drifted to {} from {start}",
            m.volume_m3
        );
        if ticks > 120 && m.active_regions == 0 {
            break;
        }
    }
    println!("rested after {:.1} s", f64::from(ticks) / 60.0);
    assert_eq!(
        sim.column_water().unwrap().world().active_region_count(),
        0,
        "the water must come to rest and sleep"
    );
    assert!(stepped > 0);
    assert!(cells_right_of_dam(&sim) > 1.0, "the breach moved water");
    // Both chambers end level and the region sleeps again.
    let w = sim.column_water().unwrap().world();
    let left = w.spans_in_column(10, 7)[0];
    let right = w.spans_in_column(40, 7)[0];
    assert!(
        (left.head_y - right.head_y).abs() < 0.2,
        "levels {} vs {}",
        left.head_y,
        right.head_y
    );
}

#[test]
fn placing_a_block_into_water_displaces_and_conserves() {
    let mut sim = simulation(ColumnWaterSetup::new(0, 32, pool_cells()));
    let start = volume_m3(&sim);
    let target = GlobalCell::new(10, 4, 7);
    edit(&mut sim, 3, EditKind::Place(STONE), target, 2);
    let report = commit(&mut sim, 3, 60);
    let m = report.column_water.unwrap();
    assert!(m.relocated_m3 > 0.0, "displaced water was relocated: {m:?}");
    assert!(matches!(
        sim.world().terrain().volume.sample(target).unwrap(),
        Sample::Filled(_)
    ));
    for _ in 0..(20.0 * 30.0) as u32 {
        let m = sim.tick().unwrap().column_water.unwrap();
        assert!((m.volume_m3 - start).abs() <= 1e-9 * start);
    }
    assert!((volume_m3(&sim) - start).abs() <= 1e-9 * start);
    // No water inside the new solid: its column has no span covering it.
    let spans = sim.column_water().unwrap().world().spans_in_column(10, 7);
    assert!(
        spans
            .iter()
            .all(|s| !(s.floor_y <= target.y && target.y < s.ceiling_y))
    );
}

#[test]
fn canonical_state_round_trips_and_restores_to_rest() {
    let mut sim = simulation(ColumnWaterSetup::new(0, 32, pool_cells()));
    edit(&mut sim, 2, EditKind::Cut, GlobalCell::new(DAM_X, 5, 7), 3);
    commit(&mut sim, 2, 60);
    for _ in 0..200 {
        sim.tick().unwrap();
    }
    let water = sim.column_water().unwrap();
    let state = water.canonical_state();
    state.validate().unwrap();
    assert!(!state.regions.is_empty());

    let restored = AuthoritativeColumnWater::restore(
        &sim.world().terrain().volume,
        &state,
        water.setup().config,
    )
    .unwrap();
    assert_eq!(restored.canonical_state(), state, "idempotent round trip");
    assert!((restored.frame().volume_m3 - water.frame().volume_m3).abs() < 1e-6);
    assert_eq!(restored.frame().seq, state.frame_seq);
    assert_eq!(
        restored.world().active_region_count(),
        0,
        "restores at rest"
    );

    // A saved span that no longer matches terrain fails loudly.
    let mut bad = state.clone();
    bad.regions[0].spans[0].floor_y += 1;
    let err = AuthoritativeColumnWater::restore(
        &sim.world().terrain().volume,
        &bad,
        water.setup().config,
    )
    .err()
    .expect("mismatched span must be refused");
    assert!(err.to_string().contains("no terrain span"), "{err}");
}

#[test]
fn springs_and_drains_keep_accounted_volume() {
    let mut setup = ColumnWaterSetup::new(0, 32, Vec::new());
    setup.sources = vec![GlobalCell::new(5, 2, 7)];
    setup.sinks = vec![GlobalCell::new(15, 2, 7)];
    let mut sim = simulation(setup);
    for _ in 0..(20.0 * 20.0) as u32 {
        sim.tick().unwrap();
    }
    let frame = sim.column_water().unwrap().frame();
    assert!(frame.spring_added_m3 > 0.0, "spring added water");
    assert!(frame.drain_removed_m3 > 0.0, "drain removed water");
    let held = frame.volume_m3;
    let expected = frame.spring_added_m3 - frame.drain_removed_m3;
    assert!(
        (held - expected).abs() < 1e-6,
        "held {held} vs added-removed {expected}"
    );
}

#[test]
fn gated_spring_only_runs_while_switched_on() {
    let mut setup = ColumnWaterSetup::new(0, 32, Vec::new());
    setup.gated_sources[0] = vec![GlobalCell::new(5, 2, 7)];
    let mut sim = simulation(setup);
    for _ in 0..120 {
        sim.tick().unwrap();
    }
    assert_eq!(sim.column_water().unwrap().frame().volume_m3, 0.0);
    sim.set_water_spring_rate(1);
    for _ in 0..120 {
        sim.tick().unwrap();
    }
    assert!(sim.column_water().unwrap().frame().volume_m3 > 0.0);
}

#[test]
fn a_discharge_spring_adds_exactly_its_rate_and_survives_a_save() {
    use spall_sim::column_water::Discharge;
    let rate = 12.0; // cells^3 per second
    let mut setup = ColumnWaterSetup::new(0, 32, Vec::new());
    setup.discharges = vec![Discharge {
        cell: GlobalCell::new(5, 2, 7),
        flow_cells3_s: rate,
    }];
    let mut sim = simulation(setup);
    let seconds = 20.0;
    for _ in 0..(seconds * 20.0) as u32 {
        sim.tick().unwrap();
    }
    let water = sim.column_water().unwrap();
    let frame = water.frame();
    // The stated rate, not a refill-to-full: the fluid time that has run times the rate.
    let expected_m3 = frame.fluid_time_s * rate * 0.25f64.powi(3);
    assert!(
        (frame.spring_added_m3 - expected_m3).abs() < 1e-6 * expected_m3.max(1.0),
        "added {} vs {expected_m3}",
        frame.spring_added_m3
    );
    assert!(
        (frame.volume_m3 - frame.spring_added_m3).abs() < 1e-6,
        "nothing is lost or made"
    );

    // The spring is part of the saved state and comes back with the same rate.
    let state = water.canonical_state();
    assert_eq!(
        state.version,
        spall_protocol::column_water::COLUMN_WATER_VERSION
    );
    assert_eq!(state.discharges.len(), 1);
    let restored = AuthoritativeColumnWater::restore(
        &sim.world().terrain().volume,
        &state,
        water.setup().config,
    )
    .unwrap();
    assert_eq!(restored.setup().discharges, water.setup().discharges);

    // A save from before discharge springs (schema 3) is refused, not migrated.
    let mut old = state.clone();
    old.version = 3;
    assert!(old.validate().is_err());
}

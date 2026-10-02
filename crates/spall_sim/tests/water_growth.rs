//! ENG-120: an authored fluid domain grows when real terrain edits land near
//! its boundary, conserving water exactly and letting it flow past the old box.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, Revision, SphereBrush, VolumeId,
};
use spall_fluid::DomainSpec;
use spall_fluid::grid_mac::MacConfig;
use spall_protocol::RequestId;
use spall_sim::fixtures;
use spall_sim::{
    EditIntent, EditTarget, GrowthPlan, GrowthRefusal, Simulation, SimulationConfig,
    WaterExecution, WaterGrowth, WaterSetup,
};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, BrickBounds, EditPlan, Volume};

const STONE: MaterialId = MaterialId(1);

/// A 64 x 32 x 32 stone block (two bricks, bounded) with a water-filled
/// cavity at x 6..=19, y 4..=13, z 4..=27 and a fluid domain covering only
/// x 0..=23, y 0..=15, z 0..=31. Everything is anchored to the floor.
fn world(growth: Option<WaterGrowth>, execution: WaterExecution) -> Simulation {
    let id = VolumeId::new(1).unwrap();
    let bounds = BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(1, 0, 0)).unwrap();
    let mut terrain = Volume::bounded(id, CellSizeCode::Quarter, bounds);
    for x in 0..2 {
        terrain
            .insert_brick(BrickCoord::new(x, 0, 0), Brick::uniform(STONE, Revision(1)))
            .unwrap();
    }
    let mut cavity = EditPlan::new(id);
    let mut initial = Vec::new();
    for z in 4..28 {
        for y in 4..14 {
            for x in 6..20 {
                let cell = GlobalCell::new(x, y, z);
                cavity.set(cell, MaterialId::AIR);
                if y < 8 {
                    initial.push((cell, 1.0));
                }
            }
        }
    }
    terrain.apply_edit(&cavity).unwrap();

    let mut setup = fixtures::flat_terrain_setup();
    setup.terrain = terrain;
    setup.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 31, 31));
    setup.anchor = AnchorPlane::at(0);
    setup.materials = fixtures::stone_manifest();

    let domain = DomainSpec::new(GlobalCell::new(0, 0, 0), [24, 16, 32], 1 << 20).unwrap();
    let mut water = WaterSetup::new(domain, initial);
    water.config = MacConfig {
        open_top: false,
        ..MacConfig::default()
    };
    water.execution = execution;
    water.growth = growth;
    let mut config = SimulationConfig::new(setup);
    config.water = Some(water);
    Simulation::new(config).unwrap()
}

fn default_growth() -> WaterGrowth {
    WaterGrowth {
        trigger_voxels: 8,
        margin_voxels: 8,
        max_voxel_cells: 1 << 20,
    }
}

fn brush(x: i64, y: i64, z: i64, radius: i64) -> SphereBrush {
    let half = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(
            x * BRUSH_UNIT + half,
            y * BRUSH_UNIT + half,
            z * BRUSH_UNIT + half,
        ),
        radius * BRUSH_UNIT,
    )
    .unwrap()
}

fn cut(sim: &mut Simulation, request: u64, x: i64, y: i64, z: i64, radius: i64) {
    sim.submit(EditIntent::cut(
        RequestId(request),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush(x, y, z, radius),
    ))
    .unwrap();
}

fn run(sim: &mut Simulation, ticks: u32) {
    for _ in 0..ticks {
        sim.tick().unwrap();
    }
}

fn total_m3(sim: &Simulation) -> f64 {
    let grid = sim.water().unwrap().grid().unwrap();
    grid.water_volume_m3() + grid.trapped_volume_m3() + grid.cumulative_open_outflow_m3()
}

/// Water volume in fluid cells at voxel x >= `x_min` (needs those cells to exist).
fn water_m3_at_or_beyond(sim: &Simulation, x_min: i64) -> f64 {
    let water = sim.water().unwrap();
    let grid = water.grid().unwrap();
    let origin = water.domain().origin();
    let dims = water.domain().dimensions();
    let mut sum = 0.0;
    for z in 0..i64::from(dims[2]) {
        for y in 0..i64::from(dims[1]) {
            for x in x_min..origin.x + i64::from(dims[0]) {
                if let Some(f) = grid.fraction_at(GlobalCell::new(x, origin.y + y, origin.z + z)) {
                    sum += f;
                }
            }
        }
    }
    sum * 0.25f64.powi(3)
}

#[test]
fn an_edit_wholly_inside_the_domain_never_grows_it() {
    let mut sim = world(Some(default_growth()), WaterExecution::Inline);
    let before = sim.water().unwrap().domain();
    run(&mut sim, 5);
    cut(&mut sim, 1, 20, 6, 16, 2); // box x 17..=23: inside
    run(&mut sim, 30);
    let water = sim.water().unwrap();
    assert_eq!(water.domain(), before);
    assert_eq!(water.growths(), 0);
}

#[test]
fn a_fixed_domain_has_no_cells_beyond_its_box_and_water_stays_inside() {
    let mut sim = world(None, WaterExecution::Inline);
    run(&mut sim, 5);
    cut(&mut sim, 1, 20, 6, 16, 2);
    run(&mut sim, 30);
    cut(&mut sim, 2, 26, 6, 16, 3);
    run(&mut sim, 200);
    let water = sim.water().unwrap();
    assert_eq!(water.domain().dimensions(), [24, 16, 32]);
    assert!(
        water
            .grid()
            .unwrap()
            .fraction_at(GlobalCell::new(26, 6, 16))
            .is_none(),
        "without growth the dug tunnel lies outside the simulated box"
    );
}

#[test]
fn growth_conserves_volume_and_lets_water_flow_past_the_old_box() {
    let mut sim = world(Some(default_growth()), WaterExecution::Inline);
    run(&mut sim, 5);
    cut(&mut sim, 1, 20, 6, 16, 2);
    run(&mut sim, 60);
    let before_volume = total_m3(&sim);
    let before_seq = sim.water().unwrap().frame().seq;
    assert!(before_volume > 1.0, "the pool holds water");
    assert_eq!(water_m3_at_or_beyond(&sim, 24), 0.0);

    cut(&mut sim, 2, 26, 6, 16, 3); // box x 22..=30: crosses x=23
    let report = sim.tick().unwrap();
    let metrics = report.water.expect("water reports every tick");
    assert_eq!(metrics.domain_growths, 1, "one growth: {metrics:?}");
    let water = sim.water().unwrap();
    let domain = water.domain();
    assert_eq!(domain.origin(), GlobalCell::new(0, 0, 0), "grew +x only");
    assert!(
        domain.dimensions()[0] >= 38,
        "edit box x_max 30 plus the 8-voxel margin"
    );
    assert_eq!(&domain.dimensions()[1..], &[16, 32], "y and z unchanged");
    assert!(water.frame().seq > before_seq, "frame sequence continues");
    let after = total_m3(&sim);
    assert!(
        (after - before_volume).abs() < 1.0e-9,
        "growth must conserve water exactly: {before_volume} -> {after}"
    );

    run(&mut sim, 300);
    let flowed = water_m3_at_or_beyond(&sim, 24);
    assert!(
        flowed > 1.0e-3,
        "water should reach the tunnel beyond the old box, got {flowed} m^3"
    );
    assert!((total_m3(&sim) - before_volume).abs() < 1.0e-8);
}

#[test]
fn growth_keeps_a_worker_region_on_its_worker() {
    let mut sim = world(
        Some(default_growth()),
        WaterExecution::Worker {
            step_dt_s: 1.0 / 60.0,
        },
    );
    run(&mut sim, 5);
    cut(&mut sim, 1, 26, 6, 16, 3);
    run(&mut sim, 5);
    let water = sim.water().unwrap();
    assert_eq!(water.growths(), 1);
    assert!(matches!(water.execution(), WaterExecution::Worker { .. }));
    let seq = water.frame().seq;
    // The new worker must keep publishing frames.
    for _ in 0..200 {
        sim.tick().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        if sim.water().unwrap().frame().seq > seq + 2 {
            return;
        }
    }
    panic!("grown worker region stopped advancing");
}

#[test]
fn growth_refused_over_the_cap_is_counted_and_changes_nothing() {
    let mut sim = world(
        Some(WaterGrowth {
            max_voxel_cells: 24 * 16 * 32, // exactly the current domain
            ..default_growth()
        }),
        WaterExecution::Inline,
    );
    run(&mut sim, 5);
    let before = sim.water().unwrap().domain();
    cut(&mut sim, 1, 26, 6, 16, 3);
    let metrics = sim.tick().unwrap().water.unwrap();
    assert_eq!(metrics.growth_refused, 1);
    assert_eq!(metrics.domain_growths, 0);
    assert_eq!(sim.water().unwrap().domain(), before);
}

#[test]
fn growth_shrinks_its_margin_to_avoid_a_neighbour_and_refuses_when_it_cannot() {
    let setup = |origin_x: i64| {
        let mut sim = world(Some(default_growth()), WaterExecution::Inline);
        let domain =
            DomainSpec::new(GlobalCell::new(origin_x, 0, 0), [8, 16, 32], 1 << 20).unwrap();
        sim.add_water_region(WaterSetup::new(domain, Vec::new()))
            .unwrap();
        sim
    };
    // Neighbour at x=36: the 8-voxel margin (to x=38) would overlap, margin 0
    // (to x=30) does not.
    let mut sim = setup(36);
    run(&mut sim, 3);
    cut(&mut sim, 1, 26, 6, 16, 3);
    run(&mut sim, 2);
    let hi = {
        let d = sim.water().unwrap().domain();
        d.origin().x + i64::from(d.dimensions()[0]) - 1
    };
    assert!(
        (30..36).contains(&hi),
        "grew without overlapping: x_max={hi}"
    );
    // Neighbour at x=26: even the bare edit box overlaps -> refuse.
    let mut sim = setup(26);
    run(&mut sim, 3);
    cut(&mut sim, 1, 26, 6, 16, 3);
    run(&mut sim, 2);
    let water = sim.water().unwrap();
    assert_eq!(water.growths(), 0);
    assert_eq!(water.domain().dimensions()[0], 24);
}

#[test]
fn plan_growth_waits_for_unresident_bricks_instead_of_pausing_the_region() {
    let sim = world(Some(default_growth()), WaterExecution::Inline);
    // Terrain with the second brick evicted: growth into it must wait.
    let mut terrain = sim.world().terrain().volume.clone();
    terrain.evict_brick(BrickCoord::new(1, 0, 0));
    let plan = sim
        .water()
        .unwrap()
        .plan_growth(&terrain, &[([22, 4, 12], [30, 10, 20])], &[]);
    assert_eq!(
        plan,
        GrowthPlan::Refused(GrowthRefusal::WaitingForResidency)
    );
}

#[test]
fn a_grown_domain_round_trips_through_canonical_state() {
    let mut sim = world(Some(default_growth()), WaterExecution::Inline);
    run(&mut sim, 5);
    cut(&mut sim, 1, 20, 6, 16, 2);
    run(&mut sim, 30);
    cut(&mut sim, 2, 26, 6, 16, 3);
    run(&mut sim, 120);
    let water = sim.water().unwrap();
    assert_eq!(water.growths(), 1);
    let state = water.canonical_state();
    assert_eq!(state.voxel_dimensions, water.domain().dimensions());
    let restored =
        spall_sim::AuthoritativeWater::restore(&sim.world().terrain().volume, &state).unwrap();
    assert_eq!(restored.domain(), water.domain());
    let (a, b) = (water.grid().unwrap(), restored.grid().unwrap());
    assert_eq!(a.fractions().len(), b.fractions().len());
    assert!((a.water_volume_m3() - b.water_volume_m3()).abs() < 1.0e-9);
}

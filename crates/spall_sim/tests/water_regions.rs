use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::{DomainSpec, grid_mac::GridReservoirFixture};
use spall_sim::{Simulation, SimulationConfig, WaterSetup};
use spall_voxel::{Brick, EditPlan, Sample, Volume};
use std::time::Instant;

fn regions() -> (Simulation, Vec<WaterSetup>) {
    let fixture = GridReservoirFixture::new(1, false).unwrap();
    let dims = fixture.grid().spec().dimensions();
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    let mut solid = EditPlan::new(terrain.id());
    let mut setups = Vec::new();
    for region in 0..3 {
        let offset = region * 32;
        terrain
            .insert_brick(
                BrickCoord::new(region, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        let mut fractions = Vec::new();
        for z in 0..i64::from(dims[2]) {
            for y in 0..i64::from(dims[1]) {
                for x in 0..i64::from(dims[0]) {
                    let local = GlobalCell::new(x, y, z);
                    let global = GlobalCell::new(x + offset, y, z);
                    if let Sample::Filled(m) = fixture.volume().sample(local).unwrap() {
                        solid.set(global, m);
                    }
                    if let Some(f) = fixture.grid().fraction_at(local)
                        && f > 0.0
                    {
                        fractions.push((global, f));
                    }
                }
            }
        }
        setups.push(WaterSetup::new(
            DomainSpec::new(GlobalCell::new(offset, 0, 0), dims, 4096).unwrap(),
            fractions,
        ));
    }
    terrain.apply_edit(&solid).unwrap();
    let mut world = spall_sim::fixtures::flat_terrain_setup();
    world.terrain = terrain;
    world.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(87, 11, 7));
    let mut cfg = SimulationConfig::new(world);
    cfg.water = Some(setups[0].clone());
    let mut sim = Simulation::new(cfg).unwrap();
    for setup in &setups[1..] {
        sim.add_water_region(setup.clone()).unwrap();
    }
    (sim, setups)
}

#[test]
fn three_reservoir_regions_conserve_and_round_trip_independently() {
    let (mut sim, setups) = regions();
    assert!(
        sim.add_water_region(setups[0].clone()).is_err(),
        "overlapping pressure domains reject"
    );
    let start: Vec<_> = sim.water_regions().map(|w| w.frame().volume_m3).collect();
    let started = Instant::now();
    let mut fluid_us = 0u128;
    let mut peak_us = 0u128;
    let mut costs = Vec::new();
    for _ in 0..600 {
        let tick_started = Instant::now();
        let report = sim.tick().unwrap();
        assert_eq!(report.water_regions.len(), 2);
        fluid_us += report.water.as_ref().unwrap().step_duration.as_micros()
            + report
                .water_regions
                .iter()
                .map(|m| m.step_duration.as_micros())
                .sum::<u128>();
        let cost = tick_started.elapsed().as_micros();
        peak_us = peak_us.max(cost);
        costs.push(cost);
        for (water, initial) in sim.water_regions().zip(&start) {
            assert!((water.frame().volume_m3 - initial).abs() < 1e-8);
        }
    }
    let states: Vec<_> = sim.water_regions().map(|w| w.canonical_state()).collect();
    let elapsed = started.elapsed();
    sim.restore_water_regions(&states).unwrap();
    assert_eq!(
        sim.water_regions()
            .map(|w| w.canonical_state())
            .collect::<Vec<_>>(),
        states
    );
    costs.sort_unstable();
    eprintln!(
        "ENG105 total tick p95 {} us, p99 {} us",
        costs[569], costs[593]
    );
    eprintln!(
        "ENG105 three full scale-1 reservoirs: 600 ticks, cells {}, total mean {:.3} ms/tick, fluid mean {:.3} ms/tick, peak {} us, volume {:?}",
        states.iter().map(|s| s.fractions.len()).sum::<usize>(),
        elapsed.as_secs_f64() * 1000.0 / 600.0,
        fluid_us as f64 / 600000.0,
        peak_us,
        start
    );
}

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, Revision, SphereBrush, VolumeId,
};
use spall_fluid::grid_mac::{GridReservoirFixture, MacConfig};
use spall_protocol::RequestId;
use spall_sim::fixtures;
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, WaterSetup};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, EditPlan, Sample, Volume};

fn simulation_with_reservoir_water() -> (Simulation, spall_fluid::DomainSpec) {
    let fixture = GridReservoirFixture::new(1, false).unwrap();
    let spec = fixture.grid().spec();
    let dims = spec.dimensions();
    let mut initial_fractions = Vec::new();
    for z in 0..dims[2] as i64 {
        for y in 0..dims[1] as i64 {
            for x in 0..dims[0] as i64 {
                let cell = GlobalCell::new(x, y, z);
                if let Some(fraction) = fixture.grid().fraction_at(cell)
                    && fraction > 0.0
                {
                    initial_fractions.push((cell, fraction));
                }
            }
        }
    }
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    terrain
        .insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )
        .unwrap();
    let mut terrain_cells = EditPlan::new(terrain.id());
    for z in 0..dims[2] as i64 {
        for y in 0..dims[1] as i64 {
            for x in 0..dims[0] as i64 {
                let cell = GlobalCell::new(x, y, z);
                if let Sample::Filled(material) = fixture.volume().sample(cell).unwrap() {
                    terrain_cells.set(cell, material);
                }
            }
        }
    }
    terrain.apply_edit(&terrain_cells).unwrap();

    let mut world = fixtures::flat_terrain_setup();
    world.terrain = terrain;
    world.terrain_collider_region = (
        spec.origin(),
        GlobalCell::new(
            i64::from(dims[0]) - 1,
            i64::from(dims[1]) - 1,
            i64::from(dims[2]) - 1,
        ),
    );
    world.anchor = AnchorPlane::at(0);

    let mut config = SimulationConfig::new(world);
    let mut water = WaterSetup::new(spec, initial_fractions);
    water.config = MacConfig {
        open_top: false,
        ..MacConfig::default()
    };
    config.water = Some(water);
    (Simulation::new(config).unwrap(), spec)
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

fn water_volume_in_x_range(
    sim: &Simulation,
    spec: spall_fluid::DomainSpec,
    x_range: std::ops::Range<i64>,
) -> f64 {
    let water = sim.water().unwrap().grid().unwrap();
    let dims = spec.dimensions();
    let mut volume_fraction = 0.0;
    for z in 0..dims[2] as i64 {
        for y in 0..dims[1] as i64 {
            for x in x_range.clone() {
                if let Some(fraction) = water.fraction_at(GlobalCell::new(x, y, z)) {
                    volume_fraction += fraction;
                }
            }
        }
    }
    volume_fraction * 0.25f64.powi(3)
}

#[test]
fn water_is_owned_and_advanced_by_the_simulation_tick() {
    let (mut sim, _) = simulation_with_reservoir_water();
    let start_volume = sim.water().unwrap().grid().unwrap().water_volume_m3();
    let report = sim.tick().unwrap();
    let water_report = report.water.expect("configured water reports each tick");

    assert!(water_report.step.is_some());
    assert_eq!(water_report.skipped_ticks, 0);
    assert!((sim.water().unwrap().grid().unwrap().water_volume_m3() - start_volume).abs() < 1.0e-9);
}

#[test]
fn authoritative_dam_breach_refreshes_boundary_then_flows_conservatively() {
    let (mut sim, spec) = simulation_with_reservoir_water();
    let starting_volume = sim.water().unwrap().grid().unwrap().water_volume_m3();
    let downstream_before = water_volume_in_x_range(&sim, spec, 13..23);
    let canal_request = RequestId(1);
    sim.submit(EditIntent::cut(
        canal_request,
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_at(GlobalCell::new(12, 1, 4), 1),
    ))
    .unwrap();

    let mut canal_committed = false;
    for _ in 0..60 {
        let report = sim.tick().unwrap();
        canal_committed |= report.committed.iter().any(|(id, _)| *id == canal_request);
        if (sim.water().unwrap().grid().unwrap().water_volume_m3() - starting_volume).abs()
            >= 1.0e-8
        {
            panic!("water volume changed after canal edit");
        }
        if canal_committed {
            break;
        }
    }
    assert!(canal_committed, "real edit intent must excavate the canal");

    let breach_request = RequestId(2);
    sim.submit(EditIntent::cut(
        breach_request,
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_at(GlobalCell::new(12, 3, 4), 3),
    ))
    .unwrap();

    let mut committed = false;
    let mut last_rejections = Vec::new();
    let mut latest_water_metrics = None;
    for _ in 0..180 {
        let report = sim.tick().unwrap();
        committed |= report.committed.iter().any(|(id, _)| *id == breach_request);
        last_rejections.extend(report.rejected.iter().map(|(_, reason)| reason.clone()));
        latest_water_metrics = report.water.clone();
        if report
            .water
            .as_ref()
            .is_some_and(|water| water.step.is_some())
        {
            let volume = sim.water().unwrap().grid().unwrap().water_volume_m3();
            assert!((volume - starting_volume).abs() < 1.0e-8);
        }
        if committed && sim.current_tick().0 >= 120 {
            break;
        }
    }
    assert!(
        committed,
        "real edit intent must breach the dam; status={:?}, rejections={last_rejections:?}",
        sim.action_status(breach_request)
    );
    assert!(matches!(
        sim.world()
            .terrain()
            .volume
            .sample(GlobalCell::new(12, 4, 4)),
        Ok(Sample::Empty { .. })
    ));
    let downstream_after = water_volume_in_x_range(&sim, spec, 13..23);
    assert!(
        downstream_after > downstream_before + 1.0e-4,
        "breach should move water downstream: before={downstream_before}, after={downstream_after}, latest={latest_water_metrics:?}"
    );
    assert!(
        (sim.water().unwrap().grid().unwrap().water_volume_m3() - starting_volume).abs() < 1.0e-8
    );
}

#[test]
fn real_placement_fills_a_full_sealed_pocket_and_reopening_releases_its_ledger() {
    let source = GlobalCell::new(1, 1, 1);
    let domain = spall_fluid::DomainSpec::new(GlobalCell::new(0, 0, 0), [3; 3], 27).unwrap();
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    terrain
        .insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )
        .unwrap();
    let mut walls = EditPlan::new(terrain.id());
    for z in 0..3 {
        for y in 0..3 {
            for x in 0..3 {
                let cell = GlobalCell::new(x, y, z);
                if cell != source {
                    walls.set(cell, MaterialId(1));
                }
            }
        }
    }
    terrain.apply_edit(&walls).unwrap();
    let mut world = fixtures::flat_terrain_setup();
    world.terrain = terrain;
    world.terrain_collider_region = (domain.origin(), GlobalCell::new(2, 2, 2));
    world.anchor = AnchorPlane::at(0);
    let mut config = SimulationConfig::new(world);
    let mut water = WaterSetup::new(domain, vec![(source, 1.0)]);
    water.config.open_top = false;
    config.water = Some(water);
    let mut sim = Simulation::new(config).unwrap();
    let initial = sim.water().unwrap().frame().volume_m3;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            BRUSH_UNIT + BRUSH_UNIT / 2,
            BRUSH_UNIT + BRUSH_UNIT / 2,
            BRUSH_UNIT + BRUSH_UNIT / 2,
        ),
        BRUSH_UNIT / 4,
    )
    .unwrap();
    for (id, kind) in [
        (105, spall_sim::EditKind::Place(MaterialId(1))),
        (106, spall_sim::EditKind::Cut),
    ] {
        sim.submit(EditIntent {
            request_id: RequestId(id),
            actor: EntityId::new(999).unwrap(),
            target: EditTarget::Terrain,
            kind,
            brush,
            explosion: None,
        })
        .unwrap();
        let mut committed = false;
        for _ in 0..30 {
            let report = sim.tick().unwrap();
            assert!((sim.water().unwrap().frame().volume_m3 - initial).abs() < 1e-12);
            if report.committed.iter().any(|(r, _)| *r == RequestId(id)) {
                committed = true;
                break;
            }
        }
        assert!(
            committed,
            "sealed-pocket edit must commit: {:?}",
            sim.action_status(RequestId(id))
        );
        let grid = sim.water().unwrap().grid().unwrap();
        if id == 105 {
            assert_eq!(grid.trapped_volume_m3(), initial);
            assert_eq!(grid.fraction_at(source), Some(0.0));
            assert!(matches!(
                sim.world().terrain().volume.sample(source).unwrap(),
                Sample::Filled(_)
            ));
        } else {
            assert_eq!(grid.trapped_volume_m3(), 0.0);
            assert_eq!(grid.fraction_at(source), Some(1.0));
        }
    }
}

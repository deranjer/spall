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
    assert_eq!(state.version, spall_protocol::WIRE_SCHEMA_VERSION);
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

/// Far water steps one slice in 2, 4 or 8, and its springs run in step with
/// it: at the full rate a far spring piled water up faster than its region
/// could carry it away, so far rivers ran deeper and spread further.
#[test]
fn a_far_spring_runs_in_slow_motion_with_its_water() {
    use spall_sim::column_water::Discharge;
    let run = |far: bool| {
        let mut setup = ColumnWaterSetup::new(0, 32, Vec::new());
        setup.discharges = vec![Discharge {
            cell: GlobalCell::new(5, 2, 7),
            flow_cells3_s: 12.0,
        }];
        setup.config.throttle_from_cells = [100.0, 200.0, 400.0];
        let terrain = basin();
        let mut water = AuthoritativeColumnWater::new(&terrain, setup).unwrap();
        water.set_focus(&[if far { [5000.0, 5000.0] } else { [8.0, 2.0] }]);
        for _ in 0..600 {
            water.tick(&terrain, &[], &[], 1.0 / 60.0).unwrap();
        }
        let frame = water.frame();
        let depth = water.world().spans_in_column(5, 7)[0].depth;
        (frame.spring_added_m3, frame.volume_m3, depth)
    };
    let (near_added, near_volume, near_depth) = run(false);
    let (far_added, far_volume, far_depth) = run(true);
    assert!((near_volume - near_added).abs() < 1e-6);
    assert!(
        (far_volume - far_added).abs() < 1e-6,
        "nothing lost or made"
    );
    // Stepped one slice in 8, the far spring adds an eighth (give or take the
    // slice the run stops on).
    let ratio = far_added / near_added;
    assert!(
        (0.1..0.15).contains(&ratio),
        "far spring ran at {ratio} of the rate"
    );
    assert!(
        far_depth <= near_depth + 0.05,
        "no mound at the far spring: {far_depth} cells against {near_depth} near"
    );
}

/// The server's water at 1 m columns (`ColumnConfig::column_cells =
/// 4`). Terrain: 128 x 32 x 128 cells, a floor below y = 2, a rim, and a dam at
/// terrain x = 62 (one cell thick) up to y = 24.
mod wide_columns {
    use super::*;
    use spall_sim::column_water::Discharge;

    const N: i64 = 128;

    fn terrain(dam: bool) -> Volume {
        terrain_with(dam, |_, _, _| false)
    }

    fn terrain_with(dam: bool, extra: impl Fn(i64, i64, i64) -> bool) -> Volume {
        let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        for bz in 0..N / 32 {
            for bx in 0..N / 32 {
                terrain
                    .insert_brick(
                        BrickCoord::new(bx, 0, bz),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )
                    .unwrap();
            }
        }
        let mut plan = EditPlan::new(terrain.id());
        for z in 0..N {
            for y in 0..32 {
                for x in 0..N {
                    let rim = x == 0 || z == 0 || x == N - 1 || z == N - 1;
                    if y < 2 || (rim && y < 28) || (dam && x == 62 && y < 24) || extra(x, y, z) {
                        plan.set(GlobalCell::new(x, y, z), STONE);
                    }
                }
            }
        }
        terrain.apply_edit(&plan).unwrap();
        terrain
    }

    /// A pool west of the dam, surface at cell 12 (top cell 11).
    fn setup() -> ColumnWaterSetup {
        let mut setup = ColumnWaterSetup::new(0, 32, Vec::new());
        setup.config.column_cells = 4;
        setup.initial_levels = (1..N - 1)
            .flat_map(|z| (1..62).map(move |x| (x, z, 1, 11)))
            .collect();
        setup
    }

    fn tick(water: &mut AuthoritativeColumnWater, terrain: &Volume, seconds: f64) {
        for _ in 0..(seconds * 60.0) as u32 {
            water.tick(terrain, &[], &[], 1.0 / 60.0).unwrap();
        }
    }

    #[test]
    fn a_pool_at_rest_is_published_per_terrain_region_at_its_level() {
        let terrain = terrain(true);
        let mut water = AuthoritativeColumnWater::new(&terrain, setup()).unwrap();
        tick(&mut water, &terrain, 5.0);
        let frame = water.frame();
        // The frame speaks terrain regions (32 x 32 terrain columns).
        assert!(
            frame
                .regions
                .keys()
                .all(|k| (0..4).contains(&k.0) && (0..4).contains(&k.1))
        );
        assert!(frame.regions.contains_key(&(0, 0)) && frame.regions.contains_key(&(1, 3)));
        assert!(
            !frame.regions.contains_key(&(3, 0)),
            "nothing east of the dam"
        );
        // Standing in the pool: the surface is at 12 cells (3 m).
        let sample = water.water_sample_at_m(5.0, 1.0, 5.0).expect("in the pool");
        assert!(
            (sample.surface_y - 3.0).abs() < 0.05,
            "surface {}",
            sample.surface_y
        );
        assert!(
            water.water_sample_at_m(25.0, 1.0, 5.0).is_none(),
            "dry past the dam"
        );
    }

    /// One raised terrain cell lifts its wide column's floor; the water is
    /// still drawn against each terrain column's own floor, so its bottom is
    /// never the raised floor hanging over lower ground.
    #[test]
    fn frames_draw_each_terrain_column_against_its_own_floor() {
        // A bump up to y = 6 at terrain (9, 9), in the wide column (2, 2).
        let terrain = terrain_with(true, |x, y, z| x == 9 && z == 9 && y < 6);
        let mut water = AuthoritativeColumnWater::new(&terrain, setup()).unwrap();
        tick(&mut water, &terrain, 5.0);
        let frame = water.frame();
        let region = frame
            .regions
            .get(&(0, 0))
            .expect("the pool's corner region");
        let span = |x: u16, z: u16| {
            region
                .spans
                .iter()
                .find(|s| s.column == x + 32 * z)
                .copied()
                .expect("wet")
        };
        let surface = |s: spall_sim::column_water::FrameSpan| {
            f64::from(s.floor_y) + f64::from(s.depth_q) / 128.0
        };
        let (beside, bump) = (span(8, 8), span(9, 9));
        assert_eq!(beside.floor_y, 2, "the real floor beside the bump");
        assert_eq!(bump.floor_y, 6, "the bump's own floor");
        assert!((surface(beside) - surface(bump)).abs() < 0.05);
        assert!(
            (surface(beside) - 12.0).abs() < 0.1,
            "surface {}",
            surface(beside)
        );
    }

    /// The wide column holding the dam (terrain x 60..64) is dry: a solid
    /// cell at the water's height closes it. The water is still drawn up to the
    /// wall in the terrain columns on the pool side, and not past it.
    #[test]
    fn frames_draw_the_water_on_to_the_true_shore_but_not_past_a_wall() {
        let terrain = terrain(true);
        let mut water = AuthoritativeColumnWater::new(&terrain, setup()).unwrap();
        tick(&mut water, &terrain, 5.0);
        assert!(
            water
                .world()
                .spans_in_column(15, 4)
                .iter()
                .all(|s| s.volume < 1e-6)
        );
        let frame = water.frame();
        let region = frame
            .regions
            .get(&(1, 0))
            .expect("the pool's region east of x = 32");
        let wet = |x: i64, z: i64| {
            region
                .spans
                .iter()
                .any(|s| i64::from(s.column) == x % 32 + 32 * z && s.depth_q > 0)
        };
        assert!(wet(60, 16) && wet(61, 16), "drawn up to the dam");
        assert!(!wet(62, 16), "the dam itself is dry");
        assert!(!wet(63, 16), "and so is the far side");
    }

    /// Flowing water slopes between wide columns: the frames blend each
    /// terrain column's surface between its wide column and the neighbours,
    /// so it does not step a metre at a time.
    #[test]
    fn frames_blend_the_surface_between_wide_columns() {
        let mut terrain = terrain(true);
        let mut water = AuthoritativeColumnWater::new(&terrain, setup()).unwrap();
        tick(&mut water, &terrain, 1.0);
        let mut plan = EditPlan::new(terrain.id());
        for z in 56..72 {
            for y in 2..24 {
                plan.set(GlobalCell::new(62, y, z), MaterialId::AIR);
            }
        }
        terrain.apply_edit(&plan).unwrap();
        let edited = [BrickCoord::new(1, 0, 1), BrickCoord::new(1, 0, 2)];
        water.tick(&terrain, &edited, &[], 1.0 / 60.0).unwrap();
        tick(&mut water, &terrain, 3.0);
        // Along terrain row z = 65 (row 1 of terrain region z = 2; wide row 16), x 40..90.
        let frame = water.frame();
        let fine: Vec<Option<f64>> = (40..90i64)
            .map(|x| {
                let region = frame.regions.get(&((x / 32) as i32, 2))?;
                region
                    .spans
                    .iter()
                    .find(|s| i64::from(s.column) == x % 32 + 32)
                    .map(|s| f64::from(s.floor_y) + f64::from(s.depth_q) / 128.0)
            })
            .collect();
        let world = water.world();
        let wide: Vec<Option<f64>> = (10..23i64)
            .map(|x| {
                world
                    .spans_in_column(x, 16)
                    .iter()
                    .find(|s| s.volume > 1e-3)
                    .map(|s| s.floor_y as f64 + s.depth)
            })
            .collect();
        let worst = |v: &[Option<f64>]| {
            v.windows(2)
                .filter_map(|w| Some((w[0]? - w[1]?).abs()))
                .filter(|d| *d < 1.0)
                .fold(0.0f64, f64::max)
        };
        let (fine_step, wide_step) = (worst(&fine), worst(&wide));
        assert!(
            wide_step > 0.05,
            "the water is flowing: steps of {wide_step}"
        );
        assert!(
            fine_step < 0.5 * wide_step,
            "terrain columns step {fine_step}, wide columns {wide_step}"
        );
    }

    /// A 1 m world saves at its width with its sea and evaporation, restores
    /// from that alone, and journals what changes after.
    #[test]
    fn a_wide_world_saves_and_restores_at_its_width() {
        let terrain = terrain(true);
        let mut setup = setup();
        setup.sea_level_y = Some(4.0);
        setup.evaporation_cells_per_s = 1.0e-4;
        setup.evaporation_above_y = 5.0;
        let mut water = AuthoritativeColumnWater::new(&terrain, setup).unwrap();
        tick(&mut water, &terrain, 2.0);
        let state = water.canonical_state();
        assert_eq!(state.column_cells, 4);
        state.validate().unwrap();
        let mut restored = AuthoritativeColumnWater::restore(
            &terrain,
            &state,
            spall_fluid::columns::ColumnConfig::default(),
        )
        .unwrap();
        let (a, b) = (water.frame().volume_m3, restored.frame().volume_m3);
        assert!((a - b).abs() < 1e-6, "{a} against {b}");
        assert_eq!(restored.setup().config.column_cells, 4);
        assert_eq!(restored.setup().sea_level_y, Some(4.0));
        assert!((restored.setup().evaporation_cells_per_s - 1.0e-4).abs() < 1e-9);
        let sample = restored
            .water_sample_at_m(5.0, 1.0, 5.0)
            .expect("the pool came back");
        assert!((sample.surface_y - 3.0).abs() < 0.05);
        // Water that moves after the restore is journalled.
        let mut changed = false;
        let mut terrain = terrain;
        let mut plan = EditPlan::new(terrain.id());
        for z in 60..68 {
            for y in 2..24 {
                plan.set(GlobalCell::new(62, y, z), MaterialId::AIR);
            }
        }
        terrain.apply_edit(&plan).unwrap();
        let edited = [BrickCoord::new(1, 0, 1), BrickCoord::new(1, 0, 2)];
        restored.tick(&terrain, &edited, &[], 1.0 / 60.0).unwrap();
        for _ in 0..120 {
            restored.tick(&terrain, &[], &[], 1.0 / 60.0).unwrap();
            changed |= restored
                .take_journal_delta()
                .is_some_and(|d| !d.regions.is_empty());
        }
        assert!(changed, "a 1 m world is journalled");
    }

    #[test]
    fn a_spring_adds_its_rate_and_nothing_is_lost() {
        let terrain = terrain(true);
        let mut setup = setup();
        setup.initial_levels.clear();
        setup.discharges = vec![Discharge {
            cell: GlobalCell::new(10, 2, 10),
            flow_cells3_s: 32.0,
        }];
        let mut water = AuthoritativeColumnWater::new(&terrain, setup).unwrap();
        tick(&mut water, &terrain, 10.0);
        let frame = water.frame();
        let expected = frame.fluid_time_s * 32.0 * 0.25f64.powi(3);
        assert!((frame.spring_added_m3 - expected).abs() < 1e-6 * expected);
        assert!((frame.volume_m3 - frame.spring_added_m3).abs() < 1e-6);
    }

    #[test]
    fn cutting_the_dam_lets_the_pool_through() {
        let mut terrain = terrain(true);
        let mut water = AuthoritativeColumnWater::new(&terrain, setup()).unwrap();
        tick(&mut water, &terrain, 2.0);
        let start = water.frame().volume_m3;
        // Open a gap 8 cells (2 m) wide in the dam.
        let mut plan = EditPlan::new(terrain.id());
        for z in 60..68 {
            for y in 2..24 {
                plan.set(GlobalCell::new(62, y, z), MaterialId::AIR);
            }
        }
        terrain.apply_edit(&plan).unwrap();
        let edited = [BrickCoord::new(1, 0, 1), BrickCoord::new(1, 0, 2)];
        water.tick(&terrain, &edited, &[], 1.0 / 60.0).unwrap();
        tick(&mut water, &terrain, 60.0);
        let east = water
            .water_sample_at_m(25.0, 1.0, 16.0)
            .expect("water got through");
        assert!(
            east.surface_y > 0.6,
            "east of the dam at {}",
            east.surface_y
        );
        let volume = water.frame().volume_m3;
        assert!(
            (volume - start).abs() < 1e-6 * start,
            "{volume} against {start}"
        );
    }
}

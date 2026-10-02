//! A generated world becomes a playable server world: the sandbox material
//! palette covers everything the generator paints, water is server state in
//! one fluid region, the terrain stands up in the authoritative simulation,
//! and a player placed at a generated spawn stays on the ground.

use std::collections::BTreeSet;
use std::time::Instant;

use sandbox::game::{self, materials};
use sandbox::worldgen_scene::{self, WorldgenSceneError};
use spall_core::{GlobalCell, MaterialId, player_entity_for};
use spall_sim::{Simulation, SimulationConfig};
use spall_voxel::Sample;

const SMALL: u32 = 256;

fn palette_ids() -> Vec<MaterialId> {
    let p = game::terrain_palette();
    vec![
        p.bedrock,
        p.deep_stone,
        p.stone,
        p.slate,
        p.dirt,
        p.grass,
        p.sand,
        p.sandstone,
        p.mud,
        p.moss,
        p.gravel,
        p.clay,
        p.snow,
    ]
}

#[test]
fn every_terrain_role_is_a_distinct_solid_material_in_the_manifest() {
    let manifest = game::manifest();
    let ids = palette_ids();
    assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
    for id in ids {
        let def = manifest
            .get(id)
            .unwrap_or_else(|| panic!("{id:?} undefined"));
        assert!(
            def.sim.flags.0 & spall_core::MaterialFlags::COLLIDES.0 != 0,
            "{id:?}"
        );
        assert!(
            def.sim.flags.0 & spall_core::MaterialFlags::OPAQUE.0 != 0,
            "{id:?}"
        );
    }
    // New terrain materials sit above the lamp and below the appearance
    // extension range, as a manifest extension requires.
    for id in [
        materials::BEDROCK,
        materials::DEEP_STONE,
        materials::SAND,
        materials::MUD,
        materials::MOSS,
        materials::GRAVEL,
        materials::CLAY,
        materials::SNOW,
    ] {
        assert!(id.0 > materials::LAMP.0 && id.0 < sandbox::appearance::EXTENSION_ID_BASE);
    }
}

#[test]
fn generated_terrain_only_uses_defined_materials() {
    let scene = worldgen_scene::generate("showcase", 1, SMALL).expect("generate");
    let manifest = game::manifest();
    let terrain = &scene.world().terrain;
    let mut used = BTreeSet::new();
    for z in (0..i64::from(SMALL)).step_by(3) {
        for x in (0..i64::from(SMALL)).step_by(3) {
            for y in (0..384).step_by(2) {
                if let Ok(Sample::Filled(m)) = terrain.sample(GlobalCell::new(x, y, z)) {
                    used.insert(m);
                }
            }
        }
    }
    assert!(used.len() >= 8, "a varied world, got {used:?}");
    for m in used {
        assert!(manifest.contains(m), "{m:?} is painted but undefined");
    }
}

#[test]
fn water_is_one_server_fluid_region_inside_the_arena() {
    let scene = worldgen_scene::generate("showcase", 1, SMALL).expect("generate");
    let water = scene.water_setup().expect("showcase has water");
    assert_eq!(
        water.initial_fractions.len(),
        scene.world().water.cells.len()
    );
    let terrain = &scene.world().terrain;
    for (cell, fraction) in &water.initial_fractions {
        assert_eq!(*fraction, 1.0);
        assert!(
            matches!(terrain.sample(*cell), Ok(Sample::Empty { .. })),
            "{cell:?}"
        );
    }
    // The domain covers every water cell and stays inside the arena.
    let (lo, hi) = scene.world().region;
    let d = water.domain;
    let (o, dims) = (d.origin(), d.dimensions());
    assert!(o.x >= lo.x && o.y >= lo.y && o.z >= lo.z);
    assert!(o.x + i64::from(dims[0]) - 1 <= hi.x);
    assert!(o.y + i64::from(dims[1]) - 1 <= hi.y);
    assert!(o.z + i64::from(dims[2]) - 1 <= hi.z);
    for (cell, _) in &water.initial_fractions {
        assert!(cell.x >= o.x && cell.x < o.x + i64::from(dims[0]));
        assert!(cell.y >= o.y && cell.y < o.y + i64::from(dims[1]));
        assert!(cell.z >= o.z && cell.z < o.z + i64::from(dims[2]));
    }
    assert!((2..=8).contains(&water.coarsen));
    let fluid_cells = water.domain.cell_count() / (water.coarsen as usize).pow(3);
    assert!(
        fluid_cells <= sandbox::worldgen_scene::WATER_FLUID_CELL_BUDGET || water.coarsen == 8,
        "{fluid_cells} fluid cells at coarsening {}",
        water.coarsen
    );
    assert!(
        water.sources.is_empty() && water.sinks.is_empty(),
        "static water"
    );
}

#[test]
fn bad_requests_fail_loudly() {
    assert!(matches!(
        worldgen_scene::generate("nonsense", 1, SMALL),
        Err(WorldgenSceneError::UnknownPreset(_))
    ));
    assert!(matches!(
        worldgen_scene::generate("showcase", 1, 100),
        Err(WorldgenSceneError::Generate(_))
    ));
}

#[test]
fn a_player_at_a_generated_spawn_stands_on_the_ground_with_water_running() {
    let scene = worldgen_scene::generate("showcase", 7, SMALL).expect("generate");
    let spawn = scene.player_spawns()[0];
    let mut config = SimulationConfig::new(scene.world_setup());
    config.water = scene.water_setup().cloned();
    let mut sim = Simulation::new(config).expect("the generated world stands up");
    let player = player_entity_for(0);
    sim.add_player(player, spawn);
    for _ in 0..90 {
        sim.tick().expect("tick");
    }
    let state = sim.player_state(player).expect("player exists");
    assert!(
        state.grounded,
        "player should rest on the terrain: {state:?}"
    );
    assert!(
        (state.position_m[1] - spawn[1]).abs() < 0.5,
        "player fell or floated: spawn {spawn:?} now {:?}",
        state.position_m
    );
    assert!(sim.water().is_some(), "server owns the fluid region");
}

/// Measures the cost of standing up the full 256 m arena. Not a pass/fail
/// gate: `cargo test -p sandbox --release --test worldgen_scene -- --ignored --nocapture measure`.
#[test]
#[ignore]
fn measure_full_arena_stand_up() {
    let t = Instant::now();
    let scene = worldgen_scene::generate("showcase", 1, worldgen_scene::DEFAULT_SIZE_CELLS)
        .expect("generate");
    println!("generate + water plan: {:?}", t.elapsed());
    println!("{:?}", scene.world().terrain.memory_report());
    let spawn = scene.player_spawns()[0];
    let mut config = SimulationConfig::new(scene.world_setup());
    config.water = scene.water_setup().cloned();
    let t = Instant::now();
    let mut sim = Simulation::new(config).expect("stands up");
    println!("Simulation::new (colliders + water): {:?}", t.elapsed());
    let player = player_entity_for(0);
    sim.add_player(player, spawn);
    let t = Instant::now();
    let mut worst = std::time::Duration::ZERO;
    for _ in 0..120 {
        let s = Instant::now();
        sim.tick().expect("tick");
        worst = worst.max(s.elapsed());
    }
    println!("120 ticks: {:?}, worst {:?}", t.elapsed(), worst);
    println!("player {:?}", sim.player_state(player));
}

/// Times the authoritative simulation around one hammer-sized cut on the full
/// arena, with and without the water region, to see where server time goes:
/// `cargo test -p sandbox --release --test worldgen_scene -- --ignored --nocapture measure_cut`.
#[test]
#[ignore]
fn measure_cut_latency_on_the_full_arena() {
    use spall_core::units::{BRUSH_UNIT, BrushPoint};
    use spall_core::{SphereBrush, player_entity_for};
    use spall_protocol::RequestId;
    use spall_sim::{EditIntent, EditTarget};

    for with_water in [false, true] {
        let scene = worldgen_scene::generate("showcase", 1, worldgen_scene::DEFAULT_SIZE_CELLS)
            .expect("generate");
        let spawn = scene.player_spawns()[0];
        let (x, z) = (
            (spawn[0] / 0.25).floor() as i64,
            (spawn[2] / 0.25).floor() as i64,
        );
        let ground_y = (spawn[1] / 0.25).round() as i64 - 1;
        let mut config = SimulationConfig::new(scene.world_setup());
        if with_water {
            config.water = scene.water_setup().cloned();
        }
        let mut sim = Simulation::new(config).expect("stands up");
        let player = player_entity_for(0);
        sim.add_player(player, spawn);
        for _ in 0..60 {
            sim.tick().expect("tick");
        }
        for cut in 0..4_i64 {
            let half = BRUSH_UNIT / 2;
            let brush = SphereBrush::new(
                BrushPoint::from_units(
                    (x + cut * 14) * BRUSH_UNIT + half,
                    ground_y * BRUSH_UNIT + half,
                    z * BRUSH_UNIT + half,
                ),
                3 * BRUSH_UNIT,
            )
            .unwrap();
            sim.submit(EditIntent::cut(
                RequestId(1 + cut as u64),
                player,
                EditTarget::Terrain,
                brush,
            ))
            .expect("submit");
            for tick in 0..60 {
                let s = Instant::now();
                let report = sim.tick().expect("tick");
                let ms = s.elapsed().as_secs_f64() * 1e3;
                if !report.committed.is_empty() {
                    println!(
                        "water {with_water}: cut {cut} committed on tick {tick}, that tick {ms:.1} ms"
                    );
                    break;
                }
            }
        }
    }
}

/// The water boundary is re-captured (a scan of the whole fluid domain that
/// also wakes the solver) exactly when a committed terrain brick is at or next
/// to the domain; edits elsewhere leave the water alone.
#[test]
fn only_edits_near_the_water_refresh_its_boundary() {
    use spall_core::units::{BRUSH_UNIT, BrushPoint};
    use spall_core::{SphereBrush, player_entity_for};
    use spall_protocol::RequestId;
    use spall_sim::{EditIntent, EditTarget};

    let scene = worldgen_scene::generate("showcase", 1, SMALL).expect("generate");
    let spawn = scene.player_spawns()[0];
    let columns = &scene.world().columns;
    let water = scene.world().water.bounds.expect("the showcase has water");
    let (lake_x, lake_z) = ((water.0.x + water.1.x) / 2, (water.0.z + water.1.z) / 2);

    let mut config = SimulationConfig::new(scene.world_setup());
    config.water = scene.water_setup().cloned();
    let mut sim = Simulation::new(config).expect("stands up");
    let player = player_entity_for(0);
    sim.add_player(player, spawn);
    for _ in 0..30 {
        sim.tick().expect("tick");
    }

    // Cuts at the surface of the column `(x, z)`; returns (refreshed, any
    // committed terrain brick near the domain).
    let mut next_id = 1_u64;
    let mut cut = |sim: &mut Simulation, x: i64, z: i64, depth: i64| -> (bool, bool) {
        let y = i64::from(columns.height(x, z)) - depth;
        let half = BRUSH_UNIT / 2;
        let brush = SphereBrush::new(
            BrushPoint::from_units(
                x * BRUSH_UNIT + half,
                y * BRUSH_UNIT + half,
                z * BRUSH_UNIT + half,
            ),
            2 * BRUSH_UNIT,
        )
        .unwrap();
        next_id += 1;
        sim.submit(EditIntent::cut(
            RequestId(next_id),
            player,
            EditTarget::Terrain,
            brush,
        ))
        .expect("submit");
        for _ in 0..60 {
            let report = sim.tick().expect("tick");
            if report.committed.is_empty() {
                continue;
            }
            let terrain = sim.world().terrain_volume_id();
            let region = sim.water().expect("water region installed");
            let near = report.committed.iter().any(|(_, c)| {
                c.topology
                    .before
                    .iter()
                    .chain(&c.topology.after)
                    .any(|b| b.volume == terrain && region.boundary_touched_by(b.coord))
            });
            let refreshed =
                report.water.expect("water tick").boundary_refresh > std::time::Duration::ZERO;
            return (refreshed, near);
        }
        panic!("cut at {x},{z} never committed");
    };

    // The invariant over several spots, and at least one cut that skipped the
    // refresh because it was far from the lake.
    let mut skipped = 0;
    for (x, z) in [
        (120, 120),
        (150, 90),
        (100, 150),
        (200, 60),
        (60, 70),
        (180, 140),
    ] {
        let (refreshed, near) = cut(&mut sim, x, z, 1);
        assert_eq!(
            refreshed, near,
            "cut at {x},{z}: refresh must follow nearness"
        );
        skipped += usize::from(!near);
    }
    assert!(
        skipped >= 1,
        "no cut was far enough from the lake to skip the refresh"
    );

    // A cut into the lake bed inside the fluid domain refreshes it.
    let (refreshed, near) = cut(&mut sim, lake_x, lake_z, 1);
    assert!(
        near && refreshed,
        "a cut at the lake must refresh the boundary"
    );
}

#[test]
fn water_coarsening_is_the_finest_that_fits_the_fluid_cell_budget() {
    use sandbox::worldgen_scene::{WATER_FLUID_CELL_BUDGET, pick_water_coarsening};
    use spall_core::GlobalCell;
    // A pond 20 x 6 x 20 voxels: fine resolution fits.
    assert_eq!(
        pick_water_coarsening(GlobalCell::new(0, 0, 0), GlobalCell::new(19, 5, 19)),
        2
    );
    // Valley-sized water: coarser, and the result fits the budget.
    let (lo, hi) = (GlobalCell::new(0, 0, 0), GlobalCell::new(259, 41, 227));
    let c = pick_water_coarsening(lo, hi);
    assert!((3..=8).contains(&c), "coarsening {c}");
    let fine = u128::from(c - 1);
    let voxels = |c: u128| (260 + 8 + c) * (42 + 6 + c) * (228 + 8 + c);
    assert!(voxels(u128::from(c)) / u128::from(c).pow(3) <= WATER_FLUID_CELL_BUDGET as u128);
    assert!(
        voxels(fine) / fine.pow(3) > WATER_FLUID_CELL_BUDGET as u128,
        "{fine} would also have fit, so {c} is not the finest"
    );
    // Absurdly large water falls back to the coarsest supported factor.
    assert_eq!(
        pick_water_coarsening(GlobalCell::new(0, 0, 0), GlobalCell::new(4000, 400, 4000)),
        8
    );
}

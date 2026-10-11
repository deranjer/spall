//! A generated world becomes a playable server world: the sandbox material
//! palette covers everything the generator paints, water is server state as
//! stacked columns, the terrain stands up in the authoritative simulation,
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

/// The column water starts from exactly the plan's cells, all in open terrain
/// inside the arena, and its height range covers the arena.
#[test]
fn water_starts_from_the_plan_inside_the_arena() {
    let scene = worldgen_scene::generate("showcase", 1, SMALL).expect("generate");
    let water = scene.column_water_setup().expect("showcase has water");
    assert_eq!(water.initial_cells.len(), scene.world().water.cells.len());
    let terrain = &scene.world().terrain;
    let (lo, hi) = scene.world().region;
    for cell in &water.initial_cells {
        assert!(
            matches!(terrain.sample(*cell), Ok(Sample::Empty { .. })),
            "{cell:?}"
        );
        assert!(cell.x >= lo.x && cell.x <= hi.x);
        assert!(cell.y >= lo.y && cell.y <= hi.y);
        assert!(cell.z >= lo.z && cell.z <= hi.z);
    }
    assert!(i64::from(water.y_lo) <= lo.y && i64::from(water.y_hi) > hi.y);
    assert_eq!(water.y_lo % 32, 0);
    assert_eq!(water.y_hi % 32, 0);
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
    config.column_water = scene.column_water_setup().cloned();
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
    assert!(sim.column_water().is_some(), "server owns the water");
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
    config.column_water = scene.column_water_setup().cloned();
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
            config.column_water = scene.column_water_setup().cloned();
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

/// A committed terrain edit remaps the water columns it changes and nothing
/// else: cuts away from the lake remap no column, a cut into the lake bed does,
/// and the water volume stays the same through both.
#[test]
fn only_edits_at_the_water_remap_its_columns() {
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
    config.column_water = scene.column_water_setup().cloned();
    let mut sim = Simulation::new(config).expect("stands up");
    let player = player_entity_for(0);
    sim.add_player(player, spawn);
    for _ in 0..30 {
        sim.tick().expect("tick");
    }
    let volume = sim.column_water().unwrap().frame().volume_m3;

    // Cuts at the surface of the column `(x, z)`; returns the columns the
    // commit remapped.
    let mut next_id = 1_u64;
    let mut cut = |sim: &mut Simulation, x: i64, z: i64, depth: i64| -> usize {
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
            return report.column_water.expect("water tick").columns_remapped;
        }
        panic!("cut at {x},{z} never committed");
    };

    // Away from the lake: nothing to remap. At least one spot must be far
    // enough from every water region for that to hold.
    let mut far = 0;
    for (x, z) in [
        (120, 120),
        (150, 90),
        (100, 150),
        (200, 60),
        (60, 70),
        (180, 140),
    ] {
        let inside = x >= water.0.x - 40
            && x <= water.1.x + 40
            && z >= water.0.z - 40
            && z <= water.1.z + 40;
        let remapped = cut(&mut sim, x, z, 1);
        if !inside {
            assert_eq!(remapped, 0, "cut at {x},{z} is far from the lake");
            far += 1;
        }
    }
    assert!(far >= 1, "no cut was far enough from the lake");

    // A cut into the lake bed remaps its columns.
    let remapped = cut(&mut sim, lake_x, lake_z, 1);
    assert!(remapped > 0, "a cut at the lake must remap water columns");
    let after = sim.column_water().unwrap().frame().volume_m3;
    assert!(
        (after - volume).abs() < 1e-6 * volume.max(1.0),
        "edits do not create or destroy water: {volume} -> {after}"
    );
}

/// ENG-167: the generated world's water runs as stacked columns. The plan's
/// lakes seed at rest, the server pays nothing while they stay at rest, none
/// of the plan is rejected, and a level surface really is level.
#[test]
fn generated_water_runs_as_columns_at_rest_with_no_rejected_cells() {
    let scene = worldgen_scene::generate("showcase", 7, SMALL).expect("generate");
    let setup = scene
        .column_water_setup()
        .expect("showcase has water")
        .clone();
    let planned = setup.initial_cells.len();
    assert!(planned > 0);
    let mut config = SimulationConfig::new(scene.world_setup());
    config.column_water = Some(setup);
    let mut sim = Simulation::new(config).expect("the generated world stands up");
    let water = sim.column_water().expect("server owns the column water");
    assert_eq!(
        water.seed_report().rejected_cells,
        0,
        "every planned cell sits in open terrain"
    );
    let seeded = water.frame().volume_m3;
    assert!((seeded - planned as f64 * 0.25f64.powi(3)).abs() < 1e-6);
    let diff = water.world().max_wetted_head_difference();
    assert!(
        diff <= 0.1,
        "generated lakes are level where they touch: head difference {diff}"
    );
    for _ in 0..120 {
        let report = sim.tick().expect("tick");
        let m = report.column_water.expect("reported each tick");
        assert_eq!(m.active_regions, 0, "an idle lake costs nothing");
        assert!((m.volume_m3 - seeded).abs() < 1e-6);
    }
}

/// `--spawn-at-lake`: the player starts on solid ground with headroom, a few
/// columns from the water of the deepest lake, not in it or inside terrain.
#[test]
fn the_lake_spawn_stands_on_dry_ground_beside_the_water() {
    let scene = worldgen_scene::generate("showcase", 7, SMALL).expect("generate");
    let spawn = scene.lake_spawn().expect("the showcase has a lake");
    let cell = |v: f64| (v / 0.25).floor() as i64;
    let (x, z) = (cell(spawn[0]), cell(spawn[2]));
    let feet = cell(spawn[1] + 0.01);
    let terrain = &scene.world().terrain;
    assert!(matches!(
        terrain.sample(GlobalCell::new(x, feet - 1, z)),
        Ok(Sample::Filled(_))
    ));
    for up in 0..9 {
        assert!(
            matches!(
                terrain.sample(GlobalCell::new(x, feet + up, z)),
                Ok(Sample::Empty { .. })
            ),
            "no headroom {up} cells above the feet"
        );
    }
    let water: BTreeSet<(i64, i64)> = scene
        .world()
        .water
        .cells
        .iter()
        .map(|c| (c.x, c.z))
        .collect();
    assert!(!water.contains(&(x, z)), "the spawn is not on water");
    let near = (-12..=12).any(|d: i64| water.contains(&(x + d, z)) || water.contains(&(x, z + d)));
    assert!(near, "water within 3 m of the spawn");
    let with = worldgen_scene::generate("showcase", 7, SMALL)
        .unwrap()
        .with_lake_spawn();
    assert_eq!(with.player_spawns(), &[spawn]);
}

/// `--spawn-raft`: the lake's floaters behave on the generated world's real
/// water. The wood raft rides 70% under, light crates float flat and high, the
/// log floats, and the stone block sinks.
#[test]
fn the_lake_floaters_float_or_sink_as_their_density_says() {
    let scene = worldgen_scene::generate("showcase", 7, 1024).expect("generate");
    let floaters = scene.lake_floaters();
    assert_eq!(floaters.len(), 6, "a raft, three crates, a log and a stone");
    let surface = (scene.world().water.surface_y + 1) as f64 * 0.25;
    let mut config = SimulationConfig::new(scene.world_setup());
    config.column_water = scene.column_water_setup().cloned();
    let mut sim = Simulation::new(config).expect("stands up");
    let ids: Vec<_> = floaters
        .iter()
        .map(|b| {
            spall_sim::playground::spawn_box_body(
                sim.world_mut(),
                b.dims,
                b.material,
                b.density_kg_m3,
                spall_sim::BodyPose::new(glam::DQuat::IDENTITY, b.position_m),
            )
            .expect("body spawns")
        })
        .collect();
    for _ in 0..(60 * 25) {
        sim.tick().expect("tick");
    }
    let pose = |i: usize| {
        sim.world()
            .bodies()
            .find(|b| b.entity == Some(ids[i]))
            .unwrap()
            .pose
    };
    let tilt = |i: usize| {
        (pose(i).rotation * glam::DVec3::Y)
            .dot(glam::DVec3::Y)
            .clamp(-1.0, 1.0)
            .acos()
            .to_degrees()
    };
    // Raft: 0.5 m thick, base 0.35 m under the surface, flat.
    let raft = pose(0).translation_m;
    assert!(
        (raft[1] - (surface - 0.35)).abs() < 0.12,
        "raft base {} vs {}",
        raft[1],
        surface - 0.35
    );
    assert!(tilt(0) < 4.0, "raft tilt {}", tilt(0));
    // Crates: 1 m cubes at a quarter of water's density float 0.25 m deep.
    for i in 1..=3 {
        let y = pose(i).translation_m[1];
        assert!((y - (surface - 0.25)).abs() < 0.15, "crate {i} base {y}");
        assert!(tilt(i) < 6.0, "crate {i} tilt {}", tilt(i));
    }
    // The log floats (it may roll): its axis stays near the surface.
    assert!(pose(4).translation_m[1] > surface - 1.5, "the log sank");
    // Stone sinks well below the surface.
    assert!(
        pose(5).translation_m[1] < surface - 1.0,
        "stone at {} vs surface {surface}",
        pose(5).translation_m[1]
    );
}

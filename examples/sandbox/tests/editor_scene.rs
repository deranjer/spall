//! Loads the checked-in forest project (`fixtures/terrain-trees-forest`) the
//! way the editor's Run button does and checks it becomes a playable world.

use std::path::Path;

use sandbox::editor_scene;
use spall_core::{GlobalCell, MaterialId};
use spall_voxel::Sample;

fn forest() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/terrain-trees-forest"
    ))
}

#[test]
fn forest_project_loads_into_a_walkable_world_with_trees() {
    let scene = editor_scene::load(forest()).expect("forest project loads");
    let (min, max) = scene.bounds();
    // 40 m x 40 m ground at 0.25 m cells, two layers, is 160 x 160 x 2.
    assert_eq!(min, [0, 0, 0]);
    assert!(max[0] >= 159 && max[2] >= 159, "ground spans 40 m: {max:?}");
    assert!(
        max[1] > 12,
        "trees rise well above the 2-cell ground: {max:?}"
    );
    assert!(
        scene.solid_cell_count() > 160 * 160 * 2 + 2_000,
        "trees add cells"
    );

    // Every spawn stands on the ground (top face at 0.5 m), not in a tree.
    assert!(!scene.player_spawns.is_empty());
    for spawn in &scene.player_spawns {
        assert!((spawn[1] - 0.5).abs() < 1e-9, "feet on the lawn: {spawn:?}");
    }

    let setup = scene.world_setup();
    let (lo, hi) = setup.terrain_collider_region;
    assert!(
        lo.y < 0 && hi.y > max[1],
        "collider covers headroom: {lo:?} {hi:?}"
    );
    let volume = setup.terrain;
    // Grass over dirt at the origin corner, material ids from the sandbox palette.
    // The lawn is authored-tinted grass: an appearance variant of grass (10).
    let Ok(Sample::Filled(lawn)) = volume.sample(GlobalCell::new(1, 1, 1)) else {
        panic!("the lawn is solid");
    };
    assert_eq!(sandbox::appearance::base_material(lawn), MaterialId(10));
    assert!(
        lawn.0 >= sandbox::appearance::VARIANT_ID_BASE,
        "the authored tint is kept as a variant"
    );
    let Ok(Sample::Filled(soil)) = volume.sample(GlobalCell::new(1, 0, 1)) else {
        panic!("the soil is solid");
    };
    assert_eq!(
        sandbox::appearance::base_material(soil),
        sandbox::game::materials::DIRT
    );
    // Resident air above the lawn, so character queries never hit unresident cells.
    assert!(matches!(
        volume.sample(GlobalCell::new(-10, 5, -10)),
        Ok(Sample::Empty { .. })
    ));
}

#[test]
fn a_directory_without_a_project_reports_the_missing_file() {
    let error = editor_scene::load(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap_err();
    assert!(error.to_string().contains("project.ron"), "{error}");
}

/// The engine plans one exact whole-terrain collider (budget: 4096 greedy
/// boxes) and refuses fragmented terrain past it. The forest must stand up
/// and step with a player standing on it, or Run fails at server start.
#[test]
fn forest_world_stands_up_and_a_player_stays_on_the_lawn() {
    use spall_core::player_entity_for;
    use spall_sim::{Simulation, SimulationConfig};

    let scene = editor_scene::load(forest()).expect("forest project loads");
    let spawn = scene.player_spawns[0];
    let mut sim = Simulation::new(SimulationConfig::new(scene.world_setup()))
        .expect("forest terrain fits the exact collider budget");
    let player = sim.add_player(player_entity_for(0), spawn);
    for _ in 0..120 {
        sim.tick().expect("tick");
    }
    let state = sim.player_state(player).expect("player exists");
    // Still standing at lawn height (0.5 m), not fallen through or launched.
    assert!(
        (state.position_m[1] - 0.5).abs() < 0.1 && state.grounded,
        "player drifted off the lawn: {:?}",
        state.position_m
    );
}

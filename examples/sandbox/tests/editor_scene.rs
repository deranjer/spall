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

fn valley() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/valley-showcase"
    ))
}

#[test]
fn authored_valley_water_is_server_state_not_solid_terrain() {
    let scene = editor_scene::load(valley()).expect("valley project loads");
    // The authored water is also the column model's seed, in open terrain.
    let column = scene
        .column_water_setup()
        .expect("scene seeds column water")
        .clone();
    assert!(!column.initial_cells.is_empty());
    let terrain = scene.world_setup().terrain;
    for cell in &column.initial_cells {
        assert!(matches!(terrain.sample(*cell), Ok(Sample::Empty { .. })));
    }
    let custom = scene.into_custom_world();
    assert!(!custom.player_spawns().is_empty());
    assert!(
        !custom
            .column_water_setup()
            .expect("custom server world retains the column water setup")
            .initial_cells
            .is_empty()
    );
}

/// The reservoir's gated spring has three increasingly large rate footprints
/// (so "fill faster" is a real option, not just on/off), and the dam gate is
/// authored as real solid stone in the scene's own terrain, closed by
/// default.
#[test]
fn valley_has_a_tiered_gated_spring_and_a_closed_solid_gate() {
    let scene = editor_scene::load(valley()).expect("valley project loads");
    let water = scene.column_water_setup().expect("valley has water");
    let sizes: Vec<usize> = water.gated_sources.iter().map(Vec::len).collect();
    assert!(
        sizes.iter().all(|&n| n > 0),
        "every gated spring rate must have a footprint: {sizes:?}"
    );
    assert!(
        sizes[0] < sizes[1] && sizes[1] < sizes[2],
        "each rate must be a bigger footprint than the last: {sizes:?}"
    );

    let gate_cells = scene.dam_gate_cells().to_vec();
    assert!(!gate_cells.is_empty(), "the scene authors a dam gate");

    let sim = spall_sim::Simulation::new(spall_sim::SimulationConfig::new(scene.world_setup()))
        .expect("the valley stands up as a server world");
    let solid_before = gate_cells
        .iter()
        .filter(|&&cell| {
            matches!(
                sim.world().terrain().volume.sample(cell),
                Ok(Sample::Filled(_))
            )
        })
        .count();
    assert_eq!(
        solid_before,
        gate_cells.len(),
        "the gate starts closed: every marked cell must be solid stone"
    );
}

/// Regression guard for a fixed bug: the pristine, never-edited valley
/// terrain used to be one 2.63M-cell anchored mass plus **~2500** tiny
/// (mostly 1-15 cell) disconnected components — almost entirely
/// `foliage.oak` (tree canopy voxels authored with diagonal-only gaps, so
/// they didn't all face-touch their neighbours or their trunk) with a
/// little `wood.oak` (thin structural bits, e.g. rails). Six-face
/// connectivity is the documented, correct rule (`docs/architecture.md`),
/// so those voxels were genuinely disconnected, not an engine bug — but
/// because `stage_edit`'s split search is global, *any* terrain edit
/// anywhere tried to detach all ~2500 of them in the same transaction as
/// its own cut, and the resulting ~87 MB of per-child baseline overhead
/// (see `spall_sim::commit::build_split_baseline_ops`) blew past
/// `spall_protocol::limits::MAX_ASSEMBLED_TRANSFER` (64 MiB), rejecting
/// every edit anywhere in the scene, including
/// `dam_gate_opens_and_closes_through_the_normal_edit_pipeline` above.
///
/// Fixed in the tree generators (`connect_foliage_to_trunk` in
/// `tools/spall_editor/examples/terrain_trees.rs` and
/// `terrain_trees_v2.rs`): every canopy voxel is now bridged back to its
/// trunk with a short straight run of the same material, so nothing is
/// deleted but nothing is disconnected either. A small number of unrelated
/// fragments (other props) is expected and harmless; this guards against
/// the count blowing back up to the old scale.
#[test]
fn valley_has_few_disconnected_fragments() {
    use spall_jobs::{Generation, TopologyEpoch};
    use spall_structure::{AnchorPlane, CancelToken, ResidencyMode, StructureIndex};

    let scene = editor_scene::load(valley()).expect("valley project loads");
    let (min, _max) = scene.bounds();
    let setup = scene.world_setup();
    let index = StructureIndex::build(
        &setup.terrain,
        AnchorPlane::at(min[1]),
        ResidencyMode::AllResident,
        Generation::START,
        TopologyEpoch::START,
        &CancelToken::new(),
    )
    .expect("structural analysis completes");
    let comps = index.graph().components();
    let unanchored: Vec<_> = comps.iter().filter(|c| !c.anchored).collect();
    let unanchored_cells: u64 = unanchored.iter().map(|c| c.cell_count).sum();
    eprintln!(
        "pristine valley: {} components, {} unanchored ({unanchored_cells} cells)",
        comps.len(),
        unanchored.len()
    );
    assert!(
        unanchored.len() < 100,
        "expected only a handful of pre-existing disconnected fragments (not the \
         old ~2500-fragment tree-canopy bug), found {}",
        unanchored.len()
    );
}

/// The dam gate actually opens and closes: a real terrain cut/refill,
/// committed through the normal edit pipeline like a player's own dig. This
/// used to be rejected — see
/// `root_cause_pristine_valley_has_thousands_of_disconnected_foliage_fragments`
/// above and `tools/spall_editor/examples/terrain_trees.rs`'s
/// `connect_foliage_to_trunk` for the fix (the trees' canopies were
/// authored with diagonal-only gaps, so thousands of them were genuinely
/// disconnected under six-face connectivity, and every edit anywhere swept
/// all of them into its own transaction, blowing the replication cap).
#[test]
fn dam_gate_opens_and_closes_through_the_normal_edit_pipeline() {
    let scene = editor_scene::load(valley()).expect("valley project loads");
    let gate_cells = scene.dam_gate_cells().to_vec();
    let mut sim = spall_sim::Simulation::new(spall_sim::SimulationConfig::new(scene.world_setup()))
        .expect("the valley stands up as a server world");
    fn any_empty(sim: &spall_sim::Simulation, cells: &[GlobalCell]) -> bool {
        cells.iter().any(|&cell| {
            matches!(
                sim.world().terrain().volume.sample(cell),
                Ok(Sample::Empty { .. })
            )
        })
    }
    assert!(!any_empty(&sim, &gate_cells), "the gate starts closed");

    let statuses = sim
        .set_dam_gate(&gate_cells, true, sandbox::game::materials::STONE)
        .expect("opening the gate submits cleanly");
    assert!(!statuses.is_empty(), "the scene authors a gate");
    let mut pending: std::collections::HashSet<_> = statuses.iter().map(|s| s.request_id).collect();
    let mut rejected = Vec::new();
    // A wide, flat notch is covered by several spheres (`covering_spheres`),
    // and neighbouring/overlapping pieces can conflict in the same tick, so
    // give every piece more than one tick each to commit.
    let ticks = statuses.len() * 8 + 8;
    for _ in 0..ticks {
        let report = sim.tick().expect("tick");
        rejected.extend(report.rejected);
        for (id, _) in &report.committed {
            pending.remove(id);
        }
    }
    assert!(
        rejected.is_empty(),
        "opening the gate must not be rejected: {rejected:?}"
    );
    assert!(
        pending.is_empty(),
        "every piece of the gate must commit within {ticks} ticks: {} still pending",
        pending.len()
    );
    assert!(any_empty(&sim, &gate_cells), "open must actually cut air");

    sim.set_dam_gate(&gate_cells, false, sandbox::game::materials::STONE)
        .expect("closing the gate submits cleanly");
    for _ in 0..ticks {
        sim.tick().expect("tick");
    }
    assert!(!any_empty(&sim, &gate_cells), "close must refill it");
}

/// A cut terrain hole is necessary but not sufficient: the fluid solver only
/// sees a passage if the gate's own cells fall inside the water domain and
/// coarsen to non-solid fluid cells there. This checks that directly against
/// the solver's own boundary capture — not just that the voxels sample as
/// air — since a gate that clips the domain's edge could cut real air the
/// water simulation still treats as a wall. Regression test for exactly that
/// bug: the domain's bounding box used to be computed from the scene's
/// water/spring/drain/basin cells only, blind to where the (separately
/// placed) gate actually was, so it could — and did — sit right at the
/// domain's edge.
#[test]
fn open_dam_gate_clears_a_passage_in_the_fluid_solvers_own_boundary() {
    let scene = editor_scene::load(valley()).expect("valley project loads");
    let gate_cells = scene.dam_gate_cells().to_vec();
    let mut config = spall_sim::SimulationConfig::new(scene.world_setup());
    config.column_water = scene.column_water_setup().cloned();
    let mut sim =
        spall_sim::Simulation::new(config).expect("the valley stands up as a server world");
    sim.set_dam_gate(&gate_cells, true, sandbox::game::materials::STONE)
        .expect("opening the gate submits cleanly");
    for _ in 0..8 {
        sim.tick().expect("tick");
    }

    let world = sim.column_water().expect("valley has column water").world();
    let open = gate_cells
        .iter()
        .filter(|cell| {
            world
                .spans_in_column(cell.x, cell.z)
                .iter()
                .any(|span| span.floor_y <= cell.y && cell.y < span.ceiling_y)
        })
        .count();
    eprintln!(
        "gate cells: {}, open to the water: {open}",
        gate_cells.len()
    );
    assert!(
        open >= gate_cells.len() / 2,
        "the water must see a real passage through the gate, not a sliver:          only {open} of {} gate cells are open",
        gate_cells.len()
    );
}

/// Slow (~tens of seconds), so `#[ignore]`d by default — run explicitly with
/// `cargo test -p sandbox --features client --test editor_scene --
/// water_actually_flows -- --ignored --nocapture`. Answers the actual
/// gameplay question directly, not "is the passage open": with a reservoir
/// pocket seeded right against the gate (a full spring-fed fill takes far
/// longer to simulate than is practical for a test), does water measurably
/// reach the downstream side once it's opened, compared to an identical run
/// with the gate left closed?
#[test]
#[ignore = "simulates real fluid ticks over the full valley domain; slow"]
fn water_actually_flows_through_an_open_gate_more_than_a_closed_one() {
    let scene = editor_scene::load(valley()).expect("valley project loads");
    let gate_cells = scene.dam_gate_cells().to_vec();
    let water_setup = scene
        .column_water_setup()
        .expect("valley has water")
        .clone();

    // The gate's own bounding box, in voxels: x/y span the opening, and its
    // z-span covers the wall's thickness (plus the sphere's overshoot past
    // each face).
    let (mut lo, mut hi) = (gate_cells[0], gate_cells[0]);
    for &cell in &gate_cells {
        lo = GlobalCell::new(lo.x.min(cell.x), lo.y.min(cell.y), lo.z.min(cell.z));
        hi = GlobalCell::new(hi.x.max(cell.x), hi.y.max(cell.y), hi.z.max(cell.z));
    }
    let north_face_z = lo.z + 4; // back off the sphere's overshoot onto the wall proper

    // Downstream-of-gate columns: just south of the wall's south face, same x
    // footprint as the gate.

    let run = |open: bool| -> f64 {
        // The water is seeded against the closed gate. Seeded water starts
        // asleep, so the gate edit is what wakes it: the same tick the real
        // pipeline would hand the water its committed bricks.
        let closed = scene.world_setup().terrain;
        // Seed a reservoir pocket pressed against the wall's north (upstream)
        // face, a stand-in for "the reservoir has filled up to here" without
        // simulating the spring for real minutes. Widened a few metres past
        // the gate's x-span and sampled against the actual terrain.
        let mut setup = water_setup.clone();
        setup.sources.clear();
        setup.gated_sources = Default::default();
        setup.sinks.clear();
        setup.initial_cells = (lo.x - 16..=hi.x + 16)
            .flat_map(|x| (lo.y..=hi.y).map(move |y| (x, y)))
            .flat_map(|(x, y)| (north_face_z - 16..north_face_z).map(move |z| (x, y, z)))
            .filter_map(|(x, y, z)| {
                let cell = GlobalCell::new(x, y, z);
                matches!(closed.sample(cell), Ok(Sample::Empty { .. })).then_some(cell)
            })
            .collect();
        assert!(
            !setup.initial_cells.is_empty(),
            "no open cell found to seed near the gate's upstream face"
        );
        let mut water = spall_sim::column_water::AuthoritativeColumnWater::new(&closed, setup)
            .expect("water builds");
        let mut terrain = scene.world_setup().terrain;
        if open {
            // Cut the gate's authored cells directly (not through the edit
            // pipeline: this test is only about the water's reaction to a cut,
            // not staging/committing it; that is covered by
            // `dam_gate_opens_and_closes_...`).
            let mut edit = spall_voxel::EditPlan::new(terrain.id());
            for &cell in &gate_cells {
                edit.set(cell, spall_core::MaterialId::AIR);
            }
            terrain.apply_edit(&edit).expect("open the gate directly");
        }
        // Both runs hand the water the gate's bricks, so both wake it; only
        // the open one has actually changed.
        let bricks: Vec<spall_core::BrickCoord> = gate_cells
            .iter()
            .map(|cell| cell.split().0)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        water
            .tick(&terrain, &bricks, &[], 1.0 / 20.0)
            .expect("tick");
        for _ in 0..150 {
            water.tick(&terrain, &[], &[], 1.0 / 20.0).expect("tick");
        }
        // Everything that got past the wall, anywhere downstream of it.
        (lo.x - 16..=hi.x + 16)
            .flat_map(|x| (hi.z + 1..hi.z + 60).map(move |z| (x, z)))
            .map(|(x, z)| water.world().column_volume(x, z))
            .sum()
    };

    let closed_downstream: f64 = run(false);
    let open_downstream: f64 = run(true);
    eprintln!(
        "water past the wall (cell volumes): closed {closed_downstream:.3}, open {open_downstream:.3}"
    );
    assert!(
        open_downstream > closed_downstream + 0.5,
        "opening the gate must measurably wet the downstream side more than \
         leaving it closed: closed {closed_downstream:.3} vs open {open_downstream:.3}"
    );
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

/// ENG-167: the authored valley carries stacked-column water, and the scene
/// ships it (not the dense domain) to a server.
#[test]
fn the_valley_ships_column_water_and_it_stands_up() {
    let scene = editor_scene::load(valley()).expect("valley project loads");
    let setup = scene
        .column_water_setup()
        .expect("valley has water")
        .clone();
    assert!(!setup.initial_cells.is_empty());
    assert!(setup.y_lo.rem_euclid(32) == 0 && setup.y_hi.rem_euclid(32) == 0);
    assert!(setup.y_lo < setup.y_hi);
    let custom = scene.into_custom_world();
    assert!(custom.column_water_setup().is_some());
}

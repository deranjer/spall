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
    let water = scene
        .water_setup()
        .expect("scene seeds authoritative water");
    assert!(!water.initial_fractions.is_empty());
    let terrain = scene.world_setup().terrain;
    for (cell, fraction) in &water.initial_fractions {
        assert_eq!(*fraction, 1.0);
        assert!(matches!(terrain.sample(*cell), Ok(Sample::Empty { .. })));
    }
    let custom = scene.into_custom_world();
    assert!(!custom.player_spawns().is_empty());
    assert!(
        !custom
            .water_setup()
            .expect("custom server world retains water setup")
            .initial_fractions
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
    let water = scene.water_setup().expect("valley has water");
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
    let water = scene.water_setup().expect("valley has water").clone();
    let domain = water.domain;

    let mut sim = spall_sim::Simulation::new(spall_sim::SimulationConfig::new(scene.world_setup()))
        .expect("the valley stands up as a server world");
    sim.set_dam_gate(&gate_cells, true, sandbox::game::materials::STONE)
        .expect("opening the gate submits cleanly");
    for _ in 0..8 {
        sim.tick().expect("tick");
    }

    let fine = spall_fluid::SolidBoundary::capture(&sim.world().terrain().volume, domain)
        .expect("every gate cell is inside the resident air envelope, well inside the domain");
    let coarse = fine
        .coarsened(water.coarsen)
        .expect("the domain is a multiple of the coarsening factor");
    let c = i64::from(water.coarsen);
    let open_fluid_cells: std::collections::BTreeSet<(i64, i64, i64)> = gate_cells
        .iter()
        .filter_map(|cell| {
            let fluid = GlobalCell::new(
                domain.origin().x + (cell.x - domain.origin().x).div_euclid(c),
                domain.origin().y + (cell.y - domain.origin().y).div_euclid(c),
                domain.origin().z + (cell.z - domain.origin().z).div_euclid(c),
            );
            (coarse.is_solid(fluid) == Some(false)).then_some((fluid.x, fluid.y, fluid.z))
        })
        .collect();
    eprintln!(
        "gate voxel cells: {}, distinct open fluid cells at the gate: {}",
        gate_cells.len(),
        open_fluid_cells.len()
    );
    assert!(
        open_fluid_cells.len() >= 4,
        "the fluid solver must see a real, multi-cell-wide passage through \
         the gate, not a sliver at its clipped edge: only {} open fluid cells",
        open_fluid_cells.len()
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
    let mut water_setup = scene.water_setup().expect("valley has water").clone();
    water_setup.execution = spall_sim::WaterExecution::Inline;
    let domain = water_setup.domain;
    let c = i64::from(water_setup.coarsen);
    let to_fluid = |cell: GlobalCell| {
        GlobalCell::new(
            domain.origin().x + (cell.x - domain.origin().x).div_euclid(c),
            domain.origin().y + (cell.y - domain.origin().y).div_euclid(c),
            domain.origin().z + (cell.z - domain.origin().z).div_euclid(c),
        )
    };

    // The gate's own bounding box, in voxels: x/y span the opening, and its
    // z-span covers the wall's thickness (plus the sphere's overshoot past
    // each face).
    let (mut lo, mut hi) = (gate_cells[0], gate_cells[0]);
    for &cell in &gate_cells {
        lo = GlobalCell::new(lo.x.min(cell.x), lo.y.min(cell.y), lo.z.min(cell.z));
        hi = GlobalCell::new(hi.x.max(cell.x), hi.y.max(cell.y), hi.z.max(cell.z));
    }
    let north_face_z = lo.z + 4; // back off the sphere's overshoot onto the wall proper
    let south_face_z = hi.z - 4;

    // Downstream-of-gate fluid cells: just south of the wall's south face,
    // same x/y footprint as the gate.
    let downstream_fluid: Vec<GlobalCell> = (lo.x..=hi.x)
        .flat_map(|x| (lo.y..=hi.y).map(move |y| (x, y)))
        .map(|(x, y)| to_fluid(GlobalCell::new(x, y, south_face_z + 4)))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert!(
        !downstream_fluid.is_empty(),
        "gate spans the wall's z-thickness"
    );

    let run = |open: bool| -> f64 {
        let mut terrain = scene.world_setup().terrain;
        if open {
            // Cut the gate's authored cells directly (not through the edit
            // pipeline — this test is only about the fluid solver's reaction
            // to a cut, not about staging/committing it; that path is
            // already covered by `dam_gate_opens_and_closes_...`).
            let mut edit = spall_voxel::EditPlan::new(terrain.id());
            for &cell in &gate_cells {
                edit.set(cell, spall_core::MaterialId::AIR);
            }
            terrain.apply_edit(&edit).expect("open the gate directly");
        }
        // Seed a reservoir pocket pressed right against the wall's north
        // (upstream) face — a stand-in for "the reservoir has filled up to
        // here", without spending real simulated minutes on the spring.
        // Widened a few metres past the gate's own x-span and sampled
        // against the actual terrain (not assumed open) because the lake's
        // authored ellipse narrows sharply right at the dam and need not
        // line up exactly with the gate's x position.
        let in_domain = |cell: GlobalCell| {
            let o = domain.origin();
            let d = domain.dimensions();
            (o.x..o.x + i64::from(d[0])).contains(&cell.x)
                && (o.y..o.y + i64::from(d[1])).contains(&cell.y)
                && (o.z..o.z + i64::from(d[2])).contains(&cell.z)
        };
        let mut setup = water_setup.clone();
        setup.initial_fractions = (lo.x - 16..=hi.x + 16)
            .flat_map(|x| (lo.y..=hi.y).map(move |y| (x, y)))
            .flat_map(|(x, y)| (north_face_z - 16..north_face_z).map(move |z| (x, y, z)))
            .filter_map(|(x, y, z)| {
                let cell = GlobalCell::new(x, y, z);
                (in_domain(cell) && matches!(terrain.sample(cell), Ok(Sample::Empty { .. })))
                    .then_some((cell, 1.0))
            })
            .collect();
        assert!(
            !setup.initial_fractions.is_empty(),
            "no open cell found to seed near the gate's upstream face"
        );
        let mut water = spall_sim::AuthoritativeWater::new(&terrain, setup).expect("water builds");
        for _ in 0..150 {
            water.tick(&terrain, false, 1.0 / 20.0).expect("tick");
        }
        let grid = water.grid().expect("inline execution keeps the grid");
        downstream_fluid
            .iter()
            .filter_map(|&cell| grid.fraction_at(cell))
            .sum()
    };

    let closed_downstream: f64 = run(false);
    let open_downstream: f64 = run(true);
    eprintln!(
        "downstream fraction sum: closed {closed_downstream:.3}, open {open_downstream:.3} \
         across {} monitored cells",
        downstream_fluid.len()
    );
    assert!(
        open_downstream > closed_downstream + 0.5,
        "opening the gate must measurably wet the downstream side more than \
         leaving it closed: closed {closed_downstream:.3} vs open {open_downstream:.3}"
    );
}

/// The valley's water fits the server budget: a bounded 0.5 m domain on the
/// water worker, with springs and a drain, spawned on dry land, and its seed
/// water all placed (none lost to coarsening).
#[test]
fn valley_water_region_is_bounded_and_seeds_fully() {
    let scene = editor_scene::load(valley()).expect("valley project loads");
    let water = scene.water_setup().expect("valley has water").clone();
    assert_eq!(water.coarsen, 2);
    assert!(matches!(
        water.execution,
        spall_sim::WaterExecution::Worker { .. }
    ));
    assert!(
        !water.sources.is_empty(),
        "springs keep the streams running"
    );
    assert!(!water.sinks.is_empty(), "the outlet creek drains");
    let fluid_cells = water.domain.cell_count() / 8;
    eprintln!(
        "valley: {} solid voxels, water domain {:?} at {:?} = {fluid_cells} fluid cells, {} seed voxels, spawns {:?}",
        scene.solid_cell_count(),
        water.domain.dimensions(),
        water.domain.origin(),
        water.initial_fractions.len(),
        scene.player_spawns,
    );
    assert!(fluid_cells <= 60_000, "domain over budget: {fluid_cells}");

    // The server builds exactly this world at startup and on every reset.
    let started = std::time::Instant::now();
    spall_sim::Simulation::new(spall_sim::SimulationConfig::new(scene.world_setup()))
        .expect("the valley stands up as a server world");
    eprintln!("valley simulation built in {:?}", started.elapsed());

    let terrain = scene.world_setup().terrain;
    for spawn in &scene.player_spawns {
        let cell = GlobalCell::new(
            (spawn[0] / 0.25).floor() as i64,
            (spawn[1] / 0.25).round() as i64,
            (spawn[2] / 0.25).floor() as i64,
        );
        let below = GlobalCell::new(cell.x, cell.y - 1, cell.z);
        assert!(matches!(terrain.sample(cell), Ok(Sample::Empty { .. })));
        assert!(matches!(terrain.sample(below), Ok(Sample::Filled(_))));
    }

    let mut inline = water.clone();
    inline.execution = spall_sim::WaterExecution::Inline;
    let mut region = spall_sim::AuthoritativeWater::new(&terrain, inline).unwrap();
    let seed = region.seed_report();
    eprintln!("seed {seed:?}");
    assert!(seed.dropped_in_solid_m3 < 1.0e-9, "{seed:?}");
    let started = std::time::Instant::now();
    for _ in 0..10 {
        region.tick(&terrain, false, 1.0 / 20.0).unwrap();
    }
    eprintln!(
        "10 valley water steps at 20 Hz: {:?} each, frame {:?}",
        started.elapsed() / 10,
        region.frame().volume_m3
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

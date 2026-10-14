use spall_core::GlobalCell;
use spall_ecology::living::{LivingState, Season, VisualFrame};
use spall_voxel::{EditPlan, Sample};

#[test]
fn generated_world_is_deterministic_grounded_and_biome_filtered() {
    let a = sandbox::worldgen_scene::generate("showcase", 1, 256).unwrap();
    let b = sandbox::worldgen_scene::generate("showcase", 1, 256).unwrap();
    assert_eq!(
        a.vegetation().encode().unwrap(),
        b.vegetation().encode().unwrap()
    );
    let state = a.vegetation();
    state.validate().unwrap();
    assert!(state.organisms.iter().any(|p| !p.wood.is_empty()));
    assert!(state.organisms.iter().any(|p| p.wood.is_empty()));
    for plant in &state.organisms {
        let s = state
            .species
            .iter()
            .find(|s| s.id == plant.species)
            .unwrap();
        let [x, y, z] = plant.root;
        assert!(s.biomes & (1 << a.world().columns.biome(x, z) as u8) != 0);
        // Land plants stand dry; underwater ones stand in their depth range.
        let depth = a.world().columns.water_depth(x, z).clamp(0, 255) as u8;
        assert!((s.depth[0]..=s.depth[1]).contains(&depth));
        assert!(
            matches!(a.world().terrain.sample(GlobalCell::new(x,y-1,z)),Ok(Sample::Filled(m)) if s.soils.contains(&m.0))
        );
        assert_eq!(y, i64::from(a.world().columns.height(x, z)) + 1);
    }
    let encoded = state.encode().unwrap();
    assert_eq!(LivingState::decode(&encoded).unwrap(), *state);
    let frame = state.visual(&a.world().terrain);
    assert_eq!(
        VisualFrame::decode(&frame.encode().unwrap()).unwrap(),
        frame
    );
}

#[test]
fn seasons_persist_and_deciduous_trees_drop_only_their_leaves() {
    let scene = sandbox::worldgen_scene::generate("showcase", 2, 256).unwrap();
    let mut state = scene.vegetation().clone();
    state.season_ms = 1000;
    state.start_season = Season::Summer;
    for (elapsed, season) in [
        (0, Season::Summer),
        (1000, Season::Autumn),
        (2000, Season::Winter),
        (3000, Season::Spring),
    ] {
        state.time_ms = elapsed;
        assert_eq!(state.season(), season);
        let frame = state.visual(&scene.world().terrain);
        for s in frame.species.iter().filter(|s| s.tree) {
            let mut one = frame.clone();
            one.plants.retain(|p| p.species == s.id);
            let parts = one.parts([32., 0., 32.], 200.);
            if season == Season::Winter && !s.evergreen {
                assert!(parts.is_empty());
            } else if one.plants.iter().any(|p| !p.tips.is_empty()) {
                assert!(!parts.is_empty());
            }
            assert!(parts.iter().all(|p| p.size.iter().all(|n| *n <= 0.25)));
        }
        assert_eq!(
            LivingState::decode(&state.encode().unwrap())
                .unwrap()
                .season(),
            season
        );
    }
}

#[test]
fn root_cuts_stop_reproduction_and_ground_plants_do_not_float_after_excavation() {
    let scene = sandbox::worldgen_scene::generate("showcase", 4, 256).unwrap();
    let mut state = scene.vegetation().clone();
    let mut terrain = scene.world().terrain.clone();
    let root = state
        .organisms
        .iter()
        .find(|p| p.committed > 0)
        .unwrap()
        .root;
    let grass = state
        .organisms
        .iter()
        .find(|p| p.wood.is_empty() && p.alive)
        .unwrap()
        .root;
    let mut cut = EditPlan::new(terrain.id());
    cut.set(
        GlobalCell::new(root[0], root[1], root[2]),
        spall_core::MaterialId::AIR,
    );
    cut.set(
        GlobalCell::new(grass[0], grass[1] - 1, grass[2]),
        spall_core::MaterialId::AIR,
    );
    terrain.apply_edit(&cut).unwrap();
    // Presentation hides damage immediately, before that plant's fair-budget turn.
    assert!(
        !state
            .visual(&terrain)
            .plants
            .iter()
            .any(|p| p.root == root || p.root == grass)
    );
    for _ in 0..100 {
        state.advance(&scene.world().columns, &terrain, 1000);
    }
    assert!(
        state
            .organisms
            .iter()
            .find(|p| p.root == root)
            .is_none_or(|p| !p.alive)
    );
}

#[test]
fn offspring_remain_on_allowed_biomes_and_survive_roundtrip() {
    let scene = sandbox::worldgen_scene::generate("showcase", 7, 256).unwrap();
    let mut state = scene.vegetation().clone();
    let mut terrain = scene.world().terrain.clone();
    // A sparse stand leaves real establishment gaps; the initial dense stand
    // can legitimately reject every tree seed because of spacing competition.
    let mut clearing = spall_voxel::EditPlan::new(terrain.id());
    state.organisms.retain(|p| {
        if !p.wood.is_empty() && p.id % 4 != 0 {
            for at in p.wood.iter().take(usize::from(p.committed)) {
                clearing.set(
                    GlobalCell::new(at[0], at[1], at[2]),
                    spall_core::MaterialId::AIR,
                );
            }
            false
        } else {
            true
        }
    });
    terrain.apply_edit(&clearing).unwrap();
    let original = state.next_id;
    // Trees seed only once fully grown (and 90 s apart), so give them time.
    for _ in 0..900 {
        state.advance(&scene.world().columns, &terrain, 1000);
    }
    assert!(
        state.next_id > original,
        "seeds should establish at least one child"
    );
    for tree in [true, false] {
        assert!(
            state.organisms.iter().any(|p| p.id >= original
                && state
                    .species
                    .iter()
                    .any(|s| s.id == p.species && s.tree == tree)),
            "both trees and ground plants must establish offspring"
        );
    }
    for child in state.organisms.iter().filter(|p| p.id >= original) {
        let def = state
            .species
            .iter()
            .find(|s| s.id == child.species)
            .unwrap();
        assert!(
            def.biomes & (1 << scene.world().columns.biome(child.root[0], child.root[2]) as u8)
                != 0
        );
        assert_eq!(child.committed, 0);
    }
    assert_eq!(
        LivingState::decode(&state.encode().unwrap()).unwrap(),
        state
    );
}

#[test]
fn authoritative_growth_and_checkpoint_recovery_keep_the_same_living_state() {
    let scene = sandbox::worldgen_scene::generate("showcase", 1, 256).unwrap();
    let setup = scene.world_setup();
    let mut config = spall_sim::SimulationConfig::new(setup);
    let mut vegetation = scene.vegetation().clone();
    // A new world's trees are all grown; plant a sapling beside the first tree
    // that has room, old enough to add its next cell as soon as it is allowed.
    let tree = vegetation
        .organisms
        .iter()
        .find(|p| !p.wood.is_empty())
        .map(|p| (p.species, p.root))
        .unwrap();
    let columns = &scene.world().columns;
    let terrain = &scene.world().terrain;
    let sapling = (6..40_i64)
        .flat_map(|d| [(d, 0), (-d, 0), (0, d), (0, -d), (d, d), (-d, -d)])
        .find_map(|(dx, dz)| {
            let (x, z) = (tree.1[0] + dx, tree.1[2] + dz);
            let root = [x, i64::from(columns.height(x, z)) + 1, z];
            vegetation.plant_sapling(columns, terrain, tree.0, root)
        })
        .expect("room for a sapling");
    for p in &mut vegetation.organisms {
        if p.id == sapling {
            p.age_ms = 100_000;
        }
    }
    let before: usize = vegetation
        .organisms
        .iter()
        .map(|p| usize::from(p.committed))
        .sum();
    config.vegetation = Some(vegetation);
    let mut sim = spall_sim::Simulation::new(config).unwrap();
    for _ in 0..180 {
        sim.tick().unwrap();
    }
    let after: usize = sim
        .vegetation_state()
        .unwrap()
        .organisms
        .iter()
        .map(|p| usize::from(p.committed))
        .sum();
    assert!(
        after > before,
        "normal owner-thread edits must commit new wood"
    );
    let dir = std::env::temp_dir().join(format!("spall-vegetation-save-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("checkpoint.sqlite");
    let cfg = spall_server::PersistConfig {
        world_id: 118,
        seed: 1,
        generator_version: 1,
    };
    let cp = spall_server::persist::capture(&sim, &cfg, 0).unwrap();
    let mut writer = spall_store::Writer::open(&db).unwrap();
    writer.publish_checkpoint(&cp).unwrap();
    let recovered = writer.recover().unwrap();
    let (restored, _) = spall_server::persist::restore(
        &recovered,
        &cfg,
        spall_server::RecoveryChoice::RequireClean,
        sandbox::game::manifest(),
        spall_structure::AnchorPlane::at(0),
        spall_physics::PhysicsConfig {
            disable_ccd: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(restored.vegetation_state(), sim.vegetation_state());
    assert_eq!(restored.world().world_hash(), sim.world().world_hash());
    // A tiny clock journal follows a full state journal without re-encoding the forest every tick.
    let mut next = sim.vegetation_state().unwrap().clone();
    next.credit_ms = 317;
    writer
        .append_journal(&[
            spall_store::JournalRecord {
                seq: 1,
                tick: 181,
                payload: spall_store::JournalPayload::VegetationState(
                    sim.vegetation_state().unwrap().encode().unwrap(),
                ),
            },
            spall_store::JournalRecord {
                seq: 2,
                tick: 182,
                payload: spall_store::JournalPayload::VegetationClock {
                    time_ms: next.time_ms,
                    credit_ms: 317,
                },
            },
        ])
        .unwrap();
    let (replayed, _) = spall_server::persist::restore(
        &writer.recover().unwrap(),
        &cfg,
        spall_server::RecoveryChoice::RequireClean,
        sandbox::game::manifest(),
        spall_structure::AnchorPlane::at(0),
        spall_physics::PhysicsConfig {
            disable_ccd: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(replayed.vegetation_state().unwrap(), &next);
    drop(writer);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Tree growth takes one wood cell per five seconds however many trees are
/// growing: every cell is a full edit commit on the tick thread, and four a
/// second stalled a 1 km server for most of each second.
#[test]
fn tree_growth_is_throttled_to_one_cell_per_interval() {
    let scene = sandbox::worldgen_scene::generate("showcase", 1, 256).unwrap();
    let setup = scene.world_setup();
    let mut config = spall_sim::SimulationConfig::new(setup);
    let mut vegetation = scene.vegetation().clone();
    let columns = &scene.world().columns;
    let terrain = &scene.world().terrain;
    let trees: Vec<(u16, [i64; 3])> = vegetation
        .organisms
        .iter()
        .filter(|p| !p.wood.is_empty())
        .map(|p| (p.species, p.root))
        .collect();
    let mut planted = Vec::new();
    for (species, root) in trees.iter().cycle().take(trees.len() * 6) {
        for (dx, dz) in [(26, 0), (-26, 0), (0, 26), (0, -26), (30, 30), (-30, -30)] {
            let (x, z) = (root[0] + dx, root[2] + dz);
            let at = [x, i64::from(columns.height(x, z)) + 1, z];
            if let Some(id) = vegetation.plant_sapling(columns, terrain, *species, at) {
                planted.push(id);
                break;
            }
        }
        if planted.len() >= 8 {
            break;
        }
    }
    assert!(planted.len() >= 4, "only {} saplings fit", planted.len());
    for p in &mut vegetation.organisms {
        if planted.contains(&p.id) {
            p.age_ms = 10_000_000;
        }
    }
    let committed = |v: &spall_ecology::living::LivingState| -> usize {
        v.organisms.iter().map(|p| usize::from(p.committed)).sum()
    };
    let before = committed(&vegetation);
    config.vegetation = Some(vegetation);
    let mut sim = spall_sim::Simulation::new(config).unwrap();
    for _ in 0..1100 {
        sim.tick().unwrap();
    }
    let grown = committed(sim.vegetation_state().unwrap()) - before;
    // About 18 seconds: the first cell as soon as a sapling is due, then one
    // every 300 ticks at most (four in all), never one per tree per second.
    assert!(
        (1..=4).contains(&grown),
        "{grown} wood cells grew in 1100 ticks with {} saplings due",
        planted.len()
    );
}

#[cfg(feature = "client")]
#[test]
fn two_network_clients_receive_generated_vegetation_and_the_same_season() {
    use spall_client::{BaselineScene, ClientNetConfig, run_replication_client_with_manifest};
    use spall_net::{Fingerprint, JoinToken, TransportConfig};
    use std::time::{Duration, Instant};
    let dir = std::env::temp_dir().join(format!("spall-vegetation-net-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let scene =
        sandbox::worldgen_scene::generate_with_season("showcase", 1, 128, Season::Autumn).unwrap();
    let spawns = scene.player_spawns().to_vec();
    let vegetation = scene.vegetation().clone();
    let world = spall_server::CustomWorld::new(spawns, move || scene.world_setup())
        .with_vegetation(vegetation);
    let token = JoinToken::generate().unwrap();
    let fp = dir.join("fingerprint");
    let addr = dir.join("addr");
    let mut config = spall_server::ServeConfig::headless(
        "127.0.0.1:0".parse().unwrap(),
        spall_server::Scene::Custom,
        token,
    );
    config.custom_world = Some(world);
    config.max_ticks = 240;
    config.quiescence_ticks = 0;
    config.paced = true;
    config.fingerprint_out = Some(fp.clone());
    config.addr_out = Some(addr.clone());
    config.log_json = dir.join("server.jsonl");
    let server = std::thread::spawn(move || {
        spall_server::serve_with_game_content(
            config,
            sandbox::game::tool_catalog(),
            sandbox::game::manifest(),
        )
        .unwrap()
    });
    let read = |path: &std::path::Path| {
        let start = Instant::now();
        loop {
            if let Ok(s) = std::fs::read_to_string(path)
                && !s.trim().is_empty()
            {
                break s.trim().to_owned();
            }
            assert!(start.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let client = ClientNetConfig {
        connect_addr: read(&addr).parse().unwrap(),
        server_fingerprint: Fingerprint::from_hex(&read(&fp)).unwrap(),
        join_token: token,
        script: vec![],
        movement_script: vec![],
        late_join: true,
        baseline_scene: BaselineScene::Walk,
        run_ticks: 240,
        idle_grace: Duration::from_millis(500),
        overall_timeout: Duration::from_secs(30),
        log_json: dir.join("client.jsonl"),
        summary_json: None,
        transport: TransportConfig::for_tests(),
        client_residency: None,
        baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
        admin_script: vec![],
    };
    let mut late = client.clone();
    late.log_json = dir.join("late.jsonl");
    let late_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        run_replication_client_with_manifest(late, sandbox::game::manifest()).unwrap()
    });
    let first = run_replication_client_with_manifest(client, sandbox::game::manifest()).unwrap();
    let second = late_thread.join().unwrap();
    let server = server.join().unwrap();
    for report in [&first, &second] {
        assert!(report.vegetation_keyframes_received >= 1);
        assert!(report.vegetation_plants_received > 0);
        assert_eq!(report.vegetation_season_received, Season::Autumn as u64);
        assert_eq!(report.final_world_hash, server.final_world_hash);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// A regional client is sent the plants around its player, not the world; a
/// client that streams everything still gets every plant. Both keep cover (the
/// climate raster is cached from the first keyframe).
#[cfg(feature = "client")]
#[test]
fn a_regional_client_receives_only_the_plants_around_its_player() {
    use spall_client::{
        BaselineScene, ClientNetConfig, ClientResidencyLimits, run_replication_client_with_manifest,
    };
    use spall_net::{Fingerprint, JoinToken, TransportConfig};
    use std::time::{Duration, Instant};
    let dir = std::env::temp_dir().join(format!("spall-vegetation-region-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let scene = sandbox::worldgen_scene::generate("natural", 4, 512).unwrap();
    let total = scene.vegetation().organisms.len() as u64;
    let spawns = scene.player_spawns().to_vec();
    let vegetation = scene.vegetation().clone();
    let world = spall_server::CustomWorld::new(spawns, move || scene.world_setup())
        .with_vegetation(vegetation);
    let token = JoinToken::generate().unwrap();
    let fp = dir.join("fingerprint");
    let addr = dir.join("addr");
    let mut config = spall_server::ServeConfig::headless(
        "127.0.0.1:0".parse().unwrap(),
        spall_server::Scene::Custom,
        token,
    );
    config.custom_world = Some(world);
    // A client joins late and then runs 600 ticks; the server must outlast it on a
    // slow machine (on CI 900 ended before a slow join finished: "connection lost"
    // in every run), so it runs 1500.
    config.max_ticks = 1500;
    config.quiescence_ticks = 0;
    config.paced = true;
    config.fingerprint_out = Some(fp.clone());
    config.addr_out = Some(addr.clone());
    config.log_json = dir.join("server.jsonl");
    let server = std::thread::spawn(move || {
        spall_server::serve_with_game_content(
            config,
            sandbox::game::tool_catalog(),
            sandbox::game::manifest(),
        )
        .unwrap()
    });
    let read = |path: &std::path::Path| {
        let start = Instant::now();
        loop {
            if let Ok(s) = std::fs::read_to_string(path)
                && !s.trim().is_empty()
            {
                break s.trim().to_owned();
            }
            assert!(start.elapsed() < Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let whole = ClientNetConfig {
        connect_addr: read(&addr).parse().unwrap(),
        server_fingerprint: Fingerprint::from_hex(&read(&fp)).unwrap(),
        join_token: token,
        script: vec![],
        movement_script: vec![],
        late_join: true,
        baseline_scene: BaselineScene::Walk,
        run_ticks: 600,
        idle_grace: Duration::from_millis(500),
        // CI runs this beside a dozen other worlds on a few cores, with ticks of
        // 50-90 ms: the clients' 600 ticks took over a minute and the old 60 s
        // limit closed the connection under them.
        overall_timeout: Duration::from_secs(240),
        log_json: dir.join("whole.jsonl"),
        summary_json: None,
        transport: TransportConfig::for_tests(),
        client_residency: None,
        baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
        admin_script: vec![],
    };
    let mut regional = whole.clone();
    regional.log_json = dir.join("regional.jsonl");
    regional.client_residency = Some(ClientResidencyLimits {
        stream_initial: true,
        defer_catalogue: false,
        budget_bricks: 512,
        interest_radius_bricks: 1,
        max_dense_bytes: 64 * 1024 * 1024,
    });
    let regional_thread = std::thread::spawn(move || {
        run_replication_client_with_manifest(regional, sandbox::game::manifest()).unwrap()
    });
    let full = run_replication_client_with_manifest(whole, sandbox::game::manifest()).unwrap();
    let region = regional_thread.join().unwrap();
    server.join().unwrap();
    assert!(full.vegetation_keyframes_received >= 1);
    assert!(
        full.vegetation_plants_received >= total * 9 / 10,
        "{} of {total}",
        full.vegetation_plants_received
    );
    assert!(region.vegetation_keyframes_received >= 1);
    assert!(
        region.vegetation_plants_received < full.vegetation_plants_received / 3,
        "regional {} against whole {}",
        region.vegetation_plants_received,
        full.vegetation_plants_received
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Far trees are drawn with half-resolution crowns: about an eighth of the leaf
/// cubes, half a metre a side, filling the same space; near trees and ground
/// plants are untouched.
#[test]
fn far_crowns_use_fewer_larger_blocks_over_the_same_space() {
    let scene = sandbox::worldgen_scene::generate("showcase", 1, 256).unwrap();
    let frame = scene.vegetation().visual(&scene.world().terrain);
    let eye = [32., 0., 32.];
    let leaf = |p: &spall_ecology::living::SoftPart, size: f32| p.size == [size; 3];
    let fine = frame.parts(eye, 200.);
    let all_far = frame.parts_lod(eye, 200., 0.0);
    let all_near = frame.parts_lod(eye, 200., 1.0e9);
    assert_eq!(
        fine.len(),
        all_near.len(),
        "a far limit past every plant changes nothing"
    );
    let fine_leaves: Vec<_> = fine
        .iter()
        .filter(|p| leaf(p, 0.25) && p.yaw == 0.)
        .collect();
    let coarse_leaves: Vec<_> = all_far.iter().filter(|p| leaf(p, 0.5)).collect();
    assert!(fine_leaves.len() > 500, "{} fine leaves", fine_leaves.len());
    assert!(
        coarse_leaves.len() * 3 < fine_leaves.len(),
        "{} coarse against {} fine",
        coarse_leaves.len(),
        fine_leaves.len()
    );
    // The crowns cover the same volume to within a block.
    let extent = |parts: &[&spall_ecology::living::SoftPart]| {
        let mut lo = [f32::MAX; 3];
        let mut hi = [f32::MIN; 3];
        for p in parts {
            for a in 0..3 {
                lo[a] = lo[a].min(p.center[a] - p.size[a] / 2.);
                hi[a] = hi[a].max(p.center[a] + p.size[a] / 2.);
            }
        }
        (lo, hi)
    };
    let (flo, fhi) = extent(&fine_leaves);
    let (clo, chi) = extent(&coarse_leaves);
    for a in 0..3 {
        assert!(
            (flo[a] - clo[a]).abs() <= 1.0 && (fhi[a] - chi[a]).abs() <= 1.0,
            "axis {a}: {flo:?}..{fhi:?} against {clo:?}..{chi:?}"
        );
    }
    // Ground plants draw the same either way.
    let blades = |parts: &[spall_ecology::living::SoftPart]| {
        parts
            .iter()
            .filter(|p| p.size[0] < 0.2 || p.yaw != 0.)
            .count()
    };
    assert_eq!(blades(&fine), blades(&all_far));
}

/// A ground plant is never placed in a tree's trunk column (the tree's wood is
/// planned before it is committed, so the cell still looks empty).
#[test]
fn no_ground_plant_roots_in_a_trunk_column() {
    for (preset, seed) in [("showcase", 4), ("natural", 2), ("natural", 5)] {
        let scene = sandbox::worldgen_scene::generate(preset, seed, 256).unwrap();
        let state = scene.vegetation();
        let trees: Vec<_> = state
            .organisms
            .iter()
            .filter(|p| !p.wood.is_empty())
            .collect();
        for g in state.organisms.iter().filter(|p| p.wood.is_empty()) {
            for t in &trees {
                let width = i64::from(
                    state
                        .species
                        .iter()
                        .find(|s| s.id == t.species)
                        .unwrap()
                        .trunk_width,
                );
                assert!(
                    !(g.root[0] >= t.root[0]
                        && g.root[0] < t.root[0] + width
                        && g.root[2] >= t.root[2]
                        && g.root[2] < t.root[2] + width
                        && g.root[1] >= t.root[1]),
                    "{preset} {seed}: plant {} roots in tree {}'s trunk",
                    g.id,
                    t.id
                );
            }
        }
    }
}

/// Digging into the ground destroys the cover there and records it; a tunnel
/// far below the surface does not.
#[test]
fn a_hammer_blow_records_the_cover_it_destroyed_and_only_that() {
    use spall_core::units::{BRUSH_UNIT, BrushPoint};
    use spall_core::{EntityId, SphereBrush};
    use spall_protocol::RequestId;
    use spall_sim::{EditIntent, EditTarget};

    let scene = sandbox::worldgen_scene::generate("showcase", 4, 256).unwrap();
    let cols = &scene.world().columns;
    let mut config = spall_sim::SimulationConfig::new(scene.world_setup());
    config.vegetation = Some(scene.vegetation().clone());
    let mut sim = spall_sim::Simulation::new(config).unwrap();
    let (x, z) = (120_i64, 120_i64);
    let (dx, dz) = (180_i64, 60_i64);
    let h = i64::from(cols.height(x, z));
    let h2 = i64::from(cols.height(dx, dz));
    let brush = |cx: i64, cy: i64, cz: i64, r: i64| {
        let half = BRUSH_UNIT / 2;
        SphereBrush::new(
            BrushPoint::from_units(
                cx * BRUSH_UNIT + half,
                cy * BRUSH_UNIT + half,
                cz * BRUSH_UNIT + half,
            ),
            r * BRUSH_UNIT,
        )
        .unwrap()
    };
    assert!(h2 > 24, "deep enough for a tunnel");
    for (id, brush) in [
        (1, brush(x, h, z, 3)),
        // A tunnel 20 cells under the surface of another column.
        (2, brush(dx, h2 - 20, dz, 2)),
    ] {
        sim.submit(EditIntent::cut(
            RequestId(id),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            brush,
        ))
        .unwrap();
    }
    sim.run_until_idle(120).unwrap();
    for _ in 0..4 {
        sim.tick().unwrap();
    }
    let cleared = &sim.vegetation_state().unwrap().cleared;
    let has = |cx: i64, cz: i64| {
        cleared
            .iter()
            .any(|c| i64::from(c.x) == cx && i64::from(c.z) == cz)
    };
    assert!(has(x, z), "the dug column is recorded");
    assert!(has(x + 2, z - 1), "so are its neighbours inside the blow");
    assert!(!has(x + 12, z), "columns outside the blow are not");
    assert!(
        !has(dx, dz),
        "a tunnel far below the surface leaves the cover alone"
    );
    // The record is part of the saved state and the frame clients draw from.
    let state = sim.vegetation_state().unwrap();
    assert_eq!(
        LivingState::decode(&state.encode().unwrap())
            .unwrap()
            .cleared,
        state.cleared
    );
    assert_eq!(sim.vegetation_visual().unwrap().cleared, state.cleared);
}

/// The record is forgotten once the cover has regrown in full.
#[test]
fn a_cleared_column_is_forgotten_after_it_regrows() {
    let scene = sandbox::worldgen_scene::generate("showcase", 4, 256).unwrap();
    let mut state = scene.vegetation().clone();
    state.clear_cover(100, 100);
    state.clear_cover(100, 101);
    assert_eq!(state.cleared.len(), 2);
    let terrain = scene.world().terrain.clone();
    for _ in 0..(spall_ecology::cover::FULL_AFTER_S - 5) {
        state.advance(&scene.world().columns, &terrain, 1000);
    }
    assert_eq!(state.cleared.len(), 2, "still regrowing");
    for _ in 0..10 {
        state.advance(&scene.world().columns, &terrain, 1000);
    }
    assert!(state.cleared.is_empty(), "grown back, nothing to remember");
    // Hitting a regrowing column again restarts its growth.
    state.clear_cover(7, 7);
    let first = state.cleared[0].at_s;
    for _ in 0..30 {
        state.advance(&scene.world().columns, &terrain, 1000);
    }
    state.clear_cover(7, 7);
    assert_eq!(state.cleared.len(), 1);
    assert!(state.cleared[0].at_s > first);
}

/// A natural world grows its underwater plants on the sea floor and nowhere
/// dry, and its land plants stay out of the water.
#[test]
fn natural_worlds_grow_underwater_plants_only_in_water() {
    let scene = sandbox::worldgen_scene::generate("natural", 4, 1024).unwrap();
    let state = scene.vegetation();
    let cols = &scene.world().columns;
    let aquatic: Vec<u16> = state
        .species
        .iter()
        .filter(|s| s.depth[1] > 0)
        .map(|s| s.id)
        .collect();
    assert!(aquatic.len() >= 6, "{aquatic:?}");
    let (mut wet, mut kinds) = (0, std::collections::BTreeSet::new());
    for p in &state.organisms {
        let depth = cols.water_depth(p.root[0], p.root[2]);
        if aquatic.contains(&p.species) {
            assert!(
                depth >= 2,
                "plant {} species {} in {depth} cells",
                p.id,
                p.species
            );
            wet += 1;
            kinds.insert(p.species);
        } else {
            assert_eq!(
                depth, 0,
                "land plant {} species {} stands in water",
                p.id, p.species
            );
        }
    }
    assert!(wet > 20, "only {wet} underwater plants");
    assert!(kinds.len() >= 4, "only {kinds:?} grew");
}

/// Pine forest is a stand of pines close together: far more trees per area
/// than the boreal forest around it, and nearly all of them pines.
#[test]
fn the_pine_forest_is_dense_and_made_of_pines() {
    use spall_worldgen::Biome;
    let scene = sandbox::worldgen_scene::generate("natural", 4, 1024).unwrap();
    let (cols, state) = (&scene.world().columns, scene.vegetation());
    let mut area = std::collections::BTreeMap::<u8, f64>::new();
    for z in 0..1024 {
        for x in 0..1024 {
            *area.entry(cols.biome(x, z) as u8).or_default() += 1.0;
        }
    }
    let pines = [5u16, 90, 91, 92, 93];
    let (mut pine_trees, mut pine_pines, mut boreal_trees) = (0.0, 0.0, 0.0);
    for p in state.organisms.iter().filter(|p| !p.wood.is_empty()) {
        match cols.biome(p.root[0], p.root[2]) {
            Biome::PineForest => {
                pine_trees += 1.0;
                pine_pines += f64::from(u8::from(pines.contains(&p.species)));
            }
            Biome::BorealForest => boreal_trees += 1.0,
            _ => {}
        }
    }
    let (pine_area, boreal_area) = (
        area[&(Biome::PineForest as u8)],
        area[&(Biome::BorealForest as u8)],
    );
    assert!(pine_area > 5_000.0 && boreal_area > 5_000.0);
    let (pine_density, boreal_density) = (pine_trees / pine_area, boreal_trees / boreal_area);
    assert!(
        pine_density > 1.5 * boreal_density,
        "pine {pine_density:.6} vs boreal {boreal_density:.6} trees per column"
    );
    assert!(
        pine_pines / pine_trees > 0.9,
        "{pine_pines} of {pine_trees} are pines"
    );
}

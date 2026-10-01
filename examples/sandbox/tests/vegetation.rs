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
        assert!(!a.world().columns.is_water(x, z));
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
    for _ in 0..240 {
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
    let scene = sandbox::worldgen_scene::generate("showcase", 1, 128).unwrap();
    let setup = scene.world_setup();
    let mut config = spall_sim::SimulationConfig::new(setup);
    let mut vegetation = scene.vegetation().clone();
    for p in &mut vegetation.organisms {
        if p.committed == 0 {
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

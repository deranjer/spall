#![cfg(feature = "client")]
//! Tint is network-visible: with a real QUIC server hosting the tinted forest,
//! an early client and a late joiner both see the exact variant materials on the
//! terrain, a trunk cut detaches a body whose cells still carry their variants,
//! and the two replicas and the server agree on the world hash. This is the
//! network acceptance the simulation/checkpoint test in `appearance.rs` does not
//! establish. See `docs/reports/ENG-95.md`.
//!
//! Run: `cargo test -p sandbox --features client --test tint_replication`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sandbox::appearance;
use sandbox::editor_scene;
use sandbox::game::{self, materials};
use spall_client::replica::ReplicaWorld;
use spall_client::{
    BaselineScene, ClientNetConfig, ClientSummary, ScriptTarget, ScriptedAction, cut_request,
    run_replication_client_with_manifest,
};
use spall_core::{GlobalCell, MaterialId};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_server::{Scene, ServeConfig};
use spall_sim::{Simulation, SimulationConfig};
use spall_voxel::Sample;

fn forest() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/terrain-trees-forest"
    ))
}

/// Test transport with a long idle timeout: committing the forest's detach split
/// keeps the server's tick thread busy for longer than the default 6 s test
/// timeout, which drops a live client (a finding recorded in ENG-95's report).
fn patient_transport() -> TransportConfig {
    let mut transport = TransportConfig::for_tests();
    transport.idle_timeout = Duration::from_secs(120);
    transport
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spall-tint-replication-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(path: &Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return text.trim().to_owned();
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "waiting for {path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn material_at(sim: &Simulation, cell: GlobalCell) -> Sample {
    sim.world()
        .volume_ref(sim.world().terrain_volume_id())
        .unwrap()
        .sample(cell)
        .unwrap()
}

/// Terrain cell samples and body cell tallies read from a replica.
struct Observed {
    terrain: Vec<Sample>,
    bodies: BTreeMap<MaterialId, u64>,
    terrain_hash: String,
}

fn observe(replica: &Arc<Mutex<ReplicaWorld>>, cells: &[GlobalCell]) -> Observed {
    let replica = replica.lock().unwrap();
    let terrain = replica.terrain_volume().expect("terrain resident");
    let samples = cells
        .iter()
        .map(|cell| terrain.sample(*cell).unwrap())
        .collect();
    let mut bodies: BTreeMap<MaterialId, u64> = BTreeMap::new();
    for (_, volume_id) in replica.body_volumes() {
        let volume = replica.volume(volume_id).expect("body volume");
        for coord in volume.resident_brick_coords() {
            for index in 0..32_768u32 {
                if let Ok(Sample::Filled(m)) = volume.sample_local(coord, index) {
                    *bodies.entry(m).or_default() += 1;
                }
            }
        }
    }
    Observed {
        terrain: samples,
        bodies,
        terrain_hash: format!("{:?}", replica.terrain_hash()),
    }
}

#[allow(clippy::too_many_arguments)]
fn client_config(
    dir: &Path,
    label: &str,
    addr: std::net::SocketAddr,
    fingerprint: Fingerprint,
    token: JoinToken,
    script: Vec<ScriptedAction>,
    run_ticks: u64,
    slot: Arc<Mutex<Option<Arc<Mutex<ReplicaWorld>>>>>,
) -> ClientNetConfig {
    ClientNetConfig {
        connect_addr: addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script,
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::BridgeCut,
        run_ticks,
        idle_grace: Duration::from_secs(25),
        overall_timeout: Duration::from_secs(60),
        log_json: dir.join(format!("{label}.jsonl")),
        summary_json: None,
        transport: patient_transport(),
        client_residency: None,
        baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
        on_replica_ready: Some(Arc::new(move |replica| {
            *slot.lock().unwrap() = Some(replica);
        })),
        interactive: None,
        client_authoritative: false,
        admin_script: Vec::new(),
    }
}

#[test]
fn tinted_terrain_and_detached_bodies_replicate_to_early_and_late_clients() {
    let scene = editor_scene::load(forest()).expect("forest loads");
    // The expected initial world: the same builder the server runs.
    let sim = Simulation::new(SimulationConfig::new(scene.world_setup())).expect("world");
    let trunk = (0..160)
        .flat_map(|x| (0..160).map(move |z| (x, z)))
        .flat_map(|(x, z)| (6..12).map(move |y| GlobalCell::new(x, y, z)))
        .find(|cell| {
            matches!(material_at(&sim, *cell), Sample::Filled(m)
                if m.0 >= appearance::VARIANT_ID_BASE
                    && appearance::base_material(m) == materials::WOOD)
        })
        .expect("a tinted trunk above the lawn");
    // Cells around the trunk (bodies form here) plus a strided lawn sample.
    let mut cells = Vec::new();
    for dx in -24..=24 {
        for dy in -8..=24 {
            for dz in -24..=24 {
                cells.push(GlobalCell::new(trunk.x + dx, trunk.y + dy, trunk.z + dz));
            }
        }
    }
    let expected: Vec<Sample> = cells.iter().map(|c| material_at(&sim, *c)).collect();
    let tinted_expected = expected
        .iter()
        .filter(|s| matches!(s, Sample::Filled(m) if m.0 >= appearance::VARIANT_ID_BASE))
        .count();
    assert!(
        tinted_expected > 200,
        "tinted cells near the trunk: {tinted_expected}"
    );

    let dir = scratch();
    let token = JoinToken::generate().unwrap();
    let mut config = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
    config.custom_world = Some(scene.into_custom_world());
    config.max_ticks = 500;
    config.quiescence_ticks = 0;
    config.min_clients = 1;
    config.max_clients = 4;
    config.paced = true;
    config.startup_timeout = Duration::from_secs(30);
    config.log_json = dir.join("server.jsonl");
    config.fingerprint_out = Some(dir.join("server.fingerprint"));
    config.addr_out = Some(dir.join("server.addr"));
    config.transport = patient_transport();
    config.dev_unvalidated_actions = true;
    let server = std::thread::spawn(move || {
        spall_server::serve_with_game_content(config, game::tool_catalog(), game::manifest())
    });
    let fingerprint = Fingerprint::from_hex(&wait_for(&dir.join("server.fingerprint"))).unwrap();
    let addr: std::net::SocketAddr = wait_for(&dir.join("server.addr")).parse().unwrap();

    // The early client cuts through the trunk, detaching the crown.
    let early_slot = Arc::new(Mutex::new(None));
    let early_cfg = client_config(
        &dir,
        "early",
        addr,
        fingerprint,
        token,
        vec![ScriptedAction {
            at_tick: 30,
            request: cut_request(1, 0, [trunk.x, trunk.y, trunk.z], 2),
            target: ScriptTarget::Terrain,
        }],
        420,
        early_slot.clone(),
    );
    let early = std::thread::spawn(move || {
        run_replication_client_with_manifest(early_cfg, game::manifest())
    });
    // The late joiner arrives after the cut has committed and bodies exist.
    std::thread::sleep(Duration::from_millis(4_500));
    let late_slot = Arc::new(Mutex::new(None));
    let late_cfg = client_config(
        &dir,
        "late",
        addr,
        fingerprint,
        token,
        Vec::new(),
        850,
        late_slot.clone(),
    );
    let late = std::thread::spawn(move || {
        run_replication_client_with_manifest(late_cfg, game::manifest())
    });

    let early_summary: ClientSummary = early.join().unwrap().expect("early client");
    let late_summary: ClientSummary = late.join().unwrap().expect("late client");
    let server_summary = server.join().unwrap().expect("server");
    println!(
        "server hash {} bodies {}; early {} bodies {}; late {} bodies {} (baseline bricks {})",
        server_summary.final_world_hash,
        server_summary.body_count,
        early_summary.final_world_hash,
        early_summary.body_count,
        late_summary.final_world_hash,
        late_summary.body_count,
        late_summary.baseline_bricks
    );
    println!(
        "early rejects {} {:?}, baseline failures {}",
        early_summary.action_requests_rejected,
        early_summary.action_reject_reasons,
        early_summary.baseline_transfer_failures
    );
    println!(
        "early: result {:?} applied {} rejected {} repairs {} actions {} last tick {} solid {} / server solid {}",
        early_summary.result,
        early_summary.transactions_applied,
        early_summary.transactions_rejected,
        early_summary.repairs_applied,
        early_summary.actions_sent,
        early_summary.last_server_tick,
        early_summary.total_solid_cells,
        server_summary.total_solid_cells
    );
    assert!(early_summary.connected && late_summary.connected);
    assert!(late_summary.late_join && late_summary.baseline_bricks > 0);
    assert!(
        server_summary.body_count > 0,
        "the cut detached bodies on the server"
    );
    assert_eq!(
        early_summary.final_world_hash,
        server_summary.final_world_hash
    );
    assert_eq!(
        late_summary.final_world_hash,
        server_summary.final_world_hash
    );

    let early_replica = early_slot.lock().unwrap().clone().expect("early replica");
    let late_replica = late_slot.lock().unwrap().clone().expect("late replica");
    let early_seen = observe(&early_replica, &cells);
    let late_seen = observe(&late_replica, &cells);
    assert_eq!(early_seen.terrain_hash, late_seen.terrain_hash);
    assert_eq!(
        early_seen.bodies, late_seen.bodies,
        "both replicas hold the same cells in the same detached bodies"
    );

    // Terrain cells keep exactly their authored variant on both replicas; a cell
    // that left the terrain is inside the cut or moved into a body.
    let centre = [trunk.x as f64, trunk.y as f64, trunk.z as f64];
    let mut moved: BTreeMap<MaterialId, u64> = BTreeMap::new();
    let mut tinted_terrain = 0;
    for (index, cell) in cells.iter().enumerate() {
        let Sample::Filled(before) = expected[index] else {
            continue;
        };
        for seen in [&early_seen, &late_seen] {
            if let Sample::Filled(now) = seen.terrain[index] {
                assert_eq!(now, before, "{cell:?} changed on a replica");
            }
        }
        match late_seen.terrain[index] {
            Sample::Filled(now) => {
                tinted_terrain += usize::from(now.0 >= appearance::VARIANT_ID_BASE);
            }
            _ => {
                let d = [
                    cell.x as f64 - centre[0],
                    cell.y as f64 - centre[1],
                    cell.z as f64 - centre[2],
                ];
                if (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() > 2.6 {
                    *moved.entry(before).or_default() += 1;
                }
            }
        }
    }
    assert!(
        tinted_terrain > 100,
        "variants replicated on terrain: {tinted_terrain}"
    );
    for (material, count) in &moved {
        assert!(
            late_seen.bodies.get(material).copied().unwrap_or(0) >= *count,
            "{material:?}: {count} cells left the terrain, bodies hold {:?}",
            late_seen.bodies.get(material)
        );
    }
    let manifest = game::manifest();
    let variant_body_cells: u64 = late_seen
        .bodies
        .iter()
        .filter(|(m, _)| m.0 >= appearance::VARIANT_ID_BASE && m.0 != materials::LAMP.0)
        .map(|(_, n)| *n)
        .sum();
    assert!(
        variant_body_cells > 0,
        "detached bodies carry variants: {:?}",
        late_seen.bodies
    );
    for material in late_seen.bodies.keys() {
        assert!(
            manifest.contains(*material),
            "unknown material {material:?}"
        );
    }
    println!(
        "tinted terrain cells {tinted_terrain}; {} cells moved into bodies; variant body cells {variant_body_cells} across {} materials",
        moved.values().sum::<u64>(),
        late_seen.bodies.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

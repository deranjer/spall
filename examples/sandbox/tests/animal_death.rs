#![cfg(feature = "client")]
//! ENG-237, end to end: a real server hosts a generated world with one deer
//! and the game's creature shape. A client swings the hammer at the deer: the
//! deer disappears from the client's replica, its voxels appear as terrain
//! where it stood, and a second swing cuts part of that carcass away.
//!
//! Run: `cargo test --release -p sandbox --features client --test animal_death`.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glam::{Quat, Vec3};
use sandbox::animal_visual::{AnimalVisual, MAX_ANIMAL_VISUAL_CELLS, material_mapping};
use sandbox::{game, worldgen_scene};
use spall_client::replica::ReplicaWorld;
use spall_client::window::hammer_request;
use spall_client::{
    BaselineScene, ClientNetConfig, InteractiveSession, run_replication_client_with_manifest,
};
use spall_core::{GlobalCell, MaterialId};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_protocol::CreatureRecord;
use spall_server::creature_service::{CreatureDirector, CreatureIdPlan, CreatureService};
use spall_server::{Scene, ServeConfig};
use spall_sim::Simulation;
use spall_voxel::Sample;

const CELL_M: f64 = 0.25;

fn patient_transport() -> TransportConfig {
    let mut transport = TransportConfig::for_tests();
    transport.idle_timeout = Duration::from_secs(120);
    transport
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spall-animal-death-{}", std::process::id()));
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
            start.elapsed() < Duration::from_secs(60),
            "waiting for {path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Spawns one idle deer at a fixed pose, once.
struct OneDeer(Option<CreatureRecord>);

impl CreatureDirector for OneDeer {
    fn plan(&mut self, _: u64, _: &Simulation, ids: &mut CreatureIdPlan) -> Vec<CreatureRecord> {
        let Some(mut record) = self.0.take() else {
            return Vec::new();
        };
        record.creature_id = ids.creature_id();
        vec![record]
    }
}

fn deer_visual() -> AnimalVisual {
    let deer = sandbox::animals::species()
        .unwrap()
        .into_iter()
        .find(|s| s.key == "deer")
        .unwrap();
    let mapping = material_mapping(&game::manifest());
    AnimalVisual::new(deer.asset, &mapping, MAX_ANIMAL_VISUAL_CELLS).unwrap()
}

fn terrain_cells(
    replica: &Arc<Mutex<ReplicaWorld>>,
    lo: GlobalCell,
    hi: GlobalCell,
) -> Option<HashMap<GlobalCell, MaterialId>> {
    let guard = replica.lock().unwrap();
    let volume = guard.terrain_volume()?;
    let mut out = HashMap::new();
    for x in lo.x..=hi.x {
        for y in lo.y..=hi.y {
            for z in lo.z..=hi.z {
                let cell = GlobalCell::new(x, y, z);
                match volume.sample(cell) {
                    Ok(Sample::Filled(m)) => {
                        out.insert(cell, m);
                    }
                    Ok(Sample::Empty { .. }) => {}
                    _ => return None,
                }
            }
        }
    }
    Some(out)
}

#[test]
fn a_hammer_strike_kills_a_deer_and_leaves_an_editable_carcass() {
    let scene = worldgen_scene::generate("showcase", 1, 256).expect("generate");
    let spawn = scene.player_spawns()[0];

    // The deer stands 3 m east of the player spawn, idle, facing +X.
    let visual = deer_visual();
    let cells = visual.sample("idle", 0.0).unwrap();
    let low = cells.iter().map(|c| c.center.y).fold(f32::MAX, f32::min);
    let root = [spawn[0] + 3.0, spawn[1] - f64::from(low) + 0.05, spawn[2]];
    let deer_materials: BTreeSet<MaterialId> = cells.iter().map(|c| c.material).collect();
    let centroid = cells.iter().map(|c| c.center).sum::<Vec3>() / cells.len() as f32;
    let body = Vec3::new(root[0] as f32, root[1] as f32, root[2] as f32) + centroid;
    // Horizontal footprint of the deer in terrain cells, plus a cell of slack.
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for c in &cells {
        lo = lo.min(c.center);
        hi = hi.max(c.center);
    }
    let cell = |m: f64| (m / CELL_M).floor() as i64;
    let box_lo = GlobalCell::new(
        cell(root[0] + f64::from(lo.x)) - 1,
        cell(root[1]) - 12,
        cell(root[2] + f64::from(lo.z)) - 1,
    );
    let box_hi = GlobalCell::new(
        cell(root[0] + f64::from(hi.x)) + 1,
        cell(root[1] + f64::from(hi.y)) + 2,
        cell(root[2] + f64::from(hi.z)) + 1,
    );

    let dir = scratch();
    let token = JoinToken::generate().unwrap();
    let mut config = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
    config.server.custom_world = Some(scene.into_custom_world());
    config.harness.max_ticks = 3000;
    config.harness.quiescence_ticks = 0;
    config.harness.min_clients = 1;
    config.server.max_clients = 1;
    config.server.paced = true;
    config.harness.startup_timeout = Duration::from_secs(60);
    config.server.log_json = dir.join("server.jsonl");
    config.harness.fingerprint_out = Some(dir.join("server.fingerprint"));
    config.harness.addr_out = Some(dir.join("server.addr"));
    config.server.transport = patient_transport();
    config.server.creatures = Some(CreatureService::new(Box::new(OneDeer(Some(
        CreatureRecord {
            creature_id: 0,
            species: "deer".into(),
            translation_m: root,
            rotation_xyzw: Quat::IDENTITY.to_array(),
            locomotion_tag: 0,
            clip_phase_tick: 0,
        },
    )))));
    config.server.creature_shape = Some(sandbox::animal_visual::creature_shape().unwrap());
    let server = std::thread::spawn(move || {
        spall_server::serve_with_game_content(config, game::tool_catalog(), game::manifest())
    });
    let fingerprint = Fingerprint::from_hex(&wait_for(&dir.join("server.fingerprint"))).unwrap();
    let addr: SocketAddr = wait_for(&dir.join("server.addr")).parse().unwrap();

    let session = InteractiveSession::new();
    let slot: Arc<Mutex<Option<Arc<Mutex<ReplicaWorld>>>>> = Arc::new(Mutex::new(None));
    let slot_for_client = slot.clone();
    let cfg = ClientNetConfig {
        connect_addr: addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::BridgeCut,
        run_ticks: 2900,
        idle_grace: Duration::from_secs(40),
        overall_timeout: Duration::from_secs(120),
        log_json: dir.join("client.jsonl"),
        summary_json: None,
        transport: patient_transport(),
        client_residency: None,
        baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
        on_replica_ready: Some(Arc::new(move |replica| {
            *slot_for_client.lock().unwrap() = Some(replica);
        })),
        interactive: Some(session.clone()),
        client_authoritative: false,
        admin_script: Vec::new(),
    };
    let client =
        std::thread::spawn(move || run_replication_client_with_manifest(cfg, game::manifest()));
    let replica = loop {
        if let Some(r) = slot.lock().unwrap().clone() {
            break r;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    // The client holds the deer and the terrain around it.
    let deadline = Instant::now() + Duration::from_secs(60);
    let before = loop {
        let has_deer = replica.lock().unwrap().creatures().records().count() == 1;
        if has_deer && let Some(cells) = terrain_cells(&replica, box_lo, box_hi) {
            break cells;
        }
        assert!(
            Instant::now() < deadline,
            "the client never got the deer and its terrain"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    // Swing at the deer's body from 3 m west and 1.5 m up.
    let eye = body + Vec3::new(-3.0, 1.5, 0.0);
    session.push_action(hammer_request(2_000_000, eye, (body - eye).normalize(), 2));

    let deadline = Instant::now() + Duration::from_secs(30);
    let carcass: HashMap<GlobalCell, MaterialId> = loop {
        let gone = replica.lock().unwrap().creatures().records().count() == 0;
        let added = terrain_cells(&replica, box_lo, box_hi).map(|now| {
            now.into_iter()
                .filter(|(cell, m)| before.get(cell) != Some(m))
                .collect::<HashMap<_, _>>()
        });
        // Every carcass cell is its own transaction; wait for the count to settle.
        if gone && let Some(added) = added.clone().filter(|a| a.len() >= 10) {
            std::thread::sleep(Duration::from_millis(500));
            let now = terrain_cells(&replica, box_lo, box_hi).unwrap();
            let settled: HashMap<_, _> = now
                .into_iter()
                .filter(|(cell, m)| before.get(cell) != Some(m))
                .collect();
            if settled.len() == added.len() {
                break settled;
            }
            continue;
        }
        assert!(
            Instant::now() < deadline,
            "no carcass: deer gone {gone}, new cells {:?}",
            added.map(|a| a.len())
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    println!("carcass of {} terrain cells", carcass.len());
    for (cell, material) in &carcass {
        assert!(
            deer_materials.contains(material),
            "carcass cell {cell:?} is {material:?}, not a deer material"
        );
        assert!(
            !before.contains_key(cell),
            "carcass replaced solid terrain at {cell:?}"
        );
    }

    // Cut the top of the carcass with a single-cell swing from straight above.
    let top = *carcass.keys().max_by_key(|c| (c.y, c.x, c.z)).unwrap();
    let above = Vec3::new(
        ((top.x as f64 + 0.5) * CELL_M) as f32,
        ((top.y as f64 + 8.0) * CELL_M) as f32,
        ((top.z as f64 + 0.5) * CELL_M) as f32,
    );
    session.push_action(hammer_request(2_000_001, above, Vec3::NEG_Y, 0));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let cells = terrain_cells(&replica, top, top).unwrap_or_default();
        if cells.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the carcass cell {top:?} was never cut"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let rest = terrain_cells(&replica, box_lo, box_hi).unwrap();
    let left = carcass.keys().filter(|c| rest.contains_key(c)).count();
    assert_eq!(left, carcass.len() - 1, "only the struck cell is gone");

    session.request_stop();
    client.join().unwrap().expect("client run");
    server.join().unwrap().expect("server run");
}

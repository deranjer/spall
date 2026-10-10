//! ENG-167: a networked client receives the stacked-column water as region
//! baselines and quantized deltas; a late joiner converges to the same water;
//! an admin world reset re-seeds every client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spall_client::{
    BaselineScene, ClientNetConfig, ScriptTarget, ScriptedAction, cut_request,
    run_replication_client,
};
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_protocol::AdminCommand;
use spall_server::{CustomWorld, Scene, ServeConfig, serve};
use spall_sim::column_water::ColumnWaterSetup;
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, EditPlan, Volume};

const DAM_X: i64 = 20;

fn unique_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "spall-eng167-{}-{}-{}",
        tag,
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn wait_for_file(path: &PathBuf, deadline: Duration) -> String {
    let start = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && !s.trim().is_empty()
        {
            return s.trim().to_string();
        }
        assert!(
            start.elapsed() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 64 x 32 x 16 basin with a dam at x=20 and a lake behind it.
fn basin_world() -> (CustomWorld, u64) {
    let mut plan = EditPlan::new(VolumeId::new(1).unwrap());
    let mut solid_cells = 0u64;
    for z in 0..16 {
        for y in 0..32 {
            for x in 0..64 {
                let wall = x == 0 || x == 63 || z == 0 || z == 15;
                if y < 2 || (wall && y < 20) || (x == DAM_X && y < 10) {
                    plan.set(GlobalCell::new(x, y, z), MaterialId(1));
                    solid_cells += 1;
                }
            }
        }
    }
    let mut cells = Vec::new();
    for z in 1..15 {
        for y in 2..9 {
            for x in 1..DAM_X {
                cells.push(GlobalCell::new(x, y, z));
            }
        }
    }
    let build = move || {
        let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        for bx in 0..2 {
            terrain
                .insert_brick(
                    BrickCoord::new(bx, 0, 0),
                    Brick::uniform(MaterialId::AIR, Revision(1)),
                )
                .unwrap();
        }
        terrain.apply_edit(&plan).unwrap();
        let mut world = spall_sim::fixtures::flat_terrain_setup();
        world.terrain = terrain;
        world.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 31, 15));
        world.anchor = AnchorPlane::at(0);
        world
    };
    // Spawn downstream of the dam, on the floor.
    let spawns = vec![[10.0, 0.6, 1.0]];
    (
        CustomWorld::new(spawns, build).with_column_water(ColumnWaterSetup::new(0, 32, cells)),
        solid_cells,
    )
}

#[test]
fn client_receives_column_water_late_joiner_converges_and_reset_reseeds() {
    let dir = unique_dir("column-water");
    let token = JoinToken::generate().unwrap();
    let fp_path = dir.join("server.fingerprint");
    let addr_path = dir.join("server.addr");
    let (world, pristine_solid_cells) = basin_world();

    let mut server_cfg = ServeConfig::headless(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        Scene::Custom,
        token,
    );
    server_cfg.max_ticks = 420;
    server_cfg.quiescence_ticks = 0;
    server_cfg.paced = true;
    server_cfg.startup_timeout = Duration::from_secs(20);
    server_cfg.log_json = dir.join("server.jsonl");
    server_cfg.summary_json = Some(dir.join("server.summary.json"));
    server_cfg.fingerprint_out = Some(fp_path.clone());
    server_cfg.addr_out = Some(addr_path.clone());
    server_cfg.dev_unvalidated_actions = true;
    server_cfg.admin_commands = true;
    server_cfg.custom_world = Some(world);
    let server_thread = std::thread::spawn(move || serve(server_cfg));

    let fingerprint =
        Fingerprint::from_hex(&wait_for_file(&fp_path, Duration::from_secs(20))).unwrap();
    let connect_addr: SocketAddr = wait_for_file(&addr_path, Duration::from_secs(20))
        .parse()
        .unwrap();

    let client_config = ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        // Breach the dam, then reset the world well after the cut committed.
        script: vec![ScriptedAction {
            at_tick: 30,
            request: cut_request(1, 0, [DAM_X, 5, 7], 3),
            target: ScriptTarget::Terrain,
        }],
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::Walk,
        run_ticks: 400,
        idle_grace: Duration::from_millis(800),
        overall_timeout: Duration::from_secs(60),
        log_json: dir.join("client.jsonl"),
        summary_json: Some(dir.join("client.summary.json")),
        transport: TransportConfig::for_tests(),
        client_residency: None,
        baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
        admin_script: vec![(180, AdminCommand::ResetWorld)],
    };
    let mut late_config = client_config.clone();
    late_config.script.clear();
    late_config.admin_script.clear();
    late_config.log_json = dir.join("late-client.jsonl");
    late_config.summary_json = None;
    let late_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        run_replication_client(late_config).expect("late client")
    });
    let client = run_replication_client(client_config).expect("client run");
    let late = late_thread.join().unwrap();
    let server = server_thread
        .join()
        .expect("server thread")
        .expect("server run");

    assert!(client.connected);
    assert_eq!(client.water_keyframe_errors, 0);
    assert_eq!(late.water_keyframe_errors, 0);
    assert!(
        client.water_regions_received >= 1,
        "the client holds the lake's regions: {}",
        client.water_regions_received
    );
    assert!(
        client.water_keyframes_received >= 1,
        "baselines arrive: {}",
        client.water_keyframes_received
    );
    assert!(
        late.water_keyframes_received >= 1,
        "a late joiner starts from baselines"
    );
    assert_eq!(
        late.water_regions_received, client.water_regions_received,
        "both clients hold the same regions"
    );
    assert!(server.water_keyframes_sent >= 1);
    eprintln!(
        "ENG167 network: {} bytes queued, client {} baselines / {} deltas, late {} baselines / {} deltas",
        server.water_bytes_queued,
        client.water_keyframes_received,
        client.water_delta_frames_received,
        late.water_keyframes_received,
        late.water_delta_frames_received
    );
    assert!(
        client.transactions_applied >= 1,
        "the dam cut replicated before the reset"
    );
    assert_eq!(
        client.admin_statuses.len(),
        1,
        "{:?}",
        client.admin_statuses
    );
    assert!(client.admin_statuses[0].accepted);
    assert_eq!(server.world_resets, 1);
    assert_eq!(client.world_resets_installed, 1);
    assert_eq!(server.total_solid_cells, pristine_solid_cells);
    assert_eq!(client.final_world_hash, server.final_world_hash);
}

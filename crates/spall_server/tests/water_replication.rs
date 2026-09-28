//! ENG-105 increment 2: a networked client receives the server's water as
//! presentation keyframes, and an admin world reset replaces every client's
//! world (terrain, bodies, water) with the scene's initial state.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use spall_client::{
    BaselineScene, ClientNetConfig, ScriptTarget, ScriptedAction, cut_request,
    run_replication_client,
};
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::grid_mac::GridReservoirFixture;
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_protocol::AdminCommand;
use spall_server::{CustomWorld, Scene, ServeConfig, serve};
use spall_sim::WaterSetup;
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, EditPlan, Sample, Volume};

fn unique_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "spall-eng105-{}-{}-{}",
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

/// The ENG-103 reservoir (a walled 6 m x 3 m x 2 m box split by a dam) as a
/// server world with its authored water, plus the box's solid cell count.
fn reservoir_world() -> (CustomWorld, u64) {
    let fixture = GridReservoirFixture::new(1, false).unwrap();
    let spec = fixture.grid().spec();
    let dims = spec.dimensions();
    let mut fractions = Vec::new();
    let mut solids = EditPlan::new(VolumeId::new(1).unwrap());
    let mut solid_cells = 0u64;
    for z in 0..i64::from(dims[2]) {
        for y in 0..i64::from(dims[1]) {
            for x in 0..i64::from(dims[0]) {
                let cell = GlobalCell::new(x, y, z);
                if let Some(f) = fixture.grid().fraction_at(cell)
                    && f > 0.0
                {
                    fractions.push((cell, f));
                }
                if let Sample::Filled(material) = fixture.volume().sample(cell).unwrap() {
                    solids.set(cell, material);
                    solid_cells += 1;
                }
            }
        }
    }
    let water = WaterSetup::new(spec, fractions);
    let build = move || {
        let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        terrain
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        terrain.apply_edit(&solids).unwrap();
        let mut world = spall_sim::fixtures::flat_terrain_setup();
        world.terrain = terrain;
        world.terrain_collider_region = (
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(
                i64::from(dims[0]) - 1,
                i64::from(dims[1]) - 1,
                i64::from(dims[2]) - 1,
            ),
        );
        world.anchor = AnchorPlane::at(0);
        world
    };
    // Spawn on the upstream floor, clear of the dam.
    let spawns = vec![[1.0, 0.5, 1.0]];
    (
        CustomWorld::new_with_water(spawns, Some(water), build),
        solid_cells,
    )
}

#[test]
fn client_receives_water_and_an_admin_reset_restores_the_scene() {
    let dir = unique_dir("water-reset");
    let token = JoinToken::generate().unwrap();
    let fp_path = dir.join("server.fingerprint");
    let addr_path = dir.join("server.addr");
    let (world, pristine_solid_cells) = reservoir_world();

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
    // The scripted cut aims at a fixed cell with no real aim ray.
    server_cfg.dev_unvalidated_actions = true;
    server_cfg.admin_commands = true;
    server_cfg.custom_world = Some(world);
    let server_thread = std::thread::spawn(move || serve(server_cfg));

    let fingerprint =
        Fingerprint::from_hex(&wait_for_file(&fp_path, Duration::from_secs(20))).unwrap();
    let connect_addr: SocketAddr = wait_for_file(&addr_path, Duration::from_secs(20))
        .parse()
        .unwrap();

    let client = run_replication_client(ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        // Breach the dam, then reset the world well after the cut committed.
        script: vec![ScriptedAction {
            at_tick: 30,
            request: cut_request(1, 0, [12, 4, 4], 2),
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
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
        admin_script: vec![(180, AdminCommand::ResetWorld)],
    })
    .expect("client run");
    let server = server_thread
        .join()
        .expect("server thread")
        .expect("server run");

    assert!(client.connected);
    assert!(
        client.transactions_applied >= 1,
        "the dam cut replicated before the reset"
    );
    assert!(
        client.water_keyframes_received >= 10,
        "water keyframes arrive continuously: {}",
        client.water_keyframes_received
    );
    assert_eq!(client.water_keyframe_errors, 0);
    assert!(server.water_keyframes_sent >= client.water_keyframes_received);

    assert_eq!(
        client.admin_statuses.len(),
        1,
        "{:?}",
        client.admin_statuses
    );
    assert!(
        client.admin_statuses[0].accepted,
        "{:?}",
        client.admin_statuses[0]
    );
    assert_eq!(server.world_resets, 1);
    assert_eq!(client.world_resets_installed, 1);

    // The reset restored every cut cell, and the client's replaced world is
    // exactly the server's.
    assert_eq!(server.total_solid_cells, pristine_solid_cells);
    assert_eq!(client.final_world_hash, server.final_world_hash);
    assert_eq!(client.total_solid_cells, server.total_solid_cells);
}

#[test]
fn admin_reset_is_refused_unless_the_server_enables_it() {
    let dir = unique_dir("reset-refused");
    let token = JoinToken::generate().unwrap();
    let fp_path = dir.join("server.fingerprint");
    let addr_path = dir.join("server.addr");
    let (world, _) = reservoir_world();

    let mut server_cfg = ServeConfig::headless(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        Scene::Custom,
        token,
    );
    server_cfg.max_ticks = 200;
    server_cfg.quiescence_ticks = 0;
    server_cfg.paced = true;
    server_cfg.startup_timeout = Duration::from_secs(20);
    server_cfg.log_json = dir.join("server.jsonl");
    server_cfg.fingerprint_out = Some(fp_path.clone());
    server_cfg.addr_out = Some(addr_path.clone());
    server_cfg.custom_world = Some(world);
    let server_thread = std::thread::spawn(move || serve(server_cfg));

    let fingerprint =
        Fingerprint::from_hex(&wait_for_file(&fp_path, Duration::from_secs(20))).unwrap();
    let connect_addr: SocketAddr = wait_for_file(&addr_path, Duration::from_secs(20))
        .parse()
        .unwrap();
    let client = run_replication_client(ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::Walk,
        run_ticks: 150,
        idle_grace: Duration::from_millis(800),
        overall_timeout: Duration::from_secs(40),
        log_json: dir.join("client.jsonl"),
        summary_json: None,
        transport: TransportConfig::for_tests(),
        client_residency: None,
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
        admin_script: vec![(60, AdminCommand::ResetWorld)],
    })
    .expect("client run");
    let server = server_thread
        .join()
        .expect("server thread")
        .expect("server run");

    assert_eq!(client.admin_statuses.len(), 1);
    assert!(!client.admin_statuses[0].accepted);
    assert_eq!(server.world_resets, 0);
    assert_eq!(client.world_resets_installed, 0);
}

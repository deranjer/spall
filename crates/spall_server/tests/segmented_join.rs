//! ENG-126: negotiate the actual streaming path and refuse admission before publication.
use spall_client::{BaselineScene, ClientNetConfig, run_replication_client};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_server::{Scene, ServeConfig, serve};
use std::time::{Duration, Instant};

fn run_case(budget: u64, admitted: bool) {
    let dir = std::env::temp_dir().join(format!(
        "spall-segmented-join-{}-{budget}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let token = JoinToken::generate().unwrap();
    let fp = dir.join("fingerprint");
    let addr = dir.join("addr");
    let mut server = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::BridgeCut, token);
    server.max_ticks = 240;
    server.quiescence_ticks = 90;
    server.paced = true;
    server.log_json = dir.join("server.jsonl");
    server.fingerprint_out = Some(fp.clone());
    server.addr_out = Some(addr.clone());
    let thread = std::thread::spawn(move || serve(server));
    let started = Instant::now();
    while !(fp.exists() && addr.exists()) {
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    let client = run_replication_client(ClientNetConfig {
        connect_addr: std::fs::read_to_string(&addr)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
        server_fingerprint: Fingerprint::from_hex(std::fs::read_to_string(&fp).unwrap().trim())
            .unwrap(),
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::BridgeCut,
        run_ticks: 0,
        idle_grace: Duration::from_secs(2),
        overall_timeout: Duration::from_secs(8),
        log_json: dir.join("client.jsonl"),
        summary_json: None,
        transport: TransportConfig::for_tests(),
        client_residency: None,
        baseline_budget_bytes: budget,
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
        admin_script: Vec::new(),
    });
    let server = thread.join().unwrap().unwrap();
    if admitted {
        let client = client.unwrap();
        assert!(client.baseline_bricks > 0);
        assert_eq!(client.final_world_hash, server.final_world_hash);
        assert_eq!(server.late_joins_completed, 1);
    } else {
        assert!(client.unwrap_err().to_string().contains("budget"));
        assert_eq!(server.late_joins_completed, 0);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn negotiated_segmented_join_has_exact_geometry() {
    run_case(64 * 1024 * 1024, true);
}
#[test]
fn insufficient_budget_fails_the_network_join_without_installing_a_world() {
    run_case(1, false);
}

#[test]
fn shutdown_drains_an_accepted_baseline_over_a_delayed_link() {
    use spall_core::{BrickCoord, CELLS_PER_BRICK, CellSizeCode, MaterialId, Revision, VolumeId};
    use spall_voxel::{Brick, Volume};
    let dir = std::env::temp_dir().join(format!("spall-shutdown-drain-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let token = JoinToken::generate().unwrap();
    let fp = dir.join("fingerprint");
    let addr = dir.join("addr");
    let mut config = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::BridgeCut, token);
    // Noisy materials force a nonuniform baseline while collision stays exact
    // and cheap: every cell is solid. Transfer outlives the 20 simulation ticks.
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    let mut random = 1u64;
    for x in 0..8 {
        let cells: Vec<_> = (0..CELLS_PER_BRICK)
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                MaterialId(1 + (random & 1) as u16)
            })
            .collect();
        terrain
            .insert_brick(
                BrickCoord::new(x, 0, 0),
                Brick::restored(&cells, Revision::ZERO, false),
            )
            .unwrap();
    }
    config.scene = Scene::Custom;
    config.custom_world = Some(spall_server::CustomWorld::new(
        vec![[0.125, 8.0, 0.125]],
        move || spall_sim::WorldSetup {
            terrain: terrain.clone(),
            ..spall_sim::fixtures::flat_terrain_setup()
        },
    ));
    config.max_ticks = 20;
    config.quiescence_ticks = 0;
    config.paced = true;
    config.transport = TransportConfig::for_tests();
    config.log_json = dir.join("server.jsonl");
    config.fingerprint_out = Some(fp.clone());
    config.addr_out = Some(addr.clone());
    let server = std::thread::spawn(move || serve(config));
    let started = Instant::now();
    while !(fp.exists() && addr.exists()) {
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let target = std::fs::read_to_string(&addr)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let proxy = runtime
        .block_on(spall_net::proxy::UdpProxy::spawn(
            target,
            spall_net::PacketFaultPlan {
                delay: Duration::from_millis(100),
                ..spall_net::PacketFaultPlan::shaped(1, 64 * 1024)
            },
        ))
        .unwrap();
    let client = run_replication_client(ClientNetConfig {
        connect_addr: proxy.local_addr(),
        server_fingerprint: Fingerprint::from_hex(std::fs::read_to_string(&fp).unwrap().trim())
            .unwrap(),
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::BridgeCut,
        run_ticks: 0,
        idle_grace: Duration::from_secs(2),
        overall_timeout: Duration::from_secs(10),
        log_json: dir.join("client.jsonl"),
        summary_json: None,
        transport: TransportConfig::for_tests(),
        client_residency: None,
        baseline_budget_bytes: 64 * 1024 * 1024,
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
        admin_script: Vec::new(),
    });
    let server = server.join().unwrap().unwrap();
    runtime.block_on(proxy.shutdown());
    let client = client.expect("accepted baseline survives bounded server shutdown");
    assert!(client.baseline_bricks > 0);
    assert_eq!(client.baseline_transfer_failures, 0);
    assert_eq!(client.final_world_hash, server.final_world_hash);
    assert_eq!(server.ticks_run, 20, "drain adds no gameplay ticks");
    assert_eq!(server.result, "passed");
    std::fs::remove_dir_all(dir).unwrap();
}

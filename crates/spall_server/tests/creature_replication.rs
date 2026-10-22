//! Creature spawns and despawns reach two real QUIC clients: one that is
//! connected when they happen (ordered stream) and one that joins later (join
//! list). Both replicas must hold identical creatures, before and after a world
//! reset.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spall_client::{BaselineScene, ClientNetConfig, ReplicaReadyHook, run_replication_client};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_protocol::CreatureRecord;
use spall_server::creature_service::{CreatureDirector, CreatureIdPlan, CreatureService};
use spall_server::{CustomWorld, Scene, ServeConfig, serve};
use spall_sim::Simulation;

const SERVER_TICKS: u64 = 1200; // 20 seconds at 60 Hz.
const BIRTH_TICKS: [u64; 2] = [20, 120];
const DESPAWN_TICK: u64 = 200;
const RESET_TICK: u64 = 600;

fn unique_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "spall-eng218-creatures-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test directory");
    dir
}

fn wait_for_file(path: &PathBuf, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        if let Ok(value) = std::fs::read_to_string(path)
            && !value.trim().is_empty()
        {
            return value.trim().to_owned();
        }
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Default)]
struct TestDirector {
    reset_seen: bool,
    rebirths_left: u32,
}

impl CreatureDirector for TestDirector {
    fn on_world_reset(&mut self) {
        self.reset_seen = true;
        self.rebirths_left = 2;
    }

    fn plan(
        &mut self,
        tick: u64,
        _sim: &Simulation,
        ids: &mut CreatureIdPlan,
    ) -> Vec<CreatureRecord> {
        if self.rebirths_left > 0 {
            self.rebirths_left -= 1;
        } else if self.reset_seen || !BIRTH_TICKS.contains(&tick) {
            return Vec::new();
        }
        let creature_id = ids.creature_id();
        vec![CreatureRecord {
            creature_id,
            species: "deer".into(),
            translation_m: [8.0 + creature_id as f64, 2.0, 8.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            locomotion_tag: 0,
            clip_phase_tick: 0,
        }]
    }

    fn despawns(&mut self, tick: u64, _sim: &Simulation) -> Vec<u64> {
        if tick == DESPAWN_TICK {
            vec![1]
        } else {
            Vec::new()
        }
    }
}

#[derive(Clone, Default)]
struct ReplicaHandle(Arc<Mutex<Option<Arc<Mutex<spall_client::replica::ReplicaWorld>>>>>);

impl ReplicaHandle {
    fn hook(&self) -> ReplicaReadyHook {
        let slot = Arc::clone(&self.0);
        Arc::new(move |replica| *slot.lock().unwrap() = Some(replica))
    }

    fn creatures(&self) -> BTreeMap<u64, CreatureRecord> {
        let Some(replica) = self.0.lock().unwrap().clone() else {
            return BTreeMap::new();
        };
        let replica = replica.lock().unwrap();
        replica
            .creatures()
            .records()
            .map(|r| (r.creature_id, r.clone()))
            .collect()
    }
}

fn client_config(
    dir: &std::path::Path,
    name: &str,
    server: (SocketAddr, Fingerprint, JoinToken),
    handle: &ReplicaHandle,
    admin_script: Vec<(u64, spall_protocol::AdminCommand)>,
) -> ClientNetConfig {
    let (connect_addr, fingerprint, token) = server;
    ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::Walk,
        run_ticks: SERVER_TICKS,
        idle_grace: Duration::from_millis(1500),
        overall_timeout: Duration::from_secs(60),
        log_json: dir.join(format!("{name}.jsonl")),
        summary_json: Some(dir.join(format!("{name}.summary.json"))),
        transport: TransportConfig::for_tests(),
        client_residency: None,
        baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
        on_replica_ready: Some(handle.hook()),
        interactive: None,
        client_authoritative: false,
        admin_script,
    }
}

fn ids(creatures: &BTreeMap<u64, CreatureRecord>) -> Vec<u64> {
    creatures.keys().copied().collect()
}

#[test]
fn present_and_late_joining_clients_converge_on_the_authoritative_creatures() {
    let dir = unique_dir();
    let token = JoinToken::generate().unwrap();
    let fingerprint_path = dir.join("server.fingerprint");
    let addr_path = dir.join("server.addr");

    let mut server_cfg = ServeConfig::headless(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        Scene::Custom,
        token,
    );
    server_cfg.harness.max_ticks = SERVER_TICKS;
    server_cfg.harness.quiescence_ticks = 0;
    server_cfg.server.paced = true;
    server_cfg.harness.startup_timeout = Duration::from_secs(20);
    server_cfg.server.log_json = dir.join("server.jsonl");
    server_cfg.harness.summary_json = Some(dir.join("server.summary.json"));
    server_cfg.harness.fingerprint_out = Some(fingerprint_path.clone());
    server_cfg.harness.addr_out = Some(addr_path.clone());
    server_cfg.server.custom_world = Some(CustomWorld::new(vec![[4.0, 0.7, 8.0]], || {
        spall_sim::fixtures::flat_terrain_setup()
    }));
    server_cfg.server.admin_commands = true;
    server_cfg.server.creatures = Some(CreatureService::new(Box::new(TestDirector::default())));
    let server_thread = thread::spawn(move || serve(server_cfg));

    let fingerprint =
        Fingerprint::from_hex(&wait_for_file(&fingerprint_path, Duration::from_secs(20)))
            .expect("server fingerprint");
    let connect_addr = wait_for_file(&addr_path, Duration::from_secs(20))
        .parse::<SocketAddr>()
        .expect("server address");
    let server = (connect_addr, fingerprint, token);

    let first = ReplicaHandle::default();
    let late = ReplicaHandle::default();
    let first_cfg = client_config(
        &dir,
        "first",
        server,
        &first,
        vec![(RESET_TICK, spall_protocol::AdminCommand::ResetWorld)],
    );
    let late_cfg = client_config(&dir, "late", server, &late, Vec::new());
    let first_thread = thread::spawn(move || run_replication_client(first_cfg));
    // Join after both spawns and the despawn (about tick 240), so only the join
    // list can deliver the survivor.
    thread::sleep(Duration::from_secs(4));
    let late_thread = thread::spawn(move || run_replication_client(late_cfg));

    // Before the reset, creature 1 is gone from both replicas and 2 remains.
    // Sample before the server stops: replicas are dropped with their clients.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (a, b) = (first.creatures(), late.creatures());
        if ids(&a) == [2] && a == b {
            break;
        }
        assert!(
            ids(&a) != [3, 4],
            "the reset passed before both replicas settled on the survivor: first={:?} late={:?}",
            ids(&a),
            ids(&b)
        );
        assert!(
            Instant::now() < deadline,
            "replicas did not converge: first={:?} late={:?}",
            ids(&a),
            ids(&b)
        );
        thread::sleep(Duration::from_millis(50));
    }

    // The reset removes every creature; the director spawns two new ones with
    // fresh ids.
    let deadline = Instant::now() + Duration::from_secs(15);
    let (first_records, late_records) = loop {
        let (a, b) = (first.creatures(), late.creatures());
        if ids(&a) == [3, 4] && a == b {
            break (a, b);
        }
        assert!(
            Instant::now() < deadline,
            "replicas did not converge after the reset: first={:?} late={:?}",
            ids(&a),
            ids(&b)
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(first_records, late_records);

    first_thread.join().unwrap().expect("first client");
    late_thread.join().unwrap().expect("late client");
    server_thread
        .join()
        .unwrap()
        .expect("server finished cleanly");
}

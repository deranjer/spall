//! Authoritative creature births reach two real QUIC clients: one that is
//! connected when they happen (ordered transactions) and one that joins later
//! (join-time sync). Both replicas must hold identical ownership records.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spall_client::{BaselineScene, ClientNetConfig, ReplicaReadyHook, run_replication_client};
use spall_core::{
    CellSizeCode, CreatureGeneration, CreatureId, CreatureKey, GlobalCell, MaterialId, WorldEpoch,
    WorldId,
};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_physics::{PartTransform, Representation};
use spall_protocol::{
    CreatureAssetIdentity, CreatureClipMode, CreatureLifecycleState, CreaturePartBinding,
    CreatureStateV1, Hash32, OwnedCreatureAsset, OwnedCreatureCatalog, OwnedCreatureSpecies,
    WIRE_SCHEMA_VERSION, content_manifest_hash,
};
use spall_server::creature_service::{CreatureDirector, CreatureIdPlan, CreatureService};
use spall_server::{CustomWorld, Scene, ServeConfig, serve};
use spall_sim::{AuthoritativeCreatureIntent, CreatureBirthPartInput, Simulation};
use spall_voxel::{EditPlan, Volume};

const SERVER_TICKS: u64 = 900; // 15 seconds at 60 Hz.
const SERVER_WORLD: u128 = spall_server::serve::T10_WORLD_ID;
const BIRTH_TICKS: [u64; 2] = [20, 120];
const RESET_TICK: u64 = 420;

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

fn asset() -> CreatureAssetIdentity {
    CreatureAssetIdentity {
        uuid: [5; 16],
        logical_hash: Hash32([6; 32]),
    }
}

fn catalog() -> Arc<OwnedCreatureCatalog> {
    Arc::new(OwnedCreatureCatalog {
        assets: vec![OwnedCreatureAsset {
            uuid: asset().uuid,
            logical_hash: asset().logical_hash,
            part_ids: vec![1],
            clip_names: vec!["idle".into()],
            portable_material_keys: vec!["stone".into()],
        }],
        species: vec![OwnedCreatureSpecies {
            key: "deer".into(),
            version: 1,
            enabled: true,
            asset: asset(),
        }],
        materials: spall_sim::fixtures::flat_terrain_setup().materials,
        material_mapping_version: 1,
        material_mapping: vec![("stone".into(), MaterialId(1))],
    })
}

struct TestDirector {
    catalog: Arc<OwnedCreatureCatalog>,
    reset_pending: bool,
    rebirths_left: u32,
}

impl CreatureDirector for TestDirector {
    fn catalog(&self) -> Arc<OwnedCreatureCatalog> {
        self.catalog.clone()
    }

    fn on_world_reset(&mut self) {
        self.reset_pending = true;
    }

    fn plan(
        &mut self,
        tick: u64,
        sim: &Simulation,
        ids: &mut CreatureIdPlan,
    ) -> Vec<AuthoritativeCreatureIntent> {
        if std::mem::take(&mut self.reset_pending) {
            self.rebirths_left = 2;
        }
        let rebirth = self.rebirths_left > 0;
        if rebirth {
            self.rebirths_left -= 1;
        } else if !BIRTH_TICKS.contains(&tick)
            || sim
                .creature_allocator_snapshot()
                .is_none_or(|a| a.world_epoch != 1)
        {
            return Vec::new();
        }
        let creature_id = ids.creature_id();
        let volume_id = spall_core::VolumeId::new(ids.volume_id()).unwrap();
        let mut volume = Volume::new(volume_id, CellSizeCode::Quarter);
        volume
            .apply_edit(&EditPlan::filled_box(
                volume_id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(0, 0, 0),
                MaterialId(1),
            ))
            .unwrap();
        let mapping_hash = self
            .catalog
            .with_view(|view| view.material_mapping_hash)
            .unwrap();
        let state = CreatureStateV1 {
            schema: WIRE_SCHEMA_VERSION,
            key: CreatureKey::new(
                WorldId::from_u128(SERVER_WORLD),
                WorldEpoch::new(sim.creature_allocator_snapshot().unwrap().world_epoch).unwrap(),
                CreatureId::new(creature_id).unwrap(),
                CreatureGeneration::INITIAL,
            )
            .unwrap(),
            state_revision: 1,
            lifecycle: CreatureLifecycleState::Alive,
            species_key: "deer".into(),
            species_version: 1,
            asset: asset(),
            material_manifest_hash: content_manifest_hash(sim.world().materials()),
            material_mapping_version: 1,
            material_mapping_hash: mapping_hash,
            clip_name: "idle".into(),
            clip_mode: CreatureClipMode::Playing,
            clip_phase_tick: 0,
            clip_subtick_remainder: 0,
            clip_epoch: 1,
            source_bindings: vec![CreaturePartBinding {
                part_id: 1,
                source_volume_id: volume_id,
                source_revision: 1,
            }],
            aggregate_topology_revision: 1,
            pose_epoch: 1,
            root_translation_m: [8.0 + creature_id as f64, 2.0, 8.0],
            root_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            locomotion_tag: 0,
        };
        vec![AuthoritativeCreatureIntent::Birth {
            state: Box::new(state),
            parts: vec![CreatureBirthPartInput {
                part_id: 1,
                parent_id: None,
                local_transform: PartTransform::IDENTITY,
                volume,
            }],
            representation: Representation::MergedCuboids,
        }]
    }
}

#[derive(Clone, Default)]
struct ReplicaHandle(Arc<Mutex<Option<Arc<Mutex<spall_client::replica::ReplicaWorld>>>>>);

impl ReplicaHandle {
    fn hook(&self) -> ReplicaReadyHook {
        let slot = Arc::clone(&self.0);
        Arc::new(move |replica| *slot.lock().unwrap() = Some(replica))
    }

    fn keys(&self) -> BTreeMap<u64, spall_protocol::CreatureOwnershipRecordV4> {
        let Some(replica) = self.0.lock().unwrap().clone() else {
            return BTreeMap::new();
        };
        let replica = replica.lock().unwrap();
        replica
            .creatures()
            .map(|c| {
                c.records()
                    .map(|r| (r.state.key.creature_id.get(), r.clone()))
                    .collect()
            })
            .unwrap_or_default()
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
        creature_catalog: Some(catalog()),
        late_join: true,
        baseline_scene: BaselineScene::Walk,
        run_ticks: SERVER_TICKS,
        idle_grace: Duration::from_millis(1500),
        overall_timeout: Duration::from_secs(45),
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
    server_cfg.max_ticks = SERVER_TICKS;
    server_cfg.quiescence_ticks = 0;
    server_cfg.paced = true;
    server_cfg.startup_timeout = Duration::from_secs(20);
    server_cfg.log_json = dir.join("server.jsonl");
    server_cfg.summary_json = Some(dir.join("server.summary.json"));
    server_cfg.fingerprint_out = Some(fingerprint_path.clone());
    server_cfg.addr_out = Some(addr_path.clone());
    server_cfg.custom_world = Some(CustomWorld::new(vec![[4.0, 0.7, 8.0]], || {
        spall_sim::fixtures::flat_terrain_setup()
    }));
    server_cfg.admin_commands = true;
    server_cfg.creatures = Some(CreatureService::new(Box::new(TestDirector {
        catalog: catalog(),
        reset_pending: false,
        rebirths_left: 0,
    })));
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
    // Join after both births (about tick 120 = 2 s) so only sync can deliver them.
    thread::sleep(Duration::from_secs(4));
    let late_thread = thread::spawn(move || run_replication_client(late_cfg));

    // Sample before the server stops: replicas are dropped with their clients.
    let deadline = Instant::now() + Duration::from_secs(10);
    let (first_records, late_records) = loop {
        let (a, b) = (first.keys(), late.keys());
        // Creatures 1 and 2 were born before the reset; only 3 and 4 survive it.
        let reset_done = a.keys().copied().eq([3, 4]);
        if a.len() == 2 && a == b && reset_done {
            break (a, b);
        }
        assert!(
            Instant::now() < deadline,
            "replicas did not converge: first={:?} late={:?}",
            a.keys().collect::<Vec<_>>(),
            b.keys().collect::<Vec<_>>()
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

//! ENG-257 world items in the cabin: each authored item rests on a surface in
//! open air inside the cabin, and (with the client feature) a real QUIC run
//! picks items up into the pack, once, with late-join replication.

use sandbox::{editor_scene, game};
use spall_core::GlobalCell;
use spall_voxel::Sample;
use std::path::Path;

fn cabin() -> editor_scene::LoadedScene {
    editor_scene::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cabin")).unwrap()
}

#[test]
fn cabin_items_rest_on_a_surface_inside_the_cabin() {
    let scene = cabin();
    let kinds: Vec<u16> = scene.world_items.iter().map(|item| item.kind).collect();
    assert!(kinds.contains(&game::items::SHOVEL.0), "{kinds:?}");
    assert!(kinds.contains(&game::items::NOTE.0), "{kinds:?}");
    let setup = scene.world_setup();
    let cell = |p: [f64; 3]| {
        let c = p.map(|v| (v * 4.0).floor() as i64);
        GlobalCell::new(c[0], c[1], c[2])
    };
    for item in &scene.world_items {
        let [x, y, z] = item.translation_m;
        assert!(
            matches!(
                setup.terrain.sample(cell([x, y + 0.05, z])),
                Ok(Sample::Empty { .. })
            ),
            "item {item:?} is buried"
        );
        assert!(
            matches!(
                setup.terrain.sample(cell([x, y - 0.05, z])),
                Ok(Sample::Filled(_))
            ),
            "item {item:?} floats"
        );
        // Under a roof: something solid above within 8 m.
        assert!(
            (1..32).any(|dy| matches!(
                setup.terrain.sample(cell([x, y + dy as f64 * 0.25, z])),
                Ok(Sample::Filled(_))
            )),
            "item {item:?} is outside"
        );
    }
}

#[cfg(feature = "client")]
mod network {
    use super::*;
    use sandbox::world_items::in_memory_pickup;
    use spall_client::replica::ReplicaWorld;
    use spall_client::{
        BaselineScene, ClientNetConfig, InteractiveSession, run_replication_client_with_manifest,
    };
    use spall_core::{BrushPoint, SphereBrush};
    use spall_net::{Fingerprint, JoinToken, TransportConfig};
    use spall_protocol::{ActionKind, ActionRequest, ClaimedTarget, InputSeq, RequestId};
    use spall_server::{CustomWorld, Scene, ServeConfig};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let start = Instant::now();
        loop {
            if let Some(value) = f() {
                return value;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "timed out: {what}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn kinds(replica: &Arc<Mutex<ReplicaWorld>>) -> Vec<u16> {
        let mut kinds: Vec<u16> = replica
            .lock()
            .unwrap()
            .world_items()
            .map(|i| i.kind)
            .collect();
        kinds.sort_unstable();
        kinds
    }

    fn interact(id: u64) -> ActionRequest {
        ActionRequest {
            request_id: RequestId(id),
            input_seq: InputSeq(id),
            action: ActionKind::Interact,
            tool: 5,
            // Bogus aim: the server picks from its own player.
            aim_origin_m: [999.0, 999.0, 999.0],
            aim_dir: [0.0, 0.0, -1.0],
            claimed_target: ClaimedTarget::Terrain,
            claimed_brush: SphereBrush::new(BrushPoint::from_units(0, 0, 0), 0).unwrap(),
        }
    }

    /// Two players beside the cabin shovel, one joining late, both press E at
    /// once: exactly one pack gains it, the world loses it on every client,
    /// and a client joining afterwards never sees it.
    #[test]
    fn pickup_goes_to_one_pack_and_replicates_to_late_joiners() {
        let dir = std::env::temp_dir().join(format!("spall-world-items-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let scene = Arc::new(cabin());
        let items = scene.world_items.clone();
        let token = JoinToken::generate().unwrap();
        let mut cfg = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
        let build = scene.clone();
        cfg.server.custom_world = Some(
            CustomWorld::new(
                vec![[9.0, 1.0, 9.5], [8.5, 1.0, 10.5], [10.125, 1.0, 8.5]],
                move || build.world_setup(),
            )
            .with_world_items(items),
        );
        let inventories = Arc::new(Mutex::new(game::PlayerInventories::default()));
        cfg.server.item_pickup = Some(in_memory_pickup(inventories.clone()));
        cfg.server.paced = true;
        cfg.server.max_clients = 3;
        cfg.server.log_json = dir.join("server.jsonl");
        cfg.server.transport = TransportConfig::for_tests();
        cfg.harness.max_ticks = 900;
        cfg.harness.min_clients = 1;
        cfg.harness.quiescence_ticks = 0;
        cfg.harness.startup_timeout = Duration::from_secs(20);
        cfg.harness.fingerprint_out = Some(dir.join("fingerprint"));
        cfg.harness.addr_out = Some(dir.join("addr"));
        let server = std::thread::spawn(move || {
            spall_server::serve_with_game_content(cfg, game::tool_catalog(), game::manifest())
                .expect("item server")
        });
        let fingerprint = Fingerprint::from_hex(
            until("fingerprint", || {
                std::fs::read_to_string(dir.join("fingerprint")).ok()
            })
            .trim(),
        )
        .unwrap();
        let addr = until("addr", || std::fs::read_to_string(dir.join("addr")).ok())
            .trim()
            .parse()
            .unwrap();
        let connect = |name: &str| {
            let session = InteractiveSession::new();
            let slot = Arc::new(Mutex::new(None));
            let ready = slot.clone();
            let cfg = ClientNetConfig {
                connect_addr: addr,
                server_fingerprint: fingerprint,
                join_token: token,
                script: Vec::new(),
                movement_script: Vec::new(),
                late_join: true,
                baseline_scene: BaselineScene::BridgeCut,
                run_ticks: 880,
                idle_grace: Duration::from_secs(20),
                overall_timeout: Duration::from_secs(30),
                log_json: dir.join(format!("{name}.jsonl")),
                summary_json: None,
                transport: TransportConfig::for_tests(),
                client_residency: None,
                baseline_budget_bytes:
                    spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
                on_replica_ready: Some(Arc::new(move |r| *ready.lock().unwrap() = Some(r))),
                interactive: Some(session.clone()),
                client_authoritative: false,
                admin_script: Vec::new(),
            };
            let client = std::thread::spawn(move || {
                run_replication_client_with_manifest(cfg, game::manifest()).expect("item client")
            });
            let replica = until("replica", || slot.lock().unwrap().clone());
            (session, client, replica)
        };
        let (shovel, note) = (game::items::SHOVEL.0, game::items::NOTE.0);
        let (first, client1, replica1) = connect("first");
        until("first sees both items", || {
            (kinds(&replica1) == [shovel, note]).then_some(())
        });
        let (second, client2, replica2) = connect("second");
        until("late joiner sees both items", || {
            (kinds(&replica2) == [shovel, note]).then_some(())
        });
        // Both reach for the shovel at once; the note is out of reach.
        first.push_action(interact(2_000_000));
        second.push_action(interact(2_000_000));
        until("the shovel leaves both worlds", || {
            (kinds(&replica1) == [note] && kinds(&replica2) == [note]).then_some(())
        });
        // A repeat changes nothing.
        first.push_action(interact(2_000_001));
        std::thread::sleep(Duration::from_millis(300));
        let held = |slot| {
            let inventories = inventories.lock().unwrap();
            inventories
                .get(slot)
                .map_or(0, |inventory| inventory.count(game::items::SHOVEL))
        };
        assert_eq!(held(0) + held(1), 1, "exactly one pack has the shovel");
        let (third, client3, replica3) = connect("third");
        until("a later joiner sees only the note", || {
            (kinds(&replica3) == [note]).then_some(())
        });
        for session in [&first, &second, &third] {
            session.request_stop();
        }
        for client in [client1, client2, client3] {
            client.join().unwrap();
        }
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

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
    use sandbox::world_items::{PackStore, pickup};
    use spall_client::replica::ReplicaWorld;
    use spall_client::{
        BaselineScene, ClientNetConfig, InteractiveSession, run_replication_client_with_manifest,
    };
    use spall_core::{BrushPoint, MaterialId, SphereBrush};
    use spall_net::{Fingerprint, JoinToken, TransportConfig};
    use spall_protocol::{ActionKind, ActionRequest, ClaimedTarget, InputSeq, RequestId};
    use spall_server::{CustomWorld, InitialWorldItem, Scene, ServeConfig};
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
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

    /// The cabin door's hinge cell material on `replica`.
    fn hinge(replica: &Arc<Mutex<ReplicaWorld>>) -> Option<MaterialId> {
        let r = replica.lock().unwrap();
        match r
            .terrain_volume()?
            .sample(GlobalCell::new(37, 4, 24))
            .ok()?
        {
            Sample::Filled(m) => Some(m),
            _ => None,
        }
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

    /// A cabin server with in-memory packs, and clients that connect to it.
    struct Cabin {
        dir: PathBuf,
        token: JoinToken,
        addr: SocketAddr,
        fingerprint: Fingerprint,
        server: JoinHandle<spall_server::ServeSummary>,
        inventories: Arc<Mutex<game::PlayerInventories>>,
    }

    struct Client {
        session: Arc<InteractiveSession>,
        thread: JoinHandle<spall_client::ClientSummary>,
        replica: Arc<Mutex<ReplicaWorld>>,
    }

    impl Cabin {
        fn start(name: &str, spawns: Vec<[f64; 3]>, items: Vec<InitialWorldItem>) -> Self {
            let dir = std::env::temp_dir().join(format!("spall-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let scene = Arc::new(cabin());
            let token = JoinToken::generate().unwrap();
            let mut cfg =
                ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
            cfg.server.max_clients = spawns.len();
            cfg.server.custom_world =
                Some(CustomWorld::new(spawns, move || scene.world_setup()).with_world_items(items));
            let inventories = Arc::new(Mutex::new(game::PlayerInventories::default()));
            cfg.server.item_pickup = Some(pickup(PackStore::InMemory(inventories.clone())));
            cfg.server.paced = true;
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
            Self {
                dir,
                token,
                addr,
                fingerprint,
                server,
                inventories,
            }
        }

        fn connect(&self, name: &str) -> Client {
            let session = InteractiveSession::new();
            let slot = Arc::new(Mutex::new(None));
            let ready = slot.clone();
            let cfg = ClientNetConfig {
                connect_addr: self.addr,
                server_fingerprint: self.fingerprint,
                join_token: self.token,
                script: Vec::new(),
                movement_script: Vec::new(),
                late_join: true,
                baseline_scene: BaselineScene::BridgeCut,
                run_ticks: 880,
                idle_grace: Duration::from_secs(20),
                overall_timeout: Duration::from_secs(30),
                log_json: self.dir.join(format!("{name}.jsonl")),
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
            let thread = std::thread::spawn(move || {
                run_replication_client_with_manifest(cfg, game::manifest()).expect("item client")
            });
            let replica = until("replica", || slot.lock().unwrap().clone());
            Client {
                session,
                thread,
                replica,
            }
        }

        /// How many of `item` the pack in `slot` holds.
        fn held(&self, slot: u32, item: game::ItemId) -> u32 {
            let inventories = self.inventories.lock().unwrap();
            inventories
                .get(slot)
                .map_or(0, |inventory| inventory.count(item))
        }

        fn stop(self, clients: Vec<Client>) {
            for client in &clients {
                client.session.request_stop();
            }
            for client in clients {
                client.thread.join().unwrap();
            }
            self.server.join().unwrap();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Two players beside the cabin shovel, one joining late, both press E at
    /// once: exactly one pack gains it, the world loses it on every client,
    /// and a client joining afterwards never sees it.
    #[test]
    fn pickup_goes_to_one_pack_and_replicates_to_late_joiners() {
        let cabin = Cabin::start(
            "world-items",
            vec![[9.0, 1.0, 9.5], [8.5, 1.0, 10.5], [10.125, 1.0, 8.5]],
            super::cabin().world_items,
        );
        let (shovel, note) = (game::items::SHOVEL.0, game::items::NOTE.0);
        let first = cabin.connect("first");
        until("first sees both items", || {
            (kinds(&first.replica) == [shovel, note]).then_some(())
        });
        let second = cabin.connect("second");
        until("late joiner sees both items", || {
            (kinds(&second.replica) == [shovel, note]).then_some(())
        });
        // Both reach for the shovel at once; the note is out of reach.
        first.session.push_action(interact(2_000_000));
        second.session.push_action(interact(2_000_000));
        until("the shovel leaves both worlds", || {
            (kinds(&first.replica) == [note] && kinds(&second.replica) == [note]).then_some(())
        });
        // A repeat changes nothing.
        first.session.push_action(interact(2_000_001));
        std::thread::sleep(Duration::from_millis(300));
        let held = cabin.held(0, game::items::SHOVEL) + cabin.held(1, game::items::SHOVEL);
        assert_eq!(held, 1, "exactly one pack has the shovel");
        let third = cabin.connect("third");
        until("a later joiner sees only the note", || {
            (kinds(&third.replica) == [note]).then_some(())
        });
        cabin.stop(vec![first, second, third]);
    }

    /// A shovel lying beside the closed cabin door: E takes the shovel, and a
    /// retry of that same request (same id) is answered from the admission
    /// ledger instead of falling through to the door. A fresh press then opens
    /// the door, so the door was in reach all along.
    #[test]
    fn a_retried_pickup_beside_a_closed_door_leaves_it_closed() {
        let shovel = game::items::SHOVEL;
        let cabin = Cabin::start(
            "item-door-retry",
            vec![[10.125, 1.0, 8.5]],
            vec![InitialWorldItem {
                kind: shovel.0,
                translation_m: [10.125, 1.0, 9.5],
                yaw_rad: 0.0,
            }],
        );
        let client = cabin.connect("player");
        let (closed, open) = (game::CABIN_DOOR.closed_hinge, game::CABIN_DOOR.open_hinge);
        until("the shovel and a closed door", || {
            (kinds(&client.replica) == [shovel.0] && hinge(&client.replica) == Some(closed))
                .then_some(())
        });
        client.session.push_action(interact(2_000_000));
        until("the shovel is picked up", || {
            kinds(&client.replica).is_empty().then_some(())
        });
        assert_eq!(cabin.held(0, shovel), 1);
        client.session.push_action(interact(2_000_000));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            hinge(&client.replica),
            Some(closed),
            "the retry left the door"
        );
        client.session.push_action(interact(2_000_001));
        until("a new press opens the door", || {
            (hinge(&client.replica) == Some(open)).then_some(())
        });
        cabin.stop(vec![client]);
    }

    /// ENG-273: every client numbers its requests from the same base, so two
    /// players pressing E with the same request id each still get their own
    /// item; the server's ledger is per player.
    #[test]
    fn two_players_using_the_same_request_id_each_pick_up_their_own_item() {
        let (shovel, note) = (game::items::SHOVEL, game::items::NOTE);
        let cabin = Cabin::start(
            "item-same-id",
            vec![[10.125, 1.0, 8.5], [12.0, 1.0, 9.0]],
            vec![
                InitialWorldItem {
                    kind: shovel.0,
                    translation_m: [10.125, 1.0, 9.5],
                    yaw_rad: 0.0,
                },
                InitialWorldItem {
                    kind: note.0,
                    translation_m: [12.0, 1.0, 10.0],
                    yaw_rad: 0.0,
                },
            ],
        );
        let first = cabin.connect("first");
        let second = cabin.connect("second");
        until("both items are replicated", || {
            (kinds(&first.replica).len() == 2 && kinds(&second.replica).len() == 2).then_some(())
        });
        first.session.push_action(interact(2_000_000));
        second.session.push_action(interact(2_000_000));
        until("both items are picked up", || {
            (kinds(&first.replica).is_empty() && kinds(&second.replica).is_empty()).then_some(())
        });
        assert_eq!(cabin.held(0, shovel), 1);
        assert_eq!(cabin.held(1, note), 1);
        cabin.stop(vec![first, second]);
    }

    /// ENG-57 with ENG-273's per-player ledger: a retry sent after a
    /// reconnect still replays the recorded answer and takes nothing more.
    #[test]
    fn a_retry_after_a_reconnect_replays_and_does_not_pick_up_again() {
        let (shovel, note) = (game::items::SHOVEL, game::items::NOTE);
        let cabin = Cabin::start(
            "item-reconnect-retry",
            vec![[10.125, 1.0, 8.5]],
            vec![
                InitialWorldItem {
                    kind: shovel.0,
                    translation_m: [10.125, 1.0, 9.5],
                    yaw_rad: 0.0,
                },
                InitialWorldItem {
                    kind: note.0,
                    translation_m: [10.125, 1.0, 7.0],
                    yaw_rad: 0.0,
                },
            ],
        );
        let before = cabin.connect("before");
        until("both items", || {
            (kinds(&before.replica).len() == 2).then_some(())
        });
        before.session.push_action(interact(2_000_000));
        until("the shovel is picked up", || {
            (kinds(&before.replica) == [note.0]).then_some(())
        });
        before.session.request_stop();
        before.thread.join().unwrap();
        let after = until("the slot is free to rejoin", || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cabin.connect("after"))).ok()
        });
        until("the note is still lying there", || {
            (kinds(&after.replica) == [note.0]).then_some(())
        });
        // The same request id on the replacement session: replayed, not run
        // again against the note now in reach.
        after.session.push_action(interact(2_000_000));
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(kinds(&after.replica), [note.0]);
        assert_eq!(cabin.held(0, note), 0);
        after.session.push_action(interact(2_000_001));
        until("a new request id picks the note up", || {
            kinds(&after.replica).is_empty().then_some(())
        });
        cabin.stop(vec![after]);
    }
}

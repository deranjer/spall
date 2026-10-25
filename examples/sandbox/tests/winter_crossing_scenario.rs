//! ENG-251 Winter Crossing scenario: where the buildings stand, the server
//! rules (objective, death and item restore, the shared win), and (with the
//! client feature) the win reaching every player over real QUIC.

use sandbox::game::items;
use sandbox::winter_crossing::layout::{Layout, WinZone, heading_deg};
use sandbox::winter_crossing::rules::{ScenarioRules, events};
use spall_core::GlobalCell;
use spall_protocol::{GameEvent, SessionId, SlotId, session_player_entity};
use spall_server::{Audience, InitialWorldItem, TickRules};
use spall_sim::fixtures::{WALK_ARENA_SPAWNS, walk_arena_setup};
use spall_sim::{PLAYER_MAX_HEALTH, Simulation, SimulationConfig};
use spall_voxel::Sample;

/// A 128 m natural world: small enough for CPU CI.
const SIZE: u32 = 512;

fn cell(p: [f64; 3], dy: f64) -> GlobalCell {
    let c = [p[0], p[1] + dy, p[2]].map(|v| (v * 4.0).floor() as i64);
    GlobalCell::new(c[0], c[1], c[2])
}

#[test]
fn headings_are_clockwise_from_north() {
    let o = [10.0, 0.0, 10.0];
    let at = |dx: f64, dz: f64| heading_deg(o, [10.0 + dx, 5.0, 10.0 + dz]);
    assert!(at(0.0, -5.0).abs() < 1e-4, "north is -Z");
    assert!((at(5.0, 0.0) - 90.0).abs() < 1e-4, "east is +X");
    assert!((at(0.0, 5.0) - 180.0).abs() < 1e-4);
    assert!((at(-5.0, 0.0) - 270.0).abs() < 1e-4);
    assert!((at(1.0, -1.0) - 45.0).abs() < 1e-4);
}

#[test]
fn the_layout_is_deterministic_and_stands_on_dry_level_ground() {
    let generate = || sandbox::winter_crossing::generate("natural", 1, SIZE).expect("lays out");
    let (scene, layout) = generate();
    assert_eq!(layout, generate().1, "same world, same layout");
    let world = scene.world();
    let size = i64::from(SIZE);
    // Cabin in the south (+Z), tower in the north, shed between them. (A map
    // this small has little level ground near its edges.)
    assert!(layout.cabin[2] > size * 3 / 4, "{layout:?}");
    assert!(layout.tower[2] < size / 2, "{layout:?}");
    assert!((layout.tower[2]..layout.cabin[2]).contains(&layout.shed[2]));
    let solid = |c: GlobalCell| matches!(world.terrain.sample(c), Ok(Sample::Filled(_)));
    for origin in [layout.cabin, layout.tower, layout.shed] {
        let [x, y, z] = origin;
        assert!(!world.columns.is_water(x, z), "{origin:?} is wet");
        assert!(
            solid(GlobalCell::new(x, y - 1, z)),
            "{origin:?} has no ground"
        );
    }
    // Players start inside the cabin on its floor, with headroom.
    assert_eq!(scene.player_spawns(), layout.spawns.as_slice());
    assert!(!layout.spawns.is_empty());
    for &spawn in &layout.spawns {
        assert!(solid(cell(spawn, -0.05)), "spawn {spawn:?} floats");
        assert!(
            (0..8).all(|i| !solid(cell(spawn, 0.05 + f64::from(i) * 0.25))),
            "spawn {spawn:?} is buried"
        );
        assert!(
            (8..48).any(|i| solid(cell(spawn, f64::from(i) * 0.25))),
            "spawn {spawn:?} is outside"
        );
    }
    // The cabin's items and the shed's axe rest on a surface in open air.
    let kinds: Vec<u16> = layout.cabin_items.iter().map(|i| i.kind).collect();
    for kind in [items::SHOVEL, items::COMPASS, items::NOTE] {
        assert!(kinds.contains(&kind.0), "{kinds:?}");
    }
    assert_eq!(layout.axe.kind, items::AXE.0);
    for item in layout.cabin_items.iter().chain([&layout.axe]) {
        assert!(solid(cell(item.translation_m, -0.05)), "{item:?} floats");
        assert!(!solid(cell(item.translation_m, 0.05)), "{item:?} is buried");
    }
    // The win zone is open platform at the top of the tower.
    let zone = layout.win_zone.centre_m;
    assert!(solid(cell(zone, -0.05)) && !solid(cell(zone, 0.05)));
    assert!(zone[1] - (layout.tower[1] as f64 * 0.25) > 20.0, "{zone:?}");
    // The note's heading points from the cabin roughly north to the tower.
    let h = layout.tower_heading_deg;
    assert!(!(60.0..=300.0).contains(&h), "heading {h}");
}

fn session(slot: u32) -> SessionId {
    SessionId::from_parts(SlotId(slot), 1)
}

/// A walk-arena simulation with two players, and rules whose win zone is
/// open floor away from both and whose "cabin" items lie by the first spawn.
fn arena() -> (Simulation, ScenarioRules, Layout) {
    let mut sim = Simulation::new(SimulationConfig::new(walk_arena_setup())).expect("arena");
    for slot in 0..2 {
        sim.add_player(
            session_player_entity(session(slot)),
            WALK_ARENA_SPAWNS[slot as usize],
        );
    }
    let s = WALK_ARENA_SPAWNS[0];
    let item = |kind: sandbox::game::ItemId, dx: f64| InitialWorldItem {
        kind: kind.0,
        translation_m: [s[0] + dx, s[1], s[2] + 1.5],
        yaw_rad: 0.0,
    };
    let layout = Layout {
        cabin: [0; 3],
        tower: [0; 3],
        shed: [0; 3],
        spawns: WALK_ARENA_SPAWNS.to_vec(),
        cabin_items: vec![
            item(items::SHOVEL, 0.0),
            item(items::COMPASS, 1.5),
            item(items::NOTE, 3.0),
        ],
        axe: item(items::AXE, 6.0),
        win_zone: WinZone {
            centre_m: [s[0] + 4.0, s[1], s[2] - 4.0],
            radius_m: 1.0,
        },
        tower_heading_deg: 7.5,
    };
    for item in &layout.cabin_items {
        sim.spawn_world_item(item.kind, item.translation_m, item.yaw_rad)
            .unwrap();
    }
    (sim, ScenarioRules::new(&layout), layout)
}

fn ev(kind: u16, value: i32) -> GameEvent {
    GameEvent { kind, value }
}

#[test]
fn each_player_gets_the_objective_once_and_one_player_at_the_top_wins_for_all() {
    let (mut sim, mut rules, layout) = arena();
    let both = [session(0), session(1)];
    assert_eq!(
        rules.after_tick(&mut sim, &both),
        vec![
            (Audience::Session(both[0]), ev(events::OBJECTIVE, 75)),
            (Audience::Session(both[1]), ev(events::OBJECTIVE, 75)),
        ]
    );
    assert!(rules.after_tick(&mut sim, &both).is_empty(), "greeted once");
    // Standing next to the zone is not the top.
    let zone = layout.win_zone.centre_m;
    let player = session_player_entity(both[1]);
    sim.teleport_player(player, [zone[0] + 1.5, zone[1], zone[2]]);
    assert!(rules.after_tick(&mut sim, &both).is_empty());
    sim.teleport_player(player, zone);
    assert_eq!(
        rules.after_tick(&mut sim, &both),
        vec![(Audience::All, ev(events::WON, 0))],
        "one player in the zone wins for everyone"
    );
    assert!(rules.after_tick(&mut sim, &both).is_empty(), "won once");
    // A player joining after the win learns of it with the objective.
    let late = session(0);
    let late = SessionId::from_parts(late.slot(), 2);
    assert_eq!(
        rules.after_tick(&mut sim, &[late, both[1]]),
        vec![
            (Audience::Session(late), ev(events::OBJECTIVE, 75)),
            (Audience::Session(late), ev(events::WON, 0)),
        ]
    );
}

fn count(sim: &Simulation, kind: u16) -> usize {
    sim.world_items().filter(|i| i.kind == kind).count()
}

#[test]
fn a_death_respawns_at_full_health_and_restores_missing_cabin_items_once() {
    let (mut sim, mut rules, layout) = arena();
    let both = [session(0), session(1)];
    rules.after_tick(&mut sim, &both);
    let player = session_player_entity(both[1]);
    // Walk off, get hurt, and take the shovel and the note from the cabin.
    sim.teleport_player(
        player,
        [
            WALK_ARENA_SPAWNS[1][0] + 3.0,
            WALK_ARENA_SPAWNS[1][1],
            WALK_ARENA_SPAWNS[1][2],
        ],
    );
    sim.damage_player(player, 40);
    for kind in [items::SHOVEL.0, items::NOTE.0] {
        let id = sim
            .world_items()
            .find(|i| i.kind == kind)
            .map(|i| i.item_id)
            .unwrap();
        sim.remove_world_item(id).unwrap();
    }
    assert!(rules.after_tick(&mut sim, &both).is_empty(), "no death yet");
    sim.damage_player(player, PLAYER_MAX_HEALTH);
    assert_eq!(
        rules.after_tick(&mut sim, &both),
        vec![(Audience::Session(both[1]), ev(events::DIED, 0))],
        "only the player who died is told"
    );
    // Back in the cabin, at full health.
    let state = sim.player_state(player).unwrap();
    let spawn = WALK_ARENA_SPAWNS[1];
    assert!(
        (state.position_m[0] - spawn[0]).abs() < 0.5
            && (state.position_m[2] - spawn[2]).abs() < 0.5,
        "{state:?}"
    );
    assert_eq!(sim.player_health(player), Some(PLAYER_MAX_HEALTH));
    // Every cabin item is back exactly once: the missing two restored at
    // their spots, the compass that was never taken not duplicated.
    for item in &layout.cabin_items {
        assert_eq!(count(&sim, item.kind), 1, "kind {}", item.kind);
        assert!(sim.world_items().any(|i| i.kind == item.kind
            && (0..3).all(|a| (i.translation_m[a] - item.translation_m[a]).abs() < 1e-6)));
    }
    // A second death with everything in place adds nothing.
    sim.damage_player(player, PLAYER_MAX_HEALTH);
    rules.after_tick(&mut sim, &both);
    for item in &layout.cabin_items {
        assert_eq!(count(&sim, item.kind), 1, "kind {}", item.kind);
    }
    assert_eq!(
        count(&sim, items::AXE.0),
        0,
        "the shed's axe is not a cabin item"
    );
}

#[cfg(feature = "client")]
mod network {
    use super::*;
    use sandbox::{editor_scene, game};
    use spall_client::{
        BaselineScene, ClientNetConfig, InteractiveSession, run_replication_client_with_manifest,
    };
    use spall_net::{Fingerprint, JoinToken, TransportConfig};
    use spall_server::{CustomWorld, GameTick, Scene, ServeConfig};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let start = Instant::now();
        loop {
            if let Some(value) = f() {
                return value;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "timed out: {what}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Two players in the cabin; the second spawns inside the win zone. The
    /// first, already playing, learns of the win from the broadcast.
    #[test]
    fn the_win_reaches_every_player() {
        let dir = std::env::temp_dir().join(format!("spall-wc-win-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let scene = Arc::new(
            editor_scene::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cabin"))
                .unwrap(),
        );
        let spawns = scene.player_spawns.clone();
        let layout = Layout {
            cabin: [0; 3],
            tower: [0; 3],
            shed: [0; 3],
            spawns: spawns.clone(),
            cabin_items: scene.world_items.clone(),
            axe: scene.world_items[0],
            win_zone: WinZone {
                centre_m: spawns[1],
                radius_m: 0.5,
            },
            tower_heading_deg: 0.0,
        };
        let token = JoinToken::generate().unwrap();
        let mut cfg = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
        let setup = scene.clone();
        cfg.server.custom_world = Some(CustomWorld::new(spawns, move || setup.world_setup()));
        cfg.server.game_tick = Some(GameTick::new(ScenarioRules::new(&layout)));
        cfg.server.paced = true;
        cfg.server.max_clients = 2;
        cfg.server.log_json = dir.join("server.jsonl");
        cfg.server.transport = TransportConfig::for_tests();
        cfg.harness.max_ticks = 1_200;
        cfg.harness.min_clients = 1;
        cfg.harness.quiescence_ticks = 0;
        cfg.harness.startup_timeout = Duration::from_secs(20);
        cfg.harness.fingerprint_out = Some(dir.join("fingerprint"));
        cfg.harness.addr_out = Some(dir.join("addr"));
        let server = std::thread::spawn(move || {
            spall_server::serve_with_game_content(cfg, game::tool_catalog(), game::manifest())
                .expect("scenario server")
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
            let cfg = ClientNetConfig {
                connect_addr: addr,
                server_fingerprint: fingerprint,
                join_token: token,
                script: Vec::new(),
                movement_script: Vec::new(),
                late_join: true,
                baseline_scene: BaselineScene::BridgeCut,
                run_ticks: 1_100,
                idle_grace: Duration::from_secs(30),
                overall_timeout: Duration::from_secs(40),
                log_json: dir.join(format!("{name}.jsonl")),
                summary_json: None,
                transport: TransportConfig::for_tests(),
                client_residency: None,
                baseline_budget_bytes:
                    spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
                on_replica_ready: None,
                interactive: Some(session.clone()),
                client_authoritative: false,
                admin_script: Vec::new(),
            };
            let client = std::thread::spawn(move || {
                run_replication_client_with_manifest(cfg, game::manifest()).expect("client")
            });
            (session, client)
        };
        let seen = |session: &InteractiveSession, log: &Mutex<Vec<GameEvent>>, kind: u16| {
            let mut log = log.lock().unwrap();
            log.extend(session.take_game_events());
            log.iter().any(|e| e.kind == kind).then_some(())
        };
        let (first, client1) = connect("first");
        let log1 = Mutex::new(Vec::new());
        until("first objective", || seen(&first, &log1, events::OBJECTIVE));
        assert!(
            seen(&first, &log1, events::WON).is_none(),
            "nobody is at the top yet"
        );
        let (second, client2) = connect("second");
        let log2 = Mutex::new(Vec::new());
        until("second player's win", || seen(&second, &log2, events::WON));
        until("the win broadcast to the first player", || {
            seen(&first, &log1, events::WON)
        });
        let wins = log1
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind == events::WON)
            .count();
        assert_eq!(wins, 1, "the win is announced once");
        first.request_stop();
        second.request_stop();
        client1.join().unwrap();
        client2.join().unwrap();
        server.join().unwrap();
    }
}

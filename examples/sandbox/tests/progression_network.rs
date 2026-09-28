#![cfg(feature = "client")]

use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sandbox::game::{self, Inventory};
use spall_client::{BaselineScene, ClientNetConfig, run_replication_client_with_progression};
use spall_net::{AuthReject, JoinToken, PlayerCredential, TransportConfig, TransportError};
use spall_protocol::{
    InventoryEntry, PlayerId, ProgressionOperation, ProgressionOutcome, ProgressionRejectCode,
    ProgressionRequest, ProgressionResponse,
};
use spall_server::{Scene, ServeConfig};

fn scratch_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("spall-progression-network-{nonce}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn response(inventory: &mut Inventory, request: ProgressionRequest) -> ProgressionResponse {
    let inspect = matches!(&request.operation, ProgressionOperation::InspectInventory);
    let mut outcome = match request.operation {
        ProgressionOperation::InspectInventory => ProgressionOutcome::Inventory,
        ProgressionOperation::Craft {
            recipe_id,
            batch_count,
        } => {
            if request.catalog_version != game::RECIPE_CATALOG_VERSION {
                ProgressionOutcome::Rejected(ProgressionRejectCode::CatalogVersion)
            } else if request.expected_inventory_revision != inventory.revision() {
                ProgressionOutcome::Rejected(ProgressionRejectCode::InventoryRevision)
            } else {
                match game::craft(
                    inventory,
                    &game::recipe_catalog(),
                    game::CraftRequest {
                        recipe: game::RecipeId(recipe_id),
                        batch_count,
                        expected_catalog_version: request.catalog_version,
                        expected_inventory_revision: request.expected_inventory_revision,
                    },
                ) {
                    Ok(_) => ProgressionOutcome::Crafted,
                    Err(game::CraftError::InsufficientItems(_)) => {
                        ProgressionOutcome::Rejected(ProgressionRejectCode::InsufficientItems)
                    }
                    Err(game::CraftError::CatalogVersionMismatch { .. }) => {
                        ProgressionOutcome::Rejected(ProgressionRejectCode::CatalogVersion)
                    }
                    Err(game::CraftError::InventoryRevisionMismatch { .. })
                    | Err(game::CraftError::StaleInventory { .. }) => {
                        ProgressionOutcome::Rejected(ProgressionRejectCode::InventoryRevision)
                    }
                    Err(_) => ProgressionOutcome::Rejected(ProgressionRejectCode::Unavailable),
                }
            }
        }
    };
    if inspect && request.catalog_version != game::RECIPE_CATALOG_VERSION {
        outcome = ProgressionOutcome::Rejected(ProgressionRejectCode::CatalogVersion);
    }
    ProgressionResponse {
        request_id: request.request_id,
        catalog_version: game::RECIPE_CATALOG_VERSION,
        inventory_revision: inventory.revision(),
        outcome,
        inventory: inventory
            .stacks()
            .map(|stack| InventoryEntry {
                item_id: stack.item.0,
                count: stack.count,
            })
            .collect(),
    }
}

fn inspect_request(request_id: u64) -> ProgressionRequest {
    ProgressionRequest {
        request_id,
        catalog_version: game::RECIPE_CATALOG_VERSION,
        expected_inventory_revision: 0,
        operation: ProgressionOperation::InspectInventory,
    }
}

fn craft_request(request_id: u64, expected_revision: u64) -> ProgressionRequest {
    ProgressionRequest {
        request_id,
        catalog_version: game::RECIPE_CATALOG_VERSION,
        expected_inventory_revision: expected_revision,
        operation: ProgressionOperation::Craft {
            recipe_id: game::recipe_ids::SAW_PLANKS.0,
            batch_count: 1,
        },
    }
}

fn serve_config(dir: &std::path::Path, ticks: u64) -> ServeConfig {
    let mut config = ServeConfig::headless(
        "127.0.0.1:0".parse().unwrap(),
        Scene::BridgeCut,
        JoinToken::generate().unwrap(),
    );
    config.max_ticks = ticks;
    config.quiescence_ticks = 0;
    config.min_clients = 0;
    config.max_clients = 8;
    config.startup_timeout = Duration::from_secs(10);
    config.paced = true;
    config.log_json = dir.join("server.jsonl");
    config.fingerprint_out = Some(dir.join("fingerprint.txt"));
    config.addr_out = Some(dir.join("address.txt"));
    config.save = Some(dir.join("world.db"));
    config.checkpoint_interval_ticks = 60;
    config.transport = TransportConfig::for_tests();
    config.credential_registry_file = Some(dir.join("player-credentials.txt"));
    config
}

fn write_credentials(dir: &std::path::Path, credentials: &[PlayerCredential]) {
    let target = dir.join("player-credentials.txt");
    let temp = dir.join("player-credentials.txt.tmp");
    let body = credentials
        .iter()
        .map(|credential| {
            format!(
                "{} {}\n",
                credential.player_id.to_hex(),
                credential.token.to_hex()
            )
        })
        .collect::<String>();
    std::fs::write(&temp, body).unwrap();
    std::fs::rename(temp, target).unwrap();
}

fn start_server(
    dir: PathBuf,
    ticks: u64,
    credentials: Vec<PlayerCredential>,
    store: Arc<sandbox::progression_store::ProgressionStore>,
    slow_once: Option<(
        Arc<AtomicBool>,
        std::sync::mpsc::SyncSender<()>,
        std::sync::mpsc::Sender<()>,
    )>,
) -> std::thread::JoinHandle<Result<spall_server::ServeSummary, spall_server::ServeError>> {
    std::thread::spawn(move || {
        let handler_store = store;
        let handler = Box::new(
            move |_: spall_protocol::SessionId,
                  principal: Option<PlayerId>,
                  request: ProgressionRequest| {
                if request.request_id == 51
                    && let Some((delayed, started, finished)) = slow_once.as_ref()
                    && !delayed.swap(true, Ordering::SeqCst)
                {
                    let _ = started.try_send(());
                    std::thread::sleep(Duration::from_millis(300));
                    let result = principal.and_then(|id| {
                        handler_store
                            .execute_request(id, request.clone(), response)
                            .ok()
                    });
                    if let Some(result) = result {
                        let _ = finished.send(());
                        return result;
                    }
                    return response(&mut Inventory::default(), request);
                }
                let Some(player_id) = principal else {
                    return response(&mut Inventory::default(), request);
                };
                handler_store
                    .execute_request(player_id, request.clone(), response)
                    .unwrap_or_else(|_| response(&mut Inventory::default(), request))
            },
        );
        let commit_handler: spall_server::CommittedEditHandler = Box::new(|_, _, _, _| {});
        spall_server::serve_with_game_content_and_player_credentials(
            serve_config(&dir, ticks),
            game::tool_catalog(),
            game::manifest(),
            game::contact_damage_profiles(),
            None,
            None,
            commit_handler,
            handler,
            credentials,
        )
    })
}

fn client_config(
    dir: &std::path::Path,
    label: &str,
    addr: std::net::SocketAddr,
    fingerprint: spall_net::Fingerprint,
    token: JoinToken,
    run_ticks: u64,
    timeout: Duration,
) -> ClientNetConfig {
    ClientNetConfig {
        connect_addr: addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: false,
        baseline_scene: BaselineScene::BridgeCut,
        run_ticks,
        idle_grace: Duration::from_millis(200),
        overall_timeout: timeout,
        log_json: dir.join(format!("{label}.jsonl")),
        summary_json: None,
        transport: TransportConfig::for_tests(),
        client_residency: None,
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
    }
}

fn run_client(
    config: ClientNetConfig,
    requests: Vec<ProgressionRequest>,
) -> spall_client::ClientSummary {
    run_replication_client_with_progression(config, game::manifest(), None, requests).unwrap()
}

fn try_run_client(
    config: ClientNetConfig,
    requests: Vec<ProgressionRequest>,
) -> Result<spall_client::ClientSummary, spall_client::ClientNetError> {
    run_replication_client_with_progression(config, game::manifest(), None, requests)
}

fn response_for(
    summary: &spall_client::ClientSummary,
    request_id: u64,
) -> Vec<ProgressionResponse> {
    summary
        .progression_responses
        .iter()
        .filter(|response| response.request_id == request_id)
        .cloned()
        .collect()
}

#[test]
fn authenticated_crafting_retries_disconnects_isolates_players_and_survives_restart() {
    let dir = scratch_dir();
    let progression_path = dir.join("player-progression.db");
    let first = PlayerCredential::generate().unwrap();
    let second = PlayerCredential::generate().unwrap();
    write_credentials(&dir, &[first, second]);
    let initial_store =
        Arc::new(sandbox::progression_store::ProgressionStore::open(&progression_path).unwrap());
    let removed = [(game::materials::WOOD, 32_u64)].into();
    initial_store
        .record_committed_cut(first.player_id, 1, &removed)
        .unwrap();

    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let delay = Arc::new(AtomicBool::new(false));
    let server = start_server(
        dir.clone(),
        600,
        vec![first, second],
        initial_store.clone(),
        Some((delay, started_tx, finished_tx)),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while (!dir.join("address.txt").exists() || !dir.join("fingerprint.txt").exists())
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(dir.join("address.txt").exists());
    // Let the runtime registry watcher observe the initially provisioned file
    // before any test client authenticates.
    std::thread::sleep(Duration::from_millis(650));
    let addr = std::fs::read_to_string(dir.join("address.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let fingerprint = spall_net::Fingerprint::from_hex(
        std::fs::read_to_string(dir.join("fingerprint.txt"))
            .unwrap()
            .trim(),
    )
    .unwrap();

    let p1_config = client_config(
        &dir,
        "p1-craft",
        addr,
        fingerprint,
        first.token,
        30,
        Duration::from_secs(4),
    );
    let p2_config = client_config(
        &dir,
        "p2-isolation",
        addr,
        fingerprint,
        second.token,
        30,
        Duration::from_secs(4),
    );
    let p1 = std::thread::spawn(move || {
        run_client(
            p1_config,
            vec![
                craft_request(41, 1),
                craft_request(41, 1),
                craft_request(42, 1),
            ],
        )
    });
    let p2 = std::thread::spawn(move || run_client(p2_config, vec![craft_request(41, 0)]));
    let p1 = p1.join().unwrap();
    let p2 = p2.join().unwrap();
    let duplicate = response_for(&p1, 41);
    assert_eq!(duplicate.len(), 2);
    assert_eq!(duplicate[0], duplicate[1]);
    assert_eq!(duplicate[0].outcome, ProgressionOutcome::Crafted);
    assert_eq!(duplicate[0].inventory_revision, 2);
    assert_eq!(
        duplicate[0].inventory,
        vec![
            InventoryEntry {
                item_id: game::items::WOOD_LOG.0,
                count: 1
            },
            InventoryEntry {
                item_id: game::items::WOOD_PLANK.0,
                count: 4
            }
        ]
    );
    let stale = response_for(&p1, 42);
    assert_eq!(stale.len(), 1);
    assert_eq!(
        stale[0].outcome,
        ProgressionOutcome::Rejected(ProgressionRejectCode::InventoryRevision)
    );
    assert_eq!(stale[0].inventory_revision, 2);
    let isolated = response_for(&p2, 41);
    assert_eq!(isolated.len(), 1);
    assert_eq!(
        isolated[0].outcome,
        ProgressionOutcome::Rejected(ProgressionRejectCode::InsufficientItems)
    );
    assert_eq!(isolated[0].inventory_revision, 0);

    let disconnect_config = client_config(
        &dir,
        "p1-disconnect",
        addr,
        fingerprint,
        first.token,
        0,
        Duration::from_millis(75),
    );
    let disconnected =
        std::thread::spawn(move || run_client(disconnect_config, vec![craft_request(51, 2)]));
    started_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("server began delayed craft");
    let disconnected = disconnected.join().unwrap();
    assert!(
        response_for(&disconnected, 51).is_empty(),
        "client disconnected before durable reply"
    );
    finished_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("delayed craft committed after disconnect");
    let retry_config = client_config(
        &dir,
        "p1-retry",
        addr,
        fingerprint,
        first.token,
        0,
        Duration::from_secs(1),
    );
    let retry = run_client(retry_config, vec![craft_request(51, 2)]);
    let replayed = response_for(&retry, 51);
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].outcome, ProgressionOutcome::Crafted);
    assert_eq!(replayed[0].inventory_revision, 3);

    let rotated = PlayerCredential {
        player_id: first.player_id,
        token: JoinToken::generate().unwrap(),
    };
    write_credentials(&dir, &[rotated, second]);
    std::thread::sleep(Duration::from_millis(700));
    let old_token_config = client_config(
        &dir,
        "p1-old-token",
        addr,
        fingerprint,
        first.token,
        0,
        Duration::from_secs(2),
    );
    let rejected = try_run_client(old_token_config, Vec::new()).unwrap_err();
    assert!(matches!(
        rejected,
        spall_client::ClientNetError::Transport(TransportError::Auth(AuthReject::BadToken))
    ));
    assert!(!format!("{rejected:?}").contains(&first.token.to_hex()));
    let rotated_config = client_config(
        &dir,
        "p1-rotated-token",
        addr,
        fingerprint,
        rotated.token,
        30,
        Duration::from_secs(3),
    );
    let rotated_summary = run_client(rotated_config, vec![inspect_request(1000)]);
    let rotated_inventory = response_for(&rotated_summary, 1000);
    assert_eq!(rotated_inventory.len(), 1);
    assert_eq!(rotated_inventory[0].inventory_revision, 3);
    assert_eq!(rotated_inventory[0].inventory[0].count, 8);

    let server_summary = server.join().unwrap().unwrap();
    assert_eq!(server_summary.result, "passed");
    drop(initial_store);

    // Reopen the game-owned database after the server process exits. Both
    // identities retain their independent authoritative inventory snapshots.
    let recovered =
        Arc::new(sandbox::progression_store::ProgressionStore::open(&progression_path).unwrap());
    let first_inventory = recovered.load_inventory(first.player_id).unwrap();
    let second_inventory = recovered.load_inventory(second.player_id).unwrap();
    assert_eq!(first_inventory.revision(), 3);
    assert_eq!(first_inventory.count(game::items::WOOD_LOG), 0);
    assert_eq!(first_inventory.count(game::items::WOOD_PLANK), 8);
    assert_eq!(second_inventory.revision(), 0);
    assert_eq!(second_inventory.count(game::items::WOOD_PLANK), 0);

    let _ = std::fs::remove_file(dir.join("address.txt"));
    let _ = std::fs::remove_file(dir.join("fingerprint.txt"));
    write_credentials(&dir, &[rotated, second]);
    let server = start_server(dir.clone(), 600, vec![rotated, second], recovered, None);
    while !dir.join("address.txt").exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(650));
    let addr = std::fs::read_to_string(dir.join("address.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let fingerprint = spall_net::Fingerprint::from_hex(
        std::fs::read_to_string(dir.join("fingerprint.txt"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    let p1_config = client_config(
        &dir,
        "p1-restart-inspect",
        addr,
        fingerprint,
        rotated.token,
        30,
        Duration::from_secs(4),
    );
    let p2_config = client_config(
        &dir,
        "p2-restart-inspect",
        addr,
        fingerprint,
        second.token,
        30,
        Duration::from_secs(4),
    );
    let p1 = std::thread::spawn(move || run_client(p1_config, vec![inspect_request(1001)]));
    let p2 = std::thread::spawn(move || run_client(p2_config, vec![inspect_request(1002)]));
    let p1 = p1.join().unwrap();
    let p2 = p2.join().unwrap();
    let p1_after_restart = response_for(&p1, 1001);
    let p2_after_restart = response_for(&p2, 1002);
    assert_eq!(p1_after_restart.len(), 1);
    assert_eq!(p1_after_restart[0].inventory_revision, 3);
    assert_eq!(p1_after_restart[0].inventory[0].count, 8);
    assert_eq!(p2_after_restart.len(), 1);
    assert_eq!(p2_after_restart[0].inventory_revision, 0);
    assert!(p2_after_restart[0].inventory.is_empty());

    write_credentials(&dir, &[rotated]);
    std::thread::sleep(Duration::from_millis(700));
    let revoked_config = client_config(
        &dir,
        "p2-revoked",
        addr,
        fingerprint,
        second.token,
        0,
        Duration::from_secs(2),
    );
    let revoked = try_run_client(revoked_config, Vec::new()).unwrap_err();
    assert!(matches!(
        revoked,
        spall_client::ClientNetError::Transport(TransportError::Auth(AuthReject::BadToken))
    ));
    assert!(!format!("{revoked:?}").contains(&second.token.to_hex()));
    assert_eq!(server.join().unwrap().unwrap().result, "passed");

    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            let log = std::fs::read_to_string(path).unwrap();
            for secret in [
                first.token.to_hex(),
                second.token.to_hex(),
                rotated.token.to_hex(),
            ] {
                assert!(!log.contains(&secret), "credential appeared in a JSONL log");
            }
        }
    }

    let _ = std::fs::remove_dir_all(dir);
}

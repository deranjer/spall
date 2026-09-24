use clap::Parser;
use spall_net::{JoinToken, PlayerCredential, TransportConfig};
use spall_server::{Scene, ServeConfig, ServerConfig, TimingWindow};
use spall_sim::world::TerrainColliderMode;
use std::{net::SocketAddr, path::PathBuf, process::ExitCode, time::Duration};

#[derive(Debug, Parser)]
#[command(name = "sandbox-server", about = "GPU-free Spall sandbox server host")]
struct Args {
    /// Directory for the T16 on-disk world; the SQLite database is
    /// `<world>/world.db`. Used by `--serve --save`; ignored by the T00 loop.
    #[arg(long, default_value = ".local/worlds/dev")]
    world: PathBuf,
    #[arg(long, default_value_t = 0)]
    seed: u64,
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=10_000_000))]
    ticks: u64,
    #[arg(long)]
    log_json: PathBuf,
    #[arg(long, default_value_t = 0)]
    ready_delay_ms: u64,
    /// Harness-only failure injection for the T00 bounded loop.
    #[arg(long, hide = true)]
    fail_after_tick: Option<u64>,

    // --- T10 networked replication host ---
    /// Run the authoritative replication host instead of the T00 bounded loop.
    /// Requires --join-token-file or --player-credentials-file.
    #[arg(long)]
    serve: bool,
    /// Per-run join secret (hex), shared with clients out of band.
    #[arg(long)]
    join_token_file: Option<PathBuf>,
    /// Optional per-player credential file. Each noncomment line is
    /// `<32-hex-player-id> <64-hex-token>`; when supplied, shared-token auth is disabled.
    #[arg(long)]
    player_credentials_file: Option<PathBuf>,
    /// Game-owned durable progression database. Defaults to
    /// `<world>/player-progression.db` and requires player credentials.
    #[arg(long)]
    progression_db: Option<PathBuf>,
    /// Write the server certificate fingerprint (hex) here for clients.
    #[arg(long)]
    fingerprint_out: Option<PathBuf>,
    /// Write the OS-resolved bound ip:port here once listening.
    #[arg(long)]
    addr_out: Option<PathBuf>,
    /// Write the machine-readable run summary here.
    #[arg(long)]
    summary_json: Option<PathBuf>,
    /// Wait for this many clients before the tick loop starts.
    #[arg(long, default_value_t = 1)]
    min_clients: usize,
    #[arg(long, default_value_t = 8)]
    max_clients: usize,
    /// Real-time 60 Hz pacing (needed for interactive / networked clients).
    #[arg(long)]
    paced: bool,
    /// Exclude this many owning server ticks from timing percentiles.
    #[arg(long)]
    timing_warmup_ticks: Option<u64>,
    /// Number of owning server ticks to measure after warmup.
    #[arg(long)]
    timing_measured_ticks: Option<u64>,
    /// Maximum retained samples for each timing percentile series.
    #[arg(long)]
    timing_max_samples: Option<usize>,
    /// Built-in scene to serve: `bridge-cut` (default, single brick),
    /// `cross-bridge-cut` (column + beam cross the x = 32 brick boundary), or
    /// `walk` (T19 player-movement arena — every client gets a predicted capsule).
    #[arg(long, default_value = "bridge-cut")]
    scene: String,
    /// Use the legacy whole-terrain collider for a controlled comparison.
    /// Per-brick terrain collision is the normal mode.
    #[arg(long)]
    whole_terrain_collider: bool,
    /// T16: persist to `<world>/world.db` — recover from it on start, journal
    /// committed transactions, checkpoint on the interval and on shutdown.
    #[arg(long)]
    save: bool,
    /// Spawn one game-owned wood crate into a new world before its first
    /// checkpoint. Existing saved worlds are restored unchanged.
    #[arg(long)]
    spawn_wood_crate: bool,
    /// Versioned sandbox content manifest; validates assets and joins its hash into the client handshake.
    #[arg(long)]
    content_manifest: Option<PathBuf>,
    /// Stable content asset ID to import and spawn in a new world.
    #[arg(long)]
    spawn_content_asset: Option<u64>,
    /// Run the fresh-world wood harvest -> plank craft -> asset placement progression scenario.
    #[arg(long)]
    progression_demo: bool,
    /// Ticks between engine checkpoints (1800 == 30 s at 60 Hz). 0 disables the
    /// periodic checkpoint (a shutdown checkpoint still happens).
    #[arg(long, default_value_t = 1_800)]
    checkpoint_interval_ticks: u64,
    /// Consecutive idle ticks (no committed work, no client input) after which
    /// the run stops early. Larger values give a scripted client more slack to
    /// land late actions under an impaired transport. 0 disables early stop.
    #[arg(long, default_value_t = 45)]
    quiescence_ticks: u64,
    /// T17: a joining client's catch-up-queue cap before its baseline transfer
    /// is cancelled and re-captured fresher.
    #[arg(long, default_value_t = spall_server::serve::DEFAULT_CATCH_UP_CAP)]
    catch_up_cap: usize,
    /// T17: bounded late-join transfer restarts before a joining client is
    /// dropped (connected clients keep running).
    #[arg(long, default_value_t = spall_server::serve::DEFAULT_MAX_JOIN_RETRIES)]
    max_join_retries: u32,
    /// ENG-30 / T23 row 11: worker threads in the background baseline-capture
    /// pool (bounds concurrent late-join captures). Defaults to a measured,
    /// environment-derived value (`docs/reports/G3.md` increment 30) --
    /// `available_parallelism() / 4`, minimum 1 -- rather than a fixed
    /// literal, since the safe number depends on the host's real core count.
    #[arg(long)]
    capture_workers: Option<usize>,
    /// ENG-47 development-scenario path: skip server-side action-claim
    /// validation and take each `ActionRequest`'s claimed target/brush verbatim.
    /// Lets a fixture harness script arbitrary cuts. Never use on a shared host.
    #[arg(long, hide = true)]
    dev_unvalidated_actions: bool,
    /// ENG-61: keep stepping physics past edit-quiescence until every detached
    /// body is asleep (bounded by `--ticks`). Used by the `body-rest-on-structure`
    /// gate fixture so the run can show the detached beam actually come to rest.
    #[arg(long)]
    await_body_settle: bool,

    // --- T23 / G3 row 7: default-off resident-cache eviction ---
    /// Enable the residency pass: evict terrain bricks outside every player's
    /// interest box (this many resident bricks is the reported ceiling), reload
    /// them on demand for edits. The committed world / hash is unchanged.
    /// `0` (default) keeps everything resident.
    #[arg(long, default_value_t = 0)]
    residency_budget_bricks: usize,
    /// Chebyshev radius (bricks) of the kept-resident box around each player.
    #[arg(long, default_value_t = 2)]
    residency_radius_bricks: i64,
    /// T23 / G3 row 7 increment 13: hard ceiling on resident terrain dense
    /// bytes, enforced the same way as `--residency-budget-bricks` (an
    /// interest-driven, non-pinned reload is deferred rather than admitted
    /// past it; over-budget out-of-interest/unpinned bricks are evicted under
    /// pressure). `0` (default) disables this cap while `--residency-budget-bricks`
    /// still applies.
    #[arg(long, default_value_t = 0)]
    residency_budget_dense_bytes: u64,
    /// T23 / G3 row 7 item 2: back the residency pass with a real on-disk
    /// SQLite store at `<world>/residency.db` instead of the in-process
    /// `MemoryBacking` default. Only meaningful with
    /// `--residency-budget-bricks > 0`; ignored otherwise.
    #[arg(long)]
    residency_disk_backing: bool,

    // --- T21 / ENG-28 increment 4 (3c): default-off contact damage + dormancy ---
    /// Enable the sandbox's versioned impact-damage policy: hard impacts carve
    /// a cut into terrain or the struck body. Off by default — this changes the
    /// committed hash.
    #[arg(long)]
    contact_damage: bool,
    /// Enable the region-dormancy pass: a settled body with nothing active
    /// nearby is deactivated (dropped from the physics step), and a dormant
    /// body a player or edit approaches is reactivated
    /// (`spall_sim::DormancyConfig::DEFAULT` tuning). Off by default — a
    /// deactivated body leaves the live physics world, which a gate scenario
    /// reading physics state directly (e.g. `--await-body-settle`) must not
    /// combine with this.
    #[arg(long)]
    dormancy: bool,
    /// Override the dormancy settle window for a deliberate networked policy
    /// evaluation. Requires `--dormancy`; omitted preserves the 120-tick
    /// default. This does not force a sleeping body or alter moving-body
    /// admission.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=1_000_000))]
    dormancy_settle_ticks: Option<u64>,

    // --- T20 interest + bandwidth scheduling ---
    /// Enable per-client interest relevance + motion bandwidth budget. Without
    /// it, one 20 Hz motion batch is broadcast unfiltered to every client
    /// (the pre-T20 behaviour). The three `--motion-*` values below apply only
    /// when this is set.
    #[arg(long)]
    motion_interest: bool,
    /// Interest near radius (m): bodies within this of a client's anchor are
    /// replicated every 20 Hz batch.
    #[arg(long, default_value_t = 48.0)]
    motion_near_m: f64,
    /// Interest far radius (m): out to here bodies replicate on the
    /// `--motion-far-interval` cadence; beyond it a non-player body is not
    /// replicated to that client.
    #[arg(long, default_value_t = 96.0)]
    motion_far_m: f64,
    /// Send `Far`-tier motion only every Nth 20 Hz batch (1 = every batch).
    #[arg(long, default_value_t = 4)]
    motion_far_interval: u64,
    /// Per-client per-batch motion byte ceiling (0 = no ceiling).
    #[arg(long, default_value_t = 0)]
    motion_client_budget_bytes: usize,
    /// Interest anchor `x,y,z` (m) for a client on a scene with no player
    /// capsule (the bridge scenes). Omitted → those clients stay unfiltered.
    #[arg(long)]
    motion_static_anchor: Option<String>,

    // --- T11 exact-replay check ---
    /// Instead of serving, recover from this world database's **oldest**
    /// checkpoint and replay its entire committed topology journal, then print
    /// the rebuilt canonical topology hash. Exit 0 iff it matches
    /// `--expect-hash` (when given).
    #[arg(long)]
    replay: Option<PathBuf>,
    /// Expected canonical topology hash (hex) for `--replay`.
    #[arg(long)]
    expect_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ProgressionOwner {
    Player(spall_protocol::PlayerId),
    LegacySlot(u32),
}

fn main() -> ExitCode {
    sandbox::init_tracing();
    let args = Args::parse();

    if args.replay.is_some() {
        return run_replay(args);
    }

    if args.serve {
        return run_serve(args);
    }

    let config = ServerConfig {
        world: args.world,
        seed: args.seed,
        listen: args.listen,
        ticks: args.ticks,
        log_json: args.log_json,
        ready_delay: Duration::from_millis(args.ready_delay_ms),
        fail_after_tick: args.fail_after_tick,
    };
    match spall_server::run(&config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sandbox-server: {error}");
            ExitCode::from(1)
        }
    }
}

fn read_player_credentials(path: &std::path::Path) -> Result<Vec<PlayerCredential>, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read player credentials {path:?}: {error}"))?;
    let mut credentials = Vec::new();
    for (line_index, line) in source.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let player_text = fields.next();
        let token_text = fields.next();
        if fields.next().is_some() {
            return Err(format!(
                "player credentials {}:{} must contain exactly a player ID and token",
                path.display(),
                line_index + 1
            ));
        }
        let (Some(player_text), Some(token_text)) = (player_text, token_text) else {
            return Err(format!(
                "player credentials {}:{} must contain a 32-hex player ID and 64-hex token",
                path.display(),
                line_index + 1
            ));
        };
        let player_id = spall_protocol::PlayerId::from_hex(player_text).ok_or_else(|| {
            format!(
                "player credentials {}:{} has an invalid player ID",
                path.display(),
                line_index + 1
            )
        })?;
        let token = JoinToken::from_hex(token_text).ok_or_else(|| {
            format!(
                "player credentials {}:{} has an invalid token",
                path.display(),
                line_index + 1
            )
        })?;
        credentials.push(PlayerCredential { player_id, token });
        if credentials.len() > 4096 {
            return Err("player credential file exceeds the 4096-entry limit".into());
        }
    }
    if credentials.is_empty() {
        return Err("player credential file contains no entries".into());
    }
    Ok(credentials)
}

fn run_serve(args: Args) -> ExitCode {
    let credentials = match args.player_credentials_file.as_ref() {
        Some(path) => match read_player_credentials(path) {
            Ok(credentials) => Some(credentials),
            Err(error) => {
                eprintln!("sandbox-server: {error}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    if credentials.is_some() && args.join_token_file.is_some() {
        eprintln!("sandbox-server: use either --player-credentials-file or --join-token-file");
        return ExitCode::from(2);
    }
    let token = if credentials.is_some() {
        match JoinToken::generate() {
            Ok(token) => token,
            Err(error) => {
                eprintln!("sandbox-server: could not initialize auth token: {error}");
                return ExitCode::from(2);
            }
        }
    } else {
        let Some(token_file) = args.join_token_file.as_ref() else {
            eprintln!(
                "sandbox-server: --serve requires --join-token-file or --player-credentials-file"
            );
            return ExitCode::from(2);
        };
        match std::fs::read_to_string(token_file)
            .ok()
            .and_then(|s| JoinToken::from_hex(s.trim()))
        {
            Some(token) => token,
            None => {
                eprintln!("sandbox-server: could not read a 64-hex join token from {token_file:?}");
                return ExitCode::from(2);
            }
        }
    };
    if credentials.is_none() && args.progression_db.is_some() {
        eprintln!("sandbox-server: --progression-db requires --player-credentials-file");
        return ExitCode::from(2);
    }
    let durable_progression = if credentials.is_some() {
        let path = args
            .progression_db
            .as_ref()
            .cloned()
            .unwrap_or_else(|| args.world.join("player-progression.db"));
        match sandbox::progression_store::ProgressionStore::open(&path) {
            Ok(store) => Some(std::sync::Arc::new(std::sync::Mutex::new(store))),
            Err(error) => {
                eprintln!("sandbox-server: could not open progression database {path:?}: {error}");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };
    let asset_store = match args
        .content_manifest
        .as_ref()
        .map(sandbox::content::AssetStore::open)
        .transpose()
    {
        Ok(store) => store,
        Err(error) => {
            eprintln!("sandbox-server: content manifest: {error}");
            return ExitCode::from(2);
        }
    };
    if args.progression_demo && (asset_store.is_none() || args.spawn_content_asset.is_none()) {
        eprintln!(
            "sandbox-server: --progression-demo requires --content-manifest and --spawn-content-asset"
        );
        return ExitCode::from(2);
    }
    if args.spawn_content_asset.is_some() && asset_store.is_none() {
        eprintln!("sandbox-server: --spawn-content-asset requires --content-manifest");
        return ExitCode::from(2);
    }
    let asset_manifest_hash = if let Some(store) = asset_store.as_ref() {
        if let Err(error) = store.verify_all() {
            eprintln!("sandbox-server: content asset verification: {error}");
            return ExitCode::from(2);
        }
        match store.manifest().canonical_hash() {
            Ok(hash) => Some(spall_protocol::Hash32(hash)),
            Err(error) => {
                eprintln!("sandbox-server: content manifest hash: {error}");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };

    let scene = match Scene::from_name(&args.scene) {
        Some(s) => s,
        None => {
            eprintln!(
                "sandbox-server: unknown --scene `{}` (expected bridge-cut, cross-bridge-cut, walk, checkerboard-split, bulk-split, separated-regions, g4-workload, separated-regions-far, g1-full-envelope, sleep-wake, or playground)",
                args.scene
            );
            return ExitCode::from(2);
        }
    };

    let save = args.save.then(|| args.world.join("world.db"));
    if let Some(db) = &save
        && let Some(parent) = db.parent()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let residency_disk_path = (args.residency_budget_bricks > 0 && args.residency_disk_backing)
        .then(|| args.world.join("residency.db"));
    if let Some(db) = &residency_disk_path
        && let Some(parent) = db.parent()
    {
        let _ = std::fs::create_dir_all(parent);
    }

    let motion_interest = if args.motion_interest {
        let static_anchor_m = match args.motion_static_anchor.as_deref() {
            None => None,
            Some(s) => match parse_vec3(s) {
                Some(v) => Some(v),
                None => {
                    eprintln!(
                        "sandbox-server: --motion-static-anchor must be `x,y,z` metres, got `{s}`"
                    );
                    return ExitCode::from(2);
                }
            },
        };
        Some(spall_server::MotionInterest {
            near_radius_m: args.motion_near_m,
            far_radius_m: args.motion_far_m,
            far_interval: args.motion_far_interval,
            per_client_budget_bytes: args.motion_client_budget_bytes,
            static_anchor_m,
        })
    } else {
        None
    };

    let dormancy = match (args.dormancy, args.dormancy_settle_ticks) {
        (false, None) => None,
        (false, Some(_)) => {
            eprintln!("sandbox-server: --dormancy-settle-ticks requires --dormancy");
            return ExitCode::from(2);
        }
        (true, None) => Some(spall_sim::DormancyConfig::DEFAULT),
        (true, Some(settle_ticks)) => Some(spall_sim::DormancyConfig {
            settle_ticks,
            ..spall_sim::DormancyConfig::DEFAULT
        }),
    };

    let timing_window = match (
        args.timing_warmup_ticks,
        args.timing_measured_ticks,
        args.timing_max_samples,
    ) {
        (None, None, None) => None,
        (Some(warmup_ticks), Some(measured_ticks), Some(max_samples))
            if measured_ticks > 0 && max_samples > 0 =>
        {
            Some(TimingWindow {
                warmup_ticks,
                measured_ticks,
                max_samples,
            })
        }
        _ => {
            eprintln!(
                "sandbox-server: --timing-warmup-ticks, --timing-measured-ticks (>0), and --timing-max-samples (>0) must be supplied together"
            );
            return ExitCode::from(2);
        }
    };

    let config = ServeConfig {
        listen: args.listen,
        scene,
        terrain_collider_mode: if args.whole_terrain_collider {
            TerrainColliderMode::WholeTerrain
        } else {
            TerrainColliderMode::PerBrick
        },
        join_token: token,
        max_ticks: args.ticks,
        quiescence_ticks: args.quiescence_ticks,
        min_clients: args.min_clients,
        max_clients: args.max_clients,
        startup_timeout: Duration::from_secs(30),
        paced: args.paced,
        log_json: args.log_json,
        summary_json: args.summary_json,
        fingerprint_out: args.fingerprint_out,
        addr_out: args.addr_out,
        transport: TransportConfig::default(),
        save,
        checkpoint_interval_ticks: args.checkpoint_interval_ticks,
        seed: args.seed,
        catch_up_cap: args.catch_up_cap,
        max_join_retries: args.max_join_retries,
        capture_workers: args
            .capture_workers
            .unwrap_or_else(spall_server::serve::default_capture_workers),
        dev_unvalidated_actions: args.dev_unvalidated_actions,
        save_faults: None,
        await_body_settle: args.await_body_settle,
        motion_interest,
        residency: (args.residency_budget_bricks > 0).then_some(spall_server::ResidencyLimits {
            budget_bricks: args.residency_budget_bricks,
            max_dense_bytes: if args.residency_budget_dense_bytes == 0 {
                u64::MAX
            } else {
                args.residency_budget_dense_bytes
            },
            interest_radius_bricks: args.residency_radius_bricks,
        }),
        residency_disk_path,
        contact_damage: args
            .contact_damage
            .then_some(sandbox::game::contact_damage_config()),
        dormancy,
        timing_window,
    };
    tracing::info!(
        damage_rules_version = sandbox::game::DAMAGE_RULES_VERSION,
        enabled = args.contact_damage,
        "sandbox impact damage policy"
    );
    tracing::info!(
        game_rules_version = sandbox::game::GAME_RULES_VERSION,
        recipe_catalog_version = sandbox::game::RECIPE_CATALOG_VERSION,
        recipe_count = sandbox::game::recipe_catalog().len(),
        "sandbox game content versions"
    );
    let setup: Option<spall_server::InitialGameWorldSetup> = (args.spawn_wood_crate || args.spawn_content_asset.is_some() || args.progression_demo).then(|| {
        let asset_store = asset_store;
        let asset_id = args.spawn_content_asset.map(sandbox::content::AssetId);
        let progression_demo = args.progression_demo;
        Box::new(move |simulation: &mut spall_sim::Simulation| {
            if progression_demo {
                let mut inventory = sandbox::game::Inventory::default();
                sandbox::game::record_gathered_material_drop(&mut inventory, sandbox::game::materials::WOOD, 1)
                    .map_err(|error| format!("progression gather failed: {error:?}"))?;
                let inventory_revision = inventory.revision();
                let receipt = sandbox::game::craft(&mut inventory, &sandbox::game::recipe_catalog(), sandbox::game::CraftRequest {
                    recipe: sandbox::game::recipe_ids::SAW_PLANKS,
                    batch_count: 1,
                    expected_catalog_version: sandbox::game::RECIPE_CATALOG_VERSION,
                    expected_inventory_revision: inventory_revision,
                }).map_err(|error| format!("progression craft failed: {error:?}"))?;
                tracing::info!(inventory_revision = inventory.revision(), planks = inventory.count(sandbox::game::items::WOOD_PLANK), recipe = ?receipt.recipe, "completed authoritative gathering and crafting progression step");
            }
            if let (Some(store), Some(asset_id)) = (asset_store.as_ref(), asset_id) {
                let loaded = store.load_voxel_asset(asset_id, &sandbox::game::asset_material_mapping())
                    .map_err(|error| format!("asset import failed: {error}"))?;
                let entity = sandbox::game::spawn_loaded_voxel_asset(simulation, loaded, [5.0, 12.0, 5.0])
                    .map_err(|error| format!("asset spawn failed: {error}"))?;
                tracing::info!(entity_id = entity.get(), asset_id = asset_id.0, "spawned versioned game content asset in new world");
            }
            if args.spawn_wood_crate {
            let entity = sandbox::game::spawn_demo_wood_crate(simulation, [5.0, 12.0, 5.0])?;
            tracing::info!(
                entity_id = entity.get(),
                spawn_m = ?[5.0, 12.0, 5.0],
                "spawned sandbox wood crate in new world"
            );
            }
            Ok(())
        }) as spall_server::InitialGameWorldSetup
    });
    let player_inventories = std::sync::Arc::new(std::sync::Mutex::new(
        sandbox::game::PlayerInventories::default(),
    ));
    let commit_inventories = player_inventories.clone();
    let commit_store = durable_progression.clone();
    let commit_handler: spall_server::CommittedEditHandler =
        Box::new(move |session, player_id, request_id, removed_materials| {
            if let (Some(player_id), Some(store)) = (player_id, commit_store.as_ref()) {
                let result = store
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .record_committed_cut(player_id, request_id.0, removed_materials);
                match result {
                    Ok(drops) if !drops.is_empty() => tracing::info!(
                        player_id = ?player_id,
                        request_id = request_id.0,
                        drops = ?drops,
                        "durably awarded progression drops for committed cut"
                    ),
                    Ok(_) => {}
                    Err(error) => tracing::error!(
                        player_id = ?player_id,
                        request_id = request_id.0,
                        error = ?error,
                        "could not durably award progression drops after committed cut"
                    ),
                }
                return;
            }
            let mut inventories = commit_inventories.lock().unwrap_or_else(|e| e.into_inner());
            let result = match player_id {
                Some(player_id) => {
                    inventories.record_committed_cut_for_player(player_id, removed_materials)
                }
                None => inventories.record_committed_cut(session.slot().0, removed_materials),
            };
            match result {
                Ok(drops) if !drops.is_empty() => tracing::info!(
                    player_slot = session.slot().0,
                    player_id = ?player_id,
                    request_id = request_id.0,
                    inventory_revision = player_id
                        .and_then(|id| inventories.get_player(id))
                        .or_else(|| inventories.get(session.slot().0))
                        .map_or(0, |inventory| inventory.revision()),
                    drops = ?drops,
                    "awarded progression drops for committed cut"
                ),
                Ok(_) => {}
                Err(error) => tracing::error!(
                    player_slot = session.slot().0,
                    request_id = request_id.0,
                    error = ?error,
                    "could not award progression drops after committed cut"
                ),
            }
        });
    let progression_ledger =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            ProgressionOwner,
            (
                u64,
                std::collections::BTreeMap<u64, spall_protocol::ProgressionResponse>,
            ),
        >::new()));
    let progression_inventories = player_inventories.clone();
    let progression_cache = progression_ledger.clone();
    let progression_store = durable_progression.clone();
    let progression_handler: spall_server::ProgressionHandler = Box::new(
        move |session, player_id, request| {
            if let (Some(player_id), Some(store)) = (player_id, progression_store.as_ref()) {
                let store = store.lock().unwrap_or_else(|e| e.into_inner());
                return match store.execute_request(player_id, request, sandbox_progression_response)
                {
                    Ok(response) => response,
                    Err(error) => {
                        tracing::error!(player_id = ?player_id, request_id = request.request_id, error = ?error, "durable progression request failed");
                        let mut inventory = store.load_inventory(player_id).unwrap_or_default();
                        let mut response = sandbox_progression_response(
                            &mut inventory,
                            spall_protocol::ProgressionRequest {
                                operation: spall_protocol::ProgressionOperation::InspectInventory,
                                ..request
                            },
                        );
                        response.outcome = spall_protocol::ProgressionOutcome::Rejected(
                            spall_protocol::ProgressionRejectCode::Unavailable,
                        );
                        response
                    }
                };
            }
            let owner = player_id.map_or(
                ProgressionOwner::LegacySlot(session.slot().0),
                ProgressionOwner::Player,
            );
            let mut cache = progression_cache.lock().unwrap_or_else(|e| e.into_inner());
            let ledger = cache
                .entry(owner)
                .or_insert_with(|| (0, Default::default()));
            if let Some(response) = ledger.1.get(&request.request_id) {
                return response.clone();
            }
            let mut inventories = progression_inventories
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let inventory = match player_id {
                Some(player_id) => inventories.ensure_player(player_id),
                None => inventories.ensure(session.slot().0),
            };
            if request.request_id <= ledger.0 {
                let mut rejected = sandbox_progression_response(
                    inventory,
                    spall_protocol::ProgressionRequest {
                        operation: spall_protocol::ProgressionOperation::InspectInventory,
                        ..request
                    },
                );
                rejected.outcome = spall_protocol::ProgressionOutcome::Rejected(
                    spall_protocol::ProgressionRejectCode::Unavailable,
                );
                return rejected;
            }
            ledger.0 = request.request_id;
            let response = sandbox_progression_response(inventory, request);
            ledger.1.insert(request.request_id, response.clone());
            while ledger.1.len() > 128 {
                ledger.1.pop_first();
            }
            response
        },
    );
    let tool_catalog = sandbox::game::tool_catalog();
    let materials = sandbox::game::manifest();
    let profiles = sandbox::game::contact_damage_profiles();
    let serve_result = if let Some(credentials) = credentials {
        spall_server::serve_with_game_content_and_player_credentials(
            config,
            tool_catalog,
            materials,
            profiles,
            setup,
            asset_manifest_hash,
            commit_handler,
            progression_handler,
            credentials,
        )
    } else {
        spall_server::serve_with_game_content_and_handlers(
            config,
            tool_catalog,
            materials,
            profiles,
            setup,
            asset_manifest_hash,
            commit_handler,
            progression_handler,
        )
    };
    match serve_result {
        Ok(summary) => {
            println!(
                "sandbox-server: {} ticks={} clients={} committed={} hash={}",
                summary.result,
                summary.ticks_run,
                summary.clients_connected,
                summary.transactions_committed,
                summary.final_world_hash
            );
            if summary.result == "passed" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            eprintln!("sandbox-server: {error}");
            ExitCode::from(1)
        }
    }
}

fn sandbox_progression_response(
    inventory: &mut sandbox::game::Inventory,
    request: spall_protocol::ProgressionRequest,
) -> spall_protocol::ProgressionResponse {
    use spall_protocol::{
        InventoryEntry, ProgressionOperation as Operation, ProgressionOutcome as Outcome,
        ProgressionRejectCode as Reject, ProgressionResponse,
    };
    let catalog = sandbox::game::recipe_catalog();
    let mut outcome = match request.operation {
        Operation::InspectInventory => Outcome::Inventory,
        Operation::Craft {
            recipe_id,
            batch_count,
        } => {
            if request.catalog_version != sandbox::game::RECIPE_CATALOG_VERSION {
                Outcome::Rejected(Reject::CatalogVersion)
            } else if request.expected_inventory_revision != inventory.revision() {
                Outcome::Rejected(Reject::InventoryRevision)
            } else {
                let craft_request = sandbox::game::CraftRequest {
                    recipe: sandbox::game::RecipeId(recipe_id),
                    batch_count,
                    expected_catalog_version: request.catalog_version,
                    expected_inventory_revision: request.expected_inventory_revision,
                };
                match catalog.stage(inventory, craft_request) {
                    Ok(transaction) => match inventory.commit(transaction) {
                        Ok(_) => Outcome::Crafted,
                        Err(error) => Outcome::Rejected(map_craft_error(error)),
                    },
                    Err(error) => Outcome::Rejected(map_craft_error(error)),
                }
            }
        }
    };
    if request.catalog_version != sandbox::game::RECIPE_CATALOG_VERSION {
        if matches!(request.operation, Operation::InspectInventory) {
            outcome = Outcome::Rejected(Reject::CatalogVersion);
        }
    }
    ProgressionResponse {
        request_id: request.request_id,
        catalog_version: sandbox::game::RECIPE_CATALOG_VERSION,
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

fn map_craft_error(error: sandbox::game::CraftError) -> spall_protocol::ProgressionRejectCode {
    use sandbox::game::CraftError as Craft;
    use spall_protocol::ProgressionRejectCode as Reject;
    match error {
        Craft::CatalogVersionMismatch { .. } => Reject::CatalogVersion,
        Craft::InventoryRevisionMismatch { .. } | Craft::StaleInventory { .. } => {
            Reject::InventoryRevision
        }
        Craft::UnknownRecipe(_) => Reject::UnknownRecipe,
        Craft::ZeroBatch => Reject::ZeroBatch,
        Craft::InsufficientItems(_) => Reject::InsufficientItems,
        Craft::Overflow | Craft::RevisionExhausted => Reject::Overflow,
        Craft::InvalidStack => Reject::Unavailable,
    }
}

/// `--replay <db>`: rebuild the world from the oldest checkpoint + the whole
/// committed topology journal and compare its canonical hash to `--expect-hash`.
fn run_replay(args: Args) -> ExitCode {
    let db = args.replay.expect("checked by caller");
    let cfg = spall_server::PersistConfig {
        world_id: spall_server::serve::T10_WORLD_ID,
        seed: args.seed,
        generator_version: 1,
    };
    let (sim, events) =
        match spall_server::replay_from_base_with_manifest(&db, &cfg, sandbox::game::manifest()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("sandbox-server: replay failed: {e}");
                if let Some(path) = &args.summary_json {
                    let _ =
                        write_replay_summary(path, "failed", 0, "", args.expect_hash.as_deref());
                }
                return ExitCode::from(1);
            }
        };
    let hash = sim.world().world_hash().to_string();
    let matches = args
        .expect_hash
        .as_deref()
        .map(|h| h.eq_ignore_ascii_case(&hash))
        .unwrap_or(true);
    let result = if matches { "passed" } else { "failed" };
    println!("sandbox-server: replay {result} topology_events={events} hash={hash}");
    if let Some(path) = &args.summary_json {
        let _ = write_replay_summary(path, result, events, &hash, args.expect_hash.as_deref());
    }
    if matches {
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "sandbox-server: replay hash {hash} != expected {}",
            args.expect_hash.as_deref().unwrap_or("<none>")
        );
        ExitCode::from(1)
    }
}

/// Parses a `x,y,z` triple of finite `f64` metres.
fn parse_vec3(s: &str) -> Option<[f64; 3]> {
    let mut it = s.split(',').map(|p| p.trim().parse::<f64>());
    let x = it.next()?.ok()?;
    let y = it.next()?.ok()?;
    let z = it.next()?.ok()?;
    if it.next().is_some() || !(x.is_finite() && y.is_finite() && z.is_finite()) {
        return None;
    }
    Some([x, y, z])
}

fn write_replay_summary(
    path: &std::path::Path,
    result: &str,
    events: u64,
    hash: &str,
    expected: Option<&str>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let expected = expected.unwrap_or("");
    let body = format!(
        "{{\n  \"version\": 1,\n  \"result\": \"{result}\",\n  \"replayed_topology_events\": {events},\n  \"replayed_world_hash\": \"{hash}\",\n  \"expected_world_hash\": \"{expected}\"\n}}\n"
    );
    std::fs::write(path, body)
}

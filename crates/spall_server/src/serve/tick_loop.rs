use super::*;

#[allow(clippy::too_many_arguments)] // Existing host configuration and game callbacks.
pub(super) async fn serve_async(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
    materials: spall_core::MaterialManifest,
    game_setup: Option<InitialGameWorldSetup>,
    damage_profiles: Vec<(
        spall_core::MaterialId,
        spall_sim::ContactDamageMaterialProfile,
    )>,
    asset_manifest_hash: Option<spall_protocol::Hash32>,
    commit_handler: Option<CommittedEditHandler>,
    progression_handler: Option<ProgressionHandler>,
    player_credentials: Option<Vec<spall_net::PlayerCredential>>,
    mut outbox_encoder: Option<CommittedEditOutboxEncoder>,
    outbox_processor: Option<OutboxProcessor>,
) -> Result<ServeSummary, ServeError> {
    if matches!(config.server.scene, Scene::Custom)
        && config
            .server
            .custom_world
            .as_ref()
            .is_none_or(|world| world.player_spawns().is_empty())
    {
        return Err(ServeError::Configuration(
            "Scene::Custom requires HostConfig::custom_world with at least one player spawn".into(),
        ));
    }
    if config.server.creatures.is_some() && config.server.save.is_some() {
        return Err(ServeError::Configuration(
            "creatures cannot run in a persistent world yet: typed actor journal replay is not implemented; omit the save path"
                .into(),
        ));
    }
    let mut log = JsonlLog::create(&config.server.log_json)?;
    log.write(&ProcessRecord::new(
        ProcessEvent::Started,
        ProcessRole::Server,
        Some(format!(
            "scene={:?}, listen={}, max_ticks={}",
            config.server.scene, config.server.listen, config.harness.max_ticks
        )),
    ))?;

    let identity = DevIdentity::generate()?;
    if let Some(path) = &config.harness.fingerprint_out {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, identity.fingerprint().to_hex())?;
        std::fs::rename(&tmp, path)?;
    }

    let handshake = server_handshake(&materials, asset_manifest_hash);
    let credential_registry_file = config.server.credential_registry_file.clone();
    if credential_registry_file.is_some() && player_credentials.is_none() {
        return Err(ServeError::Configuration(
            "credential registry reload requires player-credential authentication".into(),
        ));
    }
    let server = Arc::new(if let Some(credentials) = player_credentials {
        NetServer::bind_with_player_credentials(
            config.server.listen,
            &identity,
            credentials,
            handshake,
            config.server.transport,
        )
        .await?
    } else {
        NetServer::bind(
            config.server.listen,
            &identity,
            config.server.join_token,
            handshake,
            config.server.transport,
        )
        .await?
    });
    let bound = server.local_addr()?;
    if let Some(path) = &config.harness.addr_out {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Write atomically-ish: temp then rename, so a reader never sees a
        // half-written address.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bound.to_string())?;
        std::fs::rename(&tmp, path)?;
    }
    log.write(&ProcessRecord::new(
        ProcessEvent::Ready,
        ProcessRole::Server,
        Some(format!("bound={bound}")),
    ))?;
    tracing::info!(
        action_catalog_version = tool_catalog.version(),
        "loaded authoritative action catalog"
    );

    let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
    // T20 egress accounting: live connection handles (for a final stats read at
    // teardown) plus the summed `(app_bytes, transport_bytes)` of connections
    // that already closed. A closing `serve_conn` removes itself from `conns`
    // and folds its final counts into `egress_closed` under the `conns` lock, so
    // teardown counts every connection exactly once.
    let conns: Arc<Mutex<HashMap<u64, Arc<Connection>>>> = Arc::new(Mutex::new(HashMap::new()));
    // T20 / T23 G3 row 14: per-session (not just summed) egress, keyed by
    // `session.raw()` — same close-once-counted discipline as the aggregate
    // version above.
    let egress_closed: Arc<Mutex<HashMap<u64, (u64, u64)>>> = Arc::new(Mutex::new(HashMap::new()));
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<Inbound>(INBOUND_CHANNEL_CAP);
    // Repair requests have their own bridge: a client that is waiting out a slow tick (a world
    // reset) re-requests bricks by the thousand, and on the shared bridge that flood displaced
    // its players' edits. Edits, input and joins are drained first.
    let (repair_tx, mut repair_rx) = mpsc::channel::<Inbound>(INBOUND_CHANNEL_CAP);
    let inbound_drops = Arc::new(InboundDrops::default());
    let (count_tx, mut count_rx) = watch::channel(0usize);
    let (stop_tx, stop_rx) = watch::channel(false);
    let (accept_stop_tx, accept_stop_rx) = watch::channel(false);

    // Reload credentials atomically from the operator-managed registry. A
    // malformed or unreadable update fails closed by revoking the whole live
    // registry; no token bytes or file contents enter logs.
    if let Some(path) = credential_registry_file {
        let server_for_reload = server.clone();
        let conns_for_reload = conns.clone();
        let mut stop_for_reload = stop_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            let mut previous_content: Option<Vec<u8>> = None;
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    changed = stop_for_reload.changed() => {
                        if changed.is_err() || *stop_for_reload.borrow() { break; }
                    }
                }
                let reload_path = path.clone();
                let loaded =
                    tokio::task::spawn_blocking(move || match std::fs::read(&reload_path) {
                        Ok(bytes) => match parse_player_credentials(&bytes) {
                            Ok(credentials) => (bytes, credentials, None),
                            Err(error) => (bytes, Vec::new(), Some(error)),
                        },
                        Err(error) => (
                            b"<unreadable>".to_vec(),
                            Vec::new(),
                            Some(format!("credential registry could not be read: {error}")),
                        ),
                    })
                    .await;
                let (content, credentials, invalid) = match loaded {
                    Ok(value) => value,
                    Err(_) => {
                        tracing::error!(
                            "credential registry reload worker failed; credentials unchanged"
                        );
                        continue;
                    }
                };
                if previous_content.as_ref() == Some(&content) {
                    continue;
                }
                let accepted_count = credentials.len();
                let replacement = if invalid.is_some() {
                    Vec::new()
                } else {
                    credentials
                };
                if let Err(error) = server_for_reload.replace_player_credentials(replacement) {
                    tracing::error!(error = %error, "credential registry rejected; revoking all credentials");
                    let _ = server_for_reload.replace_player_credentials(Vec::new());
                }
                previous_content = Some(content);
                // Authentication state is deliberately not introspectable by
                // token. Close live sessions on any rotation so a credential
                // removed from the file loses its existing session at once.
                let live = conns_for_reload
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                for connection in live.values() {
                    connection.close("player credentials changed");
                }
                if let Some(reason) = invalid {
                    tracing::error!(reason = %reason, "credential registry invalid; all player credentials revoked");
                } else {
                    tracing::info!(
                        credential_count = accepted_count,
                        "player credential registry reloaded; active sessions closed"
                    );
                }
            }
        });
    }

    // Accept loop.
    let accept = {
        let server = server.clone();
        let clients = clients.clone();
        let conns = conns.clone();
        let egress_closed = egress_closed.clone();
        let inbound_tx = inbound_tx.clone();
        let repair_tx = repair_tx.clone();
        let inbound_drops = inbound_drops.clone();
        let stop_rx = stop_rx.clone();
        let accept_stop_rx = accept_stop_rx.clone();
        let admission = AdmissionGate::new(config.server.max_clients);
        tokio::spawn(async move {
            let mut connected = 0usize;
            loop {
                if *stop_rx.borrow() || *accept_stop_rx.borrow() {
                    break;
                }
                let accepted = tokio::select! {
                    r = server.accept() => r,
                    _ = wait_true(stop_rx.clone()) => break,
                    _ = wait_true(accept_stop_rx.clone()) => break,
                };
                let conn = match accepted {
                    Ok(c) => Arc::new(c),
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        continue;
                    }
                };
                let Some(slot) = admission.try_admit() else {
                    conn.close("server at capacity");
                    continue;
                };
                connected += 1;
                let _ = count_tx.send(connected);
                let handle = OutboundHandle::new();
                clients
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(conn.session().raw(), handle.clone());
                conns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(conn.session().raw(), conn.clone());
                tokio::spawn(serve_conn(
                    conn,
                    inbound_tx.clone(),
                    repair_tx.clone(),
                    inbound_drops.clone(),
                    handle,
                    clients.clone(),
                    conns.clone(),
                    egress_closed.clone(),
                    stop_rx.clone(),
                    slot,
                ));
            }
        })
    };

    // Wait for the first `min_clients` (or time out).
    if config.harness.min_clients > 0 {
        let wait = async {
            loop {
                if *count_rx.borrow() >= config.harness.min_clients {
                    return;
                }
                if count_rx.changed().await.is_err() {
                    return;
                }
            }
        };
        if tokio::time::timeout(config.harness.startup_timeout, wait)
            .await
            .is_err()
        {
            let _ = stop_tx.send(true);
            server.close();
            return Err(ServeError::StartupTimeout(config.harness.startup_timeout));
        }
    }
    log.write(&ProcessRecord::new(
        ProcessEvent::Ready,
        ProcessRole::Server,
        Some(format!(
            "clients={} — tick loop starting",
            *count_rx.borrow()
        )),
    ))?;

    // The sim runs on a blocking thread; it owns `inbound_rx` and fans out
    // through the per-client senders.
    let paced = config.server.paced;
    let max_ticks = config.harness.max_ticks;
    let quiescence = config.harness.quiescence_ticks;
    let await_body_settle = config.harness.await_body_settle;
    let motion_interest = config.server.motion_interest;
    let scene = config.server.scene;
    let custom_world = config.server.custom_world.clone();
    let terrain_collider_mode = config.server.terrain_collider_mode;
    let clients_for_sim = clients.clone();
    let save = config.server.save.clone();
    let save_faults = config.harness.save_faults.clone();
    let checkpoint_interval = config.server.checkpoint_interval_ticks;
    let catch_up_cap = config.server.catch_up_cap.max(1);
    let max_join_retries = config.server.max_join_retries;
    let capture_workers = config.server.capture_workers.max(1);
    let dev_unvalidated_actions = config.harness.dev_unvalidated_actions;
    let admin_commands = config.server.admin_commands;
    // A world reset rebuilds from the scene; startup-only game content cannot
    // be reinstalled, so its presence refuses the reset instead.
    let reset_materials = materials.clone();
    let startup_game_setup = game_setup.is_some();
    let action_catalog = tool_catalog;
    let world_materials = materials;
    let residency_limits = config.server.residency;
    let residency_disk_path = config.server.residency_disk_path.clone();
    let creature_service = config.server.creatures.clone();
    let contact_damage_cfg = config.server.contact_damage;
    let contact_damage_profiles = damage_profiles;
    let dormancy_cfg = config.server.dormancy;
    let timing_window = config.harness.timing_window;
    let persist_cfg = PersistConfig {
        world_id: T10_WORLD_ID,
        seed: config.server.seed,
        generator_version: 1,
    };

    // Progression callbacks may touch the game's SQLite store. Keep their
    // bounded FIFO off the simulation thread; one worker preserves per-player
    // request ordering and returns replies only after the callback completes.
    let progression_worker = progression_handler.map(ProgressionWorker::spawn);
    let committed_edit_worker = commit_handler.map(CommittedEditWorker::spawn);

    let sim_join = tokio::task::spawn_blocking(move || -> SimResult {
        let setup_started = std::time::Instant::now();
        // Open the world database (T16). If it already holds a checkpoint,
        // recover from it; otherwise start the built-in scene and publish an
        // initial checkpoint so recovery always has a floor.
        // `setup_persistence` arms any test-only `save_faults` on the recovered
        // writer *after* recovery and the initial checkpoint, so the fault
        // falls on a later durable write, then hands the writer to the bounded
        // off-thread `PersistPipeline` (ENG-50).
        let Persistence {
            mut sim,
            mut pipeline,
            mut journalled_through,
            mut checkpoints_published,
            retained_transfer_ids,
            outbox_processor,
        } = match setup_persistence_with_game_content(
            save.as_deref(),
            scene,
            custom_world.as_ref(),
            &persist_cfg,
            save_faults,
            terrain_collider_mode,
            world_materials,
            game_setup,
            outbox_processor,
        ) {
            Ok(parts) => parts,
            Err(e) => {
                return SimResult::error(format!("persistence setup failed: {e}"), 0);
            }
        };
        if creature_service.is_some() {
            if save.is_some() {
                return SimResult::error(
                    "creatures cannot run in a persistent world yet: typed actor journal replay is not implemented"
                        .into(),
                    0,
                );
            }
            if let Err(error) =
                sim.initialize_creature_host(spall_core::WorldId::from_u128(T10_WORLD_ID))
            {
                return SimResult::error(format!("creature host init failed: {error:?}"), 0);
            }
        }
        for (stage, elapsed) in spall_sim::prof::drain() {
            tracing::info!(
                stage,
                elapsed_us = elapsed.as_micros() as u64,
                "startup profile"
            );
        }
        tracing::info!(
            stage = "authoritative_setup",
            elapsed_us = setup_started.elapsed().as_micros() as u64,
            "startup profile"
        );
        let progression_worker = progression_worker;
        let committed_edit_worker = committed_edit_worker;
        let outbox_worker = outbox_processor.map(OutboxWorker::spawn);
        let mut journal_records_written: u64 = 0;
        let mut pending_outbox = Vec::<spall_store::OutboxRecord>::new();
        let mut waiting_outbox = std::collections::VecDeque::<spall_store::OutboxRecord>::new();
        let mut outbox_inflight = None;

        // T23 / G3 row 7, slice D: default-off residency pass. `None` -> the
        // world stays fully resident and every counter below is `0`.
        let terrain_vid = sim.world().terrain_volume_id();
        // T23 / G3 row 7, item 2: `--residency-disk-backing` swaps the
        // in-process `MemoryBacking` default for a real disk-backed
        // `DiskBrickBacking` (`crate::disk_backing`) at
        // `<world>/residency.db`. Unset (the default), behaviour is
        // byte-identical to every prior run.
        let mut residency = match residency_limits {
            None => None,
            Some(limits) => match &residency_disk_path {
                None => Some(crate::ResidencyPass::install(sim.world_mut(), limits)),
                Some(path) => match crate::disk_backing::DiskBrickBacking::open(path) {
                    Ok(backing) => Some(crate::ResidencyPass::install_with_backing(
                        sim.world_mut(),
                        limits,
                        std::sync::Arc::new(backing),
                    )),
                    Err(e) => {
                        return SimResult::error(
                            format!("residency disk backing open failed: {e}"),
                            0,
                        );
                    }
                },
            },
        };

        let mut motion = MotionPublisher::new(60, 20);
        let mut committed_total = 0u64;
        let mut rejected_total = 0u64;
        // ENG-48: `ActionRequest`s / `RepairRequest`s that exceeded a session's
        // per-tick admission quota and were bounced with a retry response
        // (actions) or dropped (repairs).
        let mut actions_throttled = 0u64;
        let mut intent_stats = IntentStats::default();
        let mut repairs_throttled = 0u64;

        // T11a / ENG-62: commit-latency measurement + admission accounting.
        // `actions_requested` counts every inbound `ActionRequest` (retries
        // included); `actions_staged` counts those the sim accepted for staging;
        // `submitted_at` times each staged request from admission to the tick
        // its transaction commits. Whatever is still in `submitted_at` at end of
        // run was staged but never committed — the "queued" bucket.
        let mut actions_requested = 0u64;
        let mut actions_staged = 0u64;
        let mut submitted_at: HashMap<RequestId, std::time::Instant> = HashMap::new();
        // The session that staged each still-pending request, so the tick-report
        // outcome (`action_statuses`) can be routed back to only that client
        // instead of every connected client.
        let mut submitted_by: HashMap<RequestId, (SessionId, Option<spall_protocol::PlayerId>)> =
            HashMap::new();
        let mut player_principals: HashMap<u64, spall_protocol::PlayerId> = HashMap::new();
        let mut commit_latency = CommitLatency::default();

        // T21 / ENG-28 increment 4 (3c): default-off passes. `None` -> every
        // counter below stays `0` and neither pass is ever called, so an
        // ordinary run is byte-identical to before this increment.
        let mut contact_damage_policy = contact_damage_cfg.map(|cfg| {
            spall_sim::ContactDamagePolicy::with_material_profiles(cfg, contact_damage_profiles)
        });
        let mut dormancy_policy = dormancy_cfg.map(spall_sim::DormancyPolicy::new);
        let mut contact_damage_cuts_submitted = 0u64;
        let mut contact_damage_cuts_rejected = 0u64;
        let mut dormancy_deactivations_total = 0u64;
        let mut dormancy_reactivations_total = 0u64;
        let mut timing = timing_window.map(TimingCollector::new);

        // Not a gate pass: the interactive playground's timed debris drops
        // (`cargo xtask play --scene playground`). `populate_playground_debris`
        // (called earlier, in `Scene::simulation`) already spawned every
        // debris body dormant; nothing else in this scene ever creates a
        // dormant body, so every currently-dormant entity is exactly this
        // population, found by a fresh scan rather than threading a
        // `Vec<EntityId>` all the way through `setup_persistence`.
        let mut playground_drops =
            matches!(scene, Scene::Playground).then(|| playground_drop_schedules(&sim));

        let mut idle_streak = 0u64;
        let mut ticks_run = 0u64;
        let tick_dt = Duration::from_nanos(1_000_000_000 / 60);

        // T20: per-client interest / bandwidth state and run counters. Empty and
        // untouched unless `motion_interest` is configured.
        let mut client_repl: HashMap<u64, ClientReplication> = HashMap::new();
        let mut motion_batch_index = 0u64;
        let mut motion_egress = MotionEgress::default();
        // T23 / G3 row 14: each slot's player spawn, so the final per-connection
        // egress report can be read alongside where that connection's interest
        // anchor was.
        let mut client_spawns: HashMap<u32, [f64; 3]> = HashMap::new();

        // ENG-61: rolling "every detached body is holding still" window. Each
        // tick we compare every body's origin Y against the previous tick; a run
        // of ticks under 1 mm of movement is the "vertical position stable for N
        // ticks" evidence the G1 body-at-rest case needs.
        let mut prev_body_y: HashMap<u64, f64> = HashMap::new();
        let mut body_stable_ticks = 0u64;

        // T17 late-join / reconnect state.
        let mut lj = LateJoin::new(catch_up_cap, max_join_retries, capture_workers);
        lj.creatures = creature_service.clone();
        if let Err(error) = lj.seed_transfer_ids_from_recovery(&sim, &retained_transfer_ids) {
            return SimResult::error(
                format!("retained journal contains an invalid transfer ID: {error}"),
                0,
            );
        }
        // T23 / G3 row 7, slice D: a late-join baseline or repair patch over a
        // brick the residency pass has evicted is filled from its durable
        // backing.
        lj.backing = residency.as_ref().map(|p| p.backing());

        let mut pacer = crate::pacing::TickPacer::new(tick_dt, std::time::Instant::now());
        let mut pending_player_inputs = PlayerInputSchedule::default();
        // The water each session was last sent, so a session that becomes
        // live later still receives the current frame.
        let mut column_water_sent: HashMap<
            u64,
            crate::column_water_replication::ColumnWaterPublisher,
        > = HashMap::new();
        let mut snow_page_cursor: HashMap<u64, Option<(i32, i32, i32)>> = HashMap::new();
        let mut snow_center_page_cursor: HashMap<u64, spall_sim::snow::SnowSnapshotCursor> =
            HashMap::new();
        let mut water_keyframes_sent = 0u64;
        let mut water_bytes_queued = 0u64;
        let mut pending_admin: Vec<(SessionId, spall_protocol::AdminRequest)> = Vec::new();
        // A dam-gate edit only reports `Queued` immediately; its real
        // outcome (committed, or rejected — e.g. the terrain-size "giant
        // split" limit) resolves a few ticks later. This maps the edit's own
        // `RequestId` back to the admin session/request to correct with a
        // follow-up `AdminStatus` once it resolves, so a silent later
        // rejection is never mistaken for success.
        let mut pending_gate_admin: HashMap<u64, (SessionId, RequestId)> = HashMap::new();
        let mut world_resets = 0u64;
        let mut actions_discarded_by_reset = 0u64;
        // A real-time server stages edits on a worker thread so a burst of cuts cannot hold
        // the tick; an unpaced (headless, deterministic) run keeps the inline default.
        // `SPALL_INLINE_STAGING` selects the inline path for comparison.
        if paced && std::env::var_os("SPALL_INLINE_STAGING").is_none() {
            sim.enable_off_thread_staging();
            tracing::info!("edit staging runs off the tick thread");
        }
        // Diagnostic: per-session input arrival, logged every 300 ticks.
        let mut input_windows: HashMap<u64, InputArrivalWindow> = HashMap::new();
        // The tick budget grows one tick at a time while regional clients drain (see
        // `HarnessConfig::drain_timeout`); `iteration` counts ticks of this loop.
        let mut tick_budget = max_ticks;
        let mut vegetation_job: Option<std::sync::mpsc::Receiver<VegetationFrames>> = None;
        // Keyframes built so far, and the sessions that have been sent the climate raster
        // (it never changes, so a client keeps the last one; it is repeated every tenth
        // keyframe in case a droppable frame carrying it was lost).
        let mut vegetation_keyframes: u64 = 0;
        let mut vegetation_field_sent: std::collections::HashSet<u64> =
            std::collections::HashSet::new();
        // Phases at least this long are logged with the tick's other phases; lower it with
        // `SPALL_SLOW_PHASE_MS` to see what the tail of the tick distribution is made of.
        let slow_phase = Duration::from_millis(
            std::env::var("SPALL_SLOW_PHASE_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50),
        );
        // Where the tick thread's time goes: before-tick (client input, admission), the
        // simulation tick, and after (replication, persistence, residency), summed and worst.
        let mut last_slow_report: Option<std::time::Instant> = None;
        let mut last_tick_spans: Vec<(&'static str, Duration)> = Vec::new();
        let mut phase_totals = [Duration::ZERO; 3];
        let mut phase_worst = [Duration::ZERO; 3];
        let mut span_totals: HashMap<&'static str, (Duration, Duration)> = HashMap::new();
        let mut iteration = 0u64;
        let drain_timeout = config.harness.drain_timeout;
        let mut drain_started: Option<std::time::Instant> = None;
        let mut drain_quiet_since: Option<std::time::Instant> = None;
        let mut drain_timed_out = false;
        while iteration < tick_budget {
            iteration += 1;
            let started = std::time::Instant::now();
            // ENG-48: drain a bounded slice of what the clients have sent since
            // the last tick, with a per-session admission quota so one flooding
            // client can neither stall this loop nor starve the others. The
            // surplus stays in the bounded channel for the next tick.
            let mut repairs: Vec<(SessionId, RepairRequest)> = Vec::new();
            let mut saw_client_work = false;
            let mut actions_admitted: HashMap<u64, u32> = HashMap::new();
            let mut repairs_admitted: HashMap<u64, u32> = HashMap::new();
            let mut progression_admitted: HashMap<u64, u32> = HashMap::new();
            if let Some(worker) = progression_worker.as_ref() {
                for _ in 0..64 {
                    match worker.completed.try_recv() {
                        Ok(done) => send_to(
                            &clients_for_sim,
                            done.session,
                            Outbound::Progression(Arc::new(done.response)),
                        ),
                        Err(std::sync::mpsc::TryRecvError::Empty)
                        | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            }
            let mut drained = 0usize;
            while drained < MAX_INBOUND_PER_TICK {
                let Ok(msg) = inbound_rx.try_recv().or_else(|_| repair_rx.try_recv()) else {
                    break;
                };
                drained += 1;
                match msg {
                    Inbound::Joined(session, player_id) => {
                        lj.on_joined(session);
                        if let Some(player_id) = player_id {
                            player_principals.insert(session.raw(), player_id);
                        } else {
                            player_principals.remove(&session.raw());
                        }
                        // Player identity is slot-stable across reconnects;
                        // no input scheduled by the old generation may leak
                        // into the replacement session.
                        pending_player_inputs.remove_slot(session.slot().0);
                        // T19: give this connection an authoritative player
                        // capsule on a player scene (respawn on reconnect).
                        let spawns = custom_world
                            .as_ref()
                            .map_or_else(|| scene.player_spawns(), CustomWorld::player_spawns);
                        if !spawns.is_empty() {
                            let slot = session.slot().0 as usize;
                            let spawn = spawns[slot.min(spawns.len() - 1)];
                            sim.add_player(session_player_entity(session), spawn);
                            // T23 / G3 row 14: remember which spawn this slot
                            // got, so the final per-connection egress report
                            // can be read alongside *where* that connection's
                            // interest anchor was — self-describing bandwidth
                            // separation evidence with no client cooperation.
                            client_spawns.insert(session.slot().0, spawn);
                        }
                    }
                    Inbound::Input(session, frame) => {
                        saw_client_work = true;
                        if lj.session_expired(session) {
                            continue;
                        }
                        let entity = session_player_entity(session);
                        input_windows.entry(session.raw()).or_default().record(
                            frame.input_seq.0,
                            frame.intended_tick.0 as i64 - sim.current_tick().0 as i64,
                            sim.player_acked_input(entity).map_or(0, |a| a.0),
                        );
                        // Recover unseen redundant copies through the same
                        // admission rule as the primary input below. A copy
                        // carries its own intended_tick, so recovering one
                        // that was dropped as a primary datagram still lands
                        // on the tick it was tagged for -- applying it
                        // immediately here would reproduce the whole-tick
                        // edge snap this scheduling exists to remove, in
                        // exactly the case (a dropped datagram) the
                        // redundancy is for.
                        for r in frame.recent.iter().rev() {
                            if let Some(input) = pending_player_inputs.admit(
                                session,
                                entity,
                                recent_input(r),
                                r.input_seq,
                                r.intended_tick,
                                sim.current_tick(),
                                sim.player_acked_input(entity),
                            ) {
                                sim.set_player_input(entity, input, r.input_seq);
                            }
                        }
                        if let Some(input) = pending_player_inputs.admit(
                            session,
                            entity,
                            frame_input(&frame),
                            frame.input_seq,
                            frame.intended_tick,
                            sim.current_tick(),
                            sim.player_acked_input(entity),
                        ) {
                            sim.set_player_input(entity, input, frame.input_seq);
                        }
                    }
                    Inbound::Action(session, req) => {
                        saw_client_work = true;
                        actions_requested += 1;
                        if lj.session_expired(session) {
                            reject(&clients_for_sim, session, req.request_id, "expired session");
                            rejected_total += 1;
                            lj.expired_actions += 1;
                            continue;
                        }
                        // ENG-57: check the authoritative admission ledger
                        // before rate limiting or resolving the claim. A
                        // committed cut can make the original ray miss, and a
                        // reconnect may legitimately retry the same reliable
                        // request on its replacement session. Replaying this
                        // status must not restage or re-apply the edit.
                        if let Some(status) = replay_admitted_status(&sim, req.request_id) {
                            send_to(
                                &clients_for_sim,
                                session,
                                Outbound::Status(Arc::new(status.clone())),
                            );
                            continue;
                        }
                        if !admit(
                            &mut actions_admitted,
                            session.raw(),
                            MAX_ACTIONS_PER_CLIENT_PER_TICK,
                        ) {
                            reject(
                                &clients_for_sim,
                                session,
                                req.request_id,
                                "throttled: too many actions this tick, retry shortly",
                            );
                            rejected_total += 1;
                            actions_throttled += 1;
                            continue;
                        }
                        let resolved = if dev_unvalidated_actions {
                            dev_intent_from_request(session, &req)
                                .ok_or(ActionReject::Unsupported("unsupported target"))
                        } else {
                            resolve_intent_with_catalog(sim.world(), session, &req, &action_catalog)
                        };
                        match resolved {
                            Ok(intent) => match sim.submit(intent) {
                                Ok(status) => {
                                    submitted_at
                                        .entry(req.request_id)
                                        .or_insert_with(std::time::Instant::now);
                                    submitted_by.entry(req.request_id).or_insert((
                                        session,
                                        player_principals.get(&session.raw()).copied(),
                                    ));
                                    actions_staged += 1;
                                    send_to(
                                        &clients_for_sim,
                                        session,
                                        Outbound::Status(Arc::new(status)),
                                    );
                                }
                                Err(e) => {
                                    if matches!(e, spall_sim::IntentError::QueueFull { .. }) {
                                        intent_stats.queue_full_rejections += 1;
                                    }
                                    reject(
                                        &clients_for_sim,
                                        session,
                                        req.request_id,
                                        &intent_rejection_reason(&e),
                                    );
                                    rejected_total += 1;
                                }
                            },
                            Err(rej) => {
                                reject(&clients_for_sim, session, req.request_id, &rej.reason());
                                rejected_total += 1;
                            }
                        }
                    }
                    Inbound::Progression(session, req) => {
                        saw_client_work = true;
                        if lj.session_expired(session) {
                            continue;
                        }
                        let admitted = admit(
                            &mut progression_admitted,
                            session.raw(),
                            MAX_PROGRESSION_PER_CLIENT_PER_TICK,
                        );
                        let queued = admitted
                            && progression_worker.as_ref().is_some_and(|worker| {
                                worker
                                    .submit
                                    .try_send(ProgressionWork {
                                        session,
                                        player_id: player_principals.get(&session.raw()).copied(),
                                        request: req,
                                    })
                                    .is_ok()
                            });
                        if !queued {
                            // The queue is bounded. This retryable response is
                            // immediate and does not perform a fallback read.
                            let response = progression_rejected(
                                &req,
                                spall_protocol::ProgressionRejectCode::RetryableCapacity,
                            );
                            send_to(
                                &clients_for_sim,
                                session,
                                Outbound::Progression(Arc::new(response)),
                            );
                        }
                    }
                    Inbound::Repair(session, req) => {
                        saw_client_work = true;
                        if lj.session_expired(session) {
                            continue;
                        }
                        if admit(
                            &mut repairs_admitted,
                            session.raw(),
                            MAX_REPAIRS_PER_CLIENT_PER_TICK,
                        ) {
                            repairs.push((session, req));
                        } else {
                            repairs_throttled += 1;
                        }
                    }
                    Inbound::Baseline(session, ack) => {
                        saw_client_work = true;
                        lj.on_baseline_ack(session, ack, &sim, &clients_for_sim, &mut motion);
                    }
                    Inbound::Admin(session, req) => {
                        saw_client_work = true;
                        if !lj.session_expired(session) {
                            pending_admin.push((session, req));
                        }
                    }
                    Inbound::Gone(session) => {
                        pending_player_inputs.remove_session(session);
                        lj.on_gone(session);
                        player_principals.remove(&session.raw());
                        column_water_sent.remove(&session.raw());
                        snow_page_cursor.remove(&session.raw());
                        snow_center_page_cursor.remove(&session.raw());
                    }
                }
            }

            // Admin commands run between ticks, before this tick's inputs.
            for (session, req) in std::mem::take(&mut pending_admin) {
                let status = match req.command {
                    spall_protocol::AdminCommand::ResetWorld => {
                        let refusal = if !admin_commands {
                            Some("admin commands are disabled on this server")
                        } else if pipeline.is_some() {
                            Some("world reset is unavailable for a persisted world")
                        } else if residency.is_some() {
                            Some("world reset is unavailable while residency eviction is enabled")
                        } else if startup_game_setup {
                            Some(
                                "world reset is unavailable: startup game content cannot be reinstalled",
                            )
                        } else if contact_damage_policy.is_some() || dormancy_policy.is_some() {
                            Some(
                                "world reset is unavailable with contact damage or dormancy passes enabled",
                            )
                        } else {
                            None
                        };
                        match refusal {
                            Some(reason) => spall_protocol::AdminStatus {
                                request_id: req.request_id,
                                accepted: false,
                                message: reason.into(),
                            },
                            None => match lj.reserve_reset_group() {
                                Err(error) => spall_protocol::AdminStatus {
                                    request_id: req.request_id,
                                    accepted: false,
                                    message: format!("world reset refused: {error}"),
                                },
                                Ok(reset_group) => {
                                    let started = std::time::Instant::now();
                                    // What deferred clients already hold of the world about to be
                                    // replaced, so their catalogue can be a delta.
                                    let catalogue_basis =
                                        lj.capture_catalogue_basis(&sim, &clients_for_sim);
                                    let basis_ms = started.elapsed().as_millis() as u64;
                                    // The old world is not needed from here (the tick thread is busy
                                    // for the whole reset): start freeing it on a background thread
                                    // before the new one is built, so the two are not both resident
                                    // at full size. A one-brick placeholder stands in meanwhile.
                                    match Simulation::new(spall_sim::SimulationConfig::new(
                                        spall_sim::fixtures::flat_terrain_setup(),
                                    )) {
                                        Ok(placeholder) => {
                                            if let Err(e) = sim.replace_world(placeholder) {
                                                return SimResult::error(
                                                    format!("world reset failed: {e}"),
                                                    ticks_run,
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            return SimResult::error(
                                                format!("world reset failed: {e}"),
                                                ticks_run,
                                            );
                                        }
                                    }
                                    sim.wait_for_retired();
                                    let freed_ms = started.elapsed().as_millis() as u64;
                                    let mut fresh = scene.simulation_with_materials(
                                        terrain_collider_mode,
                                        reset_materials.clone(),
                                        custom_world.as_ref(),
                                    );
                                    fresh.set_weather_seed(config.server.seed);
                                    let built_ms = started.elapsed().as_millis() as u64;
                                    if let Err(e) = sim.replace_world(fresh) {
                                        return SimResult::error(
                                            format!("world reset failed: {e}"),
                                            ticks_run,
                                        );
                                    }
                                    let replaced_ms = started.elapsed().as_millis() as u64;
                                    let spawns = custom_world.as_ref().map_or_else(
                                        || scene.player_spawns(),
                                        CustomWorld::player_spawns,
                                    );
                                    if !spawns.is_empty() {
                                        for session in lj.sessions().collect::<Vec<_>>() {
                                            let slot = session.slot().0 as usize;
                                            let spawn = spawns[slot.min(spawns.len() - 1)];
                                            sim.add_player(session_player_entity(session), spawn);
                                        }
                                    }
                                    // Everything below described bodies or edits of the
                                    // replaced world.
                                    // An edit staged against the replaced world will never
                                    // commit (the new world has a new pipeline). Say so, so the
                                    // client retries it instead of waiting for a receipt that
                                    // cannot come.
                                    for (request, (session, _)) in submitted_by.drain() {
                                        reject(
                                            &clients_for_sim,
                                            session,
                                            request,
                                            "overloaded: world reset, retry",
                                        );
                                        actions_discarded_by_reset += 1;
                                        rejected_total += 1;
                                    }
                                    submitted_at.clear();
                                    client_repl.clear();
                                    prev_body_y.clear();
                                    body_stable_ticks = 0;
                                    column_water_sent.clear();
                                    snow_page_cursor.clear();
                                    snow_center_page_cursor.clear();
                                    if let Some(drops) = playground_drops.as_mut() {
                                        *drops = playground_drop_schedules(&sim);
                                    }
                                    match lj.reset_world(
                                        &sim,
                                        &clients_for_sim,
                                        &mut motion,
                                        catalogue_basis.as_ref(),
                                        reset_group,
                                    ) {
                                        Ok(clients) => {
                                            world_resets += 1;
                                            if let Some(service) = creature_service.as_ref()
                                                && let Err(error) = service.on_world_reset()
                                            {
                                                return SimResult::error(error, ticks_run);
                                            }
                                            tracing::info!(
                                                clients,
                                                freed_ms,
                                                built_ms,
                                                basis_ms,
                                                replaced_ms,
                                                elapsed_ms = started.elapsed().as_millis() as u64,
                                                "admin world reset"
                                            );
                                            spall_protocol::AdminStatus {
                                                request_id: req.request_id,
                                                accepted: true,
                                                message: format!(
                                                    "world reset in {} ms",
                                                    started.elapsed().as_millis()
                                                ),
                                            }
                                        }
                                        Err(e) => {
                                            return SimResult::error(
                                                format!("world reset baseline capture failed: {e}"),
                                                ticks_run,
                                            );
                                        }
                                    }
                                }
                            },
                        }
                    }
                    spall_protocol::AdminCommand::SetWaterSpring { rate } => {
                        let has_gated_spring = custom_world.as_ref().is_some_and(|c| {
                            c.column_water_setup()
                                .is_some_and(|w| w.gated_sources.iter().any(|t| !t.is_empty()))
                        });
                        if !admin_commands {
                            spall_protocol::AdminStatus {
                                request_id: req.request_id,
                                accepted: false,
                                message: "admin commands are disabled on this server".into(),
                            }
                        } else if !has_gated_spring {
                            spall_protocol::AdminStatus {
                                request_id: req.request_id,
                                accepted: false,
                                message: "this scene authored no gated water spring".into(),
                            }
                        } else if rate > spall_sim::column_water::MAX_GATED_SOURCE_RATE {
                            spall_protocol::AdminStatus {
                                request_id: req.request_id,
                                accepted: false,
                                message: format!(
                                    "water spring rate must be 0..={}",
                                    spall_sim::column_water::MAX_GATED_SOURCE_RATE
                                ),
                            }
                        } else {
                            sim.set_water_spring_rate(rate);
                            spall_protocol::AdminStatus {
                                request_id: req.request_id,
                                accepted: true,
                                message: if rate == 0 {
                                    "water spring off".to_string()
                                } else {
                                    format!("water spring rate {rate}")
                                },
                            }
                        }
                    }
                    spall_protocol::AdminCommand::SetWeather { kind } => {
                        set_weather_admin(&mut sim, admin_commands, req.request_id, kind)
                    }
                    spall_protocol::AdminCommand::TeleportSelf { feet_mm } => {
                        let feet = feet_mm.map(|mm| mm as f64 / 1000.0);
                        let (accepted, message) = if !admin_commands {
                            (false, "admin commands are disabled on this server")
                        } else if sim.teleport_player(session_player_entity(session), feet) {
                            (true, "teleported")
                        } else {
                            (false, "no player to teleport")
                        };
                        spall_protocol::AdminStatus {
                            request_id: req.request_id,
                            accepted,
                            message: message.into(),
                        }
                    }
                    spall_protocol::AdminCommand::SetDamGate { open } => {
                        let gate = custom_world.as_ref().and_then(CustomWorld::dam_gate);
                        if !admin_commands {
                            spall_protocol::AdminStatus {
                                request_id: req.request_id,
                                accepted: false,
                                message: "admin commands are disabled on this server".into(),
                            }
                        } else if let Some(gate) = gate {
                            match sim.set_dam_gate(&gate.cells, open, gate.closed_material) {
                                Ok(edit_statuses) => {
                                    for edit_status in &edit_statuses {
                                        pending_gate_admin.insert(
                                            edit_status.request_id.0,
                                            (session, req.request_id),
                                        );
                                    }
                                    spall_protocol::AdminStatus {
                                        request_id: req.request_id,
                                        accepted: true,
                                        message: format!(
                                            "dam gate {} ({} piece{}; queued, watch for follow-up status)",
                                            if open { "opening" } else { "closing" },
                                            edit_statuses.len(),
                                            if edit_statuses.len() == 1 { "" } else { "s" }
                                        ),
                                    }
                                }
                                Err(e) => spall_protocol::AdminStatus {
                                    request_id: req.request_id,
                                    accepted: false,
                                    message: format!("dam gate edit refused: {e}"),
                                },
                            }
                        } else {
                            spall_protocol::AdminStatus {
                                request_id: req.request_id,
                                accepted: false,
                                message: "this scene authored no dam gate".into(),
                            }
                        }
                    }
                };
                send_to(&clients_for_sim, session, Outbound::Admin(Arc::new(status)));
            }

            lj.publish_ready_captures(&clients_for_sim);
            let next_tick = spall_core::Tick(sim.current_tick().0.saturating_add(1));
            for input in pending_player_inputs.take_due(next_tick) {
                if !lj.session_expired(input.session) {
                    sim.set_player_input(input.entity, input.input, input.seq);
                }
            }
            let before_tick = started.elapsed();
            // Reserve a conservative shared-ID range before the tick can
            // mutate simulation state. Unused IDs are intentionally burned.
            let split_id_bound = sim.transfer_reservation_bound();
            let split_transfer_ids = match lj.reserve_ids(TransferNamespace::Split, split_id_bound)
            {
                Ok(ids) => ids,
                Err(error) => {
                    return SimResult::error(
                        format!("split transfer ID reservation failed before tick: {error}"),
                        ticks_run,
                    );
                }
            };
            let report = match sim.tick_with_transfer_ids(split_transfer_ids) {
                Ok(r) => r,
                Err(e) => return SimResult::error(format!("tick failed: {e}"), ticks_run),
            };
            let sim_tick = started.elapsed() - before_tick;
            ticks_run += 1;
            // Commit and staging phases measured on this tick (they accumulate thread-locally and
            // would otherwise never be read at run time): report the slow ones.
            let spans = spall_sim::prof::drain();
            last_tick_spans.clone_from(&spans);
            for (name, elapsed) in &spans {
                let entry = span_totals.entry(*name).or_default();
                entry.0 += *elapsed;
                entry.1 = entry.1.max(*elapsed);
            }
            if spans.iter().any(|(_, elapsed)| *elapsed >= slow_phase) {
                let phases: Vec<String> = spans
                    .iter()
                    .map(|(name, elapsed)| format!("{name}={:.1}ms", elapsed.as_secs_f64() * 1e3))
                    .collect();
                tracing::info!(tick = sim.current_tick().0, ?phases, "slow commit phases");
            }
            if ticks_run.is_multiple_of(300) {
                for (session, window) in input_windows.iter_mut() {
                    window.report_and_reset(*session, sim.current_tick().0);
                }
            }
            intent_stats.record_tick(
                report.pending_after,
                report.committed.len(),
                report.retried.len(),
                report.discarded_stale.len(),
                report.serialized_regions.len(),
            );
            let tick = sim.current_tick();
            // Correct an earlier "queued" dam-gate admin reply once its edit
            // actually resolves — committed or (today, on this scene's giant
            // terrain) rejected — so a silent later failure is never mistaken
            // for the immediate "accepted" ack.
            if !pending_gate_admin.is_empty() {
                for (request_id, _) in &report.committed {
                    if let Some((gate_session, admin_request_id)) =
                        pending_gate_admin.remove(&request_id.0)
                    {
                        send_to(
                            &clients_for_sim,
                            gate_session,
                            Outbound::Admin(Arc::new(spall_protocol::AdminStatus {
                                request_id: admin_request_id,
                                accepted: true,
                                message: "dam gate edit committed".into(),
                            })),
                        );
                    }
                }
                for (request_id, reason) in &report.rejected {
                    if let Some((gate_session, admin_request_id)) =
                        pending_gate_admin.remove(&request_id.0)
                    {
                        send_to(
                            &clients_for_sim,
                            gate_session,
                            Outbound::Admin(Arc::new(spall_protocol::AdminStatus {
                                request_id: admin_request_id,
                                accepted: false,
                                message: format!("dam gate edit rejected: {reason}"),
                            })),
                        );
                    }
                }
            }
            // ENG-105: a periodic water line, so an operator can see whether
            // the fluid step keeps up with real time.
            if tick.get() % WATER_STATUS_INTERVAL_TICKS == 0
                && let (Some(water), Some(metrics)) =
                    (sim.column_water(), report.column_water.as_ref())
            {
                let frame = water.frame();
                tracing::info!(
                    frame = frame.seq,
                    fluid_time_s = frame.fluid_time_s,
                    skipped_s = metrics.skipped_s,
                    last_step_ms = metrics.step_duration.as_secs_f64() * 1e3,
                    active_regions = metrics.active_regions,
                    volume_m3 = frame.volume_m3,
                    springs_m3 = frame.spring_added_m3,
                    drains_m3 = frame.drain_removed_m3,
                    "water status"
                );
                if std::env::var_os("SPALL_WATER_DIAG").is_some() {
                    eprintln!("sandbox-server: water diag: {}", water.diag_summary());
                }
                eprintln!(
                    "sandbox-server: water frame {} | fluid {:.1} s | skipped {:.1} s | last step {:.1} ms | {} active regions ({} resting far from players) | {:.0} m³ (+{:.0} springs, -{:.0} drains)",
                    frame.seq,
                    frame.fluid_time_s,
                    metrics.skipped_s,
                    metrics.step_duration.as_secs_f64() * 1e3,
                    metrics.active_regions,
                    metrics.step.map_or(0, |s| s.resting_regions),
                    frame.volume_m3,
                    frame.spring_added_m3,
                    frame.drain_removed_m3,
                );
            }

            // T21 / ENG-28 increment 4 (3c): opt-in contact damage + region
            // dormancy, run every tick right after the commit they react to.
            // A contact-damage cut stages off-tick and commits on a later
            // tick, like any client edit; a dormancy deactivation/reactivation
            // applies to `sim.world()` immediately but never touches
            // authoritative geometry, ownership, or `world_hash`.
            if let Some(policy) = contact_damage_policy.as_mut() {
                let cd = sim.apply_contact_damage(policy, &report);
                contact_damage_cuts_submitted += cd.submitted as u64;
                contact_damage_cuts_rejected += cd.rejected as u64;
            }
            if let Some(policy) = dormancy_policy.as_mut() {
                let dp = sim.apply_dormancy(policy, &report);
                dormancy_deactivations_total += dp.deactivate.len() as u64;
                dormancy_reactivations_total += dp.reactivate.len() as u64;
            }
            if let Some((showcase, plinko)) = playground_drops.as_mut() {
                showcase.tick(sim.world_mut(), tick.get());
                plinko.tick(sim.world_mut(), tick.get());
            }

            // ENG-61: fold this tick into the "bodies holding still" window.
            // Only when the scenario asked for it — an ordinary run does no
            // extra per-tick work, so its timing (and an impaired run's
            // proxy-reordering outcome) is byte-for-byte unchanged.
            if await_body_settle {
                let mut max_dy = 0.0_f64;
                let mut have_history = !prev_body_y.is_empty();
                let mut seen = 0usize;
                let mut next_body_y: HashMap<u64, f64> = HashMap::with_capacity(prev_body_y.len());
                for b in sim.world().bodies() {
                    seen += 1;
                    let key = b.entity.map(|e| e.get()).unwrap_or(0);
                    let y = b.pose.translation_m[1];
                    match prev_body_y.get(&key) {
                        Some(py) => max_dy = max_dy.max((y - py).abs()),
                        None => have_history = false,
                    }
                    next_body_y.insert(key, y);
                }
                prev_body_y = next_body_y;
                if seen > 0 && have_history && max_dy < 1.0e-3 {
                    body_stable_ticks += 1;
                } else {
                    body_stable_ticks = 0;
                }
            }

            let span_serve_commit_fanout = spall_sim::prof::Span::start("serve.commit_fanout");
            for (rid, committed) in &report.committed {
                committed_total += 1;
                if !committed.removed_materials.is_empty()
                    && let Some((session, player_id)) = submitted_by.get(rid).copied()
                {
                    let encoded = outbox_encoder.as_mut().and_then(|encoder| {
                        encoder(
                            session,
                            player_id,
                            *rid,
                            committed.journal_seq,
                            &committed.removed_materials,
                        )
                    });
                    if let Some(event) = encoded.filter(|_| pipeline.is_some()) {
                        pending_outbox.push(event);
                    } else if let Some(worker) = committed_edit_worker.as_ref() {
                        match worker.submit.try_send(CommittedEditWork {
                            session,
                            player_id,
                            request_id: *rid,
                            removed: committed.removed_materials.clone(),
                        }) {
                            Ok(()) => {}
                            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                return SimResult::error(
                                    "committed progression queue is full; server stopped with the award pending only in ephemeral memory".into(),
                                    ticks_run,
                                );
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                return SimResult::error(
                                    "committed progression worker stopped".into(),
                                    ticks_run,
                                );
                            }
                        }
                    }
                }
                // T17 increment 2: a giant split ships its geometry out of band
                // under the same allocator-reserved ID embedded in its markers.
                let fan_out_started = std::time::Instant::now();
                let split_transfer = committed.bulk_baseline.as_ref().and_then(|world| {
                    let id = committed
                        .bulk_transfer_id
                        .expect("bulk split carries its reserved marker transfer ID");
                    baseline::transfer_from_world(
                        world.as_ref().clone(),
                        id,
                        InterestEpoch(1),
                        JournalSeq(sim.journal_cursor()),
                    )
                    .ok()
                    .map(Arc::new)
                });
                lj.fan_out_transaction(
                    Arc::new(committed.topology.clone()),
                    split_transfer,
                    &sim,
                    &clients_for_sim,
                );
                let fan_out = fan_out_started.elapsed();
                if fan_out >= Duration::from_millis(50) {
                    tracing::info!(
                        request = rid.0,
                        fan_out_ms = fan_out.as_secs_f64() * 1_000.0,
                        bulk_baseline = committed.bulk_baseline.is_some(),
                        "slow commit fan-out (split transfer encoding and fan-out)"
                    );
                }
                // T11a / ENG-62: bucket each commit's server-side latency
                // (admission → commit) by whether it split and how much
                // geometry detached.
                if let Some(started_at) = submitted_at.remove(rid) {
                    let class =
                        commit_latency::classify(committed.bumped_epoch, &committed.topology);
                    let waited = started_at.elapsed();
                    let timing = sim.take_edit_timing(*rid).unwrap_or_default();
                    tracing::info!(
                        request = rid.0,
                        class = ?class,
                        latency_ms = waited.as_secs_f64() * 1_000.0,
                        queued_ms = timing.queued.as_secs_f64() * 1_000.0,
                        staging_ms = timing.staging.as_secs_f64() * 1_000.0,
                        commit_ms = timing.commit.as_secs_f64() * 1_000.0,
                        fan_out_ms = fan_out.as_secs_f64() * 1_000.0,
                        tick = sim.current_tick().0,
                        "commit latency"
                    );
                    commit_latency.record(class, waited);
                }
                // Slice D: keep the durable backing current for the terrain
                // bricks this edit changed, so a later reload gets this
                // revision.
                if let Some(pass) = &residency {
                    pass.on_commit(
                        sim.world(),
                        committed
                            .topology
                            .after
                            .iter()
                            .filter(|br| br.volume == terrain_vid)
                            .map(|br| br.coord),
                    );
                }
            }
            drop(span_serve_commit_fanout);
            if let Some(service) = creature_service.as_ref() {
                match service.run_tick(&mut sim) {
                    Ok(Some(committed)) => {
                        committed_total += 1;
                        tracing::info!(
                            tick = sim.current_tick().get(),
                            transaction = committed.transaction_id.get(),
                            "creature lifecycle committed"
                        );
                        lj.fan_out_transaction(
                            Arc::new(committed.topology),
                            None,
                            &sim,
                            &clients_for_sim,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return SimResult::error(
                            format!("creature lifecycle commit failed: {error}"),
                            ticks_run,
                        );
                    }
                }
            }
            if let Some(service) = creature_service.as_ref() {
                if let Err(error) = service.run_motion(&mut sim) {
                    return SimResult::error(error, ticks_run);
                }
                // Presentation frames at 20 Hz, to live sessions only, limited to
                // creatures near that session's player.
                if sim.current_tick().get() % CREATURE_MOTION_PERIOD_TICKS == 0 {
                    for session in lj.live_sessions().collect::<Vec<_>>() {
                        if water_backlogged(&clients_for_sim, session) {
                            continue;
                        }
                        let center = sim
                            .player_state(session_player_entity(session))
                            .map(|player| player.position_m);
                        let include = move |position: [f64; 3]| match center {
                            Some(center) => {
                                let dx = position[0] - center[0];
                                let dz = position[2] - center[2];
                                dx * dx + dz * dz
                                    <= CREATURE_MOTION_RADIUS_M * CREATURE_MOTION_RADIUS_M
                            }
                            None => true,
                        };
                        if let Some(frame) = sim.creature_motion_frame(&include) {
                            send_to(
                                &clients_for_sim,
                                session,
                                Outbound::Water(Arc::new(vec![WireRecord::CreatureMotionFrame(
                                    frame,
                                )])),
                            );
                        }
                    }
                }
            }
            for status in action_statuses(&report) {
                if matches!(status.outcome, ActionOutcome::Rejected { .. }) {
                    rejected_total += 1;
                }
                // A tick-resolved outcome (commit or deterministic staging
                // rejection) belongs to whichever session staged it — route it
                // there only. `submitted_by` is best-effort bookkeeping (cleared
                // here on first resolution); an untracked request (there
                // shouldn't be one) falls back to the old broadcast so a status
                // is never silently dropped.
                match submitted_by.remove(&status.request_id) {
                    Some((session, _player_id)) => send_to(
                        &clients_for_sim,
                        session,
                        Outbound::Status(Arc::new(status)),
                    ),
                    None => broadcast(&clients_for_sim, Outbound::Status(Arc::new(status))),
                }
            }
            // The 20 Hz motion batch: send it to replicas *and* keep the full
            // batch for the durable pose journal below (durability is never
            // interest-filtered).
            let span_serve_motion = spall_sim::prof::Span::start("serve.motion");
            let pose_batch: Option<Vec<MotionSnapshot>> = if motion.due(tick) {
                let snaps = motion.snapshots(sim.world(), tick);
                if !snaps.is_empty() {
                    match motion_interest {
                        // Pre-T20: one batch, unfiltered *among live replicas*.
                        // A client still installing its late-join baseline must
                        // receive neither the live 20 Hz feed nor its stale
                        // datagram backlog: the baseline barrier supplies a
                        // current keyframe when it becomes live. The
                        // interest-aware path below already has this property
                        // through `LateJoin::live_sessions`; keep the default
                        // path on the same protocol contract.
                        None => {
                            let recipients: Vec<_> = lj.live_sessions().collect();
                            motion_egress.snapshots_sent +=
                                snaps.len() as u64 * recipients.len() as u64;
                            send_motion_to_sessions(
                                &clients_for_sim,
                                recipients,
                                Arc::new(snaps.clone()),
                            );
                        }
                        // T20: per-client interest relevance + bandwidth budget.
                        Some(mi) => {
                            dispatch_motion_by_interest(
                                &mi,
                                scene,
                                motion_batch_index,
                                &snaps,
                                &sim,
                                &lj,
                                &clients_for_sim,
                                &mut client_repl,
                                &mut motion_egress,
                            );
                        }
                    }
                    motion_batch_index += 1;
                }
                Some(snaps)
            } else {
                None
            };
            drop(span_serve_motion);
            for (session, req) in repairs {
                lj.answer_repair(session, &req, &sim, &clients_for_sim);
            }

            let span_serve_vegetation = spall_sim::prof::Span::start("serve.vegetation");
            // Building, encoding and chunking the vegetation frame took about 11 ms once a
            // second on the tick thread. A worker does it from a copy of the state and terrain;
            // the next ticks send the result. A worker that dies is an error, never silence.
            if tick.get().is_multiple_of(60)
                && vegetation_job.is_none()
                && let Some((state, terrain)) = sim.vegetation_visual_input()
            {
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let frame_tick = tick.get();
                vegetation_keyframes += 1;
                let repeat_field = vegetation_keyframes.is_multiple_of(10);
                // Each regional client is sent the plants around its player; every other
                // client gets the whole world.
                let sessions: Vec<VegetationTarget> = lj
                    .live_sessions()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|session| VegetationTarget {
                        session,
                        window: lj.vegetation_window(&sim, session),
                        with_field: repeat_field || vegetation_field_sent.insert(session.raw()),
                    })
                    .collect();
                match std::thread::Builder::new()
                    .name("spall-vegetation-frame".into())
                    .spawn(move || {
                        let _ = done_tx.send(vegetation_frames(
                            &state.visual(&terrain),
                            frame_tick,
                            &sessions,
                        ));
                    }) {
                    Ok(_) => vegetation_job = Some(done_rx),
                    Err(e) => {
                        return SimResult::error(
                            format!("vegetation frame worker could not start: {e}"),
                            ticks_run,
                        );
                    }
                }
            }
            if let Some(done_rx) = &vegetation_job {
                match done_rx.try_recv() {
                    Ok(Ok(frames)) => {
                        vegetation_job = None;
                        for (session, records) in frames {
                            if !lj.is_live(session) || water_backlogged(&clients_for_sim, session) {
                                // Not sent: it must be offered the raster again.
                                vegetation_field_sent.remove(&session.raw());
                                continue;
                            }
                            send_to(&clients_for_sim, session, Outbound::Water(records));
                        }
                    }
                    Ok(Err(e)) => {
                        return SimResult::error(
                            format!("vegetation frame encoding failed: {e}"),
                            ticks_run,
                        );
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return SimResult::error(
                            "vegetation frame worker ended without a result".into(),
                            ticks_run,
                        );
                    }
                }
            }
            drop(span_serve_vegetation);
            // Weather keyframes are tiny, bounded and supersedable. One-second cadence also
            // supplies the current state to late joiners and immediately after world reset.
            if tick.get().is_multiple_of(60) {
                let record = WireRecord::WeatherSnapshot(spall_protocol::WeatherSnapshot {
                    state: sim.weather_state().clone(),
                });
                let records = Arc::new(vec![record]);
                for session in lj.live_sessions().collect::<Vec<_>>() {
                    send_to(
                        &clients_for_sim,
                        session,
                        Outbound::Water(Arc::clone(&records)),
                    );
                }
            }
            // Alternate a bounded player-center page with a bounded regional
            // page. This gives the visible tile prompt coverage while the
            // independent regional cursor continues to visit outer tiles.
            if tick.get().is_multiple_of(6) {
                for session in lj.live_sessions().collect::<Vec<_>>() {
                    let bounds = lj.snow_interest(&sim, session);
                    let Some(bounds) = bounds else { continue };
                    if water_backlogged(&clients_for_sim, session) {
                        continue;
                    }
                    let center_page = (tick.get() / 6).is_multiple_of(2);
                    if center_page {
                        let Some(player) = sim.player_state(session_player_entity(session)) else {
                            continue;
                        };
                        let tile = |meters: f64| (meters / 4.0).floor();
                        let (x, z) = (tile(player.position_m[0]), tile(player.position_m[2]));
                        let (Ok(x), Ok(z)) = (i32::try_from(x as i64), i32::try_from(z as i64))
                        else {
                            continue;
                        };
                        let center = (x, z);
                        let (frames, next) = sim.snow_nearest_snapshot_page(
                            bounds,
                            center,
                            snow_center_page_cursor.get(&session.raw()).copied(),
                            16,
                        );
                        if let Some(next) = next {
                            snow_center_page_cursor.insert(session.raw(), next);
                        } else {
                            snow_center_page_cursor.remove(&session.raw());
                        }
                        if !frames.is_empty() {
                            let records = frames
                                .into_iter()
                                .map(WireRecord::SnowSurfaceFrame)
                                .collect();
                            send_to(
                                &clients_for_sim,
                                session,
                                Outbound::Water(Arc::new(records)),
                            );
                        }
                    } else {
                        let (frames, next) = sim.snow_snapshot_page(
                            Some(bounds),
                            snow_page_cursor.get(&session.raw()).copied().flatten(),
                            4,
                        );
                        snow_page_cursor.insert(session.raw(), next);
                        if !frames.is_empty() {
                            let records = frames
                                .into_iter()
                                .map(WireRecord::SnowSurfaceFrame)
                                .collect();
                            send_to(
                                &clients_for_sim,
                                session,
                                Outbound::Water(Arc::new(records)),
                            );
                        }
                    }
                }
            }
            let span_serve_water_fanout = spall_sim::prof::Span::start("serve.water_fanout");
            if let Some(column_water) = sim.column_water() {
                let frame = column_water.frame();
                for session in lj.live_sessions().collect::<Vec<_>>() {
                    if water_backlogged(&clients_for_sim, session) {
                        continue;
                    }
                    let interest = lj.water_interest(&sim, session);
                    match column_water_sent.entry(session.raw()).or_default().poll(
                        tick.get(),
                        sim.current_tick(),
                        &frame,
                        interest,
                    ) {
                        Ok(records) if !records.is_empty() => {
                            water_keyframes_sent += 1;
                            water_bytes_queued += records
                                .iter()
                                .map(|r| {
                                    r.encode_framed().expect("publisher validated record").len()
                                        as u64
                                        + 32
                                })
                                .sum::<u64>();
                            send_to(
                                &clients_for_sim,
                                session,
                                Outbound::Water(Arc::new(records)),
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            return SimResult::error(
                                format!("column water update encoding: {e}"),
                                ticks_run,
                            );
                        }
                    }
                }
            }
            drop(span_serve_water_fanout);

            // ENG-50: queue immutable records to the bounded off-thread writer.
            // The sim thread never blocks on the disk; when the backlog fills or
            // a durable write has failed, the run stops rather than silently
            // continuing an unsavable world (`docs/protocol.md` Persistence).
            let span_serve_persist_submit = spall_sim::prof::Span::start("serve.persist_submit");
            if let Some(pipe) = pipeline.as_ref() {
                let batch = match tick_journal_batch(
                    &mut sim,
                    &mut journalled_through,
                    pose_batch.as_deref(),
                    tick.get(),
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        return SimResult::error(format!("journal encode failed: {e}"), ticks_run);
                    }
                };
                let records = std::mem::take(&mut pending_outbox);
                if let Err(e) = pipe.submit_journal_with_outbox(batch, records.clone()) {
                    return SimResult::error(format!("persistence: {e}"), ticks_run);
                }
                waiting_outbox.extend(records);
                let durable = pipe.durable_seq();
                let mut acknowledged = Vec::new();
                if let Some(worker) = outbox_worker.as_ref() {
                    match worker.completed.try_recv() {
                        Ok(done) => {
                            if outbox_inflight != Some(done.event_id) {
                                return SimResult::error(
                                    "outbox completion did not match the in-flight event".into(),
                                    ticks_run,
                                );
                            }
                            if let Err(error) = done.result {
                                return SimResult::error(
                                    format!("harvest outbox delivery failed: {error}"),
                                    ticks_run,
                                );
                            }
                            let Some(event) = waiting_outbox.pop_front() else {
                                return SimResult::error(
                                    "outbox completion has no retained event".into(),
                                    ticks_run,
                                );
                            };
                            if event.event_id != done.event_id {
                                return SimResult::error(
                                    "outbox completion order changed".into(),
                                    ticks_run,
                                );
                            }
                            acknowledged.push(done.event_id);
                            outbox_inflight = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            return SimResult::error("outbox worker stopped".into(), ticks_run);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                    if outbox_inflight.is_none()
                        && let Some(event) = waiting_outbox
                            .front()
                            .filter(|event| event.journal_seq <= durable)
                    {
                        match worker.submit.try_send(OutboxWork(event.clone())) {
                            Ok(()) => outbox_inflight = Some(event.event_id),
                            Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                return SimResult::error("outbox worker stopped".into(), ticks_run);
                            }
                        }
                    }
                }
                if let Err(e) = pipe.submit_acknowledge_outbox(acknowledged) {
                    return SimResult::error(format!("outbox acknowledgement: {e}"), ticks_run);
                }

                // A checkpoint's journal cursor is `journalled_through`: the FIFO
                // writer commits every journal record up to that cursor *before*
                // this checkpoint, so a durable checkpoint always has a durable
                // journal prefix behind it. Retain right after, so disk use
                // stays bounded during the run — not only at shutdown.
                if checkpoint_interval > 0 && tick.get().is_multiple_of(checkpoint_interval) {
                    // T23 / G3 row 7 follow-up: a checkpoint is a full-world
                    // snapshot, but it no longer reloads evicted terrain into
                    // the live world to get there — `capture_checkpoint` folds
                    // the durable backing's evicted-brick records straight
                    // into the checkpoint, so residency's bounded memory
                    // holds even at checkpoint time.
                    let captured = match &mut residency {
                        Some(pass) => {
                            pass.capture_checkpoint(&sim, &persist_cfg, journalled_through)
                        }
                        None => persist::capture(&sim, &persist_cfg, journalled_through),
                    };
                    match captured {
                        Ok(cp) => {
                            if let Err(e) = pipe
                                .submit_checkpoint(cp)
                                .and_then(|()| pipe.submit_retain(RETAIN_CHECKPOINTS))
                            {
                                return SimResult::error(format!("persistence: {e}"), ticks_run);
                            }
                        }
                        Err(e) => {
                            return SimResult::error(
                                format!("checkpoint capture failed: {e}"),
                                ticks_run,
                            );
                        }
                    }
                }

                // Drop the in-memory journal suffix the store has acknowledged.
                sim.prune_journal(pipe.durable_seq());

                if let Some(err) = pipe.error() {
                    return SimResult::error(format!("persistence failed: {err}"), ticks_run);
                }
            }
            drop(span_serve_persist_submit);

            // Slice D: post-tick residency pass. Evicts terrain bricks outside
            // every player's interest box, reloads any back in interest. The
            // committed world (hash, conservation, result_hashes) is unchanged;
            // an edit that needs an evicted brick reloads it via the pipeline.
            //
            // ENG-30 row 7 increment 13: before running, hand the pass every
            // brick the pipeline itself reloaded this tick (pinned for a short
            // grace window) and the bounded dependency footprint of every
            // still-queued edit (pinned for as long as it stays queued) — the
            // preflight/consumer-lifetime reservation the frozen contract asks
            // for, on top of the reactive `EvictedGeometryRequired` protection
            // that already existed.
            let span_serve_residency = spall_sim::prof::Span::start("serve.residency");
            if let Some(pass) = &mut residency {
                pass.note_pipeline_reloads(report.reloaded_bricks.iter().copied());
                let pending_edit_bricks = sim.pending_edit_bricks(terrain_vid);
                let player_feet: Vec<(u64, [f64; 3])> = sim
                    .world()
                    .players()
                    .map(|p| (p.entity.get(), p.state.position_m))
                    .collect();
                pass.run(sim.world_mut(), &player_feet, &pending_edit_bricks);
            }
            drop(span_serve_residency);

            // G4 timing ends after all owning-thread work for this tick,
            // including replication, persistence submission, and residency,
            // but before the optional pacing sleep below. Warmup is explicit;
            // a short run therefore remains visibly incomplete instead of
            // turning absent measurements into a pass.
            if let Some(stats) = &mut timing {
                stats.record(ticks_run, started.elapsed(), report.physics_duration);
            }

            // Quiesce only after the pipeline is drained *and* no client has
            // sent anything for `quiescence` ticks — a late scripted action from
            // one client keeps the run alive for the others. A client still
            // mid-baseline (`lj.any_joining()`) also keeps the run alive: under
            // an impaired transport its bulk transfer can legitimately take
            // longer than `quiescence` ticks of otherwise-quiet simulation, and
            // ending the run out from under it would strand the connection
            // before the client ever gets to replicate.
            if sim.is_idle() && !saw_client_work && report.committed.is_empty() && !lj.any_joining()
            {
                idle_streak += 1;
            } else {
                idle_streak = 0;
            }
            if quiescence > 0 && committed_total > 0 && idle_streak >= quiescence {
                // ENG-61: when the scenario needs a detached body to come to
                // rest, keep stepping physics past edit-quiescence until every
                // detached body is asleep *and* has held its position still for
                // a stretch of ticks (still bounded by `max_ticks`), so the run
                // ends with real "came to rest" evidence rather than catching
                // the body mid-fall.
                let settled = sim.world().bodies().all(|b| b.sleeping)
                    && body_stable_ticks >= AWAIT_SETTLE_STABLE_TICKS;
                if !await_body_settle || settled {
                    break;
                }
            }

            // The tick budget is spent: if a regional client is still receiving what the server
            // already owes it, or has just answered with something, serve one more tick.
            if iteration >= tick_budget
                && drain_timeout > Duration::ZERO
                && let Some(draining) = lj.regional_clients_draining(&clients_for_sim)
            {
                let now = std::time::Instant::now();
                let began = *drain_started.get_or_insert(now);
                if draining || saw_client_work {
                    drain_quiet_since = None;
                } else {
                    drain_quiet_since.get_or_insert(now);
                }
                if drain_quiet_since.is_some_and(|since| now.duration_since(since) >= DRAIN_GRACE) {
                    tracing::info!(
                        extra_ticks = iteration - max_ticks,
                        waited_ms = now.duration_since(began).as_millis() as u64,
                        "clients drained; ending the run"
                    );
                } else if now.duration_since(began) >= drain_timeout {
                    drain_timed_out = true;
                    tracing::warn!(
                        extra_ticks = iteration - max_ticks,
                        limit_ms = drain_timeout.as_millis() as u64,
                        "drain limit reached with a client still receiving; ending the run"
                    );
                } else {
                    tick_budget += 1;
                }
            }

            let whole_tick = started.elapsed();
            let after_tick = whole_tick.saturating_sub(before_tick + sim_tick);
            for (slot, d) in [before_tick, sim_tick, after_tick].into_iter().enumerate() {
                phase_totals[slot] += d;
                phase_worst[slot] = phase_worst[slot].max(d);
            }
            // The replication and persistence spans of this tick are recorded
            // after the simulation's were drained; fold them in.
            let after_spans = spall_sim::prof::drain();
            for (name, elapsed) in &after_spans {
                let entry = span_totals.entry(*name).or_default();
                entry.0 += *elapsed;
                entry.1 = entry.1.max(*elapsed);
            }
            last_tick_spans.extend(after_spans);
            // A tick over three frames long is a visible stall for every client:
            // say so on stderr (at most once a second) with where the time went.
            if whole_tick >= Duration::from_millis(50)
                && last_slow_report.is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
            {
                last_slow_report = Some(std::time::Instant::now());
                let mut top: Vec<(&str, f64)> = last_tick_spans
                    .iter()
                    .map(|(name, d)| (*name, d.as_secs_f64() * 1e3))
                    .filter(|(_, ms)| *ms >= 5.0)
                    .collect();
                top.sort_by(|a, b| b.1.total_cmp(&a.1));
                let top: Vec<String> = top
                    .iter()
                    .take(5)
                    .map(|(name, ms)| format!("{name} {ms:.0} ms"))
                    .collect();
                eprintln!(
                    "sandbox-server: slow tick {} | {:.0} ms total = before {:.0} + sim {:.0} + after {:.0} | {}",
                    sim.current_tick().0,
                    whole_tick.as_secs_f64() * 1e3,
                    before_tick.as_secs_f64() * 1e3,
                    sim_tick.as_secs_f64() * 1e3,
                    after_tick.as_secs_f64() * 1e3,
                    top.join(", ")
                );
            }
            if whole_tick >= Duration::from_millis(500) {
                tracing::info!(
                    tick = sim.current_tick().0,
                    before_tick_ms = before_tick.as_secs_f64() * 1e3,
                    sim_tick_ms = sim_tick.as_secs_f64() * 1e3,
                    after_tick_ms = (whole_tick - before_tick - sim_tick).as_secs_f64() * 1e3,
                    "slow server tick"
                );
            }
            if paced {
                // Absolute deadlines, not `tick_dt - work`: a relative sleep
                // drifted the server to ~58.8 Hz under Windows sleep overshoot.
                let now = std::time::Instant::now();
                if let Some(rem) = pacer.finish_tick(now) {
                    std::thread::sleep(rem);
                }
                if pacer.ticks().is_multiple_of(600) {
                    let ms = |d: Duration| d.as_secs_f64() * 1e3;
                    let mut stages: Vec<(&str, f64, f64)> = span_totals
                        .iter()
                        .map(|(name, (total, worst))| (*name, ms(*total) / 600.0, ms(*worst)))
                        .filter(|(_, mean, _)| *mean >= 0.2)
                        .collect();
                    stages.sort_by(|a, b| b.1.total_cmp(&a.1));
                    let stages: Vec<String> = stages
                        .iter()
                        .map(|(name, mean, worst)| format!("{name}={mean:.2}/{worst:.1}ms"))
                        .collect();
                    span_totals.clear();
                    tracing::info!(
                        target: "spall_server::pacing",
                        ?stages,
                        before_mean_ms = ms(phase_totals[0]) / 600.0,
                        sim_mean_ms = ms(phase_totals[1]) / 600.0,
                        after_mean_ms = ms(phase_totals[2]) / 600.0,
                        before_worst_ms = ms(phase_worst[0]),
                        sim_worst_ms = ms(phase_worst[1]),
                        after_worst_ms = ms(phase_worst[2]),
                        ticks = pacer.ticks(),
                        achieved_hz = pacer.achieved_hz(std::time::Instant::now()),
                        max_lag_ms = pacer.max_lag().as_secs_f64() * 1e3,
                        resyncs = pacer.resyncs(),
                        "tick pacing"
                    );
                    phase_totals = [Duration::ZERO; 3];
                    phase_worst = [Duration::ZERO; 3];
                }
            }
        }

        let drain_ticks = iteration.saturating_sub(max_ticks);

        // Diagnostic: `SPALL_DUMP_LOGICAL_BRICKS=<prefix>` writes the authoritative terrain listing.
        if let Ok(prefix) = std::env::var("SPALL_DUMP_LOGICAL_BRICKS") {
            let world = sim.world();
            let id = world.terrain_volume_id();
            let written = world.volume_ref(id).map_or_else(
                || Err(std::io::Error::other("no terrain volume")),
                |volume| {
                    spall_voxel::write_logical_brick_listing(
                        std::path::Path::new(&format!("{prefix}.{}.txt", std::process::id())),
                        volume,
                        world.evicted(id),
                    )
                },
            );
            if let Err(error) = written {
                tracing::error!(%error, "could not write the logical brick listing");
            }
        }

        // Clean-shutdown durability: queue the final journal tail + checkpoint
        // + retain, then block until the off-thread writer has drained and
        // exited (`docs/protocol.md`: "Clean shutdown waits for a final
        // checkpoint/flush"). A failure on that final drain is surfaced through
        // `shutdown_error` with every counter below still reported, so the
        // failing summary keeps its diagnostics instead of returning early —
        // silently reporting success would strand an unsavable world.
        let mut persist_bytes_per_write = 0.0;
        let mut persist_commit_bytes_per_sec = 0.0;
        let mut shutdown_error: Option<String> = None;
        if let Some(pipe) = pipeline.take() {
            let final_tick = sim.current_tick().get();
            let tail = tick_journal_batch(&mut sim, &mut journalled_through, None, final_tick)
                .unwrap_or_default();
            let tail_events = std::mem::take(&mut pending_outbox);
            if let Err(error) = pipe.submit_journal_with_outbox(tail, tail_events.clone()) {
                shutdown_error = Some(format!("final journal/outbox submission failed: {error}"));
            }
            waiting_outbox.extend(tail_events);
            if shutdown_error.is_none() {
                if let Err(error) = pipe.wait_durable_through(journalled_through) {
                    shutdown_error = Some(format!("final journal durability wait failed: {error}"));
                } else {
                    let mut acknowledged = Vec::new();
                    if let Some(worker) = outbox_worker.as_ref() {
                        while let Some(event) = waiting_outbox.front() {
                            if outbox_inflight.is_none() {
                                if let Err(error) = worker.submit.send(OutboxWork(event.clone())) {
                                    shutdown_error = Some(format!(
                                        "final harvest outbox submission failed: {error}"
                                    ));
                                    break;
                                }
                                outbox_inflight = Some(event.event_id);
                            }
                            let done = match worker.completed.recv() {
                                Ok(done) => done,
                                Err(error) => {
                                    shutdown_error = Some(format!(
                                        "final harvest outbox completion failed: {error}"
                                    ));
                                    break;
                                }
                            };
                            if outbox_inflight != Some(done.event_id)
                                || event.event_id != done.event_id
                            {
                                shutdown_error =
                                    Some("final harvest outbox completion mismatch".into());
                                break;
                            }
                            if let Err(error) = done.result {
                                shutdown_error =
                                    Some(format!("final harvest outbox delivery failed: {error}"));
                                break;
                            }
                            waiting_outbox.pop_front();
                            outbox_inflight = None;
                            acknowledged.push(done.event_id);
                        }
                    }
                    if shutdown_error.is_none()
                        && let Err(error) = pipe.submit_acknowledge_outbox(acknowledged)
                    {
                        shutdown_error =
                            Some(format!("final outbox acknowledgement failed: {error}"));
                    }
                }
            }
            // T23 / G3 row 7 follow-up: the shutdown snapshot folds evicted
            // terrain in from the durable backing the same way the periodic
            // checkpoints above do now, instead of reloading it into the live
            // world first.
            let captured = match &mut residency {
                Some(pass) => pass.capture_checkpoint(&sim, &persist_cfg, journalled_through),
                None => persist::capture(&sim, &persist_cfg, journalled_through),
            };
            if let Ok(cp) = captured {
                let _ = pipe
                    .submit_checkpoint(cp)
                    .and_then(|()| pipe.submit_retain(RETAIN_CHECKPOINTS));
            }
            let outcome = pipe.shutdown();
            journal_records_written = outcome.status.journal_records;
            checkpoints_published += outcome.status.checkpoints_published;
            persist_bytes_per_write = outcome.metrics.bytes_per_journal_write();
            persist_commit_bytes_per_sec = outcome.metrics.commit_bytes_per_sec();
            // The final drain covers the journal tail, the final checkpoint, and
            // the retain pass; attribute any failure to that shutdown flush so a
            // stranded save is never reported as a pass.
            shutdown_error = shutdown_error.or_else(|| {
                outcome
                    .status
                    .error
                    .map(|e| format!("final checkpoint/flush failed: {e}"))
            });
            tracing::info!(
                durable_seq = outcome.status.durable_seq,
                journal_records = outcome.status.journal_records,
                checkpoints = outcome.status.checkpoints_published,
                max_queue_depth = outcome.status.max_queue_depth,
                last_flush_us = outcome.status.last_journal_flush.as_micros() as u64,
                max_flush_us = outcome.status.max_journal_flush.as_micros() as u64,
                max_checkpoint_us = outcome.status.max_checkpoint.as_micros() as u64,
                "persistence pipeline drained"
            );
        }

        SimResult {
            ok: shutdown_error.is_none(),
            error: shutdown_error,
            ticks_run,
            committed_total,
            rejected_total,
            intent_stats,
            final_world_hash: sim.world().geometry_topology_hash().to_string(),
            total_solid_cells: sim.world().total_solid_cells(),
            body_count: sim.world().body_count(),
            max_detached_body_brick_span: sim
                .world()
                .bodies()
                .map(|b| b.volume.resident_brick_coords().len() as u64)
                .max()
                .unwrap_or(0),
            detached_body_max_final_speed_m_s: sim
                .world()
                .bodies()
                .map(|b| {
                    (b.linvel_m_s[0].powi(2) + b.linvel_m_s[1].powi(2) + b.linvel_m_s[2].powi(2))
                        .sqrt()
                })
                .fold(0.0_f64, f64::max),
            detached_bodies_all_asleep: sim.world().bodies().all(|b| b.sleeping),
            detached_body_stable_ticks: body_stable_ticks,
            max_contact_penetration_m: f64::from(sim.world().physics().max_penetration_m()),
            detached_body_min_origin_y_m: {
                let min_y = sim
                    .world()
                    .bodies()
                    .map(|b| b.pose.translation_m[1])
                    .fold(f64::INFINITY, f64::min);
                if min_y.is_finite() { min_y } else { 0.0 }
            },
            checkpoints_published,
            journal_records_written,
            persist_bytes_per_write,
            persist_commit_bytes_per_sec,
            late_joins_completed: lj.completed,
            late_join_retries: lj.retries,
            late_joins_failed: lj.failed,
            expired_actions_rejected: lj.expired_actions,
            baseline_bytes_sent: lj.baseline_bytes,
            repairs_deferred_queue_full: lj.repairs_deferred,
            actions_throttled,
            repairs_throttled,
            water_keyframes_sent,
            water_bytes_queued,
            world_resets,
            actions_discarded_by_reset,
            drain_ticks,
            drain_timed_out,
            motion_snapshots_sent: motion_egress.snapshots_sent,
            motion_snapshots_interest_culled: motion_egress.interest_culled,
            motion_snapshots_budget_deferred: motion_egress.budget_deferred,
            max_client_motion_batch_bytes: motion_egress.max_client_batch_bytes,
            actions_requested,
            actions_staged,
            actions_queued_unresolved: submitted_at.len() as u64,
            latency: commit_latency.report(),
            residency: residency.as_ref().map(|p| p.stats_with_world(sim.world())),
            residency_backing_disk_bytes: residency.as_ref().and_then(|p| p.backing_disk_bytes()),
            client_spawns,
            contact_damage_cuts_submitted,
            contact_damage_cuts_rejected,
            dormancy_deactivations_total,
            dormancy_reactivations_total,
            // An opt-in timing window leaves no samples when it is not
            // configured; expose the same zeroed report as an ordinary run.
            timing: timing
                .map(|stats| stats.finish(ticks_run))
                .unwrap_or_default(),
        }
    });

    let mut sim_result = sim_join
        .await
        .map_err(|e| ServeError::Runtime(format!("sim thread panicked: {e}")))?;

    // Stop admission first. Keep readers and heartbeats alive while writers
    // drain every accepted reliable record, including their owned bulk stream.
    let _ = accept_stop_tx.send(true);
    let _ = accept.await;
    broadcast(
        &clients,
        Outbound::Shutdown(Connection::BYE_REASON_COMPLETE),
    );
    let drain = async {
        loop {
            if conns.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    if tokio::time::timeout(config.server.transport.idle_timeout, drain)
        .await
        .is_err()
    {
        sim_result.ok = false;
        sim_result.error =
            Some("accepted reliable traffic did not drain within the shutdown bound".into());
    }
    // The existing transport idle bound also bounds a stalled shutdown drain.
    let _ = stop_tx.send(true);
    server.close();
    let _ = tokio::time::timeout(Duration::from_secs(3), server.wait_idle()).await;

    // T20: total egress. Hold the `conns` lock across the whole read so a
    // late-closing `serve_conn` can neither remove-and-accumulate an entry
    // concurrently (it takes the same lock first) nor be missed — every
    // connection is counted once, here or in `egress_closed`.
    let (app_egress_bytes, transport_egress_bytes, per_client_egress) = {
        let conns_guard = conns.lock().unwrap_or_else(|e| e.into_inner());
        let mut acc = egress_closed.lock().unwrap_or_else(|e| e.into_inner());
        for (raw, conn) in conns_guard.iter() {
            acc.insert(
                *raw,
                (
                    conn.stats().app_bytes_sent,
                    conn.transport_stats().udp_tx.bytes,
                ),
            );
        }
        let mut app_total = 0u64;
        let mut transport_total = 0u64;
        let mut rows: Vec<PerClientEgress> = Vec::with_capacity(acc.len());
        for (&raw, &(app_bytes, transport_bytes)) in acc.iter() {
            app_total = app_total.saturating_add(app_bytes);
            transport_total = transport_total.saturating_add(transport_bytes);
            // `SessionId`'s high 32 bits are the slot (see spall_protocol::session).
            let slot = (raw >> 32) as u32;
            rows.push(PerClientEgress {
                slot,
                spawn_m: sim_result.client_spawns.get(&slot).copied(),
                app_bytes,
                transport_bytes,
            });
        }
        rows.sort_by_key(|r| r.slot);
        (app_total, transport_total, rows)
    };

    let clients_connected = *count_rx.borrow();
    let result = if sim_result.ok { "passed" } else { "failed" };
    log.write(&ProcessRecord::new(
        if sim_result.ok {
            ProcessEvent::Stopped
        } else {
            ProcessEvent::Failed
        },
        ProcessRole::Server,
        Some(format!(
            "result={result}, ticks={}, committed={}, hash={}",
            sim_result.ticks_run, sim_result.committed_total, sim_result.final_world_hash
        )),
    ))?;

    let summary = ServeSummary {
        // v5: T21 / ENG-28 increment 4 adds contact_damage_cuts_submitted /
        // contact_damage_cuts_rejected / dormancy_deactivations_total /
        // dormancy_reactivations_total; T23 / G3 row 14 adds per_client_egress.
        // v6: T23 / G3 row 7 item 3 adds resident_dense_bytes_min/max/final and
        // residency_backing_disk_bytes.
        // v7: ENG-30 row 7 increment 13 adds residency_pinned_bricks_max,
        // residency_admission_deferred_total, residency_required_over_budget_ticks,
        // residency_digest_bytes_final, residency_backing_resident_bytes, and
        // process_peak_memory_bytes.
        // v8: ENG-30 row 7 increment 15 adds
        // residency_checkpoint_bricks_captured_total and
        // residency_checkpoint_bricks_logical_total (incremental checkpoint
        // capture evidence).
        // v9: T23 / G4 bounded owning-tick and physics timing telemetry.
        // v10: ENG-109 authoritative intent admission and pipeline pressure.
        version: 10,
        result: result.to_string(),
        scene: format!("{scene:?}"),
        bound_addr: bound.to_string(),
        ticks_run: sim_result.ticks_run,
        clients_connected,
        transactions_committed: sim_result.committed_total,
        actions_rejected: sim_result.rejected_total,
        intent_stats: sim_result.intent_stats.clone(),
        final_world_hash: sim_result.final_world_hash.clone(),
        total_solid_cells: sim_result.total_solid_cells,
        body_count: sim_result.body_count,
        max_detached_body_brick_span: sim_result.max_detached_body_brick_span,
        detached_body_max_final_speed_m_s: sim_result.detached_body_max_final_speed_m_s,
        detached_bodies_all_asleep: sim_result.detached_bodies_all_asleep,
        detached_body_stable_ticks: sim_result.detached_body_stable_ticks,
        max_contact_penetration_m: sim_result.max_contact_penetration_m,
        detached_body_min_origin_y_m: sim_result.detached_body_min_origin_y_m,
        checkpoints_published: sim_result.checkpoints_published,
        journal_records_written: sim_result.journal_records_written,
        persist_bytes_per_write: sim_result.persist_bytes_per_write,
        persist_commit_bytes_per_sec: sim_result.persist_commit_bytes_per_sec,
        late_joins_completed: sim_result.late_joins_completed,
        late_join_retries: sim_result.late_join_retries,
        late_joins_failed: sim_result.late_joins_failed,
        expired_actions_rejected: sim_result.expired_actions_rejected,
        baseline_bytes_sent: sim_result.baseline_bytes_sent,
        repairs_deferred_queue_full: sim_result.repairs_deferred_queue_full,
        inbound_actions_throttled: sim_result.actions_throttled,
        inbound_repairs_throttled: sim_result.repairs_throttled,
        inbound_dropped_actions: inbound_drops
            .actions
            .load(std::sync::atomic::Ordering::Relaxed),
        inbound_dropped_repairs: inbound_drops
            .repairs
            .load(std::sync::atomic::Ordering::Relaxed),
        inbound_dropped_progression: inbound_drops
            .progression
            .load(std::sync::atomic::Ordering::Relaxed),
        inbound_dropped_admin: inbound_drops
            .admin
            .load(std::sync::atomic::Ordering::Relaxed),
        water_keyframes_sent: sim_result.water_keyframes_sent,
        water_bytes_queued: sim_result.water_bytes_queued,
        world_resets: sim_result.world_resets,
        actions_discarded_by_reset: sim_result.actions_discarded_by_reset,
        drain_ticks: sim_result.drain_ticks,
        drain_timed_out: sim_result.drain_timed_out,
        motion_snapshots_sent: sim_result.motion_snapshots_sent,
        motion_snapshots_interest_culled: sim_result.motion_snapshots_interest_culled,
        motion_snapshots_budget_deferred: sim_result.motion_snapshots_budget_deferred,
        max_client_motion_batch_bytes: sim_result.max_client_motion_batch_bytes,
        app_egress_bytes,
        transport_egress_bytes,
        actions_requested: sim_result.actions_requested,
        actions_staged: sim_result.actions_staged,
        actions_queued_unresolved: sim_result.actions_queued_unresolved,
        single_brick_commit_p95_ms: sim_result.latency.single_brick_commit_p95_ms,
        single_brick_commit_samples: sim_result.latency.single_brick_commit_samples,
        structure_split_p95_ms: sim_result.latency.structure_split_p95_ms,
        structure_split_samples: sim_result.latency.structure_split_samples,
        large_collapse_p95_ms: sim_result.latency.large_collapse_p95_ms,
        large_collapse_samples: sim_result.latency.large_collapse_samples,
        residency_evictions_total: sim_result.residency.map(|r| r.evictions_total).unwrap_or(0),
        residency_reloads_total: sim_result.residency.map(|r| r.reloads_total).unwrap_or(0),
        resident_terrain_bricks_min: sim_result
            .residency
            .map(|r| r.resident_terrain_bricks_min as u64)
            .unwrap_or(0),
        resident_terrain_bricks_max: sim_result
            .residency
            .map(|r| r.resident_terrain_bricks_max as u64)
            .unwrap_or(0),
        resident_terrain_bricks_final: sim_result
            .residency
            .map(|r| r.resident_terrain_bricks_final as u64)
            .unwrap_or(0),
        residency_budget_miss_ticks: sim_result
            .residency
            .map(|r| r.budget_miss_ticks)
            .unwrap_or(0),
        resident_dense_bytes_min: sim_result
            .residency
            .map(|r| r.resident_dense_bytes_min)
            .unwrap_or(0),
        resident_dense_bytes_max: sim_result
            .residency
            .map(|r| r.resident_dense_bytes_max)
            .unwrap_or(0),
        resident_dense_bytes_final: sim_result
            .residency
            .map(|r| r.resident_dense_bytes_final)
            .unwrap_or(0),
        residency_backing_disk_bytes: sim_result.residency_backing_disk_bytes,
        per_client_egress,
        contact_damage_cuts_submitted: sim_result.contact_damage_cuts_submitted,
        contact_damage_cuts_rejected: sim_result.contact_damage_cuts_rejected,
        dormancy_deactivations_total: sim_result.dormancy_deactivations_total,
        dormancy_reactivations_total: sim_result.dormancy_reactivations_total,
        residency_pinned_bricks_max: sim_result
            .residency
            .map(|r| r.pinned_bricks_max as u64)
            .unwrap_or(0),
        residency_admission_deferred_total: sim_result
            .residency
            .map(|r| r.admission_deferred_total)
            .unwrap_or(0),
        residency_required_over_budget_ticks: sim_result
            .residency
            .map(|r| r.required_over_budget_ticks)
            .unwrap_or(0),
        residency_digest_bytes_final: sim_result
            .residency
            .map(|r| r.digest_bytes_final)
            .unwrap_or(0),
        residency_backing_resident_bytes: sim_result.residency.and_then(|r| r.backing_bytes_final),
        process_peak_memory_bytes: crate::mem_stats::process_peak_bytes(),
        residency_checkpoint_bricks_captured_total: sim_result
            .residency
            .map(|r| r.checkpoint_bricks_captured_total)
            .unwrap_or(0),
        residency_checkpoint_bricks_logical_total: sim_result
            .residency
            .map(|r| r.checkpoint_bricks_logical_total)
            .unwrap_or(0),
        tick_busy_p95_ms: sim_result.timing.tick_busy_p95_ms,
        tick_busy_p99_ms: sim_result.timing.tick_busy_p99_ms,
        tick_busy_max_ms: sim_result.timing.tick_busy_max_ms,
        tick_busy_samples: sim_result.timing.tick_busy_samples,
        tick_busy_window_complete: sim_result.timing.window_complete,
        physics_p95_ms: sim_result.timing.physics_p95_ms,
        physics_p99_ms: sim_result.timing.physics_p99_ms,
        physics_max_ms: sim_result.timing.physics_max_ms,
        physics_samples: sim_result.timing.physics_samples,
        physics_window_complete: sim_result.timing.window_complete,
    };
    if let Some(path) = &config.harness.summary_json {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&summary).expect("serializable"),
        )?;
    }
    if let Some(err) = sim_result.error {
        return Err(ServeError::Runtime(err));
    }
    Ok(summary)
}

pub(super) struct SimResult {
    pub(super) ok: bool,
    pub(super) error: Option<String>,
    pub(super) ticks_run: u64,
    pub(super) committed_total: u64,
    pub(super) rejected_total: u64,
    pub(super) intent_stats: IntentStats,
    pub(super) final_world_hash: String,
    pub(super) total_solid_cells: u64,
    pub(super) body_count: usize,
    pub(super) max_detached_body_brick_span: u64,
    pub(super) detached_body_max_final_speed_m_s: f64,
    pub(super) detached_bodies_all_asleep: bool,
    pub(super) detached_body_stable_ticks: u64,
    pub(super) max_contact_penetration_m: f64,
    pub(super) detached_body_min_origin_y_m: f64,
    pub(super) checkpoints_published: u64,
    pub(super) journal_records_written: u64,
    pub(super) persist_bytes_per_write: f64,
    pub(super) persist_commit_bytes_per_sec: f64,
    pub(super) late_joins_completed: u64,
    pub(super) late_join_retries: u64,
    pub(super) late_joins_failed: u64,
    pub(super) expired_actions_rejected: u64,
    pub(super) baseline_bytes_sent: u64,
    pub(super) repairs_deferred_queue_full: u64,
    pub(super) actions_throttled: u64,
    pub(super) repairs_throttled: u64,
    pub(super) water_keyframes_sent: u64,
    pub(super) water_bytes_queued: u64,
    pub(super) world_resets: u64,
    pub(super) actions_discarded_by_reset: u64,
    pub(super) drain_ticks: u64,
    pub(super) drain_timed_out: bool,
    pub(super) motion_snapshots_sent: u64,
    pub(super) motion_snapshots_interest_culled: u64,
    pub(super) motion_snapshots_budget_deferred: u64,
    pub(super) max_client_motion_batch_bytes: u64,
    // T11a / ENG-62.
    pub(super) actions_requested: u64,
    pub(super) actions_staged: u64,
    pub(super) actions_queued_unresolved: u64,
    pub(super) latency: commit_latency::LatencyReport,
    pub(super) residency: Option<crate::ResidencyStats>,
    /// T23 / G3 row 7 item 3: the residency backing's on-disk footprint, when
    /// it is a real disk-backed store (`crate::disk_backing::DiskBrickBacking`).
    /// `None` for the in-process `MemoryBacking` default and when residency
    /// is off.
    pub(super) residency_backing_disk_bytes: Option<u64>,
    /// T23 / G3 row 14: each slot's player spawn, keyed by slot so the outer
    /// per-connection egress report (built after this blocking task returns)
    /// can be read alongside where that connection's interest anchor was.
    pub(super) client_spawns: HashMap<u32, [f64; 3]>,
    pub(super) contact_damage_cuts_submitted: u64,
    pub(super) contact_damage_cuts_rejected: u64,
    pub(super) dormancy_deactivations_total: u64,
    pub(super) dormancy_reactivations_total: u64,
    pub(super) timing: TimingReport,
}

impl SimResult {
    pub(super) fn error(msg: String, ticks_run: u64) -> Self {
        Self {
            ok: false,
            error: Some(msg),
            ticks_run,
            committed_total: 0,
            rejected_total: 0,
            intent_stats: IntentStats::default(),
            final_world_hash: String::new(),
            total_solid_cells: 0,
            body_count: 0,
            max_detached_body_brick_span: 0,
            detached_body_max_final_speed_m_s: 0.0,
            detached_bodies_all_asleep: true,
            detached_body_stable_ticks: 0,
            max_contact_penetration_m: 0.0,
            detached_body_min_origin_y_m: 0.0,
            checkpoints_published: 0,
            journal_records_written: 0,
            persist_bytes_per_write: 0.0,
            persist_commit_bytes_per_sec: 0.0,
            late_joins_completed: 0,
            late_join_retries: 0,
            late_joins_failed: 0,
            expired_actions_rejected: 0,
            baseline_bytes_sent: 0,
            repairs_deferred_queue_full: 0,
            actions_throttled: 0,
            repairs_throttled: 0,
            water_keyframes_sent: 0,
            water_bytes_queued: 0,
            world_resets: 0,
            actions_discarded_by_reset: 0,
            drain_ticks: 0,
            drain_timed_out: false,
            motion_snapshots_sent: 0,
            motion_snapshots_interest_culled: 0,
            motion_snapshots_budget_deferred: 0,
            max_client_motion_batch_bytes: 0,
            actions_requested: 0,
            actions_staged: 0,
            actions_queued_unresolved: 0,
            latency: commit_latency::LatencyReport::default(),
            residency: None,
            residency_backing_disk_bytes: None,
            client_spawns: HashMap::new(),
            contact_damage_cuts_submitted: 0,
            contact_damage_cuts_rejected: 0,
            dormancy_deactivations_total: 0,
            dormancy_reactivations_total: 0,
            timing: TimingReport::default(),
        }
    }
}

// --- T17 late-join / catch-up / session renewal ---------------------------

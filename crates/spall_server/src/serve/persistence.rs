use super::*;

/// Complete checkpoints kept on disk at each retention pass. Older ones — and
/// the journal rows they alone covered — are pruned (`docs/protocol.md`: "Cap
/// retention; lagging joins get a fresh baseline").
pub(super) const RETAIN_CHECKPOINTS: usize = 2;

/// What [`setup_persistence`] hands back: the simulation, the bounded async
/// persistence pipeline (`None` when `--save` is unset), the highest journal
/// sequence already durable at startup, and how many checkpoints were published
/// synchronously during setup.
pub(super) struct Persistence {
    pub(super) sim: Simulation,
    pub(super) pipeline: Option<PersistPipeline>,
    pub(super) journalled_through: u64,
    pub(super) checkpoints_published: u64,
    pub(super) retained_transfer_ids: Vec<TransferId>,
    pub(super) outbox_processor: Option<OutboxProcessor>,
}

/// Opens the world database, recovering from it when it already holds a
/// checkpoint and otherwise starting the built-in `scene` and publishing an
/// initial checkpoint. Any test-only `save_faults` are armed on the recovered
/// [`Writer`] *after* recovery and that initial checkpoint, then the writer is
/// handed to a [`PersistPipeline`] so every later durable write happens off the
/// simulation thread (ENG-50). `None` save path → no persistence.
///
/// This fails closed on corruption: only a genuinely empty database
/// ([`spall_store::StoreError::NoCheckpoint`]) is initialised with the built-in
/// scene. A recovery that reports corruption, or a database whose checkpoints
/// exist but do not decode, aborts startup without touching the file — losing
/// durable records requires an explicit operator choice
/// ([`persist::RecoveryChoice`]), not an automatic resume or reinitialisation.
#[cfg(test)]
pub(super) fn setup_persistence(
    save: Option<&std::path::Path>,
    scene: Scene,
    cfg: &PersistConfig,
    save_faults: Option<spall_store::FaultPlan>,
) -> Result<Persistence, String> {
    setup_persistence_with_terrain_collider_mode(
        save,
        scene,
        cfg,
        save_faults,
        TerrainColliderMode::PerBrick,
    )
}

#[cfg(test)]
pub(super) fn setup_persistence_with_terrain_collider_mode(
    save: Option<&std::path::Path>,
    scene: Scene,
    cfg: &PersistConfig,
    save_faults: Option<spall_store::FaultPlan>,
    terrain_collider_mode: TerrainColliderMode,
) -> Result<Persistence, String> {
    setup_persistence_with_game_content(
        save,
        scene,
        None,
        cfg,
        save_faults,
        terrain_collider_mode,
        spall_sim::fixtures::stone_manifest(),
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn setup_persistence_with_game_content(
    save: Option<&std::path::Path>,
    scene: Scene,
    custom_world: Option<&CustomWorld>,
    cfg: &PersistConfig,
    save_faults: Option<spall_store::FaultPlan>,
    terrain_collider_mode: TerrainColliderMode,
    materials: spall_core::MaterialManifest,
    mut game_setup: Option<InitialGameWorldSetup>,
    mut outbox_processor: Option<OutboxProcessor>,
) -> Result<Persistence, String> {
    let Some(path) = save else {
        let mut sim =
            scene.simulation_with_materials(terrain_collider_mode, materials, custom_world);
        if let Some(setup) = game_setup.take() {
            setup(&mut sim)?;
        }
        sim.set_weather_seed(cfg.seed);
        return Ok(Persistence {
            sim,
            pipeline: None,
            journalled_through: 0,
            checkpoints_published: 0,
            retained_transfer_ids: Vec::new(),
            outbox_processor,
        });
    };
    let mut writer = Writer::open(path).map_err(|e| e.to_string())?;
    let (sim, journalled_through, checkpoints_published, retained_transfer_ids) =
        match writer.recover() {
            Ok(recovery) => {
                let retained_transfer_ids =
                    persist::retained_transfer_ids(&recovery).map_err(|e| e.to_string())?;
                // Validate every recovered composed ID before rebuilding any state
                // or publishing a later checkpoint. Reuse the runtime allocator's
                // namespace and range checks rather than duplicating its bit rules.
                TransferIdAllocator::seeded_from_retained(retained_transfer_ids.iter().copied())
                    .map_err(|e| format!("recovered transfer ID is invalid: {e}"))?;
                let (mut sim, seq) = persist::restore_with_terrain_collider_mode(
                    &recovery,
                    cfg,
                    persist::RecoveryChoice::RequireClean,
                    materials.clone(),
                    AnchorPlane::at(0),
                    PhysicsConfig::default(),
                    terrain_collider_mode,
                )
                .map_err(|e| e.to_string())?;
                // A save carries the water, not the solver's tuning: a recovered
                // world takes its scene's (a generated world's sleep flow and far
                // throttle). Restoring resets the water to rest, as recovery did.
                let tuning = custom_world
                    .and_then(|c| c.column_water_setup())
                    .map(|setup| setup.config);
                let state = sim.column_water().map(|w| w.canonical_state());
                if let (Some(config), Some(state)) = (tuning, state) {
                    sim.restore_column_water(&state, config)
                        .map_err(|e| format!("column water retune after recovery: {e}"))?;
                }
                sim.set_column_water_worker(true);
                (sim, seq, 0, retained_transfer_ids)
            }
            // A genuinely new/empty database: seed it with the built-in scene.
            Err(spall_store::StoreError::NoCheckpoint) => {
                let mut sim =
                    scene.simulation_with_materials(terrain_collider_mode, materials, custom_world);
                if let Some(setup) = game_setup.take() {
                    setup(&mut sim)?;
                }
                sim.set_weather_seed(cfg.seed);
                let checkpoint = persist::capture(&sim, cfg, 0).map_err(|e| e.to_string())?;
                writer
                    .publish_checkpoint(&checkpoint)
                    .map_err(|e| e.to_string())?;
                (sim, 0, 1, Vec::new())
            }
            // Checkpoints exist but none decoded — corruption, not an empty DB. Do
            // not overwrite the save with a fresh scene.
            Err(e @ spall_store::StoreError::CheckpointsUnrecoverable(_)) => {
                return Err(format!(
                    "world database at {} is unrecoverable and must not be overwritten: {e}; \
                 an operator must supply a verified recovery source",
                    path.display()
                ));
            }
            Err(e) => return Err(e.to_string()),
        };
    // Test-only: arm the writer only now, so recovery and the first checkpoint
    // are unaffected and the fault falls on a later durable write.
    if let Some(faults) = save_faults {
        writer.set_faults(faults);
    }
    if let Some(processor) = outbox_processor.as_mut() {
        loop {
            let events = writer.pending_outbox(256).map_err(|e| e.to_string())?;
            if events.is_empty() {
                break;
            }
            for event in &events {
                processor(event)?;
            }
            let ids: Vec<_> = events.iter().map(|event| event.event_id).collect();
            writer.acknowledge_outbox(&ids).map_err(|e| e.to_string())?;
        }
    }
    let pipeline = PersistPipeline::spawn_with_durable_seq(
        writer,
        PipelineConfig::default(),
        journalled_through,
    );
    Ok(Persistence {
        sim,
        pipeline: Some(pipeline),
        journalled_through,
        checkpoints_published,
        retained_transfer_ids,
        outbox_processor,
    })
}

/// Builds the contiguous journal batch owed for one tick: every committed
/// topology entry past `journalled_through`, followed by this tick's 20 Hz pose
/// batch (if any). The pose sequence is reserved from the simulation so it is
/// contiguous with the topology sequences (`docs/protocol.md`: "Journal
/// periodic body pose batches at 20 Hz"; ENG-50: "contiguous sequence
/// ownership"). Advances `journalled_through` to the last sequence in the
/// batch.
pub(super) fn set_weather_admin(
    sim: &mut Simulation,
    enabled: bool,
    request_id: RequestId,
    kind: Option<spall_protocol::WeatherKind>,
    intensity: Option<spall_protocol::SnowTier>,
) -> spall_protocol::AdminStatus {
    if !enabled {
        return spall_protocol::AdminStatus {
            request_id,
            accepted: false,
            message: "admin commands are disabled on this server".into(),
        };
    }
    if intensity.is_some() && kind != Some(spall_protocol::WeatherKind::Snow) {
        return spall_protocol::AdminStatus {
            request_id,
            accepted: false,
            message: "intensity tiers apply to snow only".into(),
        };
    }
    sim.set_weather_override(kind, intensity);
    let weather = sim.weather_state();
    let mode = kind.map_or("automatic", |k| match k {
        spall_protocol::WeatherKind::Clear => "clear",
        spall_protocol::WeatherKind::Rain => "rain",
        spall_protocol::WeatherKind::Storm => "storm",
        spall_protocol::WeatherKind::Snow => "snow",
        spall_protocol::WeatherKind::HighWind => "high wind",
        spall_protocol::WeatherKind::Blizzard => "blizzard",
    });
    spall_protocol::AdminStatus {
        request_id,
        accepted: true,
        message: format!(
            "weather {mode} (precipitation q16 {}, gust q16 {})",
            weather.precipitation_q16, weather.gust_q16
        ),
    }
}

/// Handles `SetWeather` and `SetSeason`.
pub(super) fn weather_admin(
    sim: &mut Simulation,
    enabled: bool,
    request_id: RequestId,
    command: spall_protocol::AdminCommand,
) -> spall_protocol::AdminStatus {
    match command {
        spall_protocol::AdminCommand::SetWeather { kind, intensity } => {
            set_weather_admin(sim, enabled, request_id, kind, intensity)
        }
        spall_protocol::AdminCommand::SetSeason { season } => {
            set_season_admin(sim, enabled, request_id, season)
        }
        _ => admin_refused(request_id, "not a weather command"),
    }
}

/// A refused `AdminStatus`.
pub(super) fn admin_refused(request_id: RequestId, message: &str) -> spall_protocol::AdminStatus {
    spall_protocol::AdminStatus {
        request_id,
        accepted: false,
        message: message.into(),
    }
}

/// `SetWaterSpring`: sets the custom world's gated spring rate.
pub(super) fn water_spring_admin(
    sim: &mut Simulation,
    admin_commands: bool,
    custom_world: Option<&CustomWorld>,
    request_id: RequestId,
    rate: u8,
) -> spall_protocol::AdminStatus {
    let has_gated_spring = custom_world.is_some_and(|c| {
        c.column_water_setup()
            .is_some_and(|w| w.gated_sources.iter().any(|t| !t.is_empty()))
    });
    if !admin_commands {
        spall_protocol::AdminStatus {
            request_id,
            accepted: false,
            message: "admin commands are disabled on this server".into(),
        }
    } else if !has_gated_spring {
        spall_protocol::AdminStatus {
            request_id,
            accepted: false,
            message: "this scene authored no gated water spring".into(),
        }
    } else if rate > spall_sim::column_water::MAX_GATED_SOURCE_RATE {
        spall_protocol::AdminStatus {
            request_id,
            accepted: false,
            message: format!(
                "water spring rate must be 0..={}",
                spall_sim::column_water::MAX_GATED_SOURCE_RATE
            ),
        }
    } else {
        sim.set_water_spring_rate(rate);
        spall_protocol::AdminStatus {
            request_id,
            accepted: true,
            message: if rate == 0 {
                "water spring off".to_string()
            } else {
                format!("water spring rate {rate}")
            },
        }
    }
}

/// `TeleportSelf`: moves the sender's own player.
pub(super) fn teleport_admin(
    sim: &mut Simulation,
    admin_commands: bool,
    session: SessionId,
    request_id: RequestId,
    feet_mm: [i64; 3],
) -> spall_protocol::AdminStatus {
    let feet = feet_mm.map(|mm| mm as f64 / 1000.0);
    let (accepted, message) = if !admin_commands {
        (false, "admin commands are disabled on this server")
    } else if sim.teleport_player(session_player_entity(session), feet) {
        (true, "teleported")
    } else {
        (false, "no player to teleport")
    };
    spall_protocol::AdminStatus {
        request_id,
        accepted,
        message: message.into(),
    }
}

/// `SetSeason`: re-anchors the shared vegetation and weather clock.
fn set_season_admin(
    sim: &mut Simulation,
    enabled: bool,
    request_id: RequestId,
    season: u8,
) -> spall_protocol::AdminStatus {
    let (accepted, message) = if !enabled {
        (
            false,
            "admin commands are disabled on this server".to_string(),
        )
    } else {
        match sim.set_season(season) {
            Ok(()) => (
                true,
                format!(
                    "season {}",
                    ["spring", "summer", "autumn", "winter"][usize::from(season)]
                ),
            ),
            Err(e) => (false, e),
        }
    };
    spall_protocol::AdminStatus {
        request_id,
        accepted,
        message,
    }
}

pub(super) fn tick_journal_batch(
    sim: &mut Simulation,
    journalled_through: &mut u64,
    pose_batch: Option<&[MotionSnapshot]>,
    tick: u64,
) -> Result<Vec<spall_store::JournalRecord>, String> {
    let mut batch = persist::journal_records(sim.journal().entries_after(*journalled_through))
        .map_err(|e| e.to_string())?;
    if let Some(snaps) = pose_batch.filter(|s| !s.is_empty()) {
        let seq = sim.reserve_journal_seq().map_err(|e| e.to_string())?;
        batch.push(persist::pose_batch_record(seq.0, tick, snaps).map_err(|e| e.to_string())?);
    }
    if let Some(last) = batch.last() {
        *journalled_through = last.seq;
    }
    if let Some(delta) = sim.take_column_water_journal() {
        let seq = sim.reserve_journal_seq().map_err(|e| e.to_string())?;
        batch.push(spall_store::JournalRecord {
            seq: seq.0,
            tick,
            payload: spall_store::JournalPayload::ColumnWaterDelta(delta),
        });
        *journalled_through = seq.0;
    }
    if let Some(state) = sim.take_weather_journal() {
        let seq = sim.reserve_journal_seq().map_err(|e| e.to_string())?;
        batch.push(spall_store::JournalRecord {
            seq: seq.0,
            tick,
            payload: spall_store::JournalPayload::WeatherState(state),
        });
        *journalled_through = seq.0;
    }
    for snow_tile in sim.take_snow_journal_tiles().map_err(|e| e.to_string())? {
        let seq = sim.reserve_journal_seq().map_err(|e| e.to_string())?;
        batch.push(spall_store::JournalRecord {
            seq: seq.0,
            tick,
            payload: spall_store::JournalPayload::SnowTileState {
                world_epoch: sim.weather_state().world_epoch,
                tile: snow_tile.tile,
                state: snow_tile.state,
            },
        });
        *journalled_through = seq.0;
    }
    if let Some(bytes) = sim.take_vegetation_journal()? {
        let seq = sim.reserve_journal_seq().map_err(|e| e.to_string())?;
        batch.push(spall_store::JournalRecord {
            seq: seq.0,
            tick,
            payload: spall_store::JournalPayload::VegetationState(bytes),
        });
        *journalled_through = seq.0;
    }
    if let Some(state) = sim.vegetation_state() {
        let (time_ms, credit_ms) = (state.time_ms, state.credit_ms);
        let seq = sim.reserve_journal_seq().map_err(|e| e.to_string())?;
        batch.push(spall_store::JournalRecord {
            seq: seq.0,
            tick,
            payload: spall_store::JournalPayload::VegetationClock { time_ms, credit_ms },
        });
        *journalled_through = seq.0;
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_store::{JournalPayload, JournalRecord};

    #[test]
    fn weather_admin_gate_forces_snow_persists_through_ticks_and_restores_automatic() {
        let mut sim = Simulation::new(spall_sim::SimulationConfig::new(
            spall_sim::fixtures::flat_terrain_setup(),
        ))
        .unwrap();
        let initial = sim.weather_state().clone();
        let disabled = set_weather_admin(
            &mut sim,
            false,
            RequestId(1),
            Some(spall_protocol::WeatherKind::Snow),
            None,
        );
        assert!(!disabled.accepted);
        assert_eq!(sim.weather_state(), &initial);
        let forced = set_weather_admin(
            &mut sim,
            true,
            RequestId(2),
            Some(spall_protocol::WeatherKind::Snow),
            Some(spall_protocol::SnowTier::Heavy),
        );
        assert!(forced.accepted);
        assert!(forced.message.contains("precipitation q16 55705"));
        assert_eq!(sim.weather_state().kind, spall_protocol::WeatherKind::Snow);
        assert_eq!(
            sim.weather_state().override_kind,
            Some(spall_protocol::WeatherKind::Snow)
        );
        sim.tick().unwrap();
        assert_eq!(sim.weather_state().kind, spall_protocol::WeatherKind::Snow);
        assert_eq!(
            sim.weather_state().precipitation_q16,
            55_705,
            "tier survives ticks"
        );
        let refused = set_weather_admin(
            &mut sim,
            true,
            RequestId(9),
            Some(spall_protocol::WeatherKind::Rain),
            Some(spall_protocol::SnowTier::Light),
        );
        assert!(!refused.accepted);
        let automatic = set_weather_admin(&mut sim, true, RequestId(3), None, None);
        assert!(automatic.accepted);
        assert_eq!(sim.weather_state().override_kind, None);
        assert_eq!(
            sim.weather_state().kind,
            spall_protocol::WeatherState::for_clock(
                sim.weather_state().weather_seed,
                sim.weather_state().world_epoch,
                sim.weather_state().effective_tick,
                spall_protocol::WeatherClock {
                    time_ms: sim.weather_state().clock_ms,
                    source: sim.weather_state().clock_source,
                    start_season: sim.weather_state().start_season,
                    season_ms: sim.weather_state().season_ms
                },
                sim.weather_state().world_revision
            )
            .kind
        );
    }

    // --- ENG-36: recovery must fail closed on corruption -------------------

    fn persist_cfg() -> PersistConfig {
        PersistConfig {
            world_id: T10_WORLD_ID,
            seed: 42,
            generator_version: 1,
        }
    }

    fn scratch_db(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spall_eng36_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("world.db")
    }

    fn checkpoint_row_count(db: &std::path::Path) -> i64 {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM checkpoints", [], |r| r.get(0))
            .unwrap()
    }

    /// A save carries the water but not the solver's tuning: a recovered
    /// custom world takes its scene's water config again, not the default.
    #[test]
    fn a_recovered_world_takes_its_scenes_water_tuning() {
        let db = scratch_db("water_tuning");
        let mut water = spall_sim::column_water::ColumnWaterSetup::new(0, 32, Vec::new());
        water.config.sleep_flow = 0.5;
        let world = CustomWorld::new(
            vec![[2.0, 1.0, 2.0]],
            spall_sim::fixtures::flat_terrain_setup,
        )
        .with_column_water(water);
        let open = || {
            setup_persistence_with_game_content(
                Some(&db),
                Scene::Custom,
                Some(&world),
                &persist_cfg(),
                None,
                TerrainColliderMode::default(),
                spall_sim::fixtures::stone_manifest(),
                None,
                None,
            )
            .unwrap()
        };
        let first = open();
        assert_eq!(
            first.sim.column_water().unwrap().setup().config.sleep_flow,
            0.5
        );
        drop(first);
        let recovered = open();
        assert_eq!(
            recovered
                .sim
                .column_water()
                .unwrap()
                .setup()
                .config
                .sleep_flow,
            0.5,
            "the scene's tuning after recovery"
        );
    }

    #[test]
    fn setup_persistence_seeds_only_a_genuinely_empty_database() {
        let db = scratch_db("empty");
        // Force the schema to exist with no checkpoint, exactly like a fresh DB.
        drop(Writer::open(&db).unwrap());
        assert_eq!(checkpoint_row_count(&db), 0);

        let Persistence {
            journalled_through,
            pipeline,
            checkpoints_published,
            ..
        } = setup_persistence(Some(&db), Scene::BridgeCut, &persist_cfg(), None).unwrap();
        assert_eq!(journalled_through, 0);
        assert!(pipeline.is_some());
        assert_eq!(
            checkpoints_published, 1,
            "the built-in scene is published once"
        );
        drop(pipeline);
        assert_eq!(checkpoint_row_count(&db), 1);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn recovery_rebuilds_the_selected_terrain_collision_without_changing_world_hash() {
        let db = scratch_db("terrain_collider_mode");
        let original = setup_persistence_with_terrain_collider_mode(
            Some(&db),
            Scene::BridgeCut,
            &persist_cfg(),
            None,
            TerrainColliderMode::WholeTerrain,
        )
        .unwrap();
        assert!(!original.sim.world().terrain_brick_colliders_enabled());
        let hash = original.sim.world().world_hash();
        drop(original);

        let restored = setup_persistence_with_terrain_collider_mode(
            Some(&db),
            Scene::BridgeCut,
            &persist_cfg(),
            None,
            TerrainColliderMode::PerBrick,
        )
        .unwrap();
        assert!(restored.sim.world().terrain_brick_colliders_enabled());
        restored
            .sim
            .world()
            .validate_terrain_brick_colliders()
            .unwrap();
        assert_eq!(restored.sim.world().world_hash(), hash);
        drop(restored);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn durable_bulk_suffix_seeds_first_split_reservation_after_restart() {
        let db = scratch_db("bulk_transfer_restart");
        let checkpoint_sim = Scene::BulkSplit.simulation();
        let checkpoint = persist::capture(&checkpoint_sim, &persist_cfg(), 0).unwrap();
        {
            let mut writer = Writer::open(&db).unwrap();
            writer.publish_checkpoint(&checkpoint).unwrap();
        }

        let mut live = Scene::BulkSplit.simulation();
        for tick in 1..=40 {
            if tick == 4 {
                let half = BRUSH_UNIT / 2;
                let brush = SphereBrush::new(
                    BrushPoint::from_units(
                        31 * BRUSH_UNIT + half,
                        8 * BRUSH_UNIT + half,
                        31 * BRUSH_UNIT + half,
                    ),
                    8 * BRUSH_UNIT,
                )
                .unwrap();
                live.submit(EditIntent::cut(
                    RequestId(1),
                    EntityId::new(1).unwrap(),
                    EditTarget::Terrain,
                    brush,
                ))
                .unwrap();
            }
            let ids = (0..live.transfer_reservation_bound())
                .map(|offset| {
                    TransferId(spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT | (401 + offset as u64))
                })
                .collect();
            live.tick_with_transfer_ids(ids).unwrap();
        }
        let entry = live
            .journal()
            .entries()
            .first()
            .expect("bulk split committed");
        let retained_low = entry
            .transaction
            .ops
            .iter()
            .find_map(|op| op.bulk_transfer_id())
            .expect("bulk marker carries transfer ID")
            & ((1_u64 << 61) - 1);
        assert_eq!(retained_low, 401);
        let records = persist::journal_records(live.journal().entries()).unwrap();
        assert!(matches!(
            records[0].payload,
            JournalPayload::TopologyAttachments { .. }
        ));
        Writer::open(&db).unwrap().append_journal(&records).unwrap();

        let restored = setup_persistence(Some(&db), Scene::BulkSplit, &persist_cfg(), None)
            .expect("SQLite checkpoint and durable bulk suffix restore");
        assert_eq!(
            restored.retained_transfer_ids,
            vec![
                TransferId(spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT | retained_low),
                TransferId(spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT | retained_low),
            ]
        );
        let mut late_join = LateJoin::new(16, 1, 0);
        late_join
            .seed_transfer_ids_from_recovery(&restored.sim, &restored.retained_transfer_ids)
            .unwrap();
        let next_split = late_join.reserve_id(TransferNamespace::Split).unwrap();
        assert!(
            next_split.0 & ((1_u64 << 61) - 1) > retained_low,
            "the first reservation after recovery must exceed every retained suffix ID"
        );

        // A CRC-valid but namespace-invalid retained marker is rejected by the
        // same allocator checks before the recovered world can be accepted.
        let invalid_db = scratch_db("bulk_transfer_invalid_restart");
        let mut invalid_writer = Writer::open(&invalid_db).unwrap();
        invalid_writer.publish_checkpoint(&checkpoint).unwrap();
        let mut invalid_transaction = entry.transaction.clone();
        for op in &mut invalid_transaction.ops {
            match op {
                spall_protocol::TopologyOp::SplitOffBulkBaseline { transfer_id, .. }
                | spall_protocol::TopologyOp::SourcePatchBulkBaseline { transfer_id, .. } => {
                    *transfer_id = spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT | (1_u64 << 61) | 17;
                }
                _ => {}
            }
        }
        let mut invalid_set = entry.attachments.clone().unwrap();
        let invalid_id = invalid_transaction
            .ops
            .iter()
            .find_map(|op| op.bulk_transfer_id())
            .unwrap();
        for attachment in &mut invalid_set.attachments {
            let spall_protocol::TransactionAttachmentV1::Geometry { transfer_id, .. } = attachment;
            *transfer_id = invalid_id;
        }
        let invalid_record = JournalRecord {
            seq: entry.seq.0,
            tick: entry.transaction.server_tick.get(),
            payload: JournalPayload::topology_with_attachments(
                &invalid_transaction,
                &entry.participants,
                &invalid_set,
            )
            .unwrap(),
        };
        invalid_writer.append_journal(&[invalid_record]).unwrap();
        drop(invalid_writer);
        let error =
            match setup_persistence(Some(&invalid_db), Scene::BulkSplit, &persist_cfg(), None) {
                Ok(_) => panic!("invalid retained ID must fail setup"),
                Err(error) => error,
            };
        assert!(error.contains("recovered transfer ID is invalid"));
        assert_eq!(checkpoint_row_count(&invalid_db), 1);

        drop(restored);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
        let _ = std::fs::remove_dir_all(invalid_db.parent().unwrap());
    }

    #[test]
    fn setup_persistence_fails_closed_on_interior_journal_corruption() {
        let db = scratch_db("crc");
        {
            let mut w = Writer::open(&db).unwrap();
            let sim = Scene::BridgeCut.simulation();
            w.publish_checkpoint(&persist::capture(&sim, &persist_cfg(), 0).unwrap())
                .unwrap();
            w.append_journal(&[
                JournalRecord {
                    seq: 1,
                    tick: 1,
                    payload: JournalPayload::PoseBatch { snapshots: vec![] },
                },
                JournalRecord {
                    seq: 2,
                    tick: 2,
                    payload: JournalPayload::PoseBatch { snapshots: vec![] },
                },
                JournalRecord {
                    seq: 3,
                    tick: 3,
                    payload: JournalPayload::PoseBatch { snapshots: vec![] },
                },
            ])
            .unwrap();
        }
        // Corrupt seq 2's payload without fixing its CRC — an interior CRC error.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute("UPDATE journal SET payload = X'DEADBEEF' WHERE seq = 2", [])
                .unwrap();
        }

        let before = checkpoint_row_count(&db);
        let err = match setup_persistence(Some(&db), Scene::BridgeCut, &persist_cfg(), None) {
            Ok(_) => panic!("host startup must not silently continue from a shortened history"),
            Err(e) => e,
        };
        assert!(
            err.contains("corruption") || err.contains("RecoveryChoice"),
            "error surfaces the corruption and the required operator choice: {err}"
        );
        assert!(db.exists(), "the original database is left in place");
        assert_eq!(
            checkpoint_row_count(&db),
            before,
            "no fresh-scene checkpoint was published automatically"
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn setup_persistence_refuses_a_database_whose_checkpoints_do_not_decode() {
        let db = scratch_db("undecodable");
        {
            let mut w = Writer::open(&db).unwrap();
            let sim = Scene::BridgeCut.simulation();
            w.publish_checkpoint(&persist::capture(&sim, &persist_cfg(), 0).unwrap())
                .unwrap();
        }
        // Every complete checkpoint's metadata is now undecodable.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute("UPDATE checkpoints SET meta = X'DEADBEEF'", [])
                .unwrap();
        }

        let before = checkpoint_row_count(&db);
        assert_eq!(before, 1);
        let err = match setup_persistence(Some(&db), Scene::BridgeCut, &persist_cfg(), None) {
            Ok(_) => panic!("undecodable checkpoints are corruption, not an empty database"),
            Err(e) => e,
        };
        assert!(
            err.contains("unrecoverable"),
            "error distinguishes corruption from a fresh DB: {err}"
        );
        assert!(db.exists(), "the original database is left in place");
        assert_eq!(
            checkpoint_row_count(&db),
            before,
            "the corrupt save was not overwritten with the built-in scene"
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }
}

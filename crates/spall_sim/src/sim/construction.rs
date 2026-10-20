//! Construction, restore, world reset and world access.

use super::*;

impl Simulation {
    pub fn new(config: SimulationConfig) -> Result<Self, crate::world::WorldError> {
        Self::new_with_physics_origin(config, PhysicsOrigin::ZERO)
    }

    /// Creates a simulation with an explicit local physics frame. Authoritative
    /// world positions and protocol snapshots remain in global coordinates.
    pub fn new_with_physics_origin(
        config: SimulationConfig,
        physics_origin: PhysicsOrigin,
    ) -> Result<Self, crate::world::WorldError> {
        let column_water_setup = config.column_water;
        let column_water_worker = config.column_water_worker;
        let vegetation_setup = config.vegetation;
        let has_vegetation = vegetation_setup.is_some();
        let weather_start_season = vegetation_setup
            .as_ref()
            .map_or(0, |s| s.start_season as u8);
        let weather_season_ms = vegetation_setup.as_ref().map_or(900_000, |s| s.season_ms);
        let weather_seed = config.weather_seed;
        let world_span = crate::prof::Span::start("startup.world_and_collision");
        let world = SimWorld::new_with_terrain_collider_mode_and_origin(
            config.world,
            config.terrain_collider_mode,
            physics_origin,
        )?;
        drop(world_span);
        let mut simulation = Self {
            world,
            pipeline: EditPipeline::new(config.max_pending_intents, config.serialize_threshold),
            journal: JournalSink::new(),
            retiring: Retiring::default(),
            creatures: None,
            tick: Tick::ZERO,
            next_control_seq: 1,
            creature_presentation: std::collections::BTreeMap::new(),
            creature_move_timing: CreatureMoveTiming::default(),
            next_damage_seq: 0,
            column_water: None,
            column_water_worker: false,
            buoyancy_cache: Default::default(),
            vegetation: None,
            weather: spall_protocol::WeatherState::initial(
                weather_seed,
                1,
                spall_protocol::WeatherClock {
                    time_ms: 0,
                    source: if has_vegetation {
                        spall_protocol::WeatherClockSource::Vegetation
                    } else {
                        spall_protocol::WeatherClockSource::ServerFallback
                    },
                    start_season: weather_start_season,
                    season_ms: weather_season_ms,
                },
            ),
            weather_dirty: true,
            weather_seed,
            snow: crate::snow::SnowState::new(),
            snow_priority_tiles: Vec::new(),
            snow_priority_players: Vec::new(),
            snow_priority_refreshed_tick: 0,
            motion_footprints: crate::footprints::MotionFootprints::new(),
            footfall_candidates: Vec::new(),
        };
        if let Some(setup) = column_water_setup {
            let _span = crate::prof::Span::start("startup.column_water");
            simulation.column_water = Some(
                crate::column_water::AuthoritativeColumnWater::new(
                    &simulation.world.terrain().volume,
                    setup,
                )
                .map_err(|error| {
                    crate::world::WorldError::WaterInitialization(error.to_string())
                })?,
            );
            simulation.set_column_water_worker(column_water_worker);
        }
        simulation.pipeline.warm_labels(&simulation.world);
        if let Some(state) = vegetation_setup {
            let _span = crate::prof::Span::start("startup.vegetation_install");
            simulation
                .install_vegetation(state)
                .map_err(crate::world::WorldError::VegetationInitialization)?;
        }
        Ok(simulation)
    }

    /// Rebuilds a simulation from a recovered [`SimWorld`] (T16). `tick` is the
    /// checkpoint tick the world was restored to (and journal suffix replayed
    /// onto). The in-memory journal starts empty — the durable journal lives in
    /// `spall_store` — and control-stream sequencing restarts at 1 for the fresh
    /// post-restart session.
    pub fn from_restored(world: SimWorld, tick: Tick) -> Self {
        let simulation = Self {
            world,
            pipeline: EditPipeline::new(
                SimulationConfig::DEFAULT_MAX_PENDING_INTENTS,
                SimulationConfig::DEFAULT_SERIALIZE_THRESHOLD,
            ),
            journal: JournalSink::new(),
            retiring: Retiring::default(),
            creatures: None,
            tick,
            next_control_seq: 1,
            creature_presentation: std::collections::BTreeMap::new(),
            creature_move_timing: CreatureMoveTiming::default(),
            next_damage_seq: 0,
            // Column water is installed after recovery by the caller.
            column_water: None,
            column_water_worker: false,
            buoyancy_cache: Default::default(),
            vegetation: None,
            weather: spall_protocol::WeatherState::initial(
                0,
                1,
                spall_protocol::WeatherClock {
                    time_ms: tick.get().saturating_mul(1000) / 60,
                    source: spall_protocol::WeatherClockSource::ServerFallback,
                    start_season: 0,
                    season_ms: 900_000,
                },
            ),
            weather_dirty: false,
            weather_seed: 0,
            snow: crate::snow::SnowState::new(),
            snow_priority_tiles: Vec::new(),
            snow_priority_players: Vec::new(),
            snow_priority_refreshed_tick: 0,
            motion_footprints: crate::footprints::MotionFootprints::new(),
            footfall_candidates: Vec::new(),
        };
        simulation.pipeline.warm_labels(&simulation.world);
        simulation
    }

    pub fn world(&self) -> &SimWorld {
        &self.world
    }

    /// Admin world reset: adopt `fresh`'s world, water, and (empty) edit
    /// pipeline while keeping this simulation's tick, journal, and control
    /// sequencing, so every id a client or journal has seen stays monotonic.
    /// Id allocation resumes from whichever counter is further along, so no
    /// entity, volume, transaction, or journal sequence is handed out twice.
    /// Players are not carried over; the caller re-adds them. Staged edits of
    /// the old world are dropped.
    pub fn replace_world(&mut self, fresh: Simulation) -> Result<(), crate::world::WorldError> {
        if fresh.creatures.is_some() {
            return Err(crate::world::WorldError::CreatureIdentityReplacement);
        }
        if let Some(creatures) = &self.creatures {
            creatures.preview_reset_epoch()?;
        }
        let (entity, volume, transaction, journal_seq) = self.world.registry().counters();
        let Simulation {
            mut world,
            pipeline,
            column_water,
            vegetation,
            mut weather,
            weather_seed,
            snow,
            ..
        } = fresh;
        let (f_entity, f_volume, f_transaction, f_journal_seq) = world.registry().counters();
        world.resume_registry(
            entity.max(f_entity),
            volume.max(f_volume),
            transaction.max(f_transaction),
            journal_seq.max(f_journal_seq),
        )?;
        if let Some(creatures) = &mut self.creatures {
            creatures.reset_epoch()?;
        }
        self.creature_presentation.clear();
        let off_thread = self.pipeline.stages_off_thread();
        let old_world = std::mem::replace(&mut self.world, world);
        let old_pipeline = std::mem::replace(&mut self.pipeline, pipeline);
        if off_thread {
            self.pipeline.enable_off_thread_staging();
        }
        let old_column_water = std::mem::replace(&mut self.column_water, column_water);
        if self.column_water_worker
            && let Some(water) = &mut self.column_water
        {
            water.start_worker();
        }
        let old_vegetation = std::mem::replace(&mut self.vegetation, vegetation);
        weather.world_epoch = self.weather.world_epoch.saturating_add(1);
        self.weather = weather;
        self.weather_seed = weather_seed;
        self.weather_dirty = true;
        let old_snow = std::mem::replace(&mut self.snow, snow);
        self.motion_footprints.clear();
        self.footfall_candidates.clear();
        // The replaced world goes away off the tick thread.
        self.retiring.retire((
            old_world,
            old_pipeline,
            old_column_water,
            old_vegetation,
            old_snow,
        ));
        Ok(())
    }

    /// Waits until the world a reset replaced has been freed, re-raising a panic from its drop.
    pub fn wait_for_retired(&mut self) {
        self.retiring.finish();
    }

    /// Mutable world access, for standing up a scenario (spawning a pre-existing
    /// body) before ticking.
    pub fn world_mut(&mut self) -> &mut SimWorld {
        &mut self.world
    }
}

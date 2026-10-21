//! The tick pipeline and its drivers.

use super::*;

impl Simulation {
    /// Advances one server tick: run the edit pipeline, then step physics and
    /// refresh extracted body state.
    pub fn tick(&mut self) -> Result<TickReport, TickError> {
        self.tick_inner(VecDeque::new())
    }

    /// Advances a server tick using transfer IDs reserved by the server's
    /// shared allocator before any tick mutation. Unused values remain burned.
    pub fn tick_with_transfer_ids(
        &mut self,
        transfer_ids: Vec<spall_protocol::TransferId>,
    ) -> Result<TickReport, TickError> {
        let mut seen = std::collections::HashSet::with_capacity(transfer_ids.len());
        if transfer_ids.iter().any(|id| {
            id.0 == 0
                || id.0 & ((1_u64 << 61) - 1) == 0
                || id.0 & spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT == 0
                || id.0 & spall_protocol::WORLD_RESET_TRANSFER_ID_BIT != 0
                || !seen.insert(id.0)
        }) {
            return Err(CommitError::InvalidBulkTransferIds.into());
        }
        self.tick_inner(transfer_ids.into())
    }

    fn tick_inner(
        &mut self,
        mut transfer_ids: VecDeque<spall_protocol::TransferId>,
    ) -> Result<TickReport, TickError> {
        self.tick = self
            .tick
            .checked_next()
            .map_err(|_: IdError| TickError::TickExhausted)?;
        let pipeline_span = crate::prof::Span::start("tick.pipeline");
        let report = self.pipeline.run_tick_with_transfer_ids(
            &mut self.world,
            &mut self.journal,
            self.tick,
            &mut self.next_control_seq,
            &mut transfer_ids,
        )?;
        drop(pipeline_span);
        let mut report = report;
        // Only a committed change to a terrain brick at or next to a region's
        // fluid domain can change that region's boundary.
        let terrain_id = self.world.terrain_volume_id();
        let edited_terrain_bricks: Vec<spall_core::BrickCoord> = report
            .committed
            .iter()
            .flat_map(|(_, committed)| {
                committed
                    .topology
                    .before
                    .iter()
                    .chain(&committed.topology.after)
                    .filter(|b| b.volume == terrain_id)
                    .map(|b| b.coord)
            })
            .collect();
        let water_span = crate::prof::Span::start("tick.water");
        if let Some(water) = &mut self.column_water {
            let players: Vec<[f64; 2]> = self
                .world
                .players()
                .map(|player| {
                    let (lo, hi) = player.capsule_aabb_m();
                    [0.5 * (lo[0] + hi[0]), 0.5 * (lo[2] + hi[2])]
                })
                .collect();
            water.set_focus(&players);
            let reloaded: Vec<spall_core::BrickCoord> = report
                .reloaded_bricks
                .iter()
                .filter(|(volume, _)| *volume == terrain_id)
                .map(|(_, coord)| *coord)
                .collect();
            report.column_water = Some(
                water
                    .tick(
                        &self.world.terrain().volume,
                        &edited_terrain_bricks,
                        &reloaded,
                        f64::from(TICK_DT_S),
                    )
                    .map_err(|error| TickError::Water(error.to_string()))?,
            );
        }
        drop(water_span);
        let vegetation_span = crate::prof::Span::start("tick.vegetation");
        self.advance_vegetation(&report)?;
        self.advance_weather();
        self.snow
            .advance(&self.weather, self.tick.get())
            .map_err(TickError::Snow)?;
        self.refresh_snow_from_report(&report);
        self.advance_snow_layer(&report);
        drop(vegetation_span);
        // Bodies float: buoyancy and drag from the column water, before the
        // physics step that integrates them.
        if let Some(water) = &self.column_water {
            // From the published frame, so a fluid step running on its worker
            // never makes the tick wait.
            let frame = water.frame();
            let sampler = frame.sampler();
            report.buoyancy = Some(self.world.apply_buoyancy(
                &|x, y, z| sampler.sample_at_m(x, y, z).map(|w| w.surface_y),
                &|x, y, z| sampler.sample_at_m(x, y, z).map(|w| w.flow_m_s),
                f64::from(TICK_DT_S),
                &mut self.buoyancy_cache,
            ));
        }
        let physics_span = crate::prof::Span::start("tick.physics");
        let physics_started = std::time::Instant::now();
        self.world.step_physics();
        let physics_duration = physics_started.elapsed();
        report.physics_duration = physics_duration;
        drop(physics_span);
        let players_span = crate::prof::Span::start("tick.players");
        self.advance_players(&report);
        drop(players_span);
        Ok(report)
    }

    /// Advances the player capsules after the physics step, so the character
    /// sweep runs against the broad-phase BVH this tick's [`SimWorld::step_physics`]
    /// just refreshed — including any collider a commit rebuilt. A player near a
    /// cell this tick's transactions edited has its prediction epoch bumped and
    /// is depenetrated (`crate::player`).
    pub(super) fn advance_players(&mut self, report: &TickReport) {
        if self.world.player_count() == 0 {
            return;
        }
        let terrain = self.world.terrain_volume_id();
        let cell_m = self.world.terrain().cell_size().metres();
        let boxes: Vec<([f64; 3], [f64; 3])> = report
            .committed
            .iter()
            .filter_map(|(_, committed)| {
                transaction_world_box(&committed.topology, terrain, cell_m)
            })
            .collect();
        let frame = self.column_water.as_ref().map(|w| w.frame());
        let sampler = frame.as_ref().map(|f| f.sampler());
        self.world.advance_players(TICK_DT_S, &boxes, &|x, y, z| {
            sampler.as_ref().and_then(|s| s.sample_at_m(x, y, z))
        });
        self.motion_footprints
            .retain_players(self.world.players().map(|player| player.entity));
        self.footfall_candidates.clear();
        self.footfall_candidates.extend(
            self.world
                .players()
                .filter_map(|player| self.motion_footprints.observe(player, self.tick.get())),
        );
        self.stamp_snow_candidates();
        if std::env::var_os("SPALL_DEBUG_PLAYER").is_some() {
            for p in self.world.players() {
                eprintln!(
                    "DBGPLAYER tick={} pos={:?} grounded={} vel={:?}",
                    self.tick.0, p.state.position_m, p.state.grounded, p.state.velocity_m_s
                );
            }
        }
    }

    /// Runs ticks until the pipeline is idle or `max_ticks` is reached. Returns
    /// the per-tick reports.
    pub fn run_until_idle(&mut self, max_ticks: u32) -> Result<Vec<TickReport>, TickError> {
        let mut reports = Vec::new();
        for _ in 0..max_ticks {
            let report = self.tick()?;
            let done = self.pipeline.is_idle();
            reports.push(report);
            if done {
                break;
            }
        }
        Ok(reports)
    }

    /// Steps physics only (no edits), for settling / observation.
    pub fn step_physics_only(&mut self) {
        self.world.step_physics();
    }
}

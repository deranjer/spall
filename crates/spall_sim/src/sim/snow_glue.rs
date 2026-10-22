//! Snow state access and per-tick snow refresh.

use super::*;

impl Simulation {
    pub fn snow_state(&self) -> &crate::snow::SnowState {
        &self.snow
    }

    pub fn snow_state_bytes(&self) -> Result<Vec<u8>, String> {
        self.snow.to_canonical_bytes()
    }

    pub fn restore_snow_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.snow = crate::snow::SnowState::from_canonical_bytes(bytes)?;
        Ok(())
    }

    pub fn take_snow_frames(&mut self) -> Vec<spall_protocol::SnowSurfaceFrame> {
        self.snow.take_changed_frames(
            self.weather.world_epoch,
            self.weather.world_revision,
            self.tick.get(),
        )
    }

    pub fn take_snow_journal_tiles(
        &mut self,
    ) -> Result<Vec<crate::snow::SnowTileJournalState>, String> {
        self.snow
            .take_geometry_changed_keys()
            .into_iter()
            .map(|key| {
                Ok(crate::snow::SnowTileJournalState {
                    tile: [key.0, key.1, key.2],
                    state: self.snow.tile_canonical_bytes(key)?,
                })
            })
            .collect()
    }

    pub fn snow_snapshot_frames(
        &mut self,
        tile_bounds: Option<[i32; 4]>,
    ) -> Vec<spall_protocol::SnowSurfaceFrame> {
        let keys = self.snow.tile_keys();
        keys.into_iter()
            .filter(|(x, z, _)| {
                tile_bounds.is_none_or(|[min_x, max_x, min_z, max_z]| {
                    (min_x..=max_x).contains(x) && (min_z..=max_z).contains(z)
                })
            })
            .filter_map(|(x, z, y)| {
                self.snow.frame(
                    x,
                    z,
                    y,
                    self.weather.world_epoch,
                    self.weather.world_revision,
                    self.tick.get(),
                )
            })
            .collect()
    }

    pub fn snow_snapshot_page(
        &mut self,
        tile_bounds: Option<[i32; 4]>,
        after: Option<(i32, i32, i32)>,
        limit: usize,
    ) -> (
        Vec<spall_protocol::SnowSurfaceFrame>,
        Option<(i32, i32, i32)>,
    ) {
        self.snow.snapshot_page(
            tile_bounds,
            after,
            limit,
            self.weather.world_epoch,
            self.weather.world_revision,
            self.tick.get(),
        )
    }

    pub fn snow_nearest_snapshot_page(
        &mut self,
        tile_bounds: [i32; 4],
        center: (i32, i32),
        after: Option<crate::snow::SnowSnapshotCursor>,
        limit: usize,
    ) -> (
        Vec<spall_protocol::SnowSurfaceFrame>,
        Option<crate::snow::SnowSnapshotCursor>,
    ) {
        self.snow.nearest_snapshot_page(
            tile_bounds,
            center,
            after,
            limit,
            self.weather.world_epoch,
            self.weather.world_revision,
            self.tick.get(),
        )
    }

    /// Server-authoritative stride proposals produced after capsule movement.
    /// The snow subsystem revalidates each proposal against exact terrain state.
    pub fn take_footfall_candidates(&mut self) -> Vec<crate::footprints::FootfallCandidate> {
        std::mem::take(&mut self.footfall_candidates)
    }

    pub(super) fn refresh_snow_from_report(&mut self, report: &TickReport) {
        use spall_core::CellSizeCode;
        use std::collections::BTreeSet;

        let terrain_body = self.world.terrain();
        let terrain = &terrain_body.volume;
        if terrain.cell_size() != CellSizeCode::Quarter {
            return;
        }
        // Volume may be intentionally unbounded for sparse natural worlds;
        // collider_region is the durable worldgen extent and therefore the
        // authoritative limit for sky exposure scans.
        let (region_min, region_max) = terrain_body.collider_region;
        let min_tile_x_raw = region_min.x.div_euclid(16);
        let max_tile_x_raw = region_max.x.div_euclid(16);
        let min_tile_z_raw = region_min.z.div_euclid(16);
        let max_tile_z_raw = region_max.z.div_euclid(16);
        let Ok(min_tile_x) = i32::try_from(min_tile_x_raw) else {
            return;
        };
        let Ok(max_tile_x) = i32::try_from(max_tile_x_raw) else {
            return;
        };
        let Ok(min_tile_z) = i32::try_from(min_tile_z_raw) else {
            return;
        };
        let Ok(max_tile_z) = i32::try_from(max_tile_z_raw) else {
            return;
        };
        if self.snow.needs_resident_discovery_seed() {
            self.snow
                .seed_resident_discovery(&terrain.resident_brick_coords());
        }
        let mut player_tiles = Vec::new();
        for player in self.world.players() {
            let tile_x = (player.state.position_m[0] / 4.0).floor();
            let tile_z = (player.state.position_m[2] / 4.0).floor();
            if tile_x < f64::from(i32::MIN)
                || tile_x > f64::from(i32::MAX)
                || tile_z < f64::from(i32::MIN)
                || tile_z > f64::from(i32::MAX)
            {
                continue;
            }
            let (tile_x, tile_z) = (tile_x as i32, tile_z as i32);
            player_tiles.push((tile_x, tile_z));
        }
        player_tiles.retain(|(x, z)| {
            (min_tile_x..=max_tile_x).contains(x) && (min_tile_z..=max_tile_z).contains(z)
        });
        // Cache interleaved player-center/ring order. Rebuild only when a
        // player changes snow tile or once a second, never by allocating a
        // 96 m ring on every authoritative tick.
        if self.snow_priority_players != player_tiles
            || self.tick.get() <= 1
            || self
                .tick
                .get()
                .saturating_sub(self.snow_priority_refreshed_tick)
                >= 60
        {
            self.snow_priority_players.clone_from(&player_tiles);
            self.snow_priority_refreshed_tick = self.tick.get();
            self.snow_priority_tiles.clear();
            for &(x, z) in &player_tiles {
                self.snow.enqueue_scan((x, z), 0);
                self.snow_priority_tiles.push((x, z));
            }
            for radius in 1i32..=SNOW_DISCOVERY_RING_TILES {
                for &(tile_x, tile_z) in &player_tiles {
                    for dz in -radius..=radius {
                        for dx in -radius..=radius {
                            if dx.abs().max(dz.abs()) != radius {
                                continue;
                            }
                            if let (Some(x), Some(z)) =
                                (tile_x.checked_add(dx), tile_z.checked_add(dz))
                            {
                                if !(min_tile_x..=max_tile_x).contains(&x)
                                    || !(min_tile_z..=max_tile_z).contains(&z)
                                {
                                    continue;
                                }
                                self.snow.enqueue_scan((x, z), 0);
                                self.snow_priority_tiles.push((x, z));
                            }
                        }
                    }
                }
            }
        }
        let mut affected = BTreeSet::new();
        for brick in report.committed.iter().flat_map(|(_, committed)| {
            committed
                .topology
                .before
                .iter()
                .chain(&committed.topology.after)
                .filter(|brick| brick.volume == terrain.id())
                .map(|brick| brick.coord)
        }) {
            for dz in 0..2 {
                for dx in 0..2 {
                    let tx = brick.x.saturating_mul(2).saturating_add(dx);
                    let tz = brick.z.saturating_mul(2).saturating_add(dz);
                    if let (Ok(tx), Ok(tz)) = (i32::try_from(tx), i32::try_from(tz)) {
                        affected.insert((tx, tz));
                        self.snow.enqueue_edited_scan((tx, tz), self.tick.get());
                    }
                }
            }
        }
        let work = self
            .snow
            .take_scan_batch(&self.snow_priority_tiles, 16, self.tick.get());
        let water = self.column_water.as_ref().map(|w| w.frame());
        let water_sampler = water.as_ref().map(|frame| frame.sampler());
        for ((tile_x, tile_z), birth_tick) in work {
            let was_scanned = self.snow.tile_scanned((tile_x, tile_z));
            let edited_tile = affected.contains(&(tile_x, tile_z));
            let mut layers = BTreeSet::new();
            let mut masks = std::collections::BTreeMap::<i32, ([u64; 4], u64)>::new();
            let geometry = match crate::snow_query::query_snow_tile_in_region(
                terrain, tile_x, tile_z, region_min, region_max,
            ) {
                Ok(geometry) => geometry,
                Err(crate::snow_query::SnowTileQueryError::OutsideBounds) => {
                    self.snow.mark_tile_scanned((tile_x, tile_z));
                    continue;
                }
                Err(
                    crate::snow_query::SnowTileQueryError::UnsupportedCellSize
                    | crate::snow_query::SnowTileQueryError::MissingBounds
                    | crate::snow_query::SnowTileQueryError::VerticalSpan
                    | crate::snow_query::SnowTileQueryError::CoordinateRange,
                ) => {
                    self.snow.mark_tile_scanned((tile_x, tile_z));
                    continue;
                }
                Err(_) => {
                    self.snow
                        .retry_scan((tile_x, tile_z), birth_tick, self.tick.get());
                    continue;
                }
            };
            let x0 = i64::from(tile_x) * 16;
            let z0 = i64::from(tile_z) * 16;
            for (y, layer) in geometry.layers {
                let mut mask = layer.exposed_faces;
                for bit in 0..256usize {
                    if mask[bit / 64] & (1u64 << (bit % 64)) == 0 {
                        continue;
                    }
                    let dx = (bit % 16) as i64;
                    let dz = (bit / 16) as i64;
                    let top_m = (f64::from(y) + 1.0) * 0.25;
                    if water_sampler
                        .as_ref()
                        .and_then(|sampler| {
                            sampler.sample_at_m(
                                (x0 + dx) as f64 * 0.25 + 0.125,
                                top_m + 0.01,
                                (z0 + dz) as f64 * 0.25 + 0.125,
                            )
                        })
                        .is_some_and(|surface| surface.surface_y > top_m + 0.02)
                    {
                        mask[bit / 64] &= !(1u64 << (bit % 64));
                    }
                }
                if mask.iter().any(|word| *word != 0) {
                    masks.insert(y, (mask, layer.geometry_revision));
                }
            }
            layers.extend(masks.keys().copied());
            let prior = self.snow.tile_layers(tile_x, tile_z);
            let tick = self.tick.get();
            let birth = if birth_tick == 0 { tick } else { birth_tick };
            for (y, (mask, revision)) in masks {
                if was_scanned || edited_tile || birth_tick > 0 {
                    self.snow
                        .reconcile_at(tile_x, tile_z, y, revision, mask, birth, tick);
                    if edited_tile || birth_tick > 0 {
                        self.snow.mark_geometry_changed((tile_x, tile_z, y));
                    }
                } else {
                    self.snow
                        .discover_initial(tile_x, tile_z, y, revision, mask);
                }
            }
            if was_scanned {
                for (_, _, y) in prior {
                    if !layers.contains(&y) {
                        self.snow.reconcile(tile_x, tile_z, y, 0, [0; 4], tick);
                        self.snow.mark_geometry_changed((tile_x, tile_z, y));
                    }
                }
            }
            self.snow.mark_tile_scanned((tile_x, tile_z));
        }
    }

    pub(super) fn stamp_snow_candidates(&mut self) {
        let cell_m = 0.25;
        let terrain = &self.world.terrain().volume;
        if terrain.cell_size() != spall_core::CellSizeCode::Quarter {
            return;
        }
        let water = self.column_water.as_ref().map(|w| w.frame());
        let water_sampler = water.as_ref().map(|frame| frame.sampler());
        let candidates = std::mem::take(&mut self.footfall_candidates);
        for candidate in candidates {
            let x = (candidate.position_m[0] / cell_m).floor() as i64;
            let z = (candidate.position_m[2] / cell_m).floor() as i64;
            let y = (candidate.position_m[1] / cell_m).floor() as i64 - 1;
            let (Ok(x), Ok(y), Ok(z)) = (i32::try_from(x), i32::try_from(y), i32::try_from(z))
            else {
                continue;
            };
            let cell = GlobalCell::new(i64::from(x), i64::from(y), i64::from(z));
            if !matches!(terrain.sample(cell), Ok(Sample::Filled(_))) {
                continue;
            }
            let top_m = (f64::from(y) + 1.0) * cell_m;
            if (candidate.position_m[1] - top_m).abs() > 0.045 {
                continue;
            }
            if water_sampler
                .as_ref()
                .and_then(|sampler| {
                    sampler.sample_at_m(
                        candidate.position_m[0],
                        top_m + 0.01,
                        candidate.position_m[2],
                    )
                })
                .is_some_and(|surface| surface.surface_y > top_m + 0.02)
            {
                continue;
            }
            let (brick, _) = cell.split();
            let Some(revision) = terrain
                .brick_revision(brick)
                .ok()
                .flatten()
                .map(|revision| revision.get())
            else {
                continue;
            };
            self.snow
                .stamp(candidate, [x, y, z], revision, self.tick.get());
        }
    }
}

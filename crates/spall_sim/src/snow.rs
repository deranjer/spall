//! Bounded, authoritative thin-snow cohorts for exposed quarter-metre terrain.
//!
//! Geometry discovery is deliberately separate from climate advancement: the
//! caller reconciles masks when a terrain tile is observed or edited, then
//! advances every extant cohort from the same weather profile.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use spall_protocol::{
    Record, SNOW_MAX_DEPTH_Q16, SNOW_SCHEMA, SNOW_TILE_CELLS, SnowFootprint, SnowSurface,
    SnowSurfaceFrame, SnowSurfaceId, WeatherKind, WeatherState,
};

const Q32_PER_METRE: u64 = 1u64 << 32;
const MAX_DEPTH_Q32: u64 = (SNOW_MAX_DEPTH_Q16 as u64) << 16;
const MAX_CLIMATE_SEGMENTS: usize = 4_096;
const SNOW_UPDATE_TILES_PER_TICK: usize = 16;
const SNOW_STATE_SCHEMA: u16 = 1;
const MAX_SNOW_STATE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnowTileJournalState {
    pub tile: [i32; 3],
    pub state: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnowSnapshotCursor {
    pub center: (i32, i32),
    pub key: (i32, i32, i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct DepthTransform {
    bias: i128,
    low: i128,
    high: i128,
}

impl DepthTransform {
    fn identity() -> Self {
        Self {
            bias: 0,
            low: 0,
            high: i128::from(MAX_DEPTH_Q32),
        }
    }

    fn add(delta: i128) -> Self {
        Self {
            bias: delta,
            low: 0,
            high: i128::from(MAX_DEPTH_Q32),
        }
    }

    fn then(self, next: Self) -> Self {
        let clamp = |value: i128| value.clamp(next.low, next.high);
        Self {
            bias: self.bias + next.bias,
            low: clamp(self.low + next.bias),
            high: clamp(self.high + next.bias),
        }
    }

    fn apply(self, depth: u64) -> u64 {
        (i128::from(depth) + self.bias).clamp(self.low, self.high) as u64
    }

    fn repeated(delta: i64, ticks: u64) -> Self {
        Self::add(i128::from(delta) * i128::from(ticks))
    }
}

impl Default for DepthTransform {
    fn default() -> Self {
        Self::identity()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ClimateSegment {
    start_tick: u64,
    end_tick: u64,
    delta_q32_per_tick: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct PendingSnowScan {
    birth_tick: u64,
    retry_after_tick: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cohort {
    id: SnowSurfaceId,
    mask: [u64; 4],
    generation: u32,
    depth_q32: u64,
    climate_through_tick: u64,
    geometry_revision: u64,
}

/// Durable snow state keyed by 4 m tile and global cell-height.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnowState {
    tiles: BTreeMap<(i32, i32, i32), Vec<Cohort>>,
    scanned_tiles: BTreeSet<(i32, i32)>,
    pending_scans: BTreeMap<(i32, i32), PendingSnowScan>,
    scan_cursor: Option<(i32, i32)>,
    resident_discovery_seeded: bool,
    #[serde(skip)]
    footprints: BTreeMap<(i32, i32, i32), Vec<SnowFootprint>>,
    #[serde(skip)]
    footprint_credits: BTreeMap<u64, u64>,
    snapshot_seq: u64,
    #[serde(skip)]
    next_footprint_id: u64,
    #[serde(skip)]
    changed: BTreeSet<(i32, i32, i32)>,
    #[serde(skip)]
    geometry_changed: BTreeSet<(i32, i32, i32)>,
    climate_through_tick: u64,
    climate: VecDeque<ClimateSegment>,
    initial_transform: DepthTransform,
    catchup_cursor: Option<(i32, i32, i32)>,
    #[serde(skip)]
    cursor_counts: BTreeMap<u64, usize>,
    dirty: bool,
}

impl SnowState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn contains_tile_layer(&self, key: (i32, i32, i32)) -> bool {
        self.tiles.contains_key(&key)
    }

    pub fn tile_keys(&self) -> Vec<(i32, i32, i32)> {
        self.tiles.keys().copied().collect()
    }

    pub fn tile_scanned(&self, tile: (i32, i32)) -> bool {
        self.scanned_tiles.contains(&tile)
    }

    pub fn needs_resident_discovery_seed(&self) -> bool {
        !self.resident_discovery_seeded
    }

    pub fn mark_tile_scanned(&mut self, tile: (i32, i32)) {
        self.scanned_tiles.insert(tile);
        self.pending_scans.remove(&tile);
    }

    pub fn enqueue_scan(&mut self, tile: (i32, i32), birth_tick: u64) {
        if self.scanned_tiles.contains(&tile) && birth_tick == 0 {
            return;
        }
        self.pending_scans
            .entry(tile)
            .and_modify(|existing| {
                existing.birth_tick = existing.birth_tick.max(birth_tick);
                // Repeated proximity prioritization must not defeat unknown-tile
                // backoff. A real geometry edit explicitly resets it below.
                if birth_tick > 0 {
                    existing.retry_after_tick = 0;
                }
            })
            .or_insert(PendingSnowScan {
                birth_tick,
                retry_after_tick: 0,
            });
        self.dirty = true;
    }

    pub fn enqueue_edited_scan(&mut self, tile: (i32, i32), birth_tick: u64) {
        self.enqueue_scan(tile, birth_tick.max(1));
        if let Some(pending) = self.pending_scans.get_mut(&tile) {
            pending.retry_after_tick = 0;
        }
    }

    pub fn seed_resident_discovery(&mut self, bricks: &[spall_core::BrickCoord]) {
        if self.resident_discovery_seeded {
            return;
        }
        for brick in bricks {
            for dz in 0..2 {
                for dx in 0..2 {
                    let tx = brick.x.saturating_mul(2).saturating_add(dx);
                    let tz = brick.z.saturating_mul(2).saturating_add(dz);
                    if let (Ok(tx), Ok(tz)) = (i32::try_from(tx), i32::try_from(tz)) {
                        self.enqueue_scan((tx, tz), 0);
                    }
                }
            }
        }
        self.resident_discovery_seeded = true;
        self.dirty = true;
    }

    pub fn take_scan_batch(
        &mut self,
        priority: &[(i32, i32)],
        limit: usize,
        tick: u64,
    ) -> Vec<((i32, i32), u64)> {
        let mut selected = Vec::new();
        for tile in priority {
            if selected.len() == limit {
                break;
            }
            if self
                .pending_scans
                .get(tile)
                .is_some_and(|pending| pending.retry_after_tick <= tick)
                && let Some(pending) = self.pending_scans.remove(tile)
            {
                selected.push((*tile, pending.birth_tick));
            }
        }
        let cursor = self.scan_cursor;
        let ready =
            |(_, pending): &(&(i32, i32), &PendingSnowScan)| pending.retry_after_tick <= tick;
        let mut rest = Vec::new();
        if let Some(cursor) = cursor {
            rest.extend(
                self.pending_scans
                    .range((
                        std::ops::Bound::Excluded(cursor),
                        std::ops::Bound::Unbounded,
                    ))
                    .filter(ready)
                    .take(limit.saturating_sub(selected.len()))
                    .map(|(key, _)| *key),
            );
        }
        if rest.len() + selected.len() < limit {
            let wrapped_count = limit.saturating_sub(selected.len() + rest.len());
            let upper = cursor.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Included);
            rest.extend(
                self.pending_scans
                    .range((std::ops::Bound::Unbounded, upper))
                    .filter(ready)
                    .take(wrapped_count)
                    .map(|(key, _)| *key),
            );
        }
        for tile in rest {
            if selected.len() == limit {
                break;
            }
            if let Some(pending) = self.pending_scans.remove(&tile) {
                selected.push((tile, pending.birth_tick));
            }
        }
        if let Some((tile, _)) = selected.last() {
            self.scan_cursor = Some(*tile);
        }
        selected
    }

    pub fn retry_scan(&mut self, tile: (i32, i32), birth_tick: u64, tick: u64) {
        let pending = self.pending_scans.entry(tile).or_insert(PendingSnowScan {
            birth_tick,
            retry_after_tick: tick.saturating_add(60),
        });
        pending.birth_tick = pending.birth_tick.max(birth_tick);
        pending.retry_after_tick = pending.retry_after_tick.max(tick.saturating_add(60));
        self.dirty = true;
    }

    pub fn tile_layers(&self, tile_x: i32, tile_z: i32) -> Vec<(i32, i32, i32)> {
        self.tiles
            .keys()
            .filter(|(x, z, _)| *x == tile_x && *z == tile_z)
            .copied()
            .collect()
    }

    pub fn snapshot_page(
        &mut self,
        bounds: Option<[i32; 4]>,
        after: Option<(i32, i32, i32)>,
        limit: usize,
        world_epoch: u64,
        world_revision: u64,
        tick: u64,
    ) -> (Vec<SnowSurfaceFrame>, Option<(i32, i32, i32)>) {
        let (lower, upper, z_bounds) = bounds.map_or_else(
            || (std::ops::Bound::Unbounded, std::ops::Bound::Unbounded, None),
            |[min_x, max_x, min_z, max_z]| {
                (
                    std::ops::Bound::Included((min_x, i32::MIN, i32::MIN)),
                    std::ops::Bound::Included((max_x, i32::MAX, i32::MAX)),
                    Some((min_z, max_z)),
                )
            },
        );
        let in_z = |(key, _): &(&(i32, i32, i32), &Vec<Cohort>)| {
            z_bounds.is_none_or(|(min_z, max_z)| (min_z..=max_z).contains(&key.1))
        };
        let mut page = Vec::with_capacity(limit.saturating_add(1).min(17));
        // A player may move between pages, making the previous cursor sort
        // after the new upper bound. Restart within the new interest instead
        // of constructing an inverted BTreeMap range.
        let valid_after = after.filter(|(x, z, _)| {
            bounds.is_none_or(|[min_x, max_x, min_z, max_z]| {
                (min_x..=max_x).contains(x) && (min_z..=max_z).contains(z)
            })
        });
        let start = valid_after.map_or(lower, std::ops::Bound::Excluded);
        for (key, _) in self
            .tiles
            .range((start, upper))
            .filter(in_z)
            .take(limit.saturating_add(1))
        {
            page.push(*key);
        }
        let has_more = page.len() > limit;
        page.truncate(limit);
        let next = has_more.then(|| page.last().copied()).flatten();
        let frames = page
            .into_iter()
            .filter_map(|(x, z, y)| self.frame(x, z, y, world_epoch, world_revision, tick))
            .collect();
        (frames, next)
    }

    /// Bounded nearest-first page for the current regional interest. It visits
    /// tile coordinates on deterministic Chebyshev rings around the player,
    /// avoiding a sort over all world snow surfaces on each network tick.
    pub fn nearest_snapshot_page(
        &mut self,
        bounds: [i32; 4],
        center: (i32, i32),
        after: Option<SnowSnapshotCursor>,
        limit: usize,
        world_epoch: u64,
        world_revision: u64,
        tick: u64,
    ) -> (Vec<SnowSurfaceFrame>, Option<SnowSnapshotCursor>) {
        let [min_x, max_x, min_z, max_z] = bounds;
        if min_x > max_x || min_z > max_z || limit == 0 {
            return (Vec::new(), None);
        }
        let center = (center.0.clamp(min_x, max_x), center.1.clamp(min_z, max_z));
        let max_radius = [min_x, max_x]
            .into_iter()
            .flat_map(|x| [min_z, max_z].into_iter().map(move |z| (x, z)))
            .map(|(x, z)| x.abs_diff(center.0).max(z.abs_diff(center.1)))
            .max()
            .unwrap_or(0);
        let after = after.filter(|cursor| {
            cursor.center == center
                && (min_x..=max_x).contains(&cursor.key.0)
                && (min_z..=max_z).contains(&cursor.key.1)
                && self.tiles.contains_key(&cursor.key)
        });
        let start_radius = after.map_or(0, |key| {
            key.key
                .0
                .abs_diff(center.0)
                .max(key.key.1.abs_diff(center.1))
        });
        let mut keys = Vec::with_capacity(limit.saturating_add(1));
        let mut past_cursor = after.is_none();
        'rings: for radius in start_radius..=max_radius {
            let mut ring = Vec::with_capacity(if radius == 0 {
                1
            } else {
                (radius as usize).saturating_mul(8)
            });
            if radius == 0 {
                ring.push(center);
            } else {
                let r = i32::try_from(radius).unwrap_or(i32::MAX);
                for dx in -r..=r {
                    ring.push((center.0.saturating_add(dx), center.1.saturating_sub(r)));
                }
                for dz in (-r + 1)..=r {
                    ring.push((center.0.saturating_add(r), center.1.saturating_add(dz)));
                }
                for dx in (-r..r).rev() {
                    ring.push((center.0.saturating_add(dx), center.1.saturating_add(r)));
                }
                for dz in ((-r + 1)..r).rev() {
                    ring.push((center.0.saturating_sub(r), center.1.saturating_add(dz)));
                }
            }
            for (x, z) in ring {
                if !(min_x..=max_x).contains(&x) || !(min_z..=max_z).contains(&z) {
                    continue;
                }
                if !past_cursor {
                    if after.is_some_and(|cursor| cursor.key.0 == x && cursor.key.1 == z) {
                        past_cursor = true;
                    } else {
                        continue;
                    }
                }
                let tile_lower = (x, z, i32::MIN);
                let tile_upper = (x, z, i32::MAX);
                for (key, _) in self.tiles.range((
                    std::ops::Bound::Included(tile_lower),
                    std::ops::Bound::Included(tile_upper),
                )) {
                    if after.is_some_and(|cursor| {
                        cursor.key.0 == x && cursor.key.1 == z && key.2 <= cursor.key.2
                    }) {
                        continue;
                    }
                    keys.push(*key);
                    if keys.len() > limit {
                        break 'rings;
                    }
                }
            }
        }
        let has_more = keys.len() > limit;
        keys.truncate(limit);
        let last = keys.last().copied();
        let frames = keys
            .into_iter()
            .filter_map(|(x, z, y)| self.frame(x, z, y, world_epoch, world_revision, tick))
            .collect();
        (
            frames,
            has_more
                .then_some(last)
                .flatten()
                .map(|key| SnowSnapshotCursor { center, key }),
        )
    }

    pub fn mark_geometry_changed(&mut self, key: (i32, i32, i32)) {
        self.geometry_changed.insert(key);
    }

    pub fn take_geometry_changed_keys(&mut self) -> Vec<(i32, i32, i32)> {
        std::mem::take(&mut self.geometry_changed)
            .into_iter()
            .collect()
    }

    /// Exact persistence bytes for one geometry journal event, including Q32
    /// depth and climate cursor but excluding transient footprints.
    pub fn tile_canonical_bytes(&mut self, key: (i32, i32, i32)) -> Result<Vec<u8>, String> {
        self.catch_up_key(key);
        let tile = self.tiles.get(&key).cloned().unwrap_or_default();
        let body = postcard::to_stdvec(&tile).map_err(|error| error.to_string())?;
        let mut bytes = Vec::with_capacity(body.len() + 2);
        bytes.extend_from_slice(&SNOW_STATE_SCHEMA.to_le_bytes());
        bytes.extend_from_slice(&body);
        Ok(bytes)
    }

    pub fn restore_tile_canonical_bytes(
        &mut self,
        key: (i32, i32, i32),
        bytes: &[u8],
    ) -> Result<(), String> {
        if bytes.len() < 2 || bytes.len() > 64 * 1024 {
            return Err("snow tile journal payload exceeds 64 KiB".into());
        }
        if u16::from_le_bytes([bytes[0], bytes[1]]) != SNOW_STATE_SCHEMA {
            return Err("unsupported snow tile journal schema".into());
        }
        let cohorts: Vec<Cohort> = postcard::from_bytes(&bytes[2..]).map_err(|e| e.to_string())?;
        if cohorts.len() > 256 {
            return Err("snow tile journal contains too many cohorts".into());
        }
        let mut occupied = [0u64; 4];
        let mut ids = BTreeSet::new();
        for cohort in &cohorts {
            if cohort.generation == 0
                || cohort.id != surface_id(key.0, key.1, key.2, cohort.generation)
                || !ids.insert(cohort.id)
                || cohort.climate_through_tick > self.climate_through_tick
                || cohort.depth_q32 > MAX_DEPTH_Q32
                || cohort.mask.iter().all(|word| *word == 0)
            {
                return Err("snow tile journal has invalid cohort state".into());
            }
            for (seen, mask) in occupied.iter_mut().zip(cohort.mask) {
                if *seen & mask != 0 {
                    return Err("snow tile journal cohort masks overlap".into());
                }
                *seen |= mask;
            }
        }
        if let Some(old) = self.tiles.remove(&key) {
            for cohort in old {
                self.remove_cursor(cohort.climate_through_tick);
            }
        }
        for cohort in &cohorts {
            self.add_cursor(cohort.climate_through_tick);
        }
        self.tiles.insert(key, cohorts);
        self.footprints.remove(&key);
        self.changed.insert(key);
        self.dirty = true;
        Ok(())
    }

    /// Versioned canonical persistence payload. Footprints and per-client
    /// scheduling state are intentionally omitted; tracks restart empty.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, String> {
        self.validate_state()?;
        let body = postcard::to_stdvec(self).map_err(|error| error.to_string())?;
        let total = body.len().saturating_add(2);
        if total > MAX_SNOW_STATE_BYTES {
            return Err("snow state exceeds its 64 MiB canonical payload bound".into());
        }
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(&SNOW_STATE_SCHEMA.to_le_bytes());
        bytes.extend_from_slice(&body);
        Ok(bytes)
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 2 || bytes.len() > MAX_SNOW_STATE_BYTES {
            return Err("snow state payload length is outside its bounds".into());
        }
        let schema = u16::from_le_bytes([bytes[0], bytes[1]]);
        if schema != SNOW_STATE_SCHEMA {
            return Err(format!("unsupported snow state schema {schema}"));
        }
        let mut state: Self =
            postcard::from_bytes(&bytes[2..]).map_err(|error| error.to_string())?;
        state.validate_state()?;
        for (key, cohorts) in &state.tiles {
            for cohort in cohorts {
                *state
                    .cursor_counts
                    .entry(cohort.climate_through_tick)
                    .or_default() += 1;
            }
            state.changed.insert(*key);
        }
        state.footprints.clear();
        state.next_footprint_id = 0;
        state.dirty = true;
        Ok(state)
    }

    fn validate_state(&self) -> Result<(), String> {
        if self.climate.len() > MAX_CLIMATE_SEGMENTS
            || self.initial_transform.low < 0
            || self.initial_transform.high > i128::from(MAX_DEPTH_Q32)
            || self.initial_transform.low > self.initial_transform.high
        {
            return Err("snow state climate transform is out of bounds".into());
        }
        let mut prior_end = None;
        for segment in &self.climate {
            if segment.start_tick == 0
                || segment.start_tick > segment.end_tick
                || segment.end_tick > self.climate_through_tick
                || prior_end.is_some_and(|end: u64| segment.start_tick != end.saturating_add(1))
                || segment.delta_q32_per_tick.unsigned_abs() > 150_000
            {
                return Err("snow climate history is invalid".into());
            }
            prior_end = Some(segment.end_tick);
        }
        let mut ids = BTreeSet::new();
        for (key, cohorts) in &self.tiles {
            if cohorts.len() > 256 {
                return Err("snow tile has more than 256 cohorts".into());
            }
            let mut occupied = [0u64; 4];
            for cohort in cohorts {
                if cohort.generation == 0
                    || cohort.id != surface_id(key.0, key.1, key.2, cohort.generation)
                    || !ids.insert(cohort.id)
                    || cohort.depth_q32 > MAX_DEPTH_Q32
                    || cohort.climate_through_tick > self.climate_through_tick
                    || cohort.mask.iter().all(|word| *word == 0)
                {
                    return Err("snow cohort is outside its bounds".into());
                }
                for (seen, mask) in occupied.iter_mut().zip(cohort.mask) {
                    if *seen & mask != 0 {
                        return Err("snow cohort masks overlap".into());
                    }
                    *seen |= mask;
                }
            }
        }
        Ok(())
    }

    /// Reconciles one authoritative tile/Y layer. Existing exposure retains
    /// its accumulated depth; newly exposed faces receive a zero-depth cohort.
    pub fn reconcile(
        &mut self,
        tile_x: i32,
        tile_z: i32,
        top_cell_y: i32,
        geometry_revision: u64,
        exposed: [u64; 4],
        tick: u64,
    ) {
        self.reconcile_at(
            tile_x,
            tile_z,
            top_cell_y,
            geometry_revision,
            exposed,
            tick,
            tick,
        );
    }

    pub fn reconcile_at(
        &mut self,
        tile_x: i32,
        tile_z: i32,
        top_cell_y: i32,
        geometry_revision: u64,
        exposed: [u64; 4],
        exposure_birth_tick: u64,
        current_tick: u64,
    ) {
        let key = (tile_x, tile_z, top_cell_y);
        self.catch_up_key(key);
        let old = self.tiles.remove(&key).unwrap_or_default();
        let mut remaining = exposed;
        let mut next = Vec::new();
        let mut generation = old.iter().map(|c| c.generation).max().unwrap_or(0);
        for mut cohort in old {
            let retained = std::array::from_fn(|i| cohort.mask[i] & remaining[i]);
            for (r, m) in remaining.iter_mut().zip(retained) {
                *r &= !m;
            }
            cohort.mask = retained;
            cohort.geometry_revision = geometry_revision;
            if cohort.mask.iter().any(|word| *word != 0) {
                next.push(cohort);
            } else {
                self.remove_cursor(cohort.climate_through_tick);
            }
        }
        if remaining.iter().any(|word| *word != 0) {
            generation = generation.saturating_add(1).max(1);
            next.push(Cohort {
                id: surface_id(tile_x, tile_z, top_cell_y, generation),
                mask: remaining,
                generation,
                depth_q32: 0,
                climate_through_tick: exposure_birth_tick,
                geometry_revision,
            });
            self.add_cursor(exposure_birth_tick);
        }
        next.sort_by_key(|c| c.id);
        self.tiles.insert(key, next);
        self.prune_footprints(key, current_tick);
        self.changed.insert(key);
        self.dirty = true;
    }

    /// First discovery of unchanged world terrain catches up from the world
    /// climate prefix. Post-edit exposure must use [`Self::reconcile`] instead.
    pub fn discover_initial(
        &mut self,
        tile_x: i32,
        tile_z: i32,
        top_cell_y: i32,
        geometry_revision: u64,
        exposed: [u64; 4],
    ) {
        let key = (tile_x, tile_z, top_cell_y);
        if self.tiles.contains_key(&key) || exposed.iter().all(|word| *word == 0) {
            return;
        }
        self.tiles.insert(
            key,
            vec![Cohort {
                id: surface_id(tile_x, tile_z, top_cell_y, 1),
                mask: exposed,
                generation: 1,
                depth_q32: self.initial_transform.apply(0),
                climate_through_tick: self.climate_through_tick,
                geometry_revision,
            }],
        );
        self.add_cursor(self.climate_through_tick);
        self.changed.insert(key);
        self.dirty = true;
    }

    /// Adds this tick's composable clamped-depth operation, then catches up a
    /// fixed fair slice of known tiles. Initial-world discovery uses the
    /// cumulative transform; edited cohorts replay only retained suffix spans.
    pub fn advance(&mut self, weather: &WeatherState, tick: u64) -> Result<(), String> {
        let precipitation = i64::from(weather.precipitation_q16);
        let snowfall = matches!(weather.kind, WeatherKind::Snow | WeatherKind::Blizzard)
            && weather.temperature_centi_c <= 0;
        let temperature = i64::from(weather.temperature_centi_c.clamp(0, 2_000));
        let (numerator, denominator) = if snowfall {
            (
                2 * Q32_PER_METRE as i64 * precipitation,
                60 * 1_000 * 65_535,
            )
        } else {
            (-(Q32_PER_METRE as i64 * temperature), 60 * 1_000 * 2_000)
        };
        let delta = div_round_nearest(numerator, denominator);
        let from = self.climate_through_tick.saturating_add(1);
        if tick < from {
            return Ok(());
        }
        let can_extend = self.climate.back().is_some_and(|last| {
            last.delta_q32_per_tick == delta && last.end_tick.saturating_add(1) == from
        });
        if !can_extend && self.climate.len() >= MAX_CLIMATE_SEGMENTS {
            return Err("snow climate history reached its explicit 4096 segment bound".into());
        }
        let steps = tick - from + 1;
        let operation = DepthTransform::repeated(delta, steps);
        self.initial_transform = self.initial_transform.then(operation);
        if can_extend {
            let last = self.climate.back_mut().expect("checked above");
            last.end_tick = tick;
        } else {
            self.climate.push_back(ClimateSegment {
                start_tick: from,
                end_tick: tick,
                delta_q32_per_tick: delta,
            });
        }
        self.climate_through_tick = tick;
        self.dirty |= delta != 0;
        let mut keys = Vec::new();
        if let Some(cursor) = self.catchup_cursor {
            keys.extend(
                self.tiles
                    .range((
                        std::ops::Bound::Excluded(cursor),
                        std::ops::Bound::Unbounded,
                    ))
                    .take(SNOW_UPDATE_TILES_PER_TICK)
                    .map(|(key, _)| *key),
            );
            if keys.len() < SNOW_UPDATE_TILES_PER_TICK {
                keys.extend(
                    self.tiles
                        .range(..=cursor)
                        .take(SNOW_UPDATE_TILES_PER_TICK - keys.len())
                        .map(|(key, _)| *key),
                );
            }
        } else {
            keys.extend(self.tiles.keys().take(SNOW_UPDATE_TILES_PER_TICK).copied());
        }
        for key in &keys {
            self.catch_up_key(*key);
        }
        self.catchup_cursor = keys.last().copied();
        self.prune_history();
        let before: usize = self.footprints.values().map(Vec::len).sum();
        let mut expired = Vec::new();
        self.footprints.retain(|key, marks| {
            let prior = marks.len();
            marks.retain(|mark| tick.saturating_sub(mark.created_tick) <= 18_000);
            if marks.len() != prior {
                expired.push(*key);
            }
            !marks.is_empty()
        });
        self.changed.extend(expired);
        self.dirty |= before != self.footprints.values().map(Vec::len).sum::<usize>();
        if delta != 0 {
            let effect_q32 = i128::from(delta)
                .unsigned_abs()
                .saturating_mul(u128::from(steps));
            let effect_q32 = effect_q32.min(u128::from(u64::MAX)) as u64;
            let mut changed = Vec::new();
            self.footprints.retain(|key, marks| {
                for mark in marks.iter_mut() {
                    let credit = self.footprint_credits.entry(mark.id).or_default();
                    *credit = credit.saturating_add(effect_q32);
                    let lost = (*credit / (Q32_PER_METRE / 500)).min(u64::from(mark.strength_q8));
                    if lost > 0 {
                        mark.strength_q8 = mark.strength_q8.saturating_sub(lost as u8);
                        *credit %= Q32_PER_METRE / 500;
                        changed.push(*key);
                    }
                }
                let before = marks.len();
                if delta < 0 {
                    marks.retain(|mark| {
                        self.tiles.get(key).is_some_and(|cohorts| {
                            cohorts.iter().any(|cohort| {
                                cohort.id == mark.surface_id
                                    && cohort.depth_q32 >= Q32_PER_METRE / 500
                            })
                        })
                    });
                }
                marks.retain(|mark| mark.strength_q8 > 0);
                if marks.len() != before {
                    changed.push(*key);
                }
                !marks.is_empty()
            });
            for key in changed {
                self.changed.insert(key);
            }
        }
        let active: BTreeSet<_> = self
            .footprints
            .values()
            .flatten()
            .map(|mark| mark.id)
            .collect();
        self.footprint_credits.retain(|id, _| active.contains(id));
        Ok(())
    }

    /// Validates a server-derived candidate against the actual loaded terrain
    /// face, then adds it to the bounded per-layer frame.
    pub fn stamp(
        &mut self,
        candidate: crate::footprints::FootfallCandidate,
        cell: [i32; 3],
        geometry_revision: u64,
        tick: u64,
    ) -> bool {
        let tile_x = cell[0].div_euclid(SNOW_TILE_CELLS);
        let tile_z = cell[2].div_euclid(SNOW_TILE_CELLS);
        let face = cell[2].rem_euclid(SNOW_TILE_CELLS) as usize * 16
            + cell[0].rem_euclid(SNOW_TILE_CELLS) as usize;
        let key = (tile_x, tile_z, cell[1]);
        self.catch_up_key(key);
        let Some(cohort) = self.tiles.get(&key).and_then(|cohorts| {
            cohorts.iter().find(|c| {
                c.geometry_revision == geometry_revision
                    && c.depth_q32 >> 16 >= 132
                    && c.mask[face / 64] & (1u64 << (face % 64)) != 0
            })
        }) else {
            return false;
        };
        let x = (candidate.position_m[0] * 1000.0).round();
        let z = (candidate.position_m[2] * 1000.0).round();
        let ox = (x - f64::from(cell[0]) * 250.0).clamp(0.0, 249.0) as u16;
        let oz = (z - f64::from(cell[2]) * 250.0).clamp(0.0, 249.0) as u16;
        self.next_footprint_id = self.next_footprint_id.saturating_add(1).max(1);
        let mark = SnowFootprint {
            id: self.next_footprint_id,
            surface_id: cohort.id,
            geometry_revision,
            contact_cell: cell,
            offset_mm: [ox, oz],
            heading_q15: candidate.heading_q15,
            foot: candidate.foot,
            created_tick: tick,
            strength_q8: 255,
        };
        let marks = self.footprints.entry(key).or_default();
        if marks.iter().any(|m| m.id == mark.id) {
            return false;
        }
        marks.push(mark);
        self.footprint_credits.insert(mark.id, 0);
        marks.sort_by_key(|m| m.id);
        if marks.len() > 64 {
            marks.remove(0);
        }
        self.changed.insert(key);
        while self.footprints.values().map(Vec::len).sum::<usize>() > 4_096 {
            let oldest = self
                .footprints
                .iter()
                .filter_map(|(key, values)| values.first().map(|mark| (*key, mark.id)))
                .min_by_key(|(_, id)| *id);
            let Some((old_key, old_id)) = oldest else {
                break;
            };
            if let Some(values) = self.footprints.get_mut(&old_key) {
                values.retain(|mark| mark.id != old_id);
                if values.is_empty() {
                    self.footprints.remove(&old_key);
                }
            }
            self.changed.insert(old_key);
        }
        self.dirty = true;
        true
    }

    pub fn frame(
        &mut self,
        tile_x: i32,
        tile_z: i32,
        top_cell_y: i32,
        world_epoch: u64,
        world_revision: u64,
        tick: u64,
    ) -> Option<SnowSurfaceFrame> {
        let key = (tile_x, tile_z, top_cell_y);
        self.catch_up_key(key);
        let cohorts = self.tiles.get(&key).map(Vec::as_slice).unwrap_or_default();
        self.snapshot_seq = self.snapshot_seq.saturating_add(1).max(1);
        let surfaces = cohorts
            .iter()
            .map(|c| SnowSurface {
                id: c.id,
                top_cell_y,
                geometry_revision: c.geometry_revision,
                exposure_generation: c.generation,
                exposed_faces: c.mask,
                depth_q16: (c.depth_q32 >> 16).min(u64::from(SNOW_MAX_DEPTH_Q16)) as u16,
            })
            .collect();
        let footprints = self.footprints.get(&key).cloned().unwrap_or_default();
        let frame = SnowSurfaceFrame {
            schema: SNOW_SCHEMA,
            world_epoch,
            world_revision,
            effective_tick: tick,
            snapshot_seq: self.snapshot_seq,
            tile_x,
            tile_z,
            top_cell_y,
            surfaces,
            footprints,
        };
        frame.validate().ok()?;
        Some(frame)
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }
    pub fn take_changed_keys(&mut self) -> Vec<(i32, i32, i32)> {
        std::mem::take(&mut self.changed).into_iter().collect()
    }

    pub fn take_changed_frames(
        &mut self,
        world_epoch: u64,
        world_revision: u64,
        tick: u64,
    ) -> Vec<SnowSurfaceFrame> {
        let keys = self.take_changed_keys();
        keys.into_iter()
            .filter_map(|(x, z, y)| self.frame(x, z, y, world_epoch, world_revision, tick))
            .collect()
    }
    fn catch_up_key(&mut self, key: (i32, i32, i32)) {
        let mut changed = false;
        let mut cursor_changes = Vec::new();
        let Some(cohorts) = self.tiles.get_mut(&key) else {
            return;
        };
        for cohort in cohorts {
            if cohort.climate_through_tick >= self.climate_through_tick {
                continue;
            }
            let previous_cursor = cohort.climate_through_tick;
            let mut cursor = cohort.climate_through_tick.saturating_add(1);
            for segment in &self.climate {
                let start = cursor.max(segment.start_tick);
                if start > segment.end_tick {
                    continue;
                }
                let transform = DepthTransform::repeated(
                    segment.delta_q32_per_tick,
                    segment.end_tick - start + 1,
                );
                let next = transform.apply(cohort.depth_q32);
                changed |= next != cohort.depth_q32;
                cohort.depth_q32 = next;
                cursor = segment.end_tick.saturating_add(1);
                if cursor > self.climate_through_tick {
                    break;
                }
            }
            cohort.climate_through_tick = self.climate_through_tick;
            cursor_changes.push(previous_cursor);
        }
        let cursor_advanced = !cursor_changes.is_empty();
        for previous_cursor in cursor_changes {
            self.remove_cursor(previous_cursor);
            self.add_cursor(self.climate_through_tick);
        }
        if changed {
            self.changed.insert(key);
        }
        self.dirty |= changed || cursor_advanced;
    }

    fn prune_history(&mut self) {
        let mut minimum = self
            .cursor_counts
            .first_key_value()
            .map_or(self.climate_through_tick, |(tick, _)| *tick);
        minimum = minimum.min(
            self.pending_scans
                .values()
                .map(|pending| pending.birth_tick)
                .filter(|tick| *tick > 0)
                .min()
                .unwrap_or(self.climate_through_tick),
        );
        while self
            .climate
            .front()
            .is_some_and(|segment| segment.end_tick <= minimum)
        {
            self.climate.pop_front();
        }
    }

    fn add_cursor(&mut self, tick: u64) {
        *self.cursor_counts.entry(tick).or_default() += 1;
    }

    fn remove_cursor(&mut self, tick: u64) {
        if let Some(count) = self.cursor_counts.get_mut(&tick) {
            *count -= 1;
            if *count == 0 {
                self.cursor_counts.remove(&tick);
            }
        }
    }
    fn prune_footprints(&mut self, key: (i32, i32, i32), tick: u64) {
        let Some(surfaces) = self.tiles.get(&key) else {
            return;
        };
        let marks = self.footprints.entry(key).or_default();
        marks.retain(|mark| {
            mark.created_tick.saturating_add(18_000) >= tick
                && surfaces.iter().any(|c| {
                    c.id == mark.surface_id
                        && c.geometry_revision == mark.geometry_revision
                        && c.mask[((mark.contact_cell[2].rem_euclid(16) * 16
                            + mark.contact_cell[0].rem_euclid(16))
                            / 64) as usize]
                            & (1u64
                                << ((mark.contact_cell[2].rem_euclid(16) * 16
                                    + mark.contact_cell[0].rem_euclid(16))
                                    % 64))
                            != 0
                })
        });
        if marks.is_empty() {
            self.footprints.remove(&key);
        }
        let active: BTreeSet<_> = self
            .footprints
            .values()
            .flatten()
            .map(|mark| mark.id)
            .collect();
        self.footprint_credits.retain(|id, _| active.contains(id));
    }
}

fn surface_id(tile_x: i32, tile_z: i32, y: i32, generation: u32) -> SnowSurfaceId {
    let mut bytes = [0; 16];
    for (chunk, value) in bytes
        .chunks_exact_mut(4)
        .zip([tile_x, tile_z, y, generation as i32])
    {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    SnowSurfaceId(bytes)
}

fn div_round_nearest(numerator: i64, denominator: i64) -> i64 {
    if numerator < 0 {
        -((-numerator + denominator / 2) / denominator)
    } else {
        (numerator + denominator / 2) / denominator
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::EntityId;

    fn weather(kind: WeatherKind, temp: i16) -> WeatherState {
        let mut state = WeatherState::initial(
            1,
            1,
            spall_protocol::WeatherClock {
                time_ms: 0,
                source: spall_protocol::WeatherClockSource::ServerFallback,
                start_season: 3,
                season_ms: 900_000,
            },
        );
        state.force_kind(Some(kind));
        state.temperature_centi_c = temp;
        state
    }

    fn mask(bits: &[usize]) -> [u64; 4] {
        let mut mask = [0; 4];
        for bit in bits {
            mask[*bit / 64] |= 1u64 << (*bit % 64);
        }
        mask
    }

    #[test]
    fn cold_accumulation_is_thin_and_warm_profile_melts_it() {
        let mut snow = SnowState::new();
        snow.reconcile(0, 0, 4, 7, mask(&[3]), 0);
        let cold = weather(WeatherKind::Snow, -800);
        for tick in 1..=1_800 {
            snow.advance(&cold, tick).unwrap();
        }
        let frame = snow.frame(0, 0, 4, 1, 0, 1_800).unwrap();
        let after_cold = frame.surfaces[0].depth_q16;
        assert!((1_000..=1_400).contains(&after_cold), "{after_cold} Q16");
        let warm = weather(WeatherKind::Rain, 2_000);
        for tick in 1_801..=3_600 {
            snow.advance(&warm, tick).unwrap();
        }
        let frame = snow.frame(0, 0, 4, 1, 0, 3_600).unwrap();
        assert_eq!(frame.surfaces[0].depth_q16, 0);
    }

    #[test]
    fn roof_removal_keeps_old_depth_and_starts_new_exposure_empty() {
        let mut snow = SnowState::new();
        snow.reconcile(0, 0, 9, 4, mask(&[0, 1]), 0);
        let cold = weather(WeatherKind::Snow, -800);
        for tick in 1..=1_800 {
            snow.advance(&cold, tick).unwrap();
        }
        let old = snow.frame(0, 0, 9, 1, 0, 1_800).unwrap().surfaces[0].depth_q16;
        snow.reconcile(0, 0, 9, 5, mask(&[0, 1, 2]), 1_801);
        let frame = snow.frame(0, 0, 9, 1, 0, 1_801).unwrap();
        assert_eq!(frame.surfaces.len(), 2);
        let preserved = frame
            .surfaces
            .iter()
            .find(|s| s.exposed_faces[0] & 0b11 != 0)
            .unwrap();
        let revealed = frame
            .surfaces
            .iter()
            .find(|s| s.exposed_faces[0] & 0b100 != 0)
            .unwrap();
        assert_eq!(preserved.depth_q16, old);
        assert_eq!(revealed.depth_q16, 0);
        assert!(frame.surfaces.iter().all(|s| s.geometry_revision == 5));
    }

    #[test]
    fn footprint_requires_exposed_deep_face_and_frames_include_tombstones() {
        let mut snow = SnowState::new();
        snow.reconcile(0, 0, 7, 2, mask(&[0]), 0);
        let deep = weather(WeatherKind::Snow, -800);
        for tick in 1..=20_000 {
            snow.advance(&deep, tick).unwrap();
        }
        let candidate = crate::footprints::FootfallCandidate {
            player: EntityId::new(1).unwrap(),
            position_m: [0.125, 2.0, 0.125],
            heading_q15: 0,
            foot: 0,
            tick: 20_000,
        };
        assert!(snow.stamp(candidate, [0, 7, 0], 2, 20_000));
        assert_eq!(
            snow.frame(0, 0, 7, 1, 0, 20_000).unwrap().footprints.len(),
            1
        );
        snow.take_changed_keys();
        snow.reconcile(0, 0, 7, 3, [0; 4], 20_001);
        let tombstone = snow.frame(0, 0, 7, 1, 0, 20_001).unwrap();
        assert!(tombstone.surfaces.is_empty());
        assert!(tombstone.footprints.is_empty());
    }

    #[test]
    fn initial_discovery_matches_tickwise_and_batched_clamped_history() {
        let cold = weather(WeatherKind::Blizzard, -800);
        let warm = weather(WeatherKind::Rain, 2_000);
        let mut tickwise = SnowState::new();
        tickwise.discover_initial(0, 0, 5, 1, mask(&[64]));
        for tick in 1..=120 {
            tickwise.advance(&cold, tick).unwrap();
        }
        for tick in 121..=180 {
            tickwise.advance(&warm, tick).unwrap();
        }
        for tick in 181..=360 {
            tickwise.advance(&cold, tick).unwrap();
        }

        let mut batched = SnowState::new();
        batched.advance(&cold, 120).unwrap();
        batched.advance(&warm, 180).unwrap();
        batched.advance(&cold, 360).unwrap();
        batched.discover_initial(0, 0, 5, 1, mask(&[64]));

        assert_eq!(
            tickwise.frame(0, 0, 5, 1, 0, 360).unwrap().surfaces[0].depth_q16,
            batched.frame(0, 0, 5, 1, 0, 360).unwrap().surfaces[0].depth_q16
        );
    }

    #[test]
    fn fair_slice_is_bounded_and_stale_tile_replays_ordered_warm_cold_spans() {
        let mut state = SnowState::new();
        for x in 0..40 {
            state.reconcile(x, 0, 0, 1, mask(&[0]), 0);
        }
        state.take_changed_keys();
        let cold = weather(WeatherKind::Snow, -800);
        let warm = weather(WeatherKind::Rain, 2_000);
        state.advance(&cold, 1).unwrap();
        let processed = state.take_changed_keys();
        assert!(processed.len() <= SNOW_UPDATE_TILES_PER_TICK);
        state.advance(&warm, 2).unwrap();
        for tick in 3..=40 {
            state.advance(&warm, tick).unwrap();
        }
        let lagging = state.frame(39, 0, 0, 1, 0, 40).unwrap().surfaces[0].depth_q16;

        let mut reference = SnowState::new();
        reference.reconcile(0, 0, 0, 1, mask(&[0]), 0);
        reference.advance(&cold, 1).unwrap();
        for tick in 2..=40 {
            reference.advance(&warm, tick).unwrap();
        }
        let expected = reference.frame(0, 0, 0, 1, 0, 40).unwrap().surfaces[0].depth_q16;
        assert_eq!(lagging, expected, "climate spans retain operation order");
    }

    #[test]
    fn pending_discovery_retains_birth_and_unknown_retry_backoff() {
        let mut state = SnowState::new();
        state.enqueue_scan((4, 0), 0);
        state.enqueue_edited_scan((9, 0), 12);
        let first = state.take_scan_batch(&[(4, 0), (9, 0)], 1, 20);
        assert_eq!(first, vec![((4, 0), 0)]);
        state.retry_scan((4, 0), 0, 20);
        state.enqueue_scan((4, 0), 0);
        let restored =
            SnowState::from_canonical_bytes(&state.to_canonical_bytes().unwrap()).unwrap();
        assert!(restored.pending_scans.contains_key(&(9, 0)));
        assert_eq!(restored.pending_scans[&(9, 0)].birth_tick, 12);
        assert!(
            !state
                .take_scan_batch(&[(4, 0)], 1, 79)
                .iter()
                .any(|(tile, _)| *tile == (4, 0))
        );
        assert_eq!(state.take_scan_batch(&[(4, 0)], 1, 80), vec![((4, 0), 0)]);
    }

    #[test]
    fn snapshot_pages_are_bounded_and_cycle_through_keys() {
        let mut state = SnowState::new();
        for x in 0..11 {
            state.reconcile(x, 0, 0, 1, mask(&[0]), 0);
        }
        let mut cursor = None;
        let mut seen = Vec::new();
        for _ in 0..4 {
            let (page, next) = state.snapshot_page(Some([0, 10, 0, 0]), cursor, 3, 1, 0, 0);
            assert!(page.len() <= 3);
            seen.extend(page.iter().map(|frame| frame.tile_x));
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen, (0..11).collect::<Vec<_>>());
        let (moved_page, moved_cursor) =
            state.snapshot_page(Some([0, 0, 0, 0]), Some((10, 0, 0)), 3, 1, 0, 0);
        assert_eq!(moved_page.len(), 1);
        assert_eq!(moved_page[0].tile_x, 0);
        assert!(moved_cursor.is_none());
    }

    #[test]
    fn nearest_snapshot_pages_walk_outward_and_reset_on_center_move() {
        let mut state = SnowState::new();
        for x in -3..=3 {
            for z in -3..=3 {
                state.reconcile(x, z, 0, 1, mask(&[0]), 0);
            }
        }
        let mut cursor = None;
        let mut distances = Vec::new();
        let mut keys = Vec::new();
        for _ in 0..20 {
            let (page, next) =
                state.nearest_snapshot_page([-3, 3, -3, 3], (0, 0), cursor, 3, 1, 0, 0);
            assert!(page.len() <= 3);
            for frame in &page {
                distances.push(frame.tile_x.abs().max(frame.tile_z.abs()));
                keys.push((frame.tile_x, frame.tile_z, frame.top_cell_y));
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(keys.len(), 49);
        assert_eq!(keys[0], (0, 0, 0));
        assert!(distances.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(keys.iter().copied().collect::<BTreeSet<_>>().len(), 49);

        let (_, old_center_cursor) =
            state.nearest_snapshot_page([-3, 3, -3, 3], (0, 0), None, 1, 1, 0, 0);
        let (moved, _) =
            state.nearest_snapshot_page([-3, 3, -3, 3], (1, 1), old_center_cursor, 1, 1, 0, 0);
        assert_eq!((moved[0].tile_x, moved[0].tile_z), (1, 1));
    }

    #[test]
    fn fresh_snow_fades_marks_even_at_depth_cap_and_warm_melt_removes_them() {
        let mut snow = SnowState::new();
        snow.reconcile(0, 0, 7, 2, mask(&[0]), 0);
        let cold = weather(WeatherKind::Snow, -800);
        for tick in 1..=20_000 {
            snow.advance(&cold, tick).unwrap();
        }
        let candidate = crate::footprints::FootfallCandidate {
            player: EntityId::new(1).unwrap(),
            position_m: [0.125, 2.0, 0.125],
            heading_q15: 0,
            foot: 0,
            tick: 20_000,
        };
        assert!(snow.stamp(candidate, [0, 7, 0], 2, 20_000));
        let before = snow.frame(0, 0, 7, 1, 0, 20_000).unwrap().footprints[0].strength_q8;
        for tick in 20_001..=20_600 {
            snow.advance(&cold, tick).unwrap();
        }
        let covered = snow.frame(0, 0, 7, 1, 0, 20_600).unwrap();
        assert_eq!(covered.surfaces[0].depth_q16, SNOW_MAX_DEPTH_Q16);
        assert!(covered.footprints[0].strength_q8 < before);
        let warm = weather(WeatherKind::Rain, 2_000);
        for tick in 20_601..=29_000 {
            snow.advance(&warm, tick).unwrap();
        }
        assert!(
            snow.frame(0, 0, 7, 1, 0, 29_000)
                .unwrap()
                .footprints
                .is_empty()
        );
    }

    #[test]
    fn canonical_restart_preserves_snow_and_climate_continuation_but_clears_tracks() {
        let mut live = SnowState::new();
        live.reconcile(0, 0, 3, 11, mask(&[0]), 0);
        let cold = weather(WeatherKind::Snow, -800);
        for tick in 1..=1_200 {
            live.advance(&cold, tick).unwrap();
        }
        let before = live.frame(0, 0, 3, 1, 0, 1_200).unwrap().surfaces[0].depth_q16;
        let bytes = live.to_canonical_bytes().unwrap();
        let mut restored = SnowState::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(
            restored.frame(0, 0, 3, 1, 0, 1_200).unwrap().surfaces[0].depth_q16,
            before
        );
        let warm = weather(WeatherKind::Rain, 2_000);
        for tick in 1_201..=1_800 {
            live.advance(&warm, tick).unwrap();
            restored.advance(&warm, tick).unwrap();
        }
        let live_frame = live.frame(0, 0, 3, 1, 0, 1_800).unwrap();
        let restored_frame = restored.frame(0, 0, 3, 1, 0, 1_800).unwrap();
        assert_eq!(live_frame.surfaces, restored_frame.surfaces);
        assert!(restored_frame.footprints.is_empty());

        let mut bad_schema = bytes;
        bad_schema[..2].copy_from_slice(&2u16.to_le_bytes());
        assert!(SnowState::from_canonical_bytes(&bad_schema).is_err());
    }
}

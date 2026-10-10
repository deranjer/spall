//! Server-owned water as stacked columns with virtual pipes (ENG-167).
//!
//! Replaces the dense MAC grid of [`crate::water`]. There is no domain box: the
//! water world is the whole terrain volume over a fixed height range, stored
//! sparsely by region, and costs time only where water moves. Terrain commits
//! refresh the touched regions before the next fluid step (volume conserved,
//! displaced water pushed to neighbours), springs and drains act on cells, and
//! the owner publishes per-region frames. The fluid step is fixed at
//! [`COLUMN_STEP_DT_S`], independent of the server tick. Bodies are ignored by
//! water, as before.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use spall_core::{BrickCoord, GlobalCell};
use spall_fluid::columns::{
    CELL_SIZE_M, ColumnConfig, ColumnStepMetrics, ColumnWorld, RegionKey, VolumeTerrain,
};
use spall_protocol::column_water::{
    COLUMN_WATER_VERSION, ColumnRegionRecord, ColumnSpanRecord, ColumnWaterState,
    volume_from_fixed, volume_to_fixed,
};
use spall_voxel::Volume;

/// Fixed fluid step (20 Hz), decoupled from the server tick.
pub const COLUMN_STEP_DT_S: f64 = 0.05;
/// Fluid steps run in one server tick at most; more time is reported skipped.
const MAX_STEPS_PER_TICK: u32 = 2;
/// Depth in presentation frames is quantized to 1/128 cell.
pub const FRAME_DEPTH_SCALE: f64 = 128.0;
/// Spans holding less than this (cells^3) are not published or persisted.
const DRY_VOLUME: f64 = 1.0e-6;

pub use spall_fluid::columns::MemoryReport;

/// Initial water and exchange cells for an authoritative column world.
#[derive(Debug, Clone)]
pub struct ColumnWaterSetup {
    /// Global y range `[y_lo, y_hi)` (multiples of 32) every region covers; the
    /// top is open sky.
    pub y_lo: i32,
    pub y_hi: i32,
    pub config: ColumnConfig,
    /// Full water cells seeded at rest (e.g. a worldgen water plan).
    pub initial_cells: Vec<GlobalCell>,
    /// Springs: cells refilled to full after every fluid step.
    pub sources: Vec<GlobalCell>,
    /// Gated springs by rate (index 0 is rate 1); only the current rate's
    /// footprint is refilled, rate 0 refills none.
    pub gated_sources: [Vec<GlobalCell>; 3],
    /// Drains: cells emptied after every fluid step.
    pub sinks: Vec<GlobalCell>,
}

impl ColumnWaterSetup {
    pub fn new(y_lo: i32, y_hi: i32, initial_cells: Vec<GlobalCell>) -> Self {
        Self {
            y_lo,
            y_hi,
            config: ColumnConfig::default(),
            initial_cells,
            sources: Vec::new(),
            gated_sources: [Vec::new(), Vec::new(), Vec::new()],
            sinks: Vec::new(),
        }
    }
}

/// Highest gated spring rate.
pub const MAX_GATED_SOURCE_RATE: u8 = 3;

/// One wet span as published to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSpan {
    pub column: u16,
    pub floor_y: i32,
    /// Depth in 1/128 cell, capped at the u16 range (about 511 cells).
    pub depth_q: u16,
}

/// All wet spans of one region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionFrame {
    pub key: RegionKey,
    pub spans: Vec<FrameSpan>,
}

/// One published, presentation-ready view of the water.
#[derive(Debug, Clone)]
pub struct ColumnWaterFrame {
    /// Increments whenever any region's water changed (and once for the seed).
    pub seq: u64,
    pub fluid_time_s: f64,
    pub volume_m3: f64,
    pub spring_added_m3: f64,
    pub drain_removed_m3: f64,
    /// Every region that holds water, by key.
    pub regions: Arc<BTreeMap<RegionKey, Arc<RegionFrame>>>,
    /// Regions that changed since the previous frame, for delta publishing.
    pub changed: Vec<RegionKey>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ColumnWaterError {
    #[error("column water setup is invalid: {0}")]
    Setup(String),
    #[error("column water terrain refresh failed: {0}")]
    Terrain(String),
    #[error("column water restore failed: {0}")]
    Restore(String),
}

/// Seed accounting. Seeded cells in solid or non-resident terrain are counted,
/// never silently dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ColumnSeedReport {
    pub seeded_m3: f64,
    pub rejected_cells: usize,
}

/// Water timing and accounting from one server tick.
#[derive(Debug, Clone, Default)]
pub struct ColumnWaterTickMetrics {
    /// Whether a fluid step ran this tick.
    pub stepped: bool,
    pub steps: u32,
    pub step: Option<ColumnStepMetrics>,
    pub active_regions: usize,
    pub allocated_regions: usize,
    pub volume_m3: f64,
    /// Volume held above span capacity, and unplaced, in m^3.
    pub overfill_m3: f64,
    pub unplaced_m3: f64,
    /// Time capturing a committed terrain change.
    pub boundary_refresh: Duration,
    pub columns_remapped: usize,
    pub relocated_m3: f64,
    pub step_duration: Duration,
    /// Fluid time that could not be simulated this tick.
    pub skipped_s: f64,
    pub frame_seq: u64,
    pub changed_regions: usize,
}

/// Authoritative water owned by `Simulation`, never by a client or ECS entity.
pub struct AuthoritativeColumnWater {
    setup: ColumnWaterSetup,
    world: ColumnWorld,
    gated_rate: Arc<AtomicU8>,
    seed: ColumnSeedReport,
    frame: Arc<ColumnWaterFrame>,
    regions: BTreeMap<RegionKey, Arc<RegionFrame>>,
    seq: u64,
    fluid_time_s: f64,
    added_cells3: f64,
    removed_cells3: f64,
    carry_s: f64,
    skipped_s: f64,
}

fn check_setup(setup: &ColumnWaterSetup) -> Result<(), ColumnWaterError> {
    let bad = |m: &str| Err(ColumnWaterError::Setup(m.into()));
    if setup.y_lo >= setup.y_hi || setup.y_lo.rem_euclid(32) != 0 || setup.y_hi.rem_euclid(32) != 0
    {
        return bad("height range must be a non-empty multiple of 32");
    }
    let in_range = |c: &GlobalCell| c.y >= i64::from(setup.y_lo) && c.y < i64::from(setup.y_hi);
    if setup
        .sources
        .iter()
        .chain(setup.gated_sources.iter().flatten())
        .chain(&setup.sinks)
        .any(|c| !in_range(c))
    {
        return bad("source or sink outside the height range");
    }
    if setup
        .sources
        .iter()
        .chain(setup.gated_sources.iter().flatten())
        .chain(&setup.sinks)
        .count()
        > 5 * spall_protocol::water::MAX_WATER_SOURCE_CELLS
    {
        return bad("too many source and sink cells");
    }
    Ok(())
}

fn terrain_source(terrain: &Volume, setup: &ColumnWaterSetup) -> Arc<VolumeTerrain<Arc<Volume>>> {
    Arc::new(VolumeTerrain::new(
        Arc::new(terrain.clone()),
        setup.y_lo,
        setup.y_hi,
    ))
}

impl AuthoritativeColumnWater {
    pub fn new(terrain: &Volume, setup: ColumnWaterSetup) -> Result<Self, ColumnWaterError> {
        check_setup(&setup)?;
        let mut world = ColumnWorld::with_terrain(terrain_source(terrain, &setup), setup.config);
        let rejected = world.seed_cells(&setup.initial_cells);
        // Springs and drains need their regions allocated even in a dry world.
        let exchange_keys: BTreeSet<RegionKey> = setup
            .sources
            .iter()
            .chain(setup.gated_sources.iter().flatten())
            .chain(&setup.sinks)
            .map(|c| spall_fluid::columns::region_of(c.x, c.z))
            .collect();
        world.allocate_regions(&exchange_keys.into_iter().collect::<Vec<_>>());
        let seed = ColumnSeedReport {
            seeded_m3: (setup.initial_cells.len() - rejected.len()) as f64 * CELL_SIZE_M.powi(3),
            rejected_cells: rejected.len(),
        };
        let mut water = Self::assemble(setup, world, seed);
        water.rebuild_frames_for(None);
        Ok(water)
    }

    fn assemble(setup: ColumnWaterSetup, world: ColumnWorld, seed: ColumnSeedReport) -> Self {
        let empty = Arc::new(ColumnWaterFrame {
            seq: 1,
            fluid_time_s: 0.0,
            volume_m3: 0.0,
            spring_added_m3: 0.0,
            drain_removed_m3: 0.0,
            regions: Arc::new(BTreeMap::new()),
            changed: Vec::new(),
        });
        Self {
            setup,
            world,
            gated_rate: Arc::new(AtomicU8::new(0)),
            seed,
            frame: empty,
            regions: BTreeMap::new(),
            seq: 1,
            fluid_time_s: 0.0,
            added_cells3: 0.0,
            removed_cells3: 0.0,
            carry_s: 0.0,
            skipped_s: 0.0,
        }
    }

    pub fn seed_report(&self) -> ColumnSeedReport {
        self.seed
    }

    pub fn world(&self) -> &ColumnWorld {
        &self.world
    }

    pub fn setup(&self) -> &ColumnWaterSetup {
        &self.setup
    }

    pub fn memory(&self) -> MemoryReport {
        self.world.memory()
    }

    /// Sets the gated spring rate: `0` is off, `1..=3` selects a footprint
    /// (clamped into range). Takes effect on the next fluid step.
    pub fn set_gated_sources_rate(&self, rate: u8) {
        self.gated_rate
            .store(rate.min(MAX_GATED_SOURCE_RATE), Ordering::Relaxed);
    }

    pub fn gated_sources_rate(&self) -> u8 {
        self.gated_rate.load(Ordering::Relaxed)
    }

    /// The newest published frame.
    pub fn frame(&self) -> Arc<ColumnWaterFrame> {
        Arc::clone(&self.frame)
    }

    fn region_frame(&self, key: RegionKey) -> Option<RegionFrame> {
        let spans: Vec<FrameSpan> = self
            .world
            .region_wet_spans(key, DRY_VOLUME)
            .into_iter()
            .map(|(column, floor_y, volume)| {
                let height = self
                    .world
                    .span_height(key, column, floor_y)
                    .map_or(volume, f64::from);
                FrameSpan {
                    column,
                    floor_y,
                    depth_q: (volume.min(height) * FRAME_DEPTH_SCALE)
                        .round()
                        .min(f64::from(u16::MAX)) as u16,
                }
            })
            .collect();
        (!spans.is_empty()).then_some(RegionFrame { key, spans })
    }

    /// Rebuilds the published frames of `keys` (all regions when `None`) and
    /// publishes a new frame if anything changed.
    fn rebuild_frames_for(&mut self, keys: Option<Vec<RegionKey>>) -> usize {
        let keys = keys.unwrap_or_else(|| self.world.region_keys());
        let mut changed = Vec::new();
        for key in keys {
            let new = self.region_frame(key);
            let old = self.regions.get(&key);
            if new.as_ref() != old.map(|r| r.as_ref()) {
                match new {
                    Some(frame) => self.regions.insert(key, Arc::new(frame)),
                    None => self.regions.remove(&key),
                };
                changed.push(key);
            }
        }
        let first = self.frame.regions.is_empty() && self.frame.changed.is_empty() && self.seq == 1;
        if changed.is_empty() && !first {
            return 0;
        }
        let n = changed.len();
        if !first {
            self.seq += 1;
        }
        let cell3 = CELL_SIZE_M.powi(3);
        self.frame = Arc::new(ColumnWaterFrame {
            seq: self.seq,
            fluid_time_s: self.fluid_time_s,
            volume_m3: self.world.total_volume() * cell3,
            spring_added_m3: self.added_cells3 * cell3,
            drain_removed_m3: self.removed_cells3 * cell3,
            regions: Arc::new(self.regions.clone()),
            changed,
        });
        n
    }

    /// Applies springs and drains after a fluid step.
    fn exchange(&mut self) {
        let rate = usize::from(self.gated_rate.load(Ordering::Relaxed));
        let gated = rate
            .checked_sub(1)
            .and_then(|i| self.setup.gated_sources.get(i));
        for &cell in self.setup.sources.iter().chain(gated.into_iter().flatten()) {
            if let Some(added) = self.world.fill_cell(cell) {
                self.added_cells3 += added;
            }
        }
        for &cell in &self.setup.sinks {
            if let Some(removed) = self.world.drain_cell(cell) {
                self.removed_cells3 += removed;
            }
        }
    }

    /// Refreshes committed terrain, then advances by one server tick of `dt_s`.
    /// `edited` are terrain bricks changed by this tick's commits; `reloaded`
    /// are terrain bricks that became resident.
    pub fn tick(
        &mut self,
        terrain: &Volume,
        edited: &[BrickCoord],
        reloaded: &[BrickCoord],
        dt_s: f64,
    ) -> Result<ColumnWaterTickMetrics, ColumnWaterError> {
        if !dt_s.is_finite() || dt_s <= 0.0 {
            return Err(ColumnWaterError::Setup("tick dt must be positive".into()));
        }
        let mut metrics = ColumnWaterTickMetrics::default();
        let in_height = |b: &BrickCoord| {
            b.y >= i64::from(self.setup.y_lo) / 32 && b.y < i64::from(self.setup.y_hi) / 32
        };
        let region = |b: &BrickCoord| (b.x as i32, b.z as i32);
        let edited_keys: BTreeSet<RegionKey> =
            edited.iter().filter(|b| in_height(b)).map(region).collect();
        let reloaded_keys: Vec<RegionKey> = reloaded
            .iter()
            .filter(|b| in_height(b))
            .map(region)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !edited_keys.is_empty() || !reloaded_keys.is_empty() {
            let started = Instant::now();
            let report = self.world.replace_terrain(
                terrain_source(terrain, &self.setup),
                &edited_keys.iter().copied().collect::<Vec<_>>(),
            );
            self.world.notify_resident(&reloaded_keys);
            metrics.boundary_refresh = started.elapsed();
            metrics.columns_remapped = report.columns_changed;
            metrics.relocated_m3 = report.relocated_volume * CELL_SIZE_M.powi(3);
        }

        self.carry_s += dt_s;
        let started = Instant::now();
        while self.carry_s + 1e-9 >= COLUMN_STEP_DT_S && metrics.steps < MAX_STEPS_PER_TICK {
            self.carry_s -= COLUMN_STEP_DT_S;
            let step = self.world.step(COLUMN_STEP_DT_S);
            self.exchange();
            self.fluid_time_s += COLUMN_STEP_DT_S;
            metrics.steps += 1;
            metrics.step = Some(step);
        }
        // More than the cap owed: report the debt instead of running a burst.
        if self.carry_s + 1e-9 >= COLUMN_STEP_DT_S {
            let owed = (self.carry_s / COLUMN_STEP_DT_S).floor() * COLUMN_STEP_DT_S;
            self.carry_s -= owed;
            self.skipped_s += owed;
        }
        metrics.stepped = metrics.steps > 0;
        metrics.step_duration = started.elapsed();
        let dirty = self.world.take_dirty_regions();
        metrics.changed_regions = self.rebuild_frames_for(Some(dirty));
        let cell3 = CELL_SIZE_M.powi(3);
        metrics.volume_m3 = self.world.total_volume() * cell3;
        metrics.overfill_m3 = self.world.overfill_volume() * cell3;
        metrics.unplaced_m3 = self.world.unplaced_volume() * cell3;
        metrics.active_regions = self.world.active_region_count();
        metrics.allocated_regions = self.world.region_count();
        metrics.skipped_s = self.skipped_s;
        metrics.frame_seq = self.frame.seq;
        Ok(metrics)
    }

    pub fn canonical_state(&self) -> ColumnWaterState {
        let coords = |cells: &[GlobalCell]| cells.iter().map(|p| [p.x, p.y, p.z]).collect();
        let mut regions = Vec::new();
        let mut keys = self.world.region_keys();
        keys.sort_unstable();
        for key in keys {
            let spans: Vec<ColumnSpanRecord> = self
                .world
                .region_wet_spans(key, DRY_VOLUME)
                .into_iter()
                .map(|(column, floor_y, volume)| ColumnSpanRecord {
                    column,
                    floor_y,
                    volume: volume_to_fixed(volume).max(1),
                })
                .collect();
            if !spans.is_empty() {
                regions.push(ColumnRegionRecord {
                    rx: key.0,
                    rz: key.1,
                    spans,
                });
            }
        }
        ColumnWaterState {
            version: COLUMN_WATER_VERSION,
            y_lo: self.setup.y_lo,
            y_hi: self.setup.y_hi,
            frame_seq: self.seq,
            fluid_time_fixed: volume_to_fixed(self.fluid_time_s),
            spring_added_fixed: volume_to_fixed(self.added_cells3),
            drain_removed_fixed: volume_to_fixed(self.removed_cells3),
            unplaced_fixed: volume_to_fixed(self.world.unplaced_volume()),
            regions,
            sources: coords(&self.setup.sources),
            gated_sources: self.setup.gated_sources.each_ref().map(|v| coords(v)),
            sinks: coords(&self.setup.sinks),
            gated_rate: self.gated_sources_rate(),
        }
    }

    /// Restores the exact canonical amounts, deliberately resetting flow,
    /// pressure and sleep state to rest. A saved span with no matching span in
    /// the recovered terrain is an error, never silently dropped.
    pub fn restore(
        terrain: &Volume,
        state: &ColumnWaterState,
        config: ColumnConfig,
    ) -> Result<Self, ColumnWaterError> {
        state
            .validate()
            .map_err(|e| ColumnWaterError::Restore(e.to_string()))?;
        let cells = |values: &[[i64; 3]]| -> Vec<GlobalCell> {
            values
                .iter()
                .map(|p| GlobalCell::new(p[0], p[1], p[2]))
                .collect()
        };
        let mut setup = ColumnWaterSetup::new(state.y_lo, state.y_hi, Vec::new());
        setup.config = config;
        setup.sources = cells(&state.sources);
        setup.gated_sources = state.gated_sources.each_ref().map(|v| cells(v));
        setup.sinks = cells(&state.sinks);
        check_setup(&setup)?;
        let mut world = ColumnWorld::with_terrain(terrain_source(terrain, &setup), config);
        let keys: Vec<RegionKey> = state.regions.iter().map(|r| (r.rx, r.rz)).collect();
        let exchange_keys: BTreeSet<RegionKey> = setup
            .sources
            .iter()
            .chain(setup.gated_sources.iter().flatten())
            .chain(&setup.sinks)
            .map(|c| spall_fluid::columns::region_of(c.x, c.z))
            .collect();
        let mut keys = keys;
        keys.extend(exchange_keys);
        let missing = world.allocate_regions(&keys);
        if let Some(key) = missing.first() {
            return Err(ColumnWaterError::Restore(format!(
                "saved region {key:?} is not resident in the recovered terrain"
            )));
        }
        for region in &state.regions {
            for span in &region.spans {
                if !world.restore_span(
                    (region.rx, region.rz),
                    span.column,
                    span.floor_y,
                    volume_from_fixed(span.volume),
                ) {
                    return Err(ColumnWaterError::Restore(format!(
                        "saved span at region ({}, {}) column {} y {} has no terrain span",
                        region.rx, region.rz, span.column, span.floor_y
                    )));
                }
            }
        }
        world.add_unplaced(volume_from_fixed(state.unplaced_fixed));
        world.sleep_all();
        let mut water = Self::assemble(setup, world, ColumnSeedReport::default());
        water.seq = state.frame_seq;
        water.fluid_time_s = volume_from_fixed(state.fluid_time_fixed);
        water.added_cells3 = volume_from_fixed(state.spring_added_fixed);
        water.removed_cells3 = volume_from_fixed(state.drain_removed_fixed);
        water.set_gated_sources_rate(state.gated_rate);
        let first_seq = water.seq;
        water.rebuild_frames_for(None);
        // Restore publishes at the saved sequence; a new frame never reuses one.
        water.seq = first_seq;
        let mut frame = (*water.frame).clone();
        frame.seq = first_seq;
        water.frame = Arc::new(frame);
        let _ = water.world.take_dirty_regions();
        Ok(water)
    }
}

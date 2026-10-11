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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use spall_core::{BrickCoord, GlobalCell};
use spall_fluid::columns::{
    CELL_SIZE_M, ColumnConfig, ColumnStepMetrics, ColumnWorld, RegionKey, VolumeTerrain,
};
use spall_physics::WaterSample;
use spall_protocol::column_water::{
    COLUMN_WATER_VERSION, ColumnRegionRecord, ColumnSpanRecord, ColumnWaterDelta, ColumnWaterState,
    volume_from_fixed, volume_to_fixed,
};
use spall_protocol::column_water_wire::{dequantize_flow, quantize_flow};
use spall_voxel::Volume;

/// Fixed fluid step (20 Hz), decoupled from the server tick.
pub const COLUMN_STEP_DT_S: f64 = 0.05;
/// Slices a fluid step is cut into, one per server tick (so each is 1/60 s).
/// Running a whole 50 ms step on every third tick put its full cost (12 to
/// 30 ms once a big lake is moving) into one tick, past the 16.7 ms budget;
/// a slice is a third of the work, every tick. Springs and drains still act
/// once per step, after its last slice, so their strength is unchanged.
pub const COLUMN_SLICES: u32 = 3;
const SLICE_DT_S: f64 = COLUMN_STEP_DT_S / COLUMN_SLICES as f64;
/// Fluid steps run in one server tick at most; more time is reported skipped.
const MAX_STEPS_PER_TICK: u32 = 2;
/// Depth in presentation frames is quantized to 1/128 cell.
pub const FRAME_DEPTH_SCALE: f64 = 128.0;
/// Server ticks between journal records while water moves with no terrain
/// edit (an edit tick always journals, so water stays consistent with terrain).
pub const JOURNAL_INTERVAL_TICKS: u32 = 20;
/// Spans holding less than this (cells^3) are not published or persisted.
const DRY_VOLUME: f64 = 1.0e-6;

pub use spall_fluid::columns::{ColumnConfig as ColumnSolverConfig, MemoryReport};

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
    /// Water seeded at rest per column as `(x, z, bed, top)`: the span above
    /// terrain surface `bed` fills to cell `top`. Compact for big lakes and seas.
    pub initial_levels: Vec<(i64, i64, i64, i64)>,
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
            initial_levels: Vec::new(),
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
    /// Ceiling of the span (global y, cells), for finding which span holds a
    /// point. Not sent to clients.
    pub ceiling_y: i32,
    /// Horizontal velocity of the water `[x, z]`, quantized (see
    /// [`spall_protocol::column_water_wire::quantize_flow`]).
    pub flow: [i8; 2],
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
    #[error("column water worker failed: {0}")]
    Worker(String),
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

/// Everything the fluid step mutates, behind one lock so the step can run on a
/// worker thread (see [`AuthoritativeColumnWater::start_worker`]).
struct Core {
    world: ColumnWorld,
    regions: BTreeMap<RegionKey, Arc<RegionFrame>>,
    seq: u64,
    fluid_time_s: f64,
    added_cells3: f64,
    removed_cells3: f64,
    /// Slices of the current step already run (0..`COLUMN_SLICES`).
    slice: u32,
    /// Volume, overfill and unplaced totals (m^3) as of the last refresh.
    totals: Option<(f64, f64, f64)>,
    skipped_s: f64,
    /// Regions changed since the last journal delta.
    journal_dirty: BTreeSet<RegionKey>,
    /// A terrain change since the last journal delta: it journals at once, and
    /// the next frame covers it even mid-step.
    edit_pending: bool,
}

/// What the fluid step publishes for the tick thread to read without waiting
/// for the step.
struct Published {
    frame: Arc<ColumnWaterFrame>,
    /// Counters since the tick last collected them.
    steps: u32,
    last_step: Option<ColumnStepMetrics>,
    step_duration: Duration,
    changed_regions: usize,
    /// As of the last completed run.
    volume_m3: f64,
    overfill_m3: f64,
    unplaced_m3: f64,
    active_regions: usize,
    allocated_regions: usize,
    skipped_s: f64,
    /// A worker that panicked, so the tick can fail loudly.
    failure: Option<String>,
}

struct Owed {
    /// Fluid time owed, seconds.
    seconds: f64,
    /// Run even with less than a slice owed (terrain changed).
    kick: bool,
    stop: bool,
}

struct Shared {
    setup: ColumnWaterSetup,
    gated_rate: AtomicU8,
    core: Mutex<Core>,
    owed: Mutex<Owed>,
    wake: Condvar,
    published: RwLock<Published>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Authoritative water owned by `Simulation`, never by a client or ECS entity.
///
/// The fluid step runs inline in [`Self::tick`] by default (tests, tools).
/// [`Self::start_worker`] moves it to its own thread: the tick then only
/// hands over the fluid time owed and reads the last published frame, so a
/// slow step (a busy machine, a big lake) delays the water, never the tick.
pub struct AuthoritativeColumnWater {
    shared: Arc<Shared>,
    seed: ColumnSeedReport,
    worker: Option<std::thread::JoinHandle<()>>,
    journal_wait_ticks: u32,
}

/// Read access to the water world, held for as long as the guard lives. With a
/// worker, the step waits for it and it waits for the step.
pub struct WorldGuard<'a>(MutexGuard<'a, Core>);

impl std::ops::Deref for WorldGuard<'_> {
    type Target = ColumnWorld;
    fn deref(&self) -> &ColumnWorld {
        &self.0.world
    }
}

/// The solver config for stepping in slices: a region sleeps after the same
/// stretch of time, which is three times as many (shorter) steps.
fn sliced(mut config: ColumnConfig) -> ColumnConfig {
    config.sleep_steps = config.sleep_steps.saturating_mul(COLUMN_SLICES);
    config
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

impl Core {
    fn region_frame(&self, key: RegionKey) -> Option<RegionFrame> {
        // A span too shallow to quantize above zero is not published: depth
        // zero means dry on the wire, and a region of only such films would
        // otherwise vanish on the client while the server still holds it.
        let spans: Vec<FrameSpan> = self
            .world
            .region_wet_spans_with_flow(key, DRY_VOLUME)
            .into_iter()
            .map(|(column, floor_y, volume, height, flow)| {
                let height_cells = f64::from(height);
                FrameSpan {
                    column,
                    floor_y,
                    ceiling_y: floor_y + height,
                    depth_q: (volume.min(height_cells) * FRAME_DEPTH_SCALE)
                        .round()
                        .min(f64::from(u16::MAX)) as u16,
                    flow: quantize_flow(flow),
                }
            })
            .filter(|span| span.depth_q > 0)
            .collect();
        (!spans.is_empty()).then_some(RegionFrame { key, spans })
    }

    /// Rebuilds the frames of `keys` (all regions when `None`) and returns the
    /// new published frame if anything changed, with how many regions did.
    fn rebuild_frames_for(
        &mut self,
        previous: &Arc<ColumnWaterFrame>,
        keys: Option<Vec<RegionKey>>,
    ) -> Option<(Arc<ColumnWaterFrame>, usize)> {
        let keys = keys.unwrap_or_else(|| self.world.region_keys());
        let mut changed = Vec::new();
        let span_rf = crate::prof::Span::start("water.frames.regions");
        // Each region's frame reads the world only; build them in parallel.
        use rayon::prelude::*;
        let built: Vec<(RegionKey, Option<RegionFrame>)> = keys
            .par_iter()
            .map(|&key| (key, self.region_frame(key)))
            .collect();
        for (key, new) in built {
            let old = self.regions.get(&key);
            if new.as_ref() != old.map(|r| r.as_ref()) {
                match new {
                    Some(frame) => self.regions.insert(key, Arc::new(frame)),
                    None => self.regions.remove(&key),
                };
                changed.push(key);
            }
        }
        drop(span_rf);
        let first = previous.regions.is_empty() && previous.changed.is_empty() && self.seq == 1;
        if changed.is_empty() && !first {
            return None;
        }
        let n = changed.len();
        if !first {
            self.seq += 1;
        }
        let cell3 = CELL_SIZE_M.powi(3);
        let frame = Arc::new(ColumnWaterFrame {
            seq: self.seq,
            fluid_time_s: self.fluid_time_s,
            volume_m3: self.world.total_volume() * cell3,
            spring_added_m3: self.added_cells3 * cell3,
            drain_removed_m3: self.removed_cells3 * cell3,
            regions: Arc::new(self.regions.clone()),
            changed,
        });
        Some((frame, n))
    }

    fn span_records(&self, key: RegionKey) -> Vec<ColumnSpanRecord> {
        self.world
            .region_wet_spans(key, DRY_VOLUME)
            .into_iter()
            .map(|(column, floor_y, volume)| ColumnSpanRecord {
                column,
                floor_y,
                volume: volume_to_fixed(volume).max(1),
            })
            .collect()
    }
}

impl Shared {
    fn gated_rate(&self) -> u8 {
        self.gated_rate.load(Ordering::Relaxed)
    }

    /// Applies springs and drains after a fluid step.
    fn exchange(&self, core: &mut Core) {
        let rate = usize::from(self.gated_rate());
        let gated = rate
            .checked_sub(1)
            .and_then(|i| self.setup.gated_sources.get(i));
        for &cell in self.setup.sources.iter().chain(gated.into_iter().flatten()) {
            if let Some(added) = core.world.fill_cell(cell) {
                core.added_cells3 += added;
            }
        }
        for &cell in &self.setup.sinks {
            if let Some(removed) = core.world.drain_cell(cell) {
                core.removed_cells3 += removed;
            }
        }
    }

    /// Runs the fluid time owed (at most [`MAX_STEPS_PER_TICK`] steps of
    /// slices, the rest reported skipped), then builds and publishes the
    /// frame. On the worker thread, or inline in the tick.
    fn advance(&self) {
        let mut core = lock(&self.core);
        let started = Instant::now();
        let mut steps = 0u32;
        let mut last_step = None;
        loop {
            {
                let mut owed = lock(&self.owed);
                if owed.seconds + 1e-9 < SLICE_DT_S || steps >= MAX_STEPS_PER_TICK * COLUMN_SLICES {
                    break;
                }
                owed.seconds -= SLICE_DT_S;
            }
            let step_span = crate::prof::Span::start("water.step");
            last_step = Some(core.world.step(SLICE_DT_S));
            drop(step_span);
            core.fluid_time_s += SLICE_DT_S;
            core.slice += 1;
            if core.slice == COLUMN_SLICES {
                core.slice = 0;
                let _exchange = crate::prof::Span::start("water.exchange");
                self.exchange(&mut core);
            }
            steps += 1;
        }
        // More than the cap owed: report the debt instead of running a burst.
        {
            let mut owed = lock(&self.owed);
            owed.kick = false;
            if owed.seconds + 1e-9 >= SLICE_DT_S {
                let skipped = (owed.seconds / SLICE_DT_S).floor() * SLICE_DT_S;
                owed.seconds -= skipped;
                core.skipped_s += skipped;
            }
        }
        let step_duration = started.elapsed();
        // Frames (depths and currents of every region that moved) are built
        // once per step, not per slice: the publisher sends at most every
        // fourth tick, and building a frame costs about as much as a slice.
        let mut changed_regions = 0;
        let mut new_frame = None;
        if core.slice == 0 || core.edit_pending {
            let _frames = crate::prof::Span::start("water.frames");
            let dirty = core.world.take_dirty_regions();
            core.journal_dirty.extend(dirty.iter().copied());
            let previous = Arc::clone(
                &self
                    .published
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .frame,
            );
            if let Some((frame, n)) = core.rebuild_frames_for(&previous, Some(dirty)) {
                changed_regions = n;
                new_frame = Some(frame);
            }
        }
        // Totals walk every span in the world, so they are refreshed when a
        // step completes (or terrain changed), not on every slice or idle tick.
        if core.totals.is_none() || steps > 0 && core.slice == 0 || core.edit_pending {
            let _totals = crate::prof::Span::start("water.totals");
            let cell3 = CELL_SIZE_M.powi(3);
            core.totals = Some((
                core.world.total_volume() * cell3,
                core.world.overfill_volume() * cell3,
                core.world.unplaced_volume() * cell3,
            ));
        }
        let (volume_m3, overfill_m3, unplaced_m3) = core.totals.unwrap_or_default();
        let active_regions = core.world.active_region_count();
        let allocated_regions = core.world.region_count();
        let skipped_s = core.skipped_s;
        drop(core);
        let mut published = self.published.write().unwrap_or_else(|e| e.into_inner());
        if let Some(frame) = new_frame {
            published.frame = frame;
        }
        published.steps += steps;
        if last_step.is_some() {
            published.last_step = last_step;
        }
        published.step_duration += step_duration;
        published.changed_regions += changed_regions;
        published.volume_m3 = volume_m3;
        published.overfill_m3 = overfill_m3;
        published.unplaced_m3 = unplaced_m3;
        published.active_regions = active_regions;
        published.allocated_regions = allocated_regions;
        published.skipped_s = skipped_s;
    }

    /// The worker thread: runs whenever fluid time is owed or terrain changed.
    fn run_worker(self: &Arc<Self>) {
        loop {
            {
                let mut owed = lock(&self.owed);
                while !owed.stop && owed.seconds + 1e-9 < SLICE_DT_S && !owed.kick {
                    owed = self.wake.wait(owed).unwrap_or_else(|e| e.into_inner());
                }
                if owed.stop {
                    return;
                }
            }
            let this = Arc::clone(self);
            if let Err(panic) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || this.advance()))
            {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "column water worker panicked".into());
                self.published
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .failure = Some(message);
                return;
            }
        }
    }
}

impl AuthoritativeColumnWater {
    pub fn new(terrain: &Volume, setup: ColumnWaterSetup) -> Result<Self, ColumnWaterError> {
        check_setup(&setup)?;
        let mut world =
            ColumnWorld::with_terrain(terrain_source(terrain, &setup), sliced(setup.config));
        let rejected = world.seed_cells(&setup.initial_cells);
        let rejected_levels = world.seed_levels(&setup.initial_levels);
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
            seeded_m3: world.total_volume_m3(),
            rejected_cells: rejected.len() + rejected_levels.len(),
        };
        let mut water = Self::assemble(setup, world, seed);
        water.publish_all();
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
        let shared = Shared {
            setup,
            gated_rate: AtomicU8::new(0),
            core: Mutex::new(Core {
                world,
                regions: BTreeMap::new(),
                seq: 1,
                fluid_time_s: 0.0,
                added_cells3: 0.0,
                removed_cells3: 0.0,
                slice: 0,
                totals: None,
                skipped_s: 0.0,
                journal_dirty: BTreeSet::new(),
                edit_pending: false,
            }),
            owed: Mutex::new(Owed {
                seconds: 0.0,
                kick: false,
                stop: false,
            }),
            wake: Condvar::new(),
            published: RwLock::new(Published {
                frame: empty,
                steps: 0,
                last_step: None,
                step_duration: Duration::ZERO,
                changed_regions: 0,
                volume_m3: 0.0,
                overfill_m3: 0.0,
                unplaced_m3: 0.0,
                active_regions: 0,
                allocated_regions: 0,
                skipped_s: 0.0,
                failure: None,
            }),
        };
        Self {
            shared: Arc::new(shared),
            seed,
            worker: None,
            journal_wait_ticks: 0,
        }
    }

    /// Builds and publishes the frames of every region (after a seed or restore).
    fn publish_all(&mut self) {
        let mut core = lock(&self.shared.core);
        let previous = self.frame();
        if let Some((frame, _)) = core.rebuild_frames_for(&previous, None) {
            self.shared
                .published
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .frame = frame;
        }
    }

    /// Moves the fluid step to its own thread. From then on [`Self::tick`]
    /// never waits for a step (only for a terrain refresh), and the water
    /// players and bodies feel is the last published frame.
    pub fn start_worker(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        self.worker = Some(
            std::thread::Builder::new()
                .name("spall-column-water".into())
                .spawn(move || shared.run_worker())
                .expect("column water worker thread starts"),
        );
    }

    pub fn has_worker(&self) -> bool {
        self.worker.is_some()
    }

    pub fn seed_report(&self) -> ColumnSeedReport {
        self.seed
    }

    /// The water at a point as characters feel it, from the published frame
    /// (the same quantized values clients receive).
    pub fn water_sample_at_m(&self, x_m: f64, y_m: f64, z_m: f64) -> Option<WaterSample> {
        self.frame().sample_at_m(x_m, y_m, z_m)
    }

    /// The water world, locked: with a worker running, the step waits for the
    /// guard and the guard waits for a running step.
    pub fn world(&self) -> WorldGuard<'_> {
        WorldGuard(lock(&self.shared.core))
    }

    pub fn setup(&self) -> &ColumnWaterSetup {
        &self.shared.setup
    }

    pub fn memory(&self) -> MemoryReport {
        lock(&self.shared.core).world.memory()
    }

    /// Sets the gated spring rate: `0` is off, `1..=3` selects a footprint
    /// (clamped into range). Takes effect on the next fluid step.
    pub fn set_gated_sources_rate(&self, rate: u8) {
        self.shared
            .gated_rate
            .store(rate.min(MAX_GATED_SOURCE_RATE), Ordering::Relaxed);
    }

    pub fn gated_sources_rate(&self) -> u8 {
        self.shared.gated_rate()
    }

    /// The newest published frame.
    pub fn frame(&self) -> Arc<ColumnWaterFrame> {
        Arc::clone(
            &self
                .shared
                .published
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .frame,
        )
    }

    /// Refreshes committed terrain, then advances by one server tick of `dt_s`.
    /// `edited` are terrain bricks changed by this tick's commits; `reloaded`
    /// are terrain bricks that became resident. With a worker the fluid step
    /// runs beside the tick and the metrics describe the work it finished since
    /// the last call.
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
            b.y >= i64::from(self.shared.setup.y_lo) / 32
                && b.y < i64::from(self.shared.setup.y_hi) / 32
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
        let terrain_changed = !edited_keys.is_empty() || !reloaded_keys.is_empty();
        if terrain_changed {
            // Waits for a step in flight: the refresh must see a whole world.
            let started = Instant::now();
            let mut core = lock(&self.shared.core);
            let report = core.world.replace_terrain(
                terrain_source(terrain, &self.shared.setup),
                &edited_keys.iter().copied().collect::<Vec<_>>(),
            );
            core.world.notify_resident(&reloaded_keys);
            core.edit_pending = true;
            drop(core);
            metrics.boundary_refresh = started.elapsed();
            metrics.columns_remapped = report.columns_changed;
            metrics.relocated_m3 = report.relocated_volume * CELL_SIZE_M.powi(3);
        }
        {
            let mut owed = lock(&self.shared.owed);
            owed.seconds += dt_s;
            owed.kick |= terrain_changed;
        }
        if self.worker.is_some() {
            self.shared.wake.notify_one();
        } else {
            self.shared.advance();
        }
        let mut published = self
            .shared
            .published
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(message) = &published.failure {
            return Err(ColumnWaterError::Worker(message.clone()));
        }
        metrics.steps = std::mem::take(&mut published.steps);
        metrics.stepped = metrics.steps > 0;
        metrics.step = published.last_step.take();
        metrics.step_duration = std::mem::take(&mut published.step_duration);
        metrics.changed_regions = std::mem::take(&mut published.changed_regions);
        metrics.volume_m3 = published.volume_m3;
        metrics.overfill_m3 = published.overfill_m3;
        metrics.unplaced_m3 = published.unplaced_m3;
        metrics.active_regions = published.active_regions;
        metrics.allocated_regions = published.allocated_regions;
        metrics.skipped_s = published.skipped_s;
        metrics.frame_seq = published.frame.seq;
        Ok(metrics)
    }

    /// The delta owed to the journal this tick, if any: always on a tick
    /// that applied a terrain change (water stays consistent with terrain at
    /// every committed edit), otherwise every [`JOURNAL_INTERVAL_TICKS`] while
    /// water has moved. Resting water journals nothing. With a worker, a step
    /// in flight defers the delta to a later tick.
    pub fn take_journal_delta(&mut self) -> Option<ColumnWaterDelta> {
        self.journal_wait_ticks = self.journal_wait_ticks.saturating_add(1);
        let mut core = match self.shared.core.try_lock() {
            Ok(core) => core,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                // The worker is stepping; an edit tick has already waited for
                // it, so only a periodic delta can be late.
                return None;
            }
        };
        let due = core.edit_pending || self.journal_wait_ticks >= JOURNAL_INTERVAL_TICKS;
        if !due || core.journal_dirty.is_empty() {
            if !core.edit_pending && core.journal_dirty.is_empty() {
                self.journal_wait_ticks = 0;
            }
            return None;
        }
        let dirty = std::mem::take(&mut core.journal_dirty);
        let regions = dirty
            .into_iter()
            .map(|key| ColumnRegionRecord {
                rx: key.0,
                rz: key.1,
                spans: core.span_records(key),
            })
            .collect();
        self.journal_wait_ticks = 0;
        core.edit_pending = false;
        Some(ColumnWaterDelta {
            frame_seq: core.seq,
            fluid_time_fixed: volume_to_fixed(core.fluid_time_s),
            spring_added_fixed: volume_to_fixed(core.added_cells3),
            drain_removed_fixed: volume_to_fixed(core.removed_cells3),
            unplaced_fixed: volume_to_fixed(core.world.unplaced_volume()),
            gated_rate: self.shared.gated_rate(),
            regions,
        })
    }

    /// The exact canonical state. Waits for a step in flight.
    pub fn canonical_state(&self) -> ColumnWaterState {
        let core = lock(&self.shared.core);
        let setup = &self.shared.setup;
        let coords = |cells: &[GlobalCell]| cells.iter().map(|p| [p.x, p.y, p.z]).collect();
        let mut regions = Vec::new();
        let mut keys = core.world.region_keys();
        keys.sort_unstable();
        for key in keys {
            let spans = core.span_records(key);
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
            y_lo: setup.y_lo,
            y_hi: setup.y_hi,
            frame_seq: core.seq,
            fluid_time_fixed: volume_to_fixed(core.fluid_time_s),
            spring_added_fixed: volume_to_fixed(core.added_cells3),
            drain_removed_fixed: volume_to_fixed(core.removed_cells3),
            unplaced_fixed: volume_to_fixed(core.world.unplaced_volume()),
            regions,
            sources: coords(&setup.sources),
            gated_sources: setup.gated_sources.each_ref().map(|v| coords(v)),
            sinks: coords(&setup.sinks),
            gated_rate: self.shared.gated_rate(),
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
        let mut world = ColumnWorld::with_terrain(terrain_source(terrain, &setup), sliced(config));
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
        {
            let mut core = lock(&water.shared.core);
            core.seq = state.frame_seq;
            core.fluid_time_s = volume_from_fixed(state.fluid_time_fixed);
            core.added_cells3 = volume_from_fixed(state.spring_added_fixed);
            core.removed_cells3 = volume_from_fixed(state.drain_removed_fixed);
        }
        water.set_gated_sources_rate(state.gated_rate);
        let first_seq = state.frame_seq;
        water.publish_all();
        // Restore publishes at the saved sequence; a new frame never reuses one.
        {
            let mut core = lock(&water.shared.core);
            core.seq = first_seq;
            let _ = core.world.take_dirty_regions();
        }
        let mut published = water
            .shared
            .published
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let mut frame = (*published.frame).clone();
        frame.seq = first_seq;
        published.frame = Arc::new(frame);
        drop(published);
        Ok(water)
    }
}

impl Drop for AuthoritativeColumnWater {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            lock(&self.shared.owed).stop = true;
            self.shared.wake.notify_all();
            // A worker that panicked already reported it through `tick`.
            let _ = worker.join();
        }
    }
}

impl ColumnWaterFrame {
    /// The water at a point as characters and bodies feel it: the published
    /// surface and the quantized current.
    pub fn sample_at_m(&self, x_m: f64, y_m: f64, z_m: f64) -> Option<WaterSample> {
        self.sampler().sample_at_m(x_m, y_m, z_m)
    }

    /// A sampler that remembers the last region it looked in, for the many
    /// nearby samples a body or a player takes in one tick.
    pub fn sampler(&self) -> FrameSampler<'_> {
        FrameSampler {
            regions: &self.regions,
            last: std::cell::Cell::new(None),
        }
    }
}

/// See [`ColumnWaterFrame::sampler`].
pub struct FrameSampler<'a> {
    regions: &'a BTreeMap<RegionKey, Arc<RegionFrame>>,
    last: std::cell::Cell<Option<(RegionKey, &'a RegionFrame)>>,
}

impl<'a> FrameSampler<'a> {
    pub fn sample_at_m(&self, x_m: f64, y_m: f64, z_m: f64) -> Option<WaterSample> {
        let (cx, cz) = (
            (x_m / CELL_SIZE_M).floor() as i64,
            (z_m / CELL_SIZE_M).floor() as i64,
        );
        let key = spall_fluid::columns::region_of(cx, cz);
        let regions: &'a BTreeMap<RegionKey, Arc<RegionFrame>> = self.regions;
        let region: &'a RegionFrame = match self.last.get() {
            Some((last, region)) if last == key => region,
            _ => {
                let region = regions.get(&key)?.as_ref();
                self.last.set(Some((key, region)));
                region
            }
        };
        let column = (cx.rem_euclid(32) + 32 * cz.rem_euclid(32)) as u16;
        let start = region.spans.partition_point(|s| s.column < column);
        let y_cell = y_m / CELL_SIZE_M;
        for span in region.spans[start..]
            .iter()
            .take_while(|s| s.column == column)
        {
            if y_cell >= f64::from(span.floor_y) - 0.5 && y_cell < f64::from(span.ceiling_y) {
                let depth = f64::from(span.depth_q) / FRAME_DEPTH_SCALE;
                return (depth > 0.02).then(|| WaterSample {
                    surface_y: (f64::from(span.floor_y) + depth) * CELL_SIZE_M,
                    flow_m_s: dequantize_flow(span.flow),
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, MaterialId, Revision, VolumeId};
    use spall_voxel::{Brick, EditPlan};

    /// A walled basin 8 m x 4 m with a floor, filled to 1 m on its west half.
    fn basin() -> (Volume, Vec<GlobalCell>) {
        let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        for bx in 0..2 {
            terrain
                .insert_brick(
                    BrickCoord::new(bx, 0, 0),
                    Brick::uniform(MaterialId::AIR, Revision(1)),
                )
                .unwrap();
        }
        let mut plan = EditPlan::new(terrain.id());
        let mut water = Vec::new();
        for z in 0..16 {
            for y in 0..8 {
                for x in 0..32 {
                    let wall = x == 0 || x == 31 || z == 0 || z == 15;
                    if y == 0 || wall {
                        plan.set(GlobalCell::new(x, y, z), MaterialId(1));
                    } else if x < 16 && y <= 4 {
                        water.push(GlobalCell::new(x, y, z));
                    }
                }
            }
        }
        terrain.apply_edit(&plan).unwrap();
        (terrain, water)
    }

    fn east_half(water: &AuthoritativeColumnWater) -> f64 {
        let world = water.world();
        (17..31)
            .flat_map(|x| (1..15).map(move |z| (x, z)))
            .map(|(x, z)| world.column_volume(x, z))
            .sum()
    }

    /// Releases the held pool by making the water unbalanced: seeds it as a
    /// column against the west wall and wakes it with a terrain refresh.
    fn released(worker: bool) -> (Volume, AuthoritativeColumnWater) {
        let (terrain, cells) = basin();
        let mut water =
            AuthoritativeColumnWater::new(&terrain, ColumnWaterSetup::new(0, 32, cells)).unwrap();
        if worker {
            water.start_worker();
        }
        // Wake the sleeping seed: an edit tick on its region.
        let bricks = [BrickCoord::new(0, 0, 0)];
        water.tick(&terrain, &bricks, &[], 1.0 / 60.0).unwrap();
        (terrain, water)
    }

    #[test]
    fn a_worker_steps_the_same_water_the_inline_step_does() {
        let run = |worker: bool| {
            let (terrain, mut water) = released(worker);
            let start = water.frame().volume_m3;
            let deadline = Instant::now() + Duration::from_secs(20);
            for _ in 0..600 {
                water.tick(&terrain, &[], &[], 1.0 / 60.0).unwrap();
                if worker {
                    // Let the worker keep pace with the ticks.
                    std::thread::sleep(Duration::from_millis(2));
                }
                assert!(Instant::now() < deadline, "water did not finish in time");
            }
            if worker {
                // Wait for the owed time to be worked off.
                for _ in 0..2000 {
                    if lock(&water.shared.owed).seconds < SLICE_DT_S {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            let spread = east_half(&water);
            let volume = water.frame().volume_m3;
            (start, volume, spread)
        };
        let (start, inline_volume, inline_spread) = run(false);
        let (_, worker_volume, worker_spread) = run(true);
        assert!(inline_spread > 1.0, "the released water spreads east");
        assert!(
            worker_spread > 0.5 * inline_spread,
            "the worker's water spread {worker_spread} vs inline {inline_spread}"
        );
        for volume in [inline_volume, worker_volume] {
            assert!((volume - start).abs() < 1e-6 * start, "volume conserved");
        }
    }

    #[test]
    fn the_tick_does_not_wait_for_a_step_in_flight() {
        let (terrain, mut water) = released(true);
        // Stand in for a step that takes forever: hold the core.
        let shared = Arc::clone(&water.shared);
        let held = lock(&shared.core);
        let started = Instant::now();
        for _ in 0..120 {
            water.tick(&terrain, &[], &[], 1.0 / 60.0).unwrap();
            // Queries read the published frame.
            let _ = water.frame().sample_at_m(1.0, 0.5, 1.0);
            let _ = water.take_journal_delta();
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "ticks blocked behind the step: {:?}",
            started.elapsed()
        );
        drop(held);
    }

    #[test]
    fn a_worker_failure_is_reported_by_the_next_tick() {
        let (terrain, mut water) = released(true);
        water.shared.published.write().unwrap().failure = Some("boom".into());
        let error = water.tick(&terrain, &[], &[], 1.0 / 60.0).unwrap_err();
        assert!(matches!(error, ColumnWaterError::Worker(m) if m == "boom"));
    }

    #[test]
    fn the_frame_samples_the_surface_and_current_like_the_world_does() {
        let (terrain, cells) = basin();
        let water =
            AuthoritativeColumnWater::new(&terrain, ColumnWaterSetup::new(0, 32, cells)).unwrap();
        let frame = water.frame();
        // 4 cells of water over a 1-cell floor: surface at y = 5 cells = 1.25 m.
        let wet = frame
            .sample_at_m(1.0, 0.6, 1.0)
            .expect("water at the west end");
        assert!((wet.surface_y - 1.25).abs() < 0.01, "{wet:?}");
        assert_eq!(wet.flow_m_s, [0.0, 0.0]);
        assert!(frame.sample_at_m(6.0, 0.6, 1.0).is_none(), "dry east end");
        assert!(frame.sample_at_m(-5.0, 0.6, 1.0).is_none(), "outside");
        let exact = water.world().surface_at_m(1.0, 0.6, 1.0).unwrap();
        assert!((wet.surface_y - exact).abs() < 0.005);
    }
}

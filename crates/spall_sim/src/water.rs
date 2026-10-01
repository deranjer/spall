//! Server-owned finite water state for one declared, fully resident region.
//!
//! Water fractions remain separate from voxel occupancy. Terrain commits
//! refresh the grid boundary before the next fluid step; Rapier advances
//! independently. Dynamic bodies are not water obstacles yet.
//!
//! The fluid grid may be coarser than the voxel grid ([`WaterSetup::coarsen`]):
//! one fluid cell then covers `coarsen³` voxels and its solidity comes from
//! [`SolidBoundary::coarsened`]. Scenes pick this to keep a whole valley's
//! water inside the solver budget.
//!
//! Worker execution solves an immutable clone with a fixed fluid dt. The owner
//! validates its boundary revision and source rate before installing a result
//! at a tick boundary. Busy or stale work drops explicitly accounted fluid time;
//! the server tick keeps its fixed dt.

use spall_core::GlobalCell;
use spall_fluid::grid_mac::{MacConfig, MacGridWorld, MacStepMetrics, PressurePreconditioner};
use spall_fluid::{DomainSpec, SolidBoundary, grid_mac::MacError};
use spall_voxel::Volume;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Voxel edge length the fluid domain is declared in.
const VOXEL_CELL_M: f64 = 0.25;

/// Where the fluid solver runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WaterExecution {
    /// Step on the simulation thread, once per tick at the tick dt. Exact and
    /// deterministic; used by tests and small fixtures.
    Inline,
    /// Step on a dedicated worker thread at a fixed fluid dt, decoupled from
    /// the server tick.
    Worker { step_dt_s: f64 },
}

/// Initial, bounded water state installed when an authoritative simulation is
/// created. Fractions are attached to global voxel-cell coordinates.
#[derive(Debug, Clone)]
pub struct WaterSetup {
    /// Voxel-cell domain. With `coarsen > 1` every axis must be a multiple of
    /// `coarsen`.
    pub domain: DomainSpec,
    /// Voxels per fluid cell along each axis.
    pub coarsen: u32,
    pub config: MacConfig,
    pub initial_fractions: Vec<(GlobalCell, f64)>,
    pub execution: WaterExecution,
    /// Springs: voxel cells whose fluid cell is refilled to full after every
    /// accepted step, so a stream keeps flowing. Empty keeps the region
    /// strictly volume-conserving.
    pub sources: Vec<GlobalCell>,
    /// Gated springs at increasing rate: index 0 is rate 1 (the default "on"
    /// speed, refilled while the gate is set to at least rate 1), index 1 is
    /// rate 2 (a bigger, additional footprint refilled at rate 2 and above),
    /// index 2 is rate 3. Only the cells for the *current* rate are refilled
    /// (rates are not cumulative) — see
    /// [`AuthoritativeWater::set_gated_sources_rate`]. Off (rate 0) refills
    /// none of them. Lets a scene author a reservoir that starts dry and
    /// fills, at a selectable speed, once its spring is switched on. A scene
    /// with no gated spring leaves every entry empty.
    pub gated_sources: [Vec<GlobalCell>; 3],
    /// Drains: voxel cells whose fluid cell is emptied after every step.
    pub sinks: Vec<GlobalCell>,
}

/// How fast a [`WaterSetup::gated_sources`] spring fills, or off. Higher
/// rates refill a larger authored footprint, not the same cells more often —
/// a spring cell is already saturated after one step, so repeating that has
/// no effect; more inflow needs more source area.
pub const MAX_GATED_SOURCE_RATE: u8 = 3;

impl WaterSetup {
    pub fn new(domain: DomainSpec, initial_fractions: Vec<(GlobalCell, f64)>) -> Self {
        Self {
            domain,
            coarsen: 1,
            config: MacConfig {
                cell_size_m: VOXEL_CELL_M,
                open_top: true,
                ..MacConfig::default()
            },
            initial_fractions,
            execution: WaterExecution::Inline,
            sources: Vec::new(),
            gated_sources: [Vec::new(), Vec::new(), Vec::new()],
            sinks: Vec::new(),
        }
    }

    /// Use fluid cells `factor` voxels wide (and sets the matching cell size).
    pub fn with_coarsening(mut self, factor: u32) -> Self {
        self.coarsen = factor.max(1);
        self.config.cell_size_m = VOXEL_CELL_M * f64::from(self.coarsen);
        self
    }

    pub fn on_worker(mut self, step_dt_s: f64) -> Self {
        self.execution = WaterExecution::Worker { step_dt_s };
        self
    }
}

/// One published, presentation-ready view of the authoritative water.
///
/// `fractions` holds one byte per fluid cell (`round(fraction * 255)`) in
/// X-fastest, then Y, then Z order. Fluid cell `(i, j, k)` spans voxels
/// `origin + (i, j, k) * coarsen` through `+ coarsen - 1`.
#[derive(Debug, Clone, PartialEq)]
pub struct WaterFrame {
    /// Increments once per accepted fluid step (and once for the seed state).
    pub seq: u64,
    /// Simulated fluid time since the seed state.
    pub fluid_time_s: f64,
    pub origin: GlobalCell,
    pub dimensions: [u32; 3],
    pub coarsen: u32,
    pub cell_size_m: f64,
    pub volume_m3: f64,
    /// Total volume springs have added and drains removed since the seed.
    pub spring_added_m3: f64,
    pub drain_removed_m3: f64,
    /// Total volume that left through the open top of the domain.
    pub open_outflow_m3: f64,
    pub fractions: Vec<u8>,
    /// Exact owner state for checkpoints; never used as a presentation DTO.
    pub exact_fractions: Vec<f64>,
    pub trapped: Vec<f64>,
}

impl WaterFrame {
    fn capture(
        seq: u64,
        fluid_time_s: f64,
        coarsen: u32,
        grid: &MacGridWorld,
        exchange: &Exchange,
    ) -> Self {
        let spec = grid.spec();
        Self {
            seq,
            fluid_time_s,
            origin: spec.origin(),
            dimensions: spec.dimensions(),
            coarsen,
            cell_size_m: grid.config().cell_size_m,
            volume_m3: grid.water_volume_m3(),
            spring_added_m3: exchange.added_m3,
            drain_removed_m3: exchange.removed_m3,
            open_outflow_m3: grid.cumulative_open_outflow_m3(),
            fractions: grid
                .fractions()
                .iter()
                .map(|f| (f.clamp(0.0, 1.0) * 255.0).round() as u8)
                .collect(),
            exact_fractions: grid.fractions().to_vec(),
            trapped: grid.trapped_fractions().to_vec(),
        }
    }
}

/// Water timing and accounting from one authoritative server tick.
#[derive(Debug, Clone, Default)]
pub struct WaterTickMetrics {
    pub sleeping: bool,
    pub waiting_for_residency: bool,
    pub trapped_volume_m3: f64,
    pub volume_m3: f64,
    pub open_outflow_m3: f64,
    /// Time spent on the simulation thread capturing a committed boundary.
    pub boundary_refresh: Duration,
    /// Volume moved out of newly solid cells. Inline mode reports this tick's
    /// displacement; worker mode reports the worker's running total.
    pub displaced_volume_m3: f64,
    /// Duration of the most recent solver step.
    pub step_duration: Duration,
    /// Metrics of a solver step completed since the previous tick, if any.
    pub step: Option<MacStepMetrics>,
    /// Number of fixed fluid steps skipped so far, either because the stability
    /// preflight rejected a step or because a worker fell behind. The server
    /// tick and dt are unchanged.
    pub skipped_ticks: u64,
    /// Cumulative simulated fluid time dropped, including residency pauses.
    /// Observability only.
    pub skipped_duration: Duration,
    /// Newest published frame sequence.
    pub frame_seq: u64,
}

/// A failed strict boundary snapshot or solver step stops the owning tick.
#[derive(Debug, Clone, PartialEq)]
pub enum WaterError {
    Boundary(String),
    Solver(MacError),
    /// The worker thread stopped. Never silently ignored: the simulation that
    /// owns it fails its next tick with this reason.
    Worker(String),
}

impl std::fmt::Display for WaterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Boundary(error) => write!(f, "water boundary refresh failed: {error}"),
            Self::Solver(error) => write!(f, "water solver failed: {error}"),
            Self::Worker(error) => write!(f, "water worker failed: {error}"),
        }
    }
}

impl std::error::Error for WaterError {}

/// Springs and drains in fluid-cell coordinates, applied after each step.
#[derive(Debug, Clone)]
struct Exchange {
    sources: Vec<GlobalCell>,
    /// Rate 1, 2, and 3's footprints, indices 0, 1, 2.
    gated_sources: [Vec<GlobalCell>; 3],
    /// Shared with the owning [`AuthoritativeWater`], so an admin's rate
    /// change takes effect on this engine's very next step regardless of
    /// whether it runs inline or on the water worker thread. `0` is off, `1`
    /// through [`MAX_GATED_SOURCE_RATE`] index `gated_sources`.
    gated_rate: Arc<AtomicU8>,
    sinks: Vec<GlobalCell>,
    cell_m3: f64,
    added_m3: f64,
    removed_m3: f64,
}

impl Exchange {
    fn new(setup: &WaterSetup, coarsen: u32, gated_rate: Arc<AtomicU8>) -> Self {
        let origin = setup.domain.origin();
        let c = i64::from(coarsen);
        let to_fluid = |cells: &[GlobalCell]| {
            let set: std::collections::BTreeSet<(i64, i64, i64)> = cells
                .iter()
                .map(|cell| {
                    (
                        origin.x + (cell.x - origin.x).div_euclid(c),
                        origin.y + (cell.y - origin.y).div_euclid(c),
                        origin.z + (cell.z - origin.z).div_euclid(c),
                    )
                })
                .collect();
            set.into_iter()
                .map(|(x, y, z)| GlobalCell::new(x, y, z))
                .collect::<Vec<_>>()
        };
        Self {
            sources: to_fluid(&setup.sources),
            gated_sources: setup.gated_sources.clone().map(|cells| to_fluid(&cells)),
            gated_rate,
            sinks: to_fluid(&setup.sinks),
            cell_m3: (VOXEL_CELL_M * f64::from(coarsen)).powi(3),
            added_m3: 0.0,
            removed_m3: 0.0,
        }
    }

    /// Refills springs (always-on, plus the gated footprint for the current
    /// rate, if any) and empties drains. A spring or drain buried by a
    /// terrain edit (now solid) is skipped rather than failing the step.
    fn apply(&mut self, grid: &mut MacGridWorld) {
        let rate = usize::from(self.gated_rate.load(Ordering::Relaxed));
        let gated = rate
            .checked_sub(1)
            .and_then(|index| self.gated_sources.get(index));
        let sources = self.sources.iter().chain(gated.into_iter().flatten());
        for &cell in sources {
            if let Some(fraction) = grid.fraction_at(cell)
                && fraction < 1.0
                && grid.set_fraction(cell, 1.0).is_ok()
            {
                self.added_m3 += (1.0 - fraction) * self.cell_m3;
            }
        }
        for &cell in &self.sinks {
            if let Some(fraction) = grid.fraction_at(cell)
                && fraction > 0.0
                && grid.set_fraction(cell, 0.0).is_ok()
            {
                self.removed_m3 += fraction * self.cell_m3;
            }
        }
    }
}

/// Seed accounting: authored water that could not be placed because its fluid
/// cell is solid after coarsening.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WaterSeedReport {
    pub seeded_m3: f64,
    pub dropped_in_solid_m3: f64,
}

/// Authoritative water owned by `Simulation`, never by a client or ECS entity.
pub struct AuthoritativeWater {
    setup: WaterSetup,
    domain: DomainSpec,
    coarsen: u32,
    seed: WaterSeedReport,
    engine: Engine,
    /// Shared with the engine's [`Exchange`] (inline or moved onto the worker
    /// thread): which `WaterSetup::gated_sources` footprint, if any, is
    /// currently refilled.
    gated_rate: Arc<AtomicU8>,
    sleeping: bool,
    quiet_steps: u32,
    boundary_pending: bool,
    residency_skips: u64,
    residency_skipped_s: f64,
}

enum Engine {
    Inline {
        grid: Box<MacGridWorld>,
        frame: Arc<WaterFrame>,
        skipped_ticks: u64,
        skipped_s: f64,
        exchange: Exchange,
    },
    Worker(WaterWorker),
}

impl AuthoritativeWater {
    pub fn new(terrain: &Volume, setup: WaterSetup) -> Result<Self, WaterError> {
        let coarsen = setup.coarsen;
        let dims = setup.domain.dimensions();
        let cells = dims
            .iter()
            .try_fold(1usize, |n, d| n.checked_mul((*d / coarsen.max(1)) as usize));
        let voxels = dims
            .iter()
            .try_fold(1usize, |n, d| n.checked_mul(*d as usize));
        if !(1..=8).contains(&coarsen)
            || dims.iter().any(|d| *d % coarsen != 0)
            || cells.is_none_or(|n| n > spall_protocol::water::MAX_WATER_CELLS)
            || voxels.is_none_or(|n| n > 32 * 1024 * 1024)
            || setup.config.cell_size_m != VOXEL_CELL_M * f64::from(coarsen)
            || std::iter::once(&setup.sources)
                .chain(setup.gated_sources.iter())
                .chain(std::iter::once(&setup.sinks))
                .any(|v| v.len() > spall_protocol::water::MAX_WATER_SOURCE_CELLS)
        {
            return Err(WaterError::Boundary(
                "water setup exceeds canonical domain limits".into(),
            ));
        }

        let origin = setup.domain.origin();
        let lo = [origin.x, origin.y, origin.z];
        if std::iter::once(&setup.sources)
            .chain(setup.gated_sources.iter())
            .chain(std::iter::once(&setup.sinks))
            .flatten()
            .any(|p| {
                [p.x, p.y, p.z].iter().enumerate().any(|(i, v)| {
                    i128::from(*v) < i128::from(lo[i])
                        || i128::from(*v) >= i128::from(lo[i]) + i128::from(dims[i])
                })
            })
        {
            return Err(WaterError::Boundary("water source outside domain".into()));
        }
        let boundary = capture_boundary(terrain, setup.domain, coarsen)?;
        let mut grid = MacGridWorld::new(&boundary, setup.config).map_err(WaterError::Solver)?;
        grid.set_pressure_preconditioner(PressurePreconditioner::Multigrid);
        grid.set_ambient_density(1.2).map_err(WaterError::Solver)?;
        let seed = seed_fractions(&mut grid, &boundary, &setup, coarsen)?;
        let gated_rate = Arc::new(AtomicU8::new(0));
        let exchange = Exchange::new(&setup, coarsen, Arc::clone(&gated_rate));
        let frame = Arc::new(WaterFrame::capture(1, 0.0, coarsen, &grid, &exchange));
        let engine = match setup.execution {
            WaterExecution::Inline => Engine::Inline {
                grid: Box::new(grid),
                frame,
                skipped_ticks: 0,
                skipped_s: 0.0,
                exchange,
            },
            WaterExecution::Worker { step_dt_s } => {
                if !step_dt_s.is_finite() || step_dt_s <= 0.0 {
                    return Err(WaterError::Solver(MacError::InvalidTimeStep));
                }
                Engine::Worker(WaterWorker::spawn(
                    grid, frame, exchange, coarsen, step_dt_s,
                )?)
            }
        };
        Ok(Self {
            domain: setup.domain,
            coarsen,
            seed,
            engine,
            gated_rate,
            setup,
            sleeping: false,
            quiet_steps: 0,
            boundary_pending: false,
            residency_skips: 0,
            residency_skipped_s: 0.0,
        })
    }

    pub fn canonical_state(&self) -> spall_protocol::WaterState {
        let frame = self.frame();
        let c = self.setup.config;
        let coords = |cells: &[GlobalCell]| cells.iter().map(|p| [p.x, p.y, p.z]).collect();
        spall_protocol::WaterState {
            version: 1,
            origin: [
                self.domain.origin().x,
                self.domain.origin().y,
                self.domain.origin().z,
            ],
            voxel_dimensions: self.domain.dimensions(),
            coarsen: self.coarsen,
            config_bits: [
                c.cell_size_m,
                c.density_kg_m3,
                c.gravity_m_s2[0],
                c.gravity_m_s2[1],
                c.gravity_m_s2[2],
                c.cfl_limit,
                c.pressure_relative_tolerance,
                c.pressure_absolute_tolerance,
            ]
            .map(f64::to_bits),
            max_substeps: c.max_substeps,
            pressure_max_iterations: c.pressure_max_iterations,
            open_top: c.open_top,
            frame_seq: frame.seq,
            fluid_time_bits: frame.fluid_time_s.to_bits(),
            spring_added_bits: frame.spring_added_m3.to_bits(),
            drain_removed_bits: frame.drain_removed_m3.to_bits(),
            outflow_bits: frame.open_outflow_m3.to_bits(),
            fractions: frame.exact_fractions.iter().map(|v| v.to_bits()).collect(),
            trapped: frame.trapped.iter().map(|v| v.to_bits()).collect(),
            sources: coords(&self.setup.sources),
            gated_sources: self.setup.gated_sources.each_ref().map(|v| coords(v)),
            sinks: coords(&self.setup.sinks),
            gated_rate: self.gated_sources_rate(),
        }
    }

    /// Restore exact amounts and accounting, deliberately resetting velocity,
    /// pressure and numerical caches to rest. The owner installs this atomically.
    pub fn restore(
        terrain: &Volume,
        state: &spall_protocol::WaterState,
    ) -> Result<Self, WaterError> {
        state
            .validate()
            .map_err(|e| WaterError::Boundary(e.to_string()))?;
        let domain = DomainSpec::new(
            GlobalCell::new(state.origin[0], state.origin[1], state.origin[2]),
            state.voxel_dimensions,
            spall_protocol::water::MAX_WATER_CELLS * 512,
        )
        .map_err(|e| WaterError::Boundary(e.to_string()))?;
        let c = state.config_bits.map(f64::from_bits);
        let mut setup = WaterSetup::new(domain, Vec::new()).with_coarsening(state.coarsen);
        setup.config = MacConfig {
            cell_size_m: c[0],
            density_kg_m3: c[1],
            gravity_m_s2: [c[2], c[3], c[4]],
            cfl_limit: c[5],
            pressure_relative_tolerance: c[6],
            pressure_absolute_tolerance: c[7],
            max_substeps: state.max_substeps,
            pressure_max_iterations: state.pressure_max_iterations,
            pressure_diagnostics: false,
            open_top: state.open_top,
        };
        let cells = |values: &[[i64; 3]]| {
            values
                .iter()
                .map(|p| GlobalCell::new(p[0], p[1], p[2]))
                .collect()
        };
        setup.sources = cells(&state.sources);
        setup.gated_sources = state.gated_sources.each_ref().map(|v| cells(v));
        setup.sinks = cells(&state.sinks);
        let mut result = Self::new(terrain, setup)?;
        result.set_gated_sources_rate(state.gated_rate);
        let boundary = capture_boundary(terrain, result.domain, result.coarsen)?;
        if let Engine::Inline {
            grid,
            frame,
            exchange,
            ..
        } = &mut result.engine
        {
            grid.restore_trapped(
                &state
                    .trapped
                    .iter()
                    .map(|b| f64::from_bits(*b))
                    .collect::<Vec<_>>(),
            )
            .map_err(WaterError::Solver)?;
            grid.restore_displacing(
                &state
                    .fractions
                    .iter()
                    .map(|b| f64::from_bits(*b))
                    .collect::<Vec<_>>(),
                &boundary,
            )
            .map_err(WaterError::Solver)?;
            grid.restore_outflow(f64::from_bits(state.outflow_bits))
                .map_err(WaterError::Solver)?;
            exchange.added_m3 = f64::from_bits(state.spring_added_bits);
            exchange.removed_m3 = f64::from_bits(state.drain_removed_bits);
            *frame = Arc::new(WaterFrame::capture(
                state.frame_seq,
                f64::from_bits(state.fluid_time_bits),
                state.coarsen,
                grid,
                exchange,
            ));
        }
        Ok(result)
    }

    /// The voxel-cell domain this water was declared over.
    pub fn domain(&self) -> DomainSpec {
        self.domain
    }

    pub fn coarsen(&self) -> u32 {
        self.coarsen
    }

    /// Sets the gated spring rate: `0` is off, `1..=`[`MAX_GATED_SOURCE_RATE`]
    /// selects `WaterSetup::gated_sources[rate - 1]`'s footprint (clamped into
    /// range). Takes effect on this water's very next step, inline or on its
    /// worker thread. A scene with no gated sources accepts the call as a
    /// harmless no-op.
    pub fn set_gated_sources_rate(&self, rate: u8) {
        self.gated_rate
            .store(rate.min(MAX_GATED_SOURCE_RATE), Ordering::Relaxed);
    }

    /// The current gated spring rate (`0` is off).
    pub fn gated_sources_rate(&self) -> u8 {
        self.gated_rate.load(Ordering::Relaxed)
    }

    pub fn seed_report(&self) -> WaterSeedReport {
        self.seed
    }

    /// The owner's committed grid. Worker candidates are never exposed here.
    pub fn grid(&self) -> Option<&MacGridWorld> {
        match &self.engine {
            Engine::Inline { grid, .. } => Some(grid),
            Engine::Worker(worker) => Some(&worker.grid),
        }
    }

    /// The newest published frame.
    pub fn frame(&self) -> Arc<WaterFrame> {
        match &self.engine {
            Engine::Inline { frame, .. } => Arc::clone(frame),
            Engine::Worker(worker) => worker.latest(),
        }
    }

    /// Refresh committed terrain geometry and advance by one server tick.
    /// Whether an edit that changed terrain brick `coord` can change this
    /// region's solid boundary: the brick overlaps the fluid domain or is within
    /// one brick of it. An edit anywhere else cannot, so it need not wake the
    /// solver or re-capture the boundary over the whole domain.
    pub fn boundary_touched_by(&self, coord: spall_core::BrickCoord) -> bool {
        let origin = self.domain.origin();
        let dims = self.domain.dimensions();
        let lo = [origin.x, origin.y, origin.z];
        let near = |axis: usize, brick: i64| {
            let hi = lo[axis] + i64::from(dims[axis]) - 1;
            brick >= lo[axis].div_euclid(32) - 1 && brick <= hi.div_euclid(32) + 1
        };
        near(0, coord.x) && near(1, coord.y) && near(2, coord.z)
    }

    pub fn tick(
        &mut self,
        terrain: &Volume,
        boundary_dirty: bool,
        dt_s: f64,
    ) -> Result<WaterTickMetrics, WaterError> {
        if !dt_s.is_finite() || dt_s <= 0.0 {
            return Err(WaterError::Solver(MacError::InvalidTimeStep));
        }
        let frame = self.frame();
        let (skips, skipped_s) = match &self.engine {
            Engine::Inline {
                skipped_ticks,
                skipped_s,
                ..
            } => (*skipped_ticks, *skipped_s),
            Engine::Worker(w) => (w.skipped_steps, w.skipped_s),
        };
        let mut report = WaterTickMetrics {
            volume_m3: frame.volume_m3,
            open_outflow_m3: frame.open_outflow_m3,
            trapped_volume_m3: self.grid().map_or(0.0, MacGridWorld::trapped_volume_m3),
            ..WaterTickMetrics::default()
        };
        self.boundary_pending |= boundary_dirty;
        // Check every domain's declared residency even while sleeping. Missing
        // geometry suspends time instead of guessing an air/drain/wall boundary.
        let origin = self.domain.origin();
        let dims = self.domain.dimensions();
        let lo = [origin.x, origin.y, origin.z];
        let hi = std::array::from_fn::<_, 3, _>(|i| lo[i] + i64::from(dims[i]) - 1);
        let mut unknown = false;
        for bz in lo[2].div_euclid(32)..=hi[2].div_euclid(32) {
            for by in lo[1].div_euclid(32)..=hi[1].div_euclid(32) {
                for bx in lo[0].div_euclid(32)..=hi[0].div_euclid(32) {
                    let cell = GlobalCell::new(
                        (bx * 32).max(lo[0]),
                        (by * 32).max(lo[1]),
                        (bz * 32).max(lo[2]),
                    );
                    let sample = terrain
                        .sample(cell)
                        .map_err(|e| WaterError::Boundary(e.to_string()))?;
                    unknown |= matches!(sample, spall_voxel::Sample::Unknown(_));
                }
            }
        }
        if unknown {
            self.boundary_pending = true;
            self.residency_skips += 1;
            self.residency_skipped_s += dt_s;
            report.waiting_for_residency = true;
            report.skipped_ticks = skips + self.residency_skips;
            report.skipped_duration = Duration::from_secs_f64(skipped_s + self.residency_skipped_s);
            report.frame_seq = self.frame().seq;
            return Ok(report);
        }
        if self.boundary_pending || self.gated_sources_rate() > 0 {
            self.sleeping = false;
            self.quiet_steps = 0;
        }
        report.trapped_volume_m3 = self.grid().map_or(0.0, MacGridWorld::trapped_volume_m3);
        if self.sleeping {
            report.skipped_ticks = skips + self.residency_skips;
            report.skipped_duration = Duration::from_secs_f64(skipped_s + self.residency_skipped_s);
            report.sleeping = true;
            report.frame_seq = self.frame().seq;
            return Ok(report);
        }
        let boundary = if self.boundary_pending {
            let started = Instant::now();
            let boundary = capture_boundary(terrain, self.domain, self.coarsen)?;
            report.boundary_refresh = started.elapsed();
            self.boundary_pending = false;
            Some(boundary)
        } else {
            None
        };
        match &mut self.engine {
            Engine::Inline {
                grid,
                frame,
                skipped_ticks,
                skipped_s,
                exchange,
            } => {
                if let Some(boundary) = boundary {
                    report.displaced_volume_m3 = grid
                        .refresh_boundary_retaining(&boundary)
                        .map_err(WaterError::Solver)?;
                    *frame = Arc::new(WaterFrame::capture(
                        frame.seq + 1,
                        frame.fluid_time_s,
                        self.coarsen,
                        grid,
                        exchange,
                    ));
                }
                let started = Instant::now();
                match grid.step(dt_s) {
                    Ok(metrics) => {
                        report.step = Some(metrics);
                        exchange.apply(grid);
                        *frame = Arc::new(WaterFrame::capture(
                            frame.seq + 1,
                            frame.fluid_time_s + dt_s,
                            self.coarsen,
                            grid,
                            exchange,
                        ));
                    }
                    Err(MacError::SubstepBudgetExceeded { .. }) => {
                        // The solver guarantees this preflight overload leaves
                        // state unchanged; keep the fixed server dt and surface
                        // the debt.
                        *skipped_ticks = skipped_ticks.saturating_add(1);
                        *skipped_s += dt_s;
                    }
                    Err(error) => return Err(WaterError::Solver(error)),
                }
                report.skipped_ticks = *skipped_ticks;
                report.skipped_duration = Duration::from_secs_f64(*skipped_s);
                report.step_duration = started.elapsed();
                report.frame_seq = frame.seq;
            }
            Engine::Worker(worker) => worker.advance(dt_s, boundary, &mut report)?,
        }
        if report.step.is_some()
            && self.setup.sources.is_empty()
            && self.setup.sinks.is_empty()
            && self.gated_sources_rate() == 0
            && self
                .grid()
                .is_some_and(|g| g.max_face_component_velocity_m_s() <= 1e-5)
        {
            self.quiet_steps += 1;
            if self.quiet_steps >= 60 {
                self.sleeping = true;
            }
        } else if report.step.is_some() {
            self.quiet_steps = 0;
        }
        let frame = self.frame();
        report.volume_m3 = frame.volume_m3;
        report.open_outflow_m3 = frame.open_outflow_m3;
        report.sleeping = self.sleeping;
        report.trapped_volume_m3 = self.grid().map_or(0.0, MacGridWorld::trapped_volume_m3);
        report.skipped_ticks += self.residency_skips;
        report.skipped_duration += Duration::from_secs_f64(self.residency_skipped_s);
        Ok(report)
    }
}

fn capture_boundary(
    terrain: &Volume,
    domain: DomainSpec,
    coarsen: u32,
) -> Result<SolidBoundary, WaterError> {
    let fine = SolidBoundary::capture(terrain, domain)
        .map_err(|error| WaterError::Boundary(error.to_string()))?;
    fine.coarsened(coarsen)
        .map_err(|error| WaterError::Boundary(error.to_string()))
}

/// Aggregates voxel fractions into fluid cells (`sum / coarsen³`). Water in a
/// fluid cell that coarsened to solid is dropped and reported.
fn seed_fractions(
    grid: &mut MacGridWorld,
    boundary: &SolidBoundary,
    setup: &WaterSetup,
    coarsen: u32,
) -> Result<WaterSeedReport, WaterError> {
    let origin = setup.domain.origin();
    let per_cell = f64::from(coarsen).powi(3);
    let mut sums = std::collections::BTreeMap::<(i64, i64, i64), f64>::new();
    for (cell, fraction) in &setup.initial_fractions {
        let c = i64::from(coarsen);
        let key = (
            origin.x + (cell.x - origin.x).div_euclid(c),
            origin.y + (cell.y - origin.y).div_euclid(c),
            origin.z + (cell.z - origin.z).div_euclid(c),
        );
        *sums.entry(key).or_default() += fraction.clamp(0.0, 1.0) / per_cell;
    }
    let voxel_m3 = VOXEL_CELL_M.powi(3) * per_cell;
    let mut report = WaterSeedReport::default();
    for ((x, y, z), fraction) in sums {
        let cell = GlobalCell::new(x, y, z);
        let fraction = fraction.min(1.0);
        match boundary.is_solid(cell) {
            Some(false) => {
                grid.set_fraction(cell, fraction)
                    .map_err(WaterError::Solver)?;
                report.seeded_m3 += fraction * voxel_m3;
            }
            Some(true) => report.dropped_in_solid_m3 += fraction * voxel_m3,
            None => {
                return Err(WaterError::Boundary(format!(
                    "initial water at {cell:?} is outside the fluid domain"
                )));
            }
        }
    }
    Ok(report)
}

/// Immutable job input; only one fluid job may be outstanding.
struct Advance {
    grid: MacGridWorld,
    exchange: Exchange,
    revision: u64,
    rate: u8,
    dt_s: f64,
}

struct Completed {
    input: Advance,
    outcome: Result<MacStepMetrics, MacError>,
    duration: Duration,
}

struct WaterWorker {
    grid: Box<MacGridWorld>,
    exchange: Exchange,
    frame: Arc<WaterFrame>,
    submit: Option<mpsc::SyncSender<Advance>>,
    completed: mpsc::Receiver<Completed>,
    thread: Option<std::thread::JoinHandle<()>>,
    revision: u64,
    in_flight: bool,
    owed_s: f64,
    step_dt_s: f64,
    coarsen: u32,
    skipped_steps: u64,
    skipped_s: f64,
}

impl WaterWorker {
    fn spawn(
        grid: MacGridWorld,
        frame: Arc<WaterFrame>,
        exchange: Exchange,
        coarsen: u32,
        step_dt_s: f64,
    ) -> Result<Self, WaterError> {
        let (submit, work) = mpsc::sync_channel::<Advance>(1);
        let (publish, completed) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("spall-water".into())
            .spawn(move || {
                while let Ok(mut input) = work.recv() {
                    let started = Instant::now();
                    let outcome = input.grid.step(input.dt_s);
                    if outcome.is_ok() {
                        input.exchange.apply(&mut input.grid);
                    }
                    if publish
                        .send(Completed {
                            input,
                            outcome,
                            duration: started.elapsed(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|e| WaterError::Worker(e.to_string()))?;
        Ok(Self {
            grid: Box::new(grid),
            frame,
            exchange,
            submit: Some(submit),
            completed,
            thread: Some(thread),
            revision: 0,
            in_flight: false,
            owed_s: 0.0,
            step_dt_s,
            coarsen,
            skipped_steps: 0,
            skipped_s: 0.0,
        })
    }

    fn latest(&self) -> Arc<WaterFrame> {
        Arc::clone(&self.frame)
    }

    fn publish(&mut self, advanced_s: f64) {
        self.frame = Arc::new(WaterFrame::capture(
            self.frame.seq + 1,
            self.frame.fluid_time_s + advanced_s,
            self.coarsen,
            &self.grid,
            &self.exchange,
        ));
    }

    fn skip(&mut self) {
        self.skipped_steps += 1;
        self.skipped_s += self.step_dt_s;
    }

    fn advance(
        &mut self,
        dt_s: f64,
        boundary: Option<SolidBoundary>,
        report: &mut WaterTickMetrics,
    ) -> Result<(), WaterError> {
        // Refresh live owner geometry before considering a completed job. An
        // old job can never overwrite a newly committed boundary or its ledger.
        if let Some(boundary) = boundary {
            self.revision = self
                .revision
                .checked_add(1)
                .ok_or_else(|| WaterError::Worker("water revision exhausted".into()))?;
            report.displaced_volume_m3 = self
                .grid
                .refresh_boundary_retaining(&boundary)
                .map_err(WaterError::Solver)?;
            self.publish(0.0);
        }
        match self.completed.try_recv() {
            Ok(done) => {
                self.in_flight = false;
                if done.input.revision != self.revision
                    || done.input.rate != self.exchange.gated_rate.load(Ordering::Relaxed)
                {
                    self.skip();
                } else {
                    match done.outcome {
                        Ok(metrics) => {
                            report.step = Some(metrics);
                            report.step_duration = done.duration;
                            *self.grid = done.input.grid;
                            let live_rate = Arc::clone(&self.exchange.gated_rate);
                            self.exchange = done.input.exchange;
                            self.exchange.gated_rate = live_rate;
                            self.publish(self.step_dt_s);
                        }
                        Err(MacError::SubstepBudgetExceeded { .. }) => self.skip(),
                        Err(e) => return Err(WaterError::Solver(e)),
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(WaterError::Worker("water job worker exited".into()));
            }
        }
        self.owed_s += dt_s;
        while self.owed_s + 1e-9 >= self.step_dt_s {
            self.owed_s = (self.owed_s - self.step_dt_s).max(0.0);
            if self.in_flight {
                self.skip();
                continue;
            }
            let rate = self.exchange.gated_rate.load(Ordering::Relaxed);
            let mut exchange = self.exchange.clone();
            exchange.gated_rate = Arc::new(AtomicU8::new(rate));
            let input = Advance {
                grid: (*self.grid).clone(),
                exchange,
                rate,
                revision: self.revision,
                dt_s: self.step_dt_s,
            };
            self.submit
                .as_ref()
                .ok_or_else(|| WaterError::Worker("worker closed".into()))?
                .try_send(input)
                .map_err(|e| WaterError::Worker(e.to_string()))?;
            self.in_flight = true;
        }
        report.frame_seq = self.frame.seq;
        report.skipped_ticks = self.skipped_steps;
        report.skipped_duration = Duration::from_secs_f64(self.skipped_s);
        Ok(())
    }
}

impl Drop for WaterWorker {
    fn drop(&mut self) {
        self.submit = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{BrickCoord, CellSizeCode, MaterialId, Revision, VolumeId};
    use spall_voxel::{Brick, EditPlan};

    /// A 4 m x 1.5 m x 2 m open box, voxel resolution, left half full of water.
    fn tank() -> (Volume, WaterSetup) {
        let dims = [16u32, 6, 8];
        let mut volume = Volume::new(VolumeId::new(3).unwrap(), CellSizeCode::Quarter);
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        let mut plan = EditPlan::new(volume.id());
        for z in 0..dims[2] as i64 {
            for x in 0..dims[0] as i64 {
                for y in 0..dims[1] as i64 {
                    let wall = y < 2
                        || x < 2
                        || z < 2
                        || x >= dims[0] as i64 - 2
                        || z >= dims[2] as i64 - 2;
                    if wall {
                        plan.set(GlobalCell::new(x, y, z), MaterialId(1));
                    }
                }
            }
        }
        volume.apply_edit(&plan).unwrap();
        let domain = DomainSpec::new(GlobalCell::new(0, 0, 0), dims, 10_000).unwrap();
        let mut water = Vec::new();
        for z in 2..6 {
            for y in 2..6 {
                for x in 2..8 {
                    water.push((GlobalCell::new(x, y, z), 1.0));
                }
            }
        }
        (volume, WaterSetup::new(domain, water))
    }

    #[test]
    fn coarse_seed_preserves_authored_volume_and_frames_quantize_it() {
        let (volume, setup) = tank();
        let authored = setup.initial_fractions.len() as f64 * VOXEL_CELL_M.powi(3);
        let water = AuthoritativeWater::new(&volume, setup.with_coarsening(2)).unwrap();
        let seed = water.seed_report();
        assert!((seed.seeded_m3 - authored).abs() < 1.0e-12);
        assert_eq!(seed.dropped_in_solid_m3, 0.0);
        let frame = water.frame();
        assert_eq!(frame.dimensions, [8, 3, 4]);
        assert_eq!(frame.coarsen, 2);
        assert_eq!(frame.cell_size_m, 0.5);
        let quantized: f64 = frame
            .fractions
            .iter()
            .map(|f| f64::from(*f) / 255.0 * 0.125)
            .sum();
        assert!((quantized - authored).abs() < 1.0e-9);
    }

    #[test]
    fn springs_add_and_drains_remove_accounted_volume() {
        let (volume, mut setup) = tank();
        // A spring in the dry right half, a drain at the wet left floor.
        setup.sources = vec![GlobalCell::new(12, 2, 4)];
        setup.sinks = vec![GlobalCell::new(2, 2, 2)];
        let mut water = AuthoritativeWater::new(&volume, setup.with_coarsening(2)).unwrap();
        let seeded = water.frame().volume_m3;
        for _ in 0..30 {
            water.tick(&volume, false, 1.0 / 60.0).unwrap();
        }
        let frame = water.frame();
        assert!(frame.spring_added_m3 > 0.0);
        assert!(frame.drain_removed_m3 > 0.0);
        let expected =
            seeded + frame.spring_added_m3 - frame.drain_removed_m3 - frame.open_outflow_m3;
        assert!(
            (frame.volume_m3 - expected).abs() < 1.0e-8,
            "{} vs {expected}",
            frame.volume_m3
        );
    }

    /// A gated spring stays off until switched to a rate (e.g. a reservoir
    /// that starts dry), and switching back to off stops the fill without
    /// draining what it already added.
    #[test]
    fn gated_source_only_fills_once_a_rate_is_selected() {
        let (volume, mut setup) = tank();
        setup.gated_sources = [
            vec![GlobalCell::new(12, 2, 4)],
            vec![GlobalCell::new(12, 2, 4), GlobalCell::new(13, 2, 4)],
            Vec::new(),
        ];
        let mut water = AuthoritativeWater::new(&volume, setup.with_coarsening(2)).unwrap();
        assert_eq!(water.gated_sources_rate(), 0);
        let dry = water.frame().volume_m3;
        for _ in 0..10 {
            water.tick(&volume, false, 1.0 / 60.0).unwrap();
        }
        assert!(
            (water.frame().volume_m3 - dry).abs() < 1.0e-9,
            "rate 0 (off) must not add water: {} vs {dry}",
            water.frame().volume_m3
        );

        water.set_gated_sources_rate(1);
        assert_eq!(water.gated_sources_rate(), 1);
        for _ in 0..10 {
            water.tick(&volume, false, 1.0 / 60.0).unwrap();
        }
        let filling = water.frame().volume_m3;
        assert!(filling > dry, "rate 1 must add water");

        water.set_gated_sources_rate(0);
        for _ in 0..10 {
            water.tick(&volume, false, 1.0 / 60.0).unwrap();
        }
        assert!(
            (water.frame().volume_m3 - filling).abs() < 1.0e-6,
            "switching back to off must not drain what it added: {} vs {filling}",
            water.frame().volume_m3
        );
    }

    /// A higher rate refills a bigger authored footprint, so the same number
    /// of steps from dry adds more water — this is how "fill faster" works,
    /// not by refilling the same cell more often (it is already saturated
    /// after one step).
    #[test]
    fn a_higher_gated_source_rate_fills_faster() {
        let (volume, mut setup) = tank();
        // With `with_coarsening(2)` below, x=12/13 share one fluid cell, so
        // the second cell must differ in z (12,2,4) -> fluid (6,1,2) vs
        // (12,2,3) -> fluid (6,1,1) to actually add distinct source area.
        setup.gated_sources = [
            vec![GlobalCell::new(12, 2, 4)],
            vec![GlobalCell::new(12, 2, 4), GlobalCell::new(12, 2, 3)],
            Vec::new(),
        ];
        let filled_after_15_steps = |rate: u8| {
            let mut water =
                AuthoritativeWater::new(&volume, setup.clone().with_coarsening(2)).unwrap();
            water.set_gated_sources_rate(rate);
            for _ in 0..15 {
                water.tick(&volume, false, 1.0 / 60.0).unwrap();
            }
            water.frame().volume_m3
        };
        let rate1 = filled_after_15_steps(1);
        let rate2 = filled_after_15_steps(2);
        assert!(
            rate2 > rate1,
            "rate 2 ({rate2} m3) must fill faster than rate 1 ({rate1} m3)"
        );
    }

    #[test]
    fn worker_advances_on_its_own_thread_and_conserves_volume() {
        let (volume, setup) = tank();
        let mut water =
            AuthoritativeWater::new(&volume, setup.with_coarsening(2).on_worker(1.0 / 30.0))
                .unwrap();
        let start = water.frame();
        assert!(water.grid().is_some());
        for _ in 0..30 {
            water.tick(&volume, false, 1.0 / 60.0).unwrap();
        }
        // How many steps run depends on host speed (a slow worker drops its
        // backlog), so wait only for the first published step.
        let deadline = Instant::now() + Duration::from_secs(20);
        while water.frame().seq == start.seq && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            water.tick(&volume, false, 1.0 / 60.0).unwrap();
        }
        let frame = water.frame();
        assert!(frame.seq > start.seq, "worker never published a step");
        assert!(frame.fluid_time_s > 0.0);
        assert!((frame.volume_m3 - start.volume_m3).abs() < 1.0e-8);
        assert_ne!(frame.fractions, start.fractions, "released water must move");
    }

    #[test]
    fn unknown_residency_suspends_without_guessing_and_resumes_conservatively() {
        let (mut volume, setup) = tank();
        let resident = volume.clone();
        let mut water = AuthoritativeWater::new(&volume, setup).unwrap();
        let before = water.canonical_state();
        volume.evict_brick(BrickCoord::new(0, 0, 0));
        let metrics = water.tick(&volume, false, 1.0 / 60.0).unwrap();
        assert!(metrics.waiting_for_residency);
        assert_eq!(metrics.skipped_ticks, 1);
        assert_eq!(water.canonical_state(), before);
        let metrics = water.tick(&resident, false, 1.0 / 60.0).unwrap();
        assert!(metrics.step.is_some());
        assert!(
            (water.frame().volume_m3
                - before
                    .fractions
                    .iter()
                    .map(|b| f64::from_bits(*b))
                    .sum::<f64>()
                    * 0.25_f64.powi(3))
            .abs()
                < 1e-10
        );
    }

    #[test]
    fn quiet_water_sleeps_and_a_committed_boundary_wakes_it() {
        let (volume, mut setup) = tank();
        setup.config.gravity_m_s2 = [0.0; 3];
        let mut water = AuthoritativeWater::new(&volume, setup).unwrap();
        for _ in 0..60 {
            water.tick(&volume, false, 1.0 / 60.0).unwrap();
        }
        let before = water.canonical_state();
        let sleeping = water.tick(&volume, false, 1.0 / 60.0).unwrap();
        assert!(sleeping.sleeping);
        assert!(sleeping.step.is_none());
        assert_eq!(water.canonical_state(), before);
        assert!(
            water
                .tick(&volume, true, 1.0 / 60.0)
                .unwrap()
                .step
                .is_some()
        );
    }

    #[test]
    fn stale_worker_result_cannot_overwrite_a_placement_or_trapped_ledger() {
        let (mut volume, setup) = tank();
        let mut water = AuthoritativeWater::new(&volume, setup.on_worker(1.0 / 30.0)).unwrap();
        let Engine::Worker(worker) = &mut water.engine else {
            panic!("worker");
        };
        let input = Advance {
            grid: (*worker.grid).clone(),
            exchange: worker.exchange.clone(),
            revision: 0,
            rate: 0,
            dt_s: 1.0 / 60.0,
        };
        let (send, recv) = mpsc::sync_channel(1);
        worker.completed = recv;
        worker.in_flight = true;
        send.send(Completed {
            input,
            outcome: Ok(MacStepMetrics::default()),
            duration: Duration::ZERO,
        })
        .unwrap();
        let cell = GlobalCell::new(3, 2, 3);
        let initial = water.frame().volume_m3;
        let mut edit = EditPlan::new(volume.id());
        edit.set(cell, MaterialId(1));
        volume.apply_edit(&edit).unwrap();
        let report = water.tick(&volume, true, 1.0 / 60.0).unwrap();
        assert_eq!(report.skipped_ticks, 1);
        assert_eq!(water.grid().unwrap().fraction_at(cell), Some(0.0));
        assert!((water.frame().volume_m3 - initial).abs() < 1e-12);
        assert_eq!(water.frame().fluid_time_s, 0.0);
        volume.evict_brick(BrickCoord::new(0, 0, 0));
        let paused = water.tick(&volume, false, 1.0 / 60.0).unwrap();
        assert!(paused.waiting_for_residency);
        assert_eq!(paused.skipped_ticks, 2, "preserve earlier stale-job skips");
        assert!(paused.skipped_duration.as_secs_f64() >= 1.0 / 30.0);
    }
}

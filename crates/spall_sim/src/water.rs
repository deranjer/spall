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
//! [`WaterExecution::Worker`] moves the solver onto its own thread. The
//! simulation tick then only forwards elapsed time and committed boundaries;
//! the worker steps at its own fixed rate and publishes the newest
//! [`WaterFrame`]. When the solver cannot keep up, the worker drops the excess
//! simulated time (reported as skipped) rather than stalling the server tick,
//! so water runs slower than real time instead of delaying players.

use spall_core::GlobalCell;
use spall_fluid::grid_mac::{MacConfig, MacGridWorld, MacStepMetrics, PressurePreconditioner};
use spall_fluid::{DomainSpec, SolidBoundary, grid_mac::MacError};
use spall_voxel::Volume;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Voxel edge length the fluid domain is declared in.
const VOXEL_CELL_M: f64 = 0.25;
/// Accumulated simulated time the worker will run behind before it discards
/// the excess instead of trying to catch up.
const MAX_WORKER_BACKLOG_STEPS: f64 = 3.0;

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
        }
    }
}

/// Water timing and accounting from one authoritative server tick.
#[derive(Debug, Clone, Default)]
pub struct WaterTickMetrics {
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
    /// Simulated fluid time dropped by this tick's skips (inline) or so far
    /// (worker). Observability only.
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
    domain: DomainSpec,
    coarsen: u32,
    seed: WaterSeedReport,
    engine: Engine,
    /// Shared with the engine's [`Exchange`] (inline or moved onto the worker
    /// thread): which `WaterSetup::gated_sources` footprint, if any, is
    /// currently refilled.
    gated_rate: Arc<AtomicU8>,
}

enum Engine {
    Inline {
        grid: Box<MacGridWorld>,
        frame: Arc<WaterFrame>,
        skipped_ticks: u64,
        exchange: Exchange,
    },
    Worker(WaterWorker),
}

impl AuthoritativeWater {
    pub fn new(terrain: &Volume, setup: WaterSetup) -> Result<Self, WaterError> {
        let coarsen = setup.coarsen.max(1);
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
        })
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

    /// The live solver grid, only available for inline execution (a worker
    /// owns its grid on another thread; read [`Self::frame`] instead).
    pub fn grid(&self) -> Option<&MacGridWorld> {
        match &self.engine {
            Engine::Inline { grid, .. } => Some(grid),
            Engine::Worker(_) => None,
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
    pub fn tick(
        &mut self,
        terrain: &Volume,
        boundary_dirty: bool,
        dt_s: f64,
    ) -> Result<WaterTickMetrics, WaterError> {
        let mut report = WaterTickMetrics::default();
        let boundary = if boundary_dirty {
            let started = Instant::now();
            let boundary = capture_boundary(terrain, self.domain, self.coarsen)?;
            report.boundary_refresh = started.elapsed();
            Some(boundary)
        } else {
            None
        };
        match &mut self.engine {
            Engine::Inline {
                grid,
                frame,
                skipped_ticks,
                exchange,
            } => {
                if let Some(boundary) = boundary {
                    report.displaced_volume_m3 = grid
                        .refresh_boundary_displacing(&boundary)
                        .map_err(WaterError::Solver)?;
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
                        report.skipped_duration = Duration::from_secs_f64(dt_s);
                    }
                    Err(error) => return Err(WaterError::Solver(error)),
                }
                report.skipped_ticks = *skipped_ticks;
                report.step_duration = started.elapsed();
                report.frame_seq = frame.seq;
            }
            Engine::Worker(worker) => worker.advance(dt_s, boundary, &mut report)?,
        }
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

/// Messages from the simulation thread to the worker.
struct Advance {
    dt_s: f64,
    boundary: Option<SolidBoundary>,
}

#[derive(Default)]
struct WorkerStatus {
    failure: Option<String>,
    skipped_steps: u64,
    skipped_s: f64,
    displaced_m3: f64,
    last_step: Option<(MacStepMetrics, Duration)>,
}

struct WorkerShared {
    latest: Mutex<Arc<WaterFrame>>,
    status: Mutex<WorkerStatus>,
}

struct WaterWorker {
    submit: Option<mpsc::Sender<Advance>>,
    shared: Arc<WorkerShared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WaterWorker {
    fn spawn(
        mut grid: MacGridWorld,
        frame: Arc<WaterFrame>,
        mut exchange: Exchange,
        coarsen: u32,
        step_dt_s: f64,
    ) -> Result<Self, WaterError> {
        let shared = Arc::new(WorkerShared {
            latest: Mutex::new(frame),
            status: Mutex::new(WorkerStatus::default()),
        });
        let (submit, work) = mpsc::channel::<Advance>();
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("spall-water".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_worker(
                        &mut grid,
                        &mut exchange,
                        &work,
                        &thread_shared,
                        coarsen,
                        step_dt_s,
                    )
                }));
                let failure = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.to_string()),
                    Err(panic) => Some(format!(
                        "panicked: {}",
                        panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_owned())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "non-string panic payload".into())
                    )),
                };
                if let Some(failure) = failure {
                    eprintln!("spall-water: worker stopped: {failure}");
                    lock(&thread_shared.status).failure = Some(failure);
                }
            })
            .map_err(|error| WaterError::Worker(format!("thread did not start: {error}")))?;
        Ok(Self {
            submit: Some(submit),
            shared,
            thread: Some(thread),
        })
    }

    fn latest(&self) -> Arc<WaterFrame> {
        Arc::clone(&lock(&self.shared.latest))
    }

    fn advance(
        &mut self,
        dt_s: f64,
        boundary: Option<SolidBoundary>,
        report: &mut WaterTickMetrics,
    ) -> Result<(), WaterError> {
        {
            let mut status = lock(&self.shared.status);
            if let Some(failure) = &status.failure {
                return Err(WaterError::Worker(failure.clone()));
            }
            if let Some((metrics, duration)) = status.last_step.take() {
                report.step = Some(metrics);
                report.step_duration = duration;
            }
            report.skipped_ticks = status.skipped_steps;
            report.skipped_duration = Duration::from_secs_f64(status.skipped_s);
            report.displaced_volume_m3 = status.displaced_m3;
        }
        report.frame_seq = self.latest().seq;
        let sent = self
            .submit
            .as_ref()
            .is_some_and(|submit| submit.send(Advance { dt_s, boundary }).is_ok());
        if !sent || self.thread.as_ref().is_some_and(|t| t.is_finished()) {
            let failure = lock(&self.shared.status)
                .failure
                .clone()
                .unwrap_or_else(|| "worker thread exited without reporting a reason".into());
            return Err(WaterError::Worker(failure));
        }
        Ok(())
    }
}

impl Drop for WaterWorker {
    fn drop(&mut self) {
        // Closing the channel ends the worker loop.
        self.submit = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_worker(
    grid: &mut MacGridWorld,
    exchange: &mut Exchange,
    work: &mpsc::Receiver<Advance>,
    shared: &WorkerShared,
    coarsen: u32,
    step_dt_s: f64,
) -> Result<(), WaterError> {
    let mut owed_s = 0.0;
    let mut seq = lock(&shared.latest).seq;
    let mut fluid_time_s = 0.0;
    while let Ok(first) = work.recv() {
        // Coalesce everything that queued while the last step ran: boundaries
        // apply in commit order, elapsed time sums.
        let mut pending = vec![first];
        pending.extend(work.try_iter());
        for message in pending {
            owed_s += message.dt_s;
            if let Some(boundary) = message.boundary {
                let displaced = grid
                    .refresh_boundary_displacing(&boundary)
                    .map_err(WaterError::Solver)?;
                lock(&shared.status).displaced_m3 += displaced;
            }
        }
        let backlog_s = step_dt_s * MAX_WORKER_BACKLOG_STEPS;
        if owed_s > backlog_s {
            let dropped = owed_s - backlog_s;
            let mut status = lock(&shared.status);
            status.skipped_s += dropped;
            status.skipped_steps += (dropped / step_dt_s).floor() as u64;
            owed_s = backlog_s;
        }
        let mut stepped = false;
        // Tolerate float accumulation so 2 x 1/60 still pays for one 1/30 step.
        while owed_s + 1.0e-9 >= step_dt_s {
            owed_s -= step_dt_s;
            let started = Instant::now();
            match grid.step(step_dt_s) {
                Ok(metrics) => {
                    stepped = true;
                    exchange.apply(grid);
                    fluid_time_s += step_dt_s;
                    lock(&shared.status).last_step = Some((metrics, started.elapsed()));
                }
                Err(MacError::SubstepBudgetExceeded { .. }) => {
                    let mut status = lock(&shared.status);
                    status.skipped_steps += 1;
                    status.skipped_s += step_dt_s;
                }
                Err(error) => return Err(WaterError::Solver(error)),
            }
        }
        if stepped {
            seq += 1;
            let frame = Arc::new(WaterFrame::capture(
                seq,
                fluid_time_s,
                coarsen,
                grid,
                exchange,
            ));
            *lock(&shared.latest) = frame;
        }
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        assert!(water.grid().is_none());
        for _ in 0..30 {
            water.tick(&volume, false, 1.0 / 60.0).unwrap();
        }
        // How many steps run depends on host speed (a slow worker drops its
        // backlog), so wait only for the first published step.
        let deadline = Instant::now() + Duration::from_secs(20);
        while water.frame().seq == start.seq && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let frame = water.frame();
        assert!(frame.seq > start.seq, "worker never published a step");
        assert!(frame.fluid_time_s > 0.0);
        assert!((frame.volume_m3 - start.volume_m3).abs() < 1.0e-8);
        assert_ne!(frame.fractions, start.fractions, "released water must move");
    }
}

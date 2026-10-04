//! Bounded CPU MAC/VOF comparison backend. See `docs/reports/ENG-103-grid-method.md`.
use std::collections::VecDeque;

use rayon::prelude::*;
use std::time::Instant;

use spall_core::GlobalCell;

use crate::phase_graph::{GraphError, GraphLimits, PhaseGraph, PressureSmoother};
use crate::phase_water::PhaseWater;
use crate::{DomainSpec, SolidBoundary};
mod momentum;

#[derive(Debug, Clone, Copy)]
pub struct MacConfig {
    pub cell_size_m: f64,
    pub density_kg_m3: f64,
    pub gravity_m_s2: [f64; 3],
    pub cfl_limit: f64,
    pub max_substeps: u32,
    pub pressure_max_iterations: u32,
    pub pressure_relative_tolerance: f64,
    pub pressure_absolute_tolerance: f64,
    pub pressure_diagnostics: bool,
    pub open_top: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressurePreconditioner {
    Jacobi,
    Ic0,
    /// Modified IC(0): dropped fill is lumped onto the pivot (Bridson,
    /// tau = 0.97) with a 0.25 safety fallback to the unmodified diagonal.
    Mic0,
    /// One symmetric multigrid V-cycle: Galerkin coarse operators over 2x2x2
    /// piecewise-constant aggregates and symmetric Gauss-Seidel smoothing.
    Multigrid,
}

impl Default for MacConfig {
    fn default() -> Self {
        Self {
            cell_size_m: 0.25,
            density_kg_m3: 1000.0,
            gravity_m_s2: [0.0, -9.81, 0.0],
            cfl_limit: 0.45,
            max_substeps: 8,
            pressure_max_iterations: 400,
            pressure_relative_tolerance: 1.0e-8,
            pressure_absolute_tolerance: 1.0e-8,
            pressure_diagnostics: false,
            open_top: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MacStepMetrics {
    /// Advection-only mixture momentum ledger, excluding gravity/pressure.
    pub momentum_transport_error_kg_m_s: [f64; 3],
    pub momentum_wall_impulse_kg_m_s: [f64; 3],
    pub momentum_open_outflow_kg_m_s: [f64; 3],
    pub momentum_dual_mass_defect_kg: f64,
    pub momentum_transport_subcycles: u32,
    /// Additional momentum numerical payload, not whole peak/RSS.
    pub momentum_scratch_bytes: usize,
    pub substeps: u32,
    /// Sum of pressure matrix rows assembled across this outer tick's
    /// projections; partial cells participate under the documented C > 0 rule.
    pub pressure_active_rows_total: u64,
    pub pressure_iterations: u64,
    pub pressure_residual_initial_max: f64,
    pub pressure_residual_final_max: f64,
    pub pressure_converged_substeps: u32,
    /// Opt-in phase-aware coarse predictor; final pressure rows remain fine.
    pub phase_predictor_rows_total: u64,
    pub phase_predictor_reuses: u32,
    pub phase_predictor_rebuilds: u32,
    pub phase_predictor_micros: u64,
    pub phase_preconditioner_applications: u32,
    pub phase_preconditioner_micros: u64,
    /// Additional per-projection numerical arrays; not total peak or RSS.
    pub phase_preconditioner_scratch_bytes: usize,
    pub divergence_before_max_s: f64,
    pub divergence_after_max_s: f64,
    pub water_volume_before_m3: f64,
    pub water_volume_after_m3: f64,
    pub permitted_outflow_m3: f64,
    pub tracked_region_outflow_m3: f64,
    pub tracked_region_inflow_m3: f64,
    pub tracked_region_net_outflow_m3: f64,
    pub conservation_error_m3: f64,
    pub active_cells: usize,
    pub velocity_advection_micros: u64,
    pub pressure_solve_micros: u64,
    pub transport_micros: u64,
    pub strict_path_repair_count: u64,
    /// Additional transient CSR/BFS array capacities; excludes FCT scratch.
    pub strict_path_scratch_bytes: usize,
    pub boundary_micros: u64,
    pub total_micros: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct LiquidSpeedSample {
    pub cell: GlobalCell,
    pub fraction: f64,
    pub speed_m_s: f64,
    pub wall_adjacent: bool,
}

/// Detailed, diagnostic-only liquid fraction bucket. Buckets are logarithmic
/// except the exact-zero bucket; they never alter solver participation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FractionBandDiagnostic {
    pub label: &'static str,
    pub cells: usize,
    pub pressure_active_cells: usize,
    pub water_volume_m3: f64,
    pub maximum_cell_speed_m_s: f64,
    pub maximum_cell_speed_fraction: f64,
    pub maximum_face_outflow_l1_m_s: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MacError {
    MomentumPressureNotConverged { residual: f64 },
    MomentumInvalidState,
    MomentumMassMismatch { face: usize, defect_kg: f64 },
    MomentumSubcycleBudget { required: u64 },
    PhaseGraph(GraphError),
    InvalidConfig,
    InvalidTimeStep,
    SubstepBudgetExceeded { required: u32, maximum: u32 },
    InvalidWaterFraction,
    WaterOverlapsSolid(GlobalCell),
    WaterDisplacementCapacityExceeded { cell: GlobalCell },
    BoundaryMismatch,
    IncompatibleEnclosedPressureRegion,
    Ic0NonPositivePivot { cell: usize, pivot: f64 },
    TransportBoundsViolation { minimum: f64, maximum: f64 },
    LowOrderCflViolation { minimum: f64, maximum: f64 },
    NegativeOpenBoundaryFlux { volume_fraction: f64 },
}

impl std::fmt::Display for MacError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MomentumPressureNotConverged { residual } => write!(
                f,
                "momentum projection did not converge: residual {residual}"
            ),
            Self::MomentumInvalidState => write!(f, "nonfinite staggered momentum candidate"),
            Self::MomentumMassMismatch { face, defect_kg } => {
                write!(f, "dual mass mismatch at face {face}: {defect_kg} kg")
            }
            Self::MomentumSubcycleBudget { required } => write!(
                f,
                "momentum requires {required} subcycles, above 1024 budget"
            ),
            Self::PhaseGraph(error) => write!(f, "phase pressure predictor: {error}"),
            Self::InvalidConfig => write!(f, "invalid MAC grid configuration"),
            Self::InvalidTimeStep => write!(f, "outer timestep must be finite and positive"),
            Self::SubstepBudgetExceeded { required, maximum } => write!(
                f,
                "stability requires {required} substeps, above configured budget {maximum}"
            ),
            Self::InvalidWaterFraction => write!(f, "water fraction must be finite and in 0..=1"),
            Self::WaterOverlapsSolid(cell) => write!(f, "water overlaps solid voxel {cell:?}"),
            Self::WaterDisplacementCapacityExceeded { cell } => write!(
                f,
                "placing solid at {cell:?} leaves no open capacity for all displaced water"
            ),
            Self::BoundaryMismatch => write!(f, "candidate boundary dimensions do not match grid"),
            Self::IncompatibleEnclosedPressureRegion => write!(
                f,
                "fully enclosed pressure region has an incompatible divergence source"
            ),
            Self::Ic0NonPositivePivot { cell, pivot } => write!(
                f,
                "IC(0) encountered non-positive pivot {pivot} at active cell {cell}"
            ),
            Self::TransportBoundsViolation { minimum, maximum } => write!(
                f,
                "FCT update escaped fraction bounds: minimum {minimum}, maximum {maximum}"
            ),
            Self::LowOrderCflViolation { minimum, maximum } => write!(
                f,
                "donor-cell transport exceeded its CFL bounds: minimum {minimum}, maximum {maximum}"
            ),
            Self::NegativeOpenBoundaryFlux { volume_fraction } => write!(
                f,
                "open-top flux accounting produced negative net outflow {volume_fraction}"
            ),
        }
    }
}

impl std::error::Error for MacError {}

/// Dense, fully resident MAC grid with fractional cell-centered liquid volume.
#[derive(Debug, Clone)]
pub struct MacGridWorld {
    spec: DomainSpec,
    config: MacConfig,
    solid: Vec<bool>,
    fraction: Vec<f64>,
    /// Cell-volume units retained at placement sites with no connected capacity.
    trapped: Vec<f64>,
    pressure_pa: Vec<f64>,
    previous_liquid: Vec<bool>,
    previous_pressure_diagonal: Option<Vec<f64>>,
    previous_component_labels: Option<Vec<Option<usize>>>,
    previous_component_anchors: Option<Vec<bool>>,
    previous_pressure_preconditioner: Option<PressurePreconditioner>,
    u: Vec<f64>,
    v: Vec<f64>,
    w: Vec<f64>,
    cumulative_open_outflow_m3: f64,
    pressure_preconditioner: PressurePreconditioner,
    ambient_density_kg_m3: Option<f64>,
    freely_displaced_air: bool,
    experimental_surface_films: bool,
    compressible_enclosed_air: bool,
    stage_diagnostics: bool,
    diagnostic_outer_step: u64,
    strict_phase_bounds: bool,
    conservative_momentum: bool,
    phase_predictor: Option<PhasePredictor>,
}

impl MacGridWorld {
    pub fn new(boundary: &SolidBoundary, config: MacConfig) -> Result<Self, MacError> {
        if !config.cell_size_m.is_finite()
            || config.cell_size_m <= 0.0
            || !config.density_kg_m3.is_finite()
            || config.density_kg_m3 <= 0.0
            || !config.cfl_limit.is_finite()
            || !(0.0..=1.0).contains(&config.cfl_limit)
            || config.cfl_limit == 0.0
            || config.max_substeps == 0
            || config.pressure_max_iterations == 0
            || !config.pressure_relative_tolerance.is_finite()
            || config.pressure_relative_tolerance <= 0.0
            || !config.pressure_absolute_tolerance.is_finite()
            || config.pressure_absolute_tolerance <= 0.0
        {
            return Err(MacError::InvalidConfig);
        }
        let dims = boundary.spec().dimensions();
        let nx = dims[0] as usize;
        let ny = dims[1] as usize;
        let nz = dims[2] as usize;
        let mut solid = Vec::with_capacity(boundary.spec().cell_count());
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    solid.push(
                        boundary
                            .is_solid(GlobalCell::new(
                                boundary.spec().origin().x + x as i64,
                                boundary.spec().origin().y + y as i64,
                                boundary.spec().origin().z + z as i64,
                            ))
                            .unwrap_or(false),
                    );
                }
            }
        }
        let cell_count = boundary.spec().cell_count();
        Ok(Self {
            spec: boundary.spec(),
            config,
            solid,
            fraction: vec![0.0; cell_count],
            trapped: vec![0.0; cell_count],
            pressure_pa: vec![0.0; cell_count],
            previous_liquid: vec![false; cell_count],
            previous_pressure_diagonal: None,
            previous_component_labels: None,
            previous_component_anchors: None,
            previous_pressure_preconditioner: None,
            u: vec![0.0; (nx + 1) * ny * nz],
            v: vec![0.0; nx * (ny + 1) * nz],
            w: vec![0.0; nx * ny * (nz + 1)],
            cumulative_open_outflow_m3: 0.0,
            pressure_preconditioner: PressurePreconditioner::Jacobi,
            ambient_density_kg_m3: None,
            freely_displaced_air: false,
            experimental_surface_films: false,
            compressible_enclosed_air: false,
            stage_diagnostics: false,
            diagnostic_outer_step: 0,
            strict_phase_bounds: false,
            conservative_momentum: false,
            phase_predictor: None,
        })
    }

    pub fn spec(&self) -> DomainSpec {
        self.spec
    }

    pub fn config(&self) -> MacConfig {
        self.config
    }

    /// Reference phase coupling requires strictly bounded canonical fractions.
    /// Legacy replay retains its existing roundoff-tolerant transport path.
    pub(crate) fn set_strict_phase_bounds(&mut self) {
        self.strict_phase_bounds = true;
    }

    pub(crate) fn set_conservative_momentum(&mut self) -> Result<(), MacError> {
        if self.ambient_density_kg_m3.is_none() || !self.strict_phase_bounds {
            return Err(MacError::InvalidConfig);
        }
        self.conservative_momentum = true;
        // Mixture mass needs a more accurate incompressible volume flux than
        // the historical velocity-only reference. Tighten, never loosen, CG.
        self.config.pressure_relative_tolerance =
            self.config.pressure_relative_tolerance.min(1e-12);
        self.config.pressure_absolute_tolerance = self.config.pressure_absolute_tolerance.min(1e-8);
        Ok(())
    }

    pub fn set_pressure_preconditioner(&mut self, value: PressurePreconditioner) {
        self.pressure_preconditioner = value;
    }

    pub(crate) fn set_phase_pressure_predictor(
        &mut self,
        phase: &PhaseWater,
        limits: GraphLimits,
        sweeps: u32,
    ) -> Result<(), MacError> {
        if self.ambient_density_kg_m3.is_none()
            || sweeps == 0
            || sweeps > 64
            || self.spec != phase.geometry().fine_spec()
            || self.config.cell_size_m.to_bits() != phase.voxel_size_m().to_bits()
            || self
                .fraction
                .iter()
                .zip(phase.fractions())
                .any(|(a, b)| a.to_bits() != b.to_bits())
            || self
                .solid
                .iter()
                .enumerate()
                .any(|(i, &s)| s != phase.geometry().component_at_index(i).is_none())
        {
            return Err(MacError::InvalidConfig);
        }
        let graph = PhaseGraph::build(phase, limits).map_err(MacError::PhaseGraph)?;
        self.phase_predictor = Some(PhasePredictor {
            phase: phase.clone(),
            graph,
            limits,
            sweeps,
            iterative: false,
        });
        Ok(())
    }

    pub(crate) fn set_phase_pressure_preconditioner(
        &mut self,
        phase: &PhaseWater,
        limits: GraphLimits,
        sweeps: u32,
    ) -> Result<(), MacError> {
        self.set_phase_pressure_predictor(phase, limits, sweeps)?;
        self.phase_predictor.as_mut().unwrap().iterative = true;
        Ok(())
    }

    /// Opt-in variable-density, two-phase pressure comparison. Both water and
    /// ambient air participate; C is retained as conservative water volume.
    /// Enables geometric PLIC target fluxes within the conservative FCT limiter.
    /// This changes the physical model, including inertia of the ambient phase.
    pub fn set_ambient_density(&mut self, density: f64) -> Result<(), MacError> {
        if !density.is_finite() || density <= 0.0 || density >= self.config.density_kg_m3 {
            return Err(MacError::InvalidConfig);
        }
        self.ambient_density_kg_m3 = Some(density);
        self.freely_displaced_air = false;
        self.experimental_surface_films = false;
        // Sealed air defaults to isothermal compressible gas; see
        // `set_compressible_enclosed_air`.
        self.compressible_enclosed_air = true;
        self.pressure_pa.fill(0.0);
        self.previous_pressure_diagonal = None;
        Ok(())
    }

    /// Liquid-only pressure with atmospheric empty space, including sealed
    /// pockets. Retains geometric PLIC/FCT transport and advect-before-force
    /// ordering, but has no air inertia, pressure rows or compression.
    pub fn set_freely_displaced_air(&mut self) -> Result<(), MacError> {
        if self.conservative_momentum || self.phase_predictor.is_some() {
            return Err(MacError::InvalidConfig);
        }
        self.ambient_density_kg_m3 = None;
        self.compressible_enclosed_air = false;
        self.freely_displaced_air = true;
        self.experimental_surface_films = false;
        self.strict_phase_bounds = true;
        // Wet-only projection has much less work, but saturated donor cells
        // need pressure residuals below the unchanged 1e-10 low-order bound.
        self.config.pressure_relative_tolerance =
            self.config.pressure_relative_tolerance.min(1.0e-10);
        self.pressure_pa.fill(0.0);
        self.previous_pressure_diagonal = None;
        Ok(())
    }

    /// ENG-122 diagnostic prototype, NOT an accepted gameplay policy. Its
    /// half-cell velocity/mass support fails shallow-film energy and rest
    /// gates. Only the explicitly opted-in shallow-shore probe uses it.
    pub fn set_experimental_surface_films(&mut self) -> Result<(), MacError> {
        if !self.freely_displaced_air {
            return Err(MacError::InvalidConfig);
        }
        self.experimental_surface_films = true;
        Ok(())
    }

    /// Two-phase only (on by default there): treat air that is sealed off from
    /// the open top as an isothermal ideal gas (pV = const) instead of an
    /// incompressible phase.
    /// Its cells get div(u) = -(1 - C) (dp/dt) / P_abs, which adds a positive
    /// diagonal term to the pressure system; the stored gauge pressure then
    /// carries physical meaning inside sealed regions.
    pub fn set_compressible_enclosed_air(&mut self, enabled: bool) -> Result<(), MacError> {
        if enabled && self.ambient_density_kg_m3.is_none() {
            return Err(MacError::InvalidConfig);
        }
        self.compressible_enclosed_air = enabled;
        Ok(())
    }

    /// Per cell, the air fraction (1 - C) that is sealed from the atmosphere,
    /// else 0. Air-dominated cells (C < 1/2) connected to the open top are
    /// vented; cells adjacent to vented air share its free surface.
    fn sealed_air_fractions(&self) -> Vec<f64> {
        let [nx, ny, nz] = self.dims();
        let n = self.fraction.len();
        let air_path = |i: usize| !self.solid[i] && self.fraction[i] < 0.5;
        let mut vented = vec![false; n];
        let mut queue = VecDeque::new();
        if self.config.open_top {
            for z in 0..nz {
                for x in 0..nx {
                    let i = self.cell_index(x, ny - 1, z);
                    if air_path(i) {
                        vented[i] = true;
                        queue.push_back(i);
                    }
                }
            }
        }
        let neighbors = |i: usize| {
            let (x, y, z) = (i % nx, (i / nx) % ny, i / (nx * ny));
            [
                (x > 0).then(|| i - 1),
                (x + 1 < nx).then(|| i + 1),
                (y > 0).then(|| i - nx),
                (y + 1 < ny).then(|| i + nx),
                (z > 0).then(|| i - nx * ny),
                (z + 1 < nz).then(|| i + nx * ny),
            ]
        };
        while let Some(i) = queue.pop_front() {
            for j in neighbors(i).into_iter().flatten() {
                if !vented[j] && air_path(j) {
                    vented[j] = true;
                    queue.push_back(j);
                }
            }
        }
        (0..n)
            .map(|i| {
                let c = self.fraction[i];
                let touches_vent = vented[i]
                    || (self.config.open_top && (i / nx) % ny == ny - 1)
                    || neighbors(i).into_iter().flatten().any(|j| vented[j]);
                if self.solid[i] || c >= 1.0 || touches_vent {
                    0.0
                } else {
                    1.0 - c
                }
            })
            .collect()
    }

    fn relative_inverse_face_density(&self, a: usize, b: usize) -> f64 {
        self.ambient_density_kg_m3.map_or(1.0, |air| {
            let c = 0.5 * (self.fraction[a] + self.fraction[b]);
            self.config.density_kg_m3 / (air + c * (self.config.density_kg_m3 - air))
        })
    }

    /// Emit per-substep water, pressure, energy, interface velocity, and
    /// applied open-boundary flux snapshots. Disabled unless explicitly asked.
    pub fn set_stage_diagnostics(&mut self, enabled: bool) {
        self.stage_diagnostics = enabled;
    }

    pub fn set_pressure_tolerances(
        &mut self,
        relative: f64,
        absolute: f64,
    ) -> Result<(), MacError> {
        if !relative.is_finite() || relative <= 0.0 || !absolute.is_finite() || absolute <= 0.0 {
            return Err(MacError::InvalidConfig);
        }
        self.config.pressure_relative_tolerance = relative;
        self.config.pressure_absolute_tolerance = absolute;
        Ok(())
    }

    pub fn set_pressure_diagnostics(&mut self, enabled: bool) {
        if enabled && !self.config.pressure_diagnostics {
            self.previous_pressure_diagonal = Some(vec![0.0; self.fraction.len()]);
            self.previous_component_labels = Some(vec![None; self.fraction.len()]);
            self.previous_component_anchors = Some(Vec::new());
            self.previous_pressure_preconditioner = None;
        } else if !enabled {
            self.previous_pressure_diagonal = None;
            self.previous_component_labels = None;
            self.previous_component_anchors = None;
            self.previous_pressure_preconditioner = None;
        }
        self.config.pressure_diagnostics = enabled;
    }

    pub fn fraction_at(&self, cell: GlobalCell) -> Option<f64> {
        self.cell_index_global(cell).map(|i| self.fraction[i])
    }

    /// Every cell's liquid fraction in [`DomainSpec`] linear order (X fastest,
    /// then Y, then Z). Solid cells are always zero.
    pub fn fractions(&self) -> &[f64] {
        &self.fraction
    }

    pub fn pressure_at(&self, cell: GlobalCell) -> Option<f64> {
        self.spec.index_of(cell).map(|i| self.pressure_pa[i])
    }

    /// Cell-centred liquid momentum diagnostic; not a conservative advection
    /// ledger or a mixture-pressure wall-force accounting proof.
    pub fn liquid_momentum_kg_m_s(&self) -> [f64; 3] {
        let [nx, ny, nz] = self.dims();
        let mut total = [0.0; 3];
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let mass = self.config.density_kg_m3
                        * self.fraction[self.cell_index(x, y, z)]
                        * self.cell_volume();
                    let velocity = [
                        0.5 * (self.u[self.u_index(x, y, z)] + self.u[self.u_index(x + 1, y, z)]),
                        0.5 * (self.v[self.v_index(x, y, z)] + self.v[self.v_index(x, y + 1, z)]),
                        0.5 * (self.w[self.w_index(x, y, z)] + self.w[self.w_index(x, y, z + 1)]),
                    ];
                    for axis in 0..3 {
                        total[axis] += mass * velocity[axis];
                    }
                }
            }
        }
        total
    }

    pub fn trapped_fractions(&self) -> &[f64] {
        &self.trapped
    }

    pub fn restore_fractions(&mut self, values: &[f64]) -> Result<(), MacError> {
        if values.len() != self.fraction.len()
            || values.iter().enumerate().any(|(i, v)| {
                !v.is_finite() || !(-1e-9..=1.0 + 1e-9).contains(v) || (self.solid[i] && *v != 0.0)
            })
        {
            return Err(MacError::InvalidWaterFraction);
        }
        self.fraction.copy_from_slice(values);
        Ok(())
    }

    /// Recover a durable amount snapshot against newer committed geometry.
    /// Any overlap is conservatively displaced/retained before publication.
    pub fn restore_displacing(
        &mut self,
        values: &[f64],
        boundary: &SolidBoundary,
    ) -> Result<(), MacError> {
        if values.len() != self.fraction.len()
            || values
                .iter()
                .any(|v| !v.is_finite() || !(-1e-9..=1.0 + 1e-9).contains(v))
        {
            return Err(MacError::InvalidWaterFraction);
        }
        let mut candidate = self.clone();
        candidate.fraction.copy_from_slice(values);
        candidate.refresh_boundary_retaining(boundary)?;
        *self = candidate;
        Ok(())
    }

    pub fn trapped_volume_m3(&self) -> f64 {
        self.trapped.iter().sum::<f64>() * self.cell_volume()
    }

    pub fn restore_trapped(&mut self, values: &[f64]) -> Result<(), MacError> {
        if values.len() != self.trapped.len() || values.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return Err(MacError::InvalidWaterFraction);
        }
        self.trapped.copy_from_slice(values);
        Ok(())
    }

    pub fn restore_outflow(&mut self, value: f64) -> Result<(), MacError> {
        if !value.is_finite() || value < 0.0 {
            return Err(MacError::InvalidWaterFraction);
        }
        self.cumulative_open_outflow_m3 = value;
        Ok(())
    }

    pub fn set_fraction(&mut self, cell: GlobalCell, fraction: f64) -> Result<(), MacError> {
        if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
            return Err(MacError::InvalidWaterFraction);
        }
        let i = self
            .cell_index_global(cell)
            .ok_or(MacError::BoundaryMismatch)?;
        if self.solid[i] && fraction > 0.0 {
            return Err(MacError::WaterOverlapsSolid(cell));
        }
        self.fraction[i] = fraction;
        Ok(())
    }

    pub fn water_volume_m3(&self) -> f64 {
        self.fraction.iter().sum::<f64>() * self.cell_volume() + self.trapped_volume_m3()
    }

    pub fn water_mass_kg(&self) -> f64 {
        self.water_volume_m3() * self.config.density_kg_m3
    }

    pub fn cumulative_open_outflow_m3(&self) -> f64 {
        self.cumulative_open_outflow_m3
    }

    pub fn active_cells(&self) -> usize {
        self.fraction.iter().filter(|c| **c > 0.0).count()
    }

    pub fn allocated_bytes(&self) -> usize {
        (self.solid.capacity() + self.previous_liquid.capacity()) * size_of::<bool>()
            + (self.fraction.capacity() + self.trapped.capacity() + self.pressure_pa.capacity())
                * size_of::<f64>()
            + (self.u.capacity() + self.v.capacity() + self.w.capacity()) * size_of::<f64>()
            + self
                .previous_pressure_diagonal
                .as_ref()
                .map_or(0, |v| v.capacity() * size_of::<f64>())
            + self
                .previous_component_labels
                .as_ref()
                .map_or(0, |v| v.capacity() * size_of::<Option<usize>>())
            + self
                .previous_component_anchors
                .as_ref()
                .map_or(0, |v| v.capacity() * size_of::<bool>())
            // Conservative retained upper bound: the initial canonical phase
            // may share these fractions. Immutable geometry/faces are excluded.
            + self.phase_predictor.as_ref().map_or(0, |s|
                std::mem::size_of_val(s.phase.fractions()) + s.graph.array_storage_bytes())
    }

    pub fn fraction_bounds(&self) -> (f64, f64) {
        (
            self.fraction.iter().copied().fold(f64::INFINITY, f64::min),
            self.fraction
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max),
        )
    }

    pub fn max_face_component_velocity_m_s(&self) -> f64 {
        self.u
            .iter()
            .chain(&self.v)
            .chain(&self.w)
            .map(|v| v.abs())
            .fold(0.0, f64::max)
    }

    /// Largest reconstructed cell-centred speed in cells carrying liquid.
    pub fn max_liquid_speed_m_s(&self) -> f64 {
        let [nx, ny, nz] = self.dims();
        let mut maximum: f64 = 0.0;
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let i = self.cell_index(x, y, z);
                    if self.fraction[i] <= 0.0 || self.solid[i] {
                        continue;
                    }
                    let vx =
                        0.5 * (self.u[self.u_index(x, y, z)] + self.u[self.u_index(x + 1, y, z)]);
                    let vy =
                        0.5 * (self.v[self.v_index(x, y, z)] + self.v[self.v_index(x, y + 1, z)]);
                    let vz =
                        0.5 * (self.w[self.w_index(x, y, z)] + self.w[self.w_index(x, y, z + 1)]);
                    maximum = maximum.max(vx.hypot(vy).hypot(vz));
                }
            }
        }
        maximum
    }

    /// Reconstructed cell-centred speed samples for measurement and fixture diagnostics.
    pub fn liquid_speed_samples(&self) -> Vec<LiquidSpeedSample> {
        let [nx, ny, nz] = self.dims();
        let mut samples = Vec::with_capacity(self.active_cells());
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let i = self.cell_index(x, y, z);
                    let fraction = self.fraction[i];
                    if fraction <= 0.0 || self.solid[i] {
                        continue;
                    }
                    let vx =
                        0.5 * (self.u[self.u_index(x, y, z)] + self.u[self.u_index(x + 1, y, z)]);
                    let vy =
                        0.5 * (self.v[self.v_index(x, y, z)] + self.v[self.v_index(x, y + 1, z)]);
                    let vz =
                        0.5 * (self.w[self.w_index(x, y, z)] + self.w[self.w_index(x, y, z + 1)]);
                    let wall_adjacent = (x > 0 && self.solid[self.cell_index(x - 1, y, z)])
                        || (x + 1 < nx && self.solid[self.cell_index(x + 1, y, z)])
                        || (y > 0 && self.solid[self.cell_index(x, y - 1, z)])
                        || (y + 1 < ny && self.solid[self.cell_index(x, y + 1, z)])
                        || (z > 0 && self.solid[self.cell_index(x, y, z - 1)])
                        || (z + 1 < nz && self.solid[self.cell_index(x, y, z + 1)]);
                    samples.push(LiquidSpeedSample {
                        cell: self.spec.cell_at(i),
                        fraction,
                        speed_m_s: vx.hypot(vy).hypot(vz),
                        wall_adjacent,
                    });
                }
            }
        }
        samples
    }

    /// Expensive per-cell statistics for separate tracing runs. All positive
    /// fractions remain in the pressure active set, irrespective of bucket.
    pub fn fraction_band_diagnostics(&self) -> Vec<FractionBandDiagnostic> {
        const LABELS: [&str; 8] = [
            "C == 0",
            "0 < C < 1e-8",
            "1e-8 <= C < 1e-6",
            "1e-6 <= C < 1e-4",
            "1e-4 <= C < 1e-3",
            "1e-3 <= C < 1e-2",
            "1e-2 <= C < 1e-1",
            "1e-1 <= C <= 1",
        ];
        let mut bands: Vec<_> = LABELS
            .iter()
            .map(|label| FractionBandDiagnostic {
                label,
                cells: 0,
                pressure_active_cells: 0,
                water_volume_m3: 0.0,
                maximum_cell_speed_m_s: 0.0,
                maximum_cell_speed_fraction: 0.0,
                maximum_face_outflow_l1_m_s: 0.0,
            })
            .collect();
        let [nx, ny, nz] = self.dims();
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let i = self.cell_index(x, y, z);
                    let c = self.fraction[i];
                    let band = if c == 0.0 {
                        0
                    } else if c < 1.0e-8 {
                        1
                    } else if c < 1.0e-6 {
                        2
                    } else if c < 1.0e-4 {
                        3
                    } else if c < 1.0e-2 {
                        if c < 1.0e-3 { 4 } else { 5 }
                    } else if c < 1.0e-1 {
                        6
                    } else {
                        7
                    };
                    let item = &mut bands[band];
                    item.cells += 1;
                    if !self.solid[i] && (c > 0.0 || self.ambient_density_kg_m3.is_some()) {
                        item.pressure_active_cells += 1;
                    }
                    if c == 0.0 || self.solid[i] {
                        continue;
                    }
                    item.water_volume_m3 += c * self.cell_volume();
                    let vx =
                        0.5 * (self.u[self.u_index(x, y, z)] + self.u[self.u_index(x + 1, y, z)]);
                    let vy =
                        0.5 * (self.v[self.v_index(x, y, z)] + self.v[self.v_index(x, y + 1, z)]);
                    let vz =
                        0.5 * (self.w[self.w_index(x, y, z)] + self.w[self.w_index(x, y, z + 1)]);
                    let cell_speed = vx.hypot(vy).hypot(vz);
                    if cell_speed > item.maximum_cell_speed_m_s {
                        item.maximum_cell_speed_m_s = cell_speed;
                        item.maximum_cell_speed_fraction = c;
                    }
                    let outgoing = self.u[self.u_index(x + 1, y, z)].max(0.0)
                        + (-self.u[self.u_index(x, y, z)]).max(0.0)
                        + self.v[self.v_index(x, y + 1, z)].max(0.0)
                        + (-self.v[self.v_index(x, y, z)]).max(0.0)
                        + self.w[self.w_index(x, y, z + 1)].max(0.0)
                        + (-self.w[self.w_index(x, y, z)]).max(0.0);
                    item.maximum_face_outflow_l1_m_s =
                        item.maximum_face_outflow_l1_m_s.max(outgoing);
                }
            }
        }
        bands
    }

    /// Connected pressure regions after diagnostic-only removal of fractions
    /// below each threshold. This does not affect the solver's active set.
    pub fn pressure_component_counts_at_fraction_thresholds(&self) -> Vec<(f64, usize)> {
        [0.0, 1.0e-8, 1.0e-6, 1.0e-4, 1.0e-3, 1.0e-2, 1.0e-1]
            .into_iter()
            .map(|minimum| (minimum, self.pressure_component_count(minimum)))
            .collect()
    }

    fn pressure_component_count(&self, minimum_fraction: f64) -> usize {
        let [nx, ny, nz] = self.dims();
        let mut seen = vec![false; self.fraction.len()];
        let mut count = 0;
        for seed in 0..seen.len() {
            if seen[seed] || self.solid[seed] || self.fraction[seed] < minimum_fraction {
                continue;
            }
            if minimum_fraction == 0.0 && self.fraction[seed] == 0.0 {
                continue;
            }
            count += 1;
            seen[seed] = true;
            let mut queue = VecDeque::from([seed]);
            while let Some(i) = queue.pop_front() {
                let x = i % nx;
                let y = (i / nx) % ny;
                let z = i / (nx * ny);
                for axis in 0..3 {
                    for dir in [-1isize, 1] {
                        let mut p = [x as isize, y as isize, z as isize];
                        p[axis] += dir;
                        if p[0] < 0
                            || p[1] < 0
                            || p[2] < 0
                            || p[0] >= nx as isize
                            || p[1] >= ny as isize
                            || p[2] >= nz as isize
                        {
                            continue;
                        }
                        let j = self.cell_index(p[0] as usize, p[1] as usize, p[2] as usize);
                        if !seen[j]
                            && !self.solid[j]
                            && self.fraction[j] >= minimum_fraction
                            && self.fraction[j] > 0.0
                        {
                            seen[j] = true;
                            queue.push_back(j);
                        }
                    }
                }
            }
        }
        count
    }

    pub fn kinetic_energy_j(&self) -> f64 {
        let [nx, ny, nz] = self.dims();
        let mut energy = 0.0;
        let half_face_mass = 0.25 * self.config.density_kg_m3 * self.cell_volume();
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..=nx {
                    let left = if x > 0 {
                        self.fraction[self.cell_index(x - 1, y, z)]
                    } else {
                        0.0
                    };
                    let right = if x < nx {
                        self.fraction[self.cell_index(x, y, z)]
                    } else {
                        0.0
                    };
                    energy +=
                        half_face_mass * (left + right) * self.u[self.u_index(x, y, z)].powi(2);
                }
            }
        }
        for z in 0..nz {
            for y in 0..=ny {
                for x in 0..nx {
                    let bottom = if y > 0 {
                        self.fraction[self.cell_index(x, y - 1, z)]
                    } else {
                        0.0
                    };
                    let top = if y < ny {
                        self.fraction[self.cell_index(x, y, z)]
                    } else {
                        0.0
                    };
                    energy +=
                        half_face_mass * (bottom + top) * self.v[self.v_index(x, y, z)].powi(2);
                }
            }
        }
        for z in 0..=nz {
            for y in 0..ny {
                for x in 0..nx {
                    let back = if z > 0 {
                        self.fraction[self.cell_index(x, y, z - 1)]
                    } else {
                        0.0
                    };
                    let front = if z < nz {
                        self.fraction[self.cell_index(x, y, z)]
                    } else {
                        0.0
                    };
                    energy +=
                        half_face_mass * (back + front) * self.w[self.w_index(x, y, z)].powi(2);
                }
            }
        }
        energy
    }

    pub fn gravitational_potential_energy_j(&self) -> f64 {
        let [nx, ny, nz] = self.dims();
        let mut energy = 0.0;
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let i = self.cell_index(x, y, z);
                    let mass = self.config.density_kg_m3 * self.fraction[i] * self.cell_volume();
                    let origin = self.spec.origin();
                    let position = [
                        (origin.x as f64 + x as f64 + 0.5) * self.config.cell_size_m,
                        (origin.y as f64 + y as f64 + 0.5) * self.config.cell_size_m,
                        (origin.z as f64 + z as f64 + 0.5) * self.config.cell_size_m,
                    ];
                    let gravity_dot_position = self.config.gravity_m_s2[0] * position[0]
                        + self.config.gravity_m_s2[1] * position[1]
                        + self.config.gravity_m_s2[2] * position[2];
                    energy -= mass * gravity_dot_position;
                }
            }
        }
        energy
    }

    pub fn mean_liquid_pressure_pa(&self) -> f64 {
        let mut sum = 0.0;
        let mut count = 0usize;
        for (i, p) in self.pressure_pa.iter().enumerate() {
            if self.fraction[i] > 0.0 && !self.solid[i] {
                sum += p;
                count += 1;
            }
        }
        if count == 0 { 0.0 } else { sum / count as f64 }
    }

    /// Validate a proposed solid snapshot without changing live fluid state.
    /// The caller publishes the staged voxel volume and this prepared grid
    /// together only after both preparations succeed.
    pub fn prepare_boundary(&self, boundary: &SolidBoundary) -> Result<Vec<bool>, MacError> {
        if boundary.spec() != self.spec {
            return Err(MacError::BoundaryMismatch);
        }
        let mut candidate = Vec::with_capacity(self.solid.len());
        for index in 0..self.solid.len() {
            let cell = self.spec.cell_at(index);
            let solid = boundary.is_solid(cell).ok_or(MacError::BoundaryMismatch)?;
            if solid && self.fraction[index] > 0.0 {
                return Err(MacError::WaterOverlapsSolid(cell));
            }
            candidate.push(solid);
        }
        Ok(candidate)
    }

    pub fn commit_boundary(&mut self, prepared: Vec<bool>) -> Result<(), MacError> {
        if prepared.len() != self.solid.len() {
            return Err(MacError::BoundaryMismatch);
        }
        self.solid = prepared;
        self.pressure_pa.fill(0.0);
        self.previous_liquid.fill(false);
        if let Some(diagonal) = &mut self.previous_pressure_diagonal {
            diagonal.fill(0.0);
        }
        if let Some(labels) = &mut self.previous_component_labels {
            labels.fill(None);
        }
        if let Some(anchors) = &mut self.previous_component_anchors {
            anchors.clear();
        }
        self.previous_pressure_preconditioner = None;
        self.enforce_wall_velocities();
        Ok(())
    }

    /// Atomically installs a new solid boundary and conservatively relocates
    /// any water cells covered by the new solids. Each source distributes its
    /// fraction breadth-first through face-adjacent open cells, in stable
    /// `-X,+X,-Y,+Y,-Z,+Z` order. No water is deleted on capacity failure.
    pub fn refresh_boundary_displacing(
        &mut self,
        boundary: &SolidBoundary,
    ) -> Result<f64, MacError> {
        self.refresh_boundary_policy(boundary, false)
    }

    /// Production placement policy: retain any unplaceable volume at its source
    /// in an explicit ledger. Reopening connected capacity releases that volume.
    /// The ledger never participates in flow or crosses intervening solid cells.
    pub fn refresh_boundary_retaining(
        &mut self,
        boundary: &SolidBoundary,
    ) -> Result<f64, MacError> {
        self.refresh_boundary_policy(boundary, true)
    }

    fn refresh_boundary_policy(
        &mut self,
        boundary: &SolidBoundary,
        retain: bool,
    ) -> Result<f64, MacError> {
        if boundary.spec() != self.spec {
            return Err(MacError::BoundaryMismatch);
        }
        let mut candidate_solid = Vec::with_capacity(self.solid.len());
        let mut candidate_fraction = self.fraction.clone();
        let mut displaced = Vec::new();
        let mut candidate_trapped = self.trapped.clone();
        // `index` addresses three different parallel arrays here (and feeds
        // `cell_at`), not just `candidate_fraction`, so `enumerate()` over one
        // of them would not actually simplify this.
        #[allow(clippy::needless_range_loop)]
        for index in 0..self.solid.len() {
            let cell = self.spec.cell_at(index);
            let is_solid = boundary.is_solid(cell).ok_or(MacError::BoundaryMismatch)?;
            candidate_solid.push(is_solid);
            if is_solid && candidate_fraction[index] > 0.0 {
                displaced.push((index, candidate_fraction[index]));
                candidate_fraction[index] = 0.0;
            }
            if candidate_trapped[index] > 0.0 {
                displaced.push((index, candidate_trapped[index]));
                candidate_trapped[index] = 0.0;
            }
        }

        let mut visited = vec![false; self.solid.len()];
        let mut queue = VecDeque::new();
        let neighbor_cells = |cell: GlobalCell| {
            [
                cell.x
                    .checked_sub(1)
                    .map(|x| GlobalCell::new(x, cell.y, cell.z)),
                cell.x
                    .checked_add(1)
                    .map(|x| GlobalCell::new(x, cell.y, cell.z)),
                cell.y
                    .checked_sub(1)
                    .map(|y| GlobalCell::new(cell.x, y, cell.z)),
                cell.y
                    .checked_add(1)
                    .map(|y| GlobalCell::new(cell.x, y, cell.z)),
                cell.z
                    .checked_sub(1)
                    .map(|z| GlobalCell::new(cell.x, cell.y, z)),
                cell.z
                    .checked_add(1)
                    .map(|z| GlobalCell::new(cell.x, cell.y, z)),
            ]
        };
        for (source, mut remaining) in displaced.iter().copied() {
            visited.fill(false);
            queue.clear();
            visited[source] = true;
            if !candidate_solid[source] {
                queue.push_back(source);
            }
            for neighbor in neighbor_cells(self.spec.cell_at(source))
                .into_iter()
                .flatten()
            {
                if let Some(index) = self.cell_index_global(neighbor)
                    && !visited[index]
                    && !candidate_solid[index]
                {
                    visited[index] = true;
                    queue.push_back(index);
                }
            }
            while let Some(index) = queue.pop_front() {
                if remaining > 0.0 {
                    let capacity = (1.0 - candidate_fraction[index]).max(0.0);
                    let transfer = capacity.min(remaining);
                    candidate_fraction[index] += transfer;
                    remaining -= transfer;
                }
                if remaining == 0.0 {
                    break;
                }
                for neighbor in neighbor_cells(self.spec.cell_at(index))
                    .into_iter()
                    .flatten()
                {
                    if let Some(next) = self.cell_index_global(neighbor)
                        && !visited[next]
                        && !candidate_solid[next]
                    {
                        visited[next] = true;
                        queue.push_back(next);
                    }
                }
            }
            if remaining > 0.0 && retain {
                candidate_trapped[source] += remaining;
            } else if remaining > 0.0 {
                return Err(MacError::WaterDisplacementCapacityExceeded {
                    cell: self.spec.cell_at(source),
                });
            }
        }

        let moved_fraction: f64 = displaced.iter().map(|(_, fraction)| fraction).sum();
        self.solid = candidate_solid;
        self.fraction = candidate_fraction;
        self.trapped = candidate_trapped;
        self.pressure_pa.fill(0.0);
        self.previous_liquid.fill(false);
        if let Some(diagonal) = &mut self.previous_pressure_diagonal {
            diagonal.fill(0.0);
        }
        if let Some(labels) = &mut self.previous_component_labels {
            labels.fill(None);
        }
        if let Some(anchors) = &mut self.previous_component_anchors {
            anchors.clear();
        }
        self.previous_pressure_preconditioner = None;
        self.enforce_wall_velocities();
        Ok(moved_fraction * self.cell_volume())
    }

    /// Advance one outer fixed tick. Stability overload returns before any
    /// state changes; every accepted substep is included in reported time.
    pub fn step(&mut self, outer_dt_s: f64) -> Result<MacStepMetrics, MacError> {
        self.step_tracking_region(outer_dt_s, None)
    }

    pub fn step_tracking_region(
        &mut self,
        outer_dt_s: f64,
        tracked_region: Option<&[bool]>,
    ) -> Result<MacStepMetrics, MacError> {
        if !outer_dt_s.is_finite() || outer_dt_s <= 0.0 {
            return Err(MacError::InvalidTimeStep);
        }
        if tracked_region.is_some_and(|region| region.len() != self.fraction.len()) {
            return Err(MacError::BoundaryMismatch);
        }
        let (max_speed, max_speed_cell) = self.max_face_speed_l1();
        let advective_dt = if max_speed > 0.0 {
            self.config.cfl_limit * self.config.cell_size_m / max_speed
        } else {
            f64::INFINITY
        };
        let gravity = self
            .config
            .gravity_m_s2
            .iter()
            .map(|g| g.abs())
            .fold(0.0, f64::max);
        let gravity_dt = if gravity > 0.0 {
            (2.0 * self.config.cfl_limit * self.config.cell_size_m / gravity).sqrt()
        } else {
            f64::INFINITY
        };
        let stable_dt = advective_dt.min(gravity_dt);
        let required = (outer_dt_s / stable_dt).ceil().max(1.0) as u32;
        if self.config.pressure_diagnostics {
            let occupancy_thresholds = [0.0, 1.0e-6, 1.0e-4, 1.0e-3, 1.0e-2];
            let filtered_speeds: Vec<f64> = occupancy_thresholds
                .iter()
                .map(|&c| self.max_face_speed_l1_for_occupancy(c).0)
                .collect();
            let filtered_substeps: Vec<u32> = filtered_speeds
                .iter()
                .map(|&speed| {
                    let adv = if speed > 0.0 {
                        self.config.cfl_limit * self.config.cell_size_m / speed
                    } else {
                        f64::INFINITY
                    };
                    (outer_dt_s / adv.min(gravity_dt)).ceil().max(1.0) as u32
                })
                .collect();
            println!(
                "{{\"type\":\"cfl_selection\",\"max_face_outflow_speed_l1_m_s\":{:.12e},\"limiting_cell_linear_index\":{},\"limiting_cell_fraction\":{:.12e},\"occupancy_thresholds_C\":{:?},\"filtered_max_face_speed_l1_m_s\":{:?},\"filtered_required_substeps_diagnostic_only\":{:?},\"advective_dt_s\":{},\"gravity_dt_s\":{},\"stable_dt_s\":{},\"outer_dt_s\":{:.12e},\"required_substeps\":{}}}",
                max_speed,
                max_speed_cell,
                self.fraction[max_speed_cell],
                occupancy_thresholds,
                filtered_speeds,
                filtered_substeps,
                json_number(advective_dt),
                json_number(gravity_dt),
                json_number(stable_dt),
                outer_dt_s,
                required
            );
        }
        if required > self.config.max_substeps {
            return Err(MacError::SubstepBudgetExceeded {
                required,
                maximum: self.config.max_substeps,
            });
        }
        let start = Instant::now();
        let before = self.water_volume_m3();
        let sub_dt = outer_dt_s / f64::from(required);
        let mut metrics = MacStepMetrics {
            substeps: required,
            water_volume_before_m3: before,
            ..Default::default()
        };
        let mut outflow = 0.0;
        let mut region_outflow = 0.0;
        let mut region_inflow = 0.0;
        for substep in 0..required {
            // Preserve the historical single-phase backend for matched replay.
            // In the two-phase candidate, advect before applying forces.
            if (self.ambient_density_kg_m3.is_some() || self.freely_displaced_air)
                && !self.conservative_momentum
                && !self.experimental_surface_films
            {
                let stage = Instant::now();
                self.advect_velocity(sub_dt);
                metrics.velocity_advection_micros += stage.elapsed().as_micros() as u64;
                self.enforce_wall_velocities();
            }
            if self.stage_diagnostics {
                self.print_stage_diagnostic(substep, "pre_gravity", sub_dt, 0.0);
            }
            let boundary_stage = Instant::now();
            for (velocity, g) in self
                .v
                .iter_mut()
                .zip(std::iter::repeat(self.config.gravity_m_s2[1]))
            {
                *velocity += g * sub_dt;
            }
            if self.ambient_density_kg_m3.is_some() || self.freely_displaced_air {
                for velocity in &mut self.u {
                    *velocity += self.config.gravity_m_s2[0] * sub_dt;
                }
                for velocity in &mut self.w {
                    *velocity += self.config.gravity_m_s2[2] * sub_dt;
                }
            }
            self.enforce_wall_velocities();
            metrics.boundary_micros += boundary_stage.elapsed().as_micros() as u64;
            if self.stage_diagnostics {
                self.print_stage_diagnostic(substep, "after_gravity", sub_dt, 0.0);
            }

            if self.ambient_density_kg_m3.is_none() && !self.freely_displaced_air {
                let stage = Instant::now();
                self.advect_velocity(sub_dt);
                metrics.velocity_advection_micros += stage.elapsed().as_micros() as u64;
            }
            let boundary_stage = Instant::now();
            self.enforce_wall_velocities();
            metrics.boundary_micros += boundary_stage.elapsed().as_micros() as u64;
            if self.stage_diagnostics {
                self.print_stage_diagnostic(substep, "after_velocity_advection", sub_dt, 0.0);
            }

            let stage = Instant::now();
            let p = self.project(sub_dt)?;
            if self.conservative_momentum && !p.converged {
                return Err(MacError::MomentumPressureNotConverged {
                    residual: p.residual_final,
                });
            }
            metrics.pressure_solve_micros += stage.elapsed().as_micros() as u64;
            metrics.pressure_iterations += p.iterations as u64;
            metrics.phase_predictor_rows_total += p.phase_rows as u64;
            metrics.phase_predictor_reuses += u32::from(p.phase_reused);
            metrics.phase_predictor_rebuilds += u32::from(p.phase_rows > 0 && !p.phase_reused);
            metrics.phase_predictor_micros += p.phase_micros;
            metrics.phase_preconditioner_applications += p.phase_applications;
            metrics.phase_preconditioner_micros += p.phase_apply_micros;
            metrics.phase_preconditioner_scratch_bytes = metrics
                .phase_preconditioner_scratch_bytes
                .max(p.phase_scratch_bytes);
            metrics.pressure_active_rows_total += p.active_cells as u64;
            metrics.pressure_residual_initial_max = metrics
                .pressure_residual_initial_max
                .max(p.residual_initial);
            metrics.pressure_residual_final_max =
                metrics.pressure_residual_final_max.max(p.residual_final);
            metrics.divergence_before_max_s =
                metrics.divergence_before_max_s.max(p.divergence_before);
            metrics.divergence_after_max_s = metrics.divergence_after_max_s.max(p.divergence_after);
            if p.converged {
                metrics.pressure_converged_substeps += 1;
            }
            if self.stage_diagnostics {
                self.print_stage_diagnostic(substep, "after_pressure_projection", sub_dt, 0.0);
            }

            let stage = Instant::now();
            let (permitted_outflow, tracked_out, tracked_in, strict) =
                self.advect_fraction_fct(sub_dt, tracked_region)?;
            metrics.strict_path_repair_count += strict.path_repairs;
            metrics.strict_path_scratch_bytes =
                metrics.strict_path_scratch_bytes.max(strict.scratch_bytes);
            for axis in 0..3 {
                metrics.momentum_transport_error_kg_m_s[axis] += strict.momentum.error[axis];
                metrics.momentum_wall_impulse_kg_m_s[axis] += strict.momentum.wall[axis];
                metrics.momentum_open_outflow_kg_m_s[axis] += strict.momentum.exterior[axis];
            }
            metrics.momentum_dual_mass_defect_kg = metrics
                .momentum_dual_mass_defect_kg
                .max(strict.momentum.mass_defect);
            metrics.momentum_transport_subcycles = metrics
                .momentum_transport_subcycles
                .max(strict.momentum.sweeps);
            metrics.momentum_scratch_bytes =
                metrics.momentum_scratch_bytes.max(strict.momentum.bytes);
            outflow += permitted_outflow;
            region_outflow += tracked_out;
            region_inflow += tracked_in;
            metrics.transport_micros += stage.elapsed().as_micros() as u64;
            if self.conservative_momentum {
                // Density and momentum just moved together. Restore the fine
                // incompressibility constraint on the new phase before commit.
                let stage = Instant::now();
                let force_pressure = self.pressure_pa.clone();
                metrics.momentum_scratch_bytes = metrics
                    .momentum_scratch_bytes
                    .max(force_pressure.capacity() * size_of::<f64>());
                let post = self.project(sub_dt)?;
                if !post.converged {
                    return Err(MacError::MomentumPressureNotConverged {
                        residual: post.residual_final,
                    });
                }
                // The second projection is an incremental pressure impulse.
                // Retain force + correction pressure for diagnostics/warm start.
                for (pressure, force) in self.pressure_pa.iter_mut().zip(force_pressure) {
                    *pressure += force;
                }
                metrics.pressure_solve_micros += stage.elapsed().as_micros() as u64;
                metrics.pressure_iterations += post.iterations as u64;
                metrics.pressure_active_rows_total += post.active_cells as u64;
                metrics.pressure_residual_initial_max = metrics
                    .pressure_residual_initial_max
                    .max(post.residual_initial);
                metrics.pressure_residual_final_max =
                    metrics.pressure_residual_final_max.max(post.residual_final);
                metrics.divergence_before_max_s =
                    metrics.divergence_before_max_s.max(post.divergence_before);
                metrics.divergence_after_max_s =
                    metrics.divergence_after_max_s.max(post.divergence_after);
                metrics.phase_predictor_rows_total += post.phase_rows as u64;
                metrics.phase_predictor_reuses += u32::from(post.phase_reused);
                metrics.phase_predictor_rebuilds +=
                    u32::from(post.phase_rows > 0 && !post.phase_reused);
                metrics.phase_predictor_micros += post.phase_micros;
                metrics.phase_preconditioner_applications += post.phase_applications;
                metrics.phase_preconditioner_micros += post.phase_apply_micros;
                metrics.phase_preconditioner_scratch_bytes = metrics
                    .phase_preconditioner_scratch_bytes
                    .max(post.phase_scratch_bytes);
                if p.converged && !post.converged {
                    metrics.pressure_converged_substeps -= 1;
                }
            }
            metrics.active_cells = self.active_cells();
            if self.stage_diagnostics {
                self.print_stage_diagnostic(
                    substep,
                    "after_fraction_transport",
                    sub_dt,
                    permitted_outflow,
                );
            }
        }
        self.cumulative_open_outflow_m3 += outflow;
        let after = self.water_volume_m3();
        metrics.water_volume_after_m3 = after;
        metrics.permitted_outflow_m3 = outflow;
        metrics.tracked_region_outflow_m3 = region_outflow;
        metrics.tracked_region_inflow_m3 = region_inflow;
        metrics.tracked_region_net_outflow_m3 = region_outflow - region_inflow;
        metrics.conservation_error_m3 = (before - after - outflow).abs();
        metrics.total_micros = start.elapsed().as_micros() as u64;
        self.diagnostic_outer_step = self.diagnostic_outer_step.saturating_add(1);
        Ok(metrics)
    }

    fn print_stage_diagnostic(
        &self,
        substep: u32,
        stage: &str,
        dt_s: f64,
        applied_open_outflow_m3: f64,
    ) {
        let [nx, ny, nz] = self.dims();
        let mut active = 0usize;
        let mut tiny = 0usize;
        let mut partial = 0usize;
        let mut full = 0usize;
        let mut tiny_volume = 0.0;
        let mut pressure_sum = 0.0;
        let mut pressure_min = f64::INFINITY;
        let mut pressure_max: f64 = 0.0;
        let mut center_y_moment = 0.0;
        let mut wetted_columns = 0usize;
        let mut min_wetted_column_depth = f64::INFINITY;
        let mut max_wetted_column_depth: f64 = 0.0;
        for z in 0..nz {
            for x in 0..nx {
                let depth = (0..ny)
                    .map(|y| self.fraction[self.cell_index(x, y, z)])
                    .sum::<f64>()
                    * self.config.cell_size_m;
                if depth > 0.0 {
                    wetted_columns += 1;
                    min_wetted_column_depth = min_wetted_column_depth.min(depth);
                    max_wetted_column_depth = max_wetted_column_depth.max(depth);
                }
            }
            for y in 0..ny {
                for x in 0..nx {
                    let i = self.cell_index(x, y, z);
                    let c = self.fraction[i];
                    if c <= 0.0 || self.solid[i] {
                        continue;
                    }
                    active += 1;
                    let water = c * self.cell_volume();
                    center_y_moment += water
                        * (self.spec.origin().y as f64 + y as f64 + 0.5)
                        * self.config.cell_size_m;
                    if c < 1.0e-8 {
                        tiny += 1;
                        tiny_volume += water;
                    } else if c < 1.0 {
                        partial += 1;
                    } else {
                        full += 1;
                    }
                    let p = self.pressure_pa[i];
                    pressure_sum += p;
                    pressure_min = pressure_min.min(p);
                    pressure_max = pressure_max.max(p.abs());
                }
            }
        }
        let mut max_upward_wet_air_face_m_s: f64 = 0.0;
        let mut max_upward_wet_adjacent_face_m_s: f64 = 0.0;
        let mut max_upward_partial_adjacent_face_m_s: f64 = 0.0;
        let mut max_downward_wet_air_face_m_s: f64 = 0.0;
        for z in 0..nz {
            for y in 0..=ny {
                for x in 0..nx {
                    let below = (y > 0).then(|| self.cell_index(x, y - 1, z));
                    let above = (y < ny).then(|| self.cell_index(x, y, z));
                    let below_c = below.map_or(0.0, |i| self.fraction[i]);
                    let above_c = above.map_or(0.0, |i| self.fraction[i]);
                    let touches_wet = below.is_some_and(|i| !self.solid[i] && below_c > 0.0)
                        || above.is_some_and(|i| !self.solid[i] && above_c > 0.0);
                    let touches_partial =
                        below_c > 0.0 && below_c < 1.0 || above_c > 0.0 && above_c < 1.0;
                    let v = self.v[self.v_index(x, y, z)];
                    if touches_wet {
                        max_upward_wet_adjacent_face_m_s =
                            max_upward_wet_adjacent_face_m_s.max(v.max(0.0));
                    }
                    if touches_partial {
                        max_upward_partial_adjacent_face_m_s =
                            max_upward_partial_adjacent_face_m_s.max(v.max(0.0));
                    }
                    if below.is_some_and(|i| !self.solid[i] && below_c > 0.0) && above_c == 0.0 {
                        max_upward_wet_air_face_m_s = max_upward_wet_air_face_m_s.max(v.max(0.0));
                        max_downward_wet_air_face_m_s =
                            max_downward_wet_air_face_m_s.max((-v).max(0.0));
                    }
                }
            }
        }
        let mut top_outflow_rate_m3_s = 0.0;
        let mut top_inflow_rate_m3_s = 0.0;
        if self.config.open_top {
            for z in 0..nz {
                for x in 0..nx {
                    let i = self.cell_index(x, ny - 1, z);
                    if self.solid[i] {
                        continue;
                    }
                    let rate = self.v[self.v_index(x, ny, z)].max(0.0)
                        * self.fraction[i]
                        * self.config.cell_size_m.powi(2);
                    let reverse = (-self.v[self.v_index(x, ny, z)]).max(0.0)
                        * self.fraction[i]
                        * self.config.cell_size_m.powi(2);
                    top_outflow_rate_m3_s += rate;
                    top_inflow_rate_m3_s += reverse;
                }
            }
        }
        let volume = self.water_volume_m3();
        let center_y = if volume > 0.0 {
            center_y_moment / volume
        } else {
            0.0
        };
        println!(
            "{{\"type\":\"mac_stage\",\"outer_step\":{},\"substep\":{},\"stage\":\"{}\",\"dt_s\":{:.12e},\"water_volume_m3\":{:.12e},\"cumulative_open_outflow_m3\":{:.12e},\"applied_open_outflow_m3\":{:.12e},\"top_donor_outflow_rate_m3_s\":{:.12e},\"top_reverse_inflow_rate_m3_s\":{:.12e},\"active_pressure_rows_C_gt_0\":{},\"tiny_rows_C_lt_1e-8\":{},\"tiny_water_volume_m3\":{:.12e},\"partial_cells\":{},\"full_cells\":{},\"wetted_columns\":{},\"min_wetted_column_depth_m\":{:.12e},\"max_wetted_column_depth_m\":{:.12e},\"pressure_mean_pa\":{:.12e},\"pressure_min_pa\":{:.12e},\"pressure_max_abs_pa\":{:.12e},\"kinetic_energy_j\":{:.12e},\"potential_energy_j\":{:.12e},\"total_mechanical_energy_j\":{:.12e},\"water_center_y_m\":{:.12e},\"max_upward_wet_air_cell_face_speed_m_s\":{:.12e},\"max_upward_wet_adjacent_face_speed_m_s\":{:.12e},\"max_upward_partial_adjacent_face_speed_m_s\":{:.12e},\"max_downward_wet_air_cell_face_speed_m_s\":{:.12e}}}",
            self.diagnostic_outer_step,
            substep,
            stage,
            dt_s,
            volume,
            self.cumulative_open_outflow_m3,
            applied_open_outflow_m3,
            top_outflow_rate_m3_s,
            top_inflow_rate_m3_s,
            active,
            tiny,
            tiny_volume,
            partial,
            full,
            wetted_columns,
            if min_wetted_column_depth.is_finite() {
                min_wetted_column_depth
            } else {
                0.0
            },
            max_wetted_column_depth,
            if active > 0 {
                pressure_sum / active as f64
            } else {
                0.0
            },
            if pressure_min.is_finite() {
                pressure_min
            } else {
                0.0
            },
            pressure_max,
            self.kinetic_energy_j(),
            self.gravitational_potential_energy_j(),
            self.kinetic_energy_j() + self.gravitational_potential_energy_j(),
            center_y,
            max_upward_wet_air_face_m_s,
            max_upward_wet_adjacent_face_m_s,
            max_upward_partial_adjacent_face_m_s,
            max_downward_wet_air_face_m_s,
        );
    }

    fn dims(&self) -> [usize; 3] {
        self.spec.dimensions().map(|d| d as usize)
    }
    fn cell_volume(&self) -> f64 {
        self.config.cell_size_m.powi(3)
    }
    fn cell_index(&self, x: usize, y: usize, z: usize) -> usize {
        let [nx, ny, _] = self.dims();
        x + nx * (y + ny * z)
    }
    fn cell_index_global(&self, cell: GlobalCell) -> Option<usize> {
        self.spec.index_of(cell)
    }
    fn u_index(&self, x: usize, y: usize, z: usize) -> usize {
        let [nx, ny, _] = self.dims();
        x + (nx + 1) * (y + ny * z)
    }
    fn v_index(&self, x: usize, y: usize, z: usize) -> usize {
        let [nx, ny, _] = self.dims();
        x + nx * (y + (ny + 1) * z)
    }
    fn w_index(&self, x: usize, y: usize, z: usize) -> usize {
        let [nx, ny, _] = self.dims();
        x + nx * (y + ny * z)
    }

    fn max_face_speed_l1(&self) -> (f64, usize) {
        self.max_face_speed_l1_for_occupancy(0.0)
    }

    fn max_face_speed_l1_for_occupancy(&self, minimum_fraction: f64) -> (f64, usize) {
        let [nx, ny, nz] = self.dims();
        let mut maximum: f64 = 0.0;
        let mut maximum_cell = 0usize;
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let cell = self.cell_index(x, y, z);
                    if self.fraction[cell] < minimum_fraction {
                        continue;
                    }
                    let outgoing = self.u[self.u_index(x + 1, y, z)].max(0.0)
                        + (-self.u[self.u_index(x, y, z)]).max(0.0)
                        + self.v[self.v_index(x, y + 1, z)].max(0.0)
                        + (-self.v[self.v_index(x, y, z)]).max(0.0)
                        + self.w[self.w_index(x, y, z + 1)].max(0.0)
                        + (-self.w[self.w_index(x, y, z)]).max(0.0);
                    if outgoing > maximum {
                        maximum = outgoing;
                        maximum_cell = cell;
                    }
                }
            }
        }
        (maximum, maximum_cell)
    }

    fn enforce_wall_velocities(&mut self) {
        let [nx, ny, nz] = self.dims();
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..=nx {
                    if x == 0
                        || x == nx
                        || (x > 0 && self.solid[self.cell_index(x - 1, y, z)])
                        || (x < nx && self.solid[self.cell_index(x, y, z)])
                    {
                        let i = x + (nx + 1) * (y + ny * z);
                        self.u[i] = 0.0;
                    }
                }
            }
        }
        for z in 0..nz {
            for y in 0..=ny {
                for x in 0..nx {
                    if (y == 0 || (y == ny && !self.config.open_top))
                        || (y > 0 && self.solid[self.cell_index(x, y - 1, z)])
                        || (y < ny && self.solid[self.cell_index(x, y, z)])
                    {
                        let i = x + nx * (y + (ny + 1) * z);
                        self.v[i] = 0.0;
                    }
                }
            }
        }
        for z in 0..=nz {
            for y in 0..ny {
                for x in 0..nx {
                    if z == 0
                        || z == nz
                        || (z > 0 && self.solid[self.cell_index(x, y, z - 1)])
                        || (z < nz && self.solid[self.cell_index(x, y, z)])
                    {
                        let i = x + nx * (y + ny * z);
                        self.w[i] = 0.0;
                    }
                }
            }
        }
    }

    fn advect_velocity(&mut self, dt: f64) {
        let old = [self.u.clone(), self.v.clone(), self.w.clone()];
        let trace = [old[0].as_slice(), old[1].as_slice(), old[2].as_slice()];
        let [u, v, w] = self.semi_lagrangian(trace, trace, dt);
        (self.u, self.v, self.w) = (u, v, w);
    }

    /// Trilinear semi-Lagrangian transport of the staggered `source` field
    /// along `trace` velocities over `dt`.
    fn semi_lagrangian(&self, source: [&[f64]; 3], trace: [&[f64]; 3], dt: f64) -> [Vec<f64>; 3] {
        let [nx, ny, nz] = self.dims();
        let dims = [nx, ny, nz];
        let h = self.config.cell_size_m;
        let mut result = [
            vec![0.0; source[0].len()],
            vec![0.0; source[1].len()],
            vec![0.0; source[2].len()],
        ];
        for (component, output) in result.iter_mut().enumerate() {
            let mut extent = dims;
            extent[component] += 1;
            // Face positions in cell units: integer on the normal axis,
            // half-integer on the tangential axes.
            let mut offset = [0.5; 3];
            offset[component] = 0.0;
            // Each face writes only its own slot: parallel rows are
            // bit-identical to the serial loop for any thread count.
            output
                .par_chunks_mut(extent[0])
                .enumerate()
                .for_each(|(row, faces)| {
                    let (y, z) = (row % extent[1], row / extent[1]);
                    for (x, face) in faces.iter_mut().enumerate() {
                        let p = [
                            x as f64 + offset[0],
                            y as f64 + offset[1],
                            z as f64 + offset[2],
                        ];
                        let vel = sample_velocity(trace[0], trace[1], trace[2], dims, p);
                        let back = [
                            p[0] - vel[0] * dt / h - offset[0],
                            p[1] - vel[1] * dt / h - offset[1],
                            p[2] - vel[2] * dt / h - offset[2],
                        ];
                        *face = sample_component(source[component], extent, back);
                    }
                });
        }
        result
    }

    fn divergence(&self, x: usize, y: usize, z: usize) -> f64 {
        let h = self.config.cell_size_m;
        (self.u[self.u_index(x + 1, y, z)] - self.u[self.u_index(x, y, z)]
            + self.v[self.v_index(x, y + 1, z)]
            - self.v[self.v_index(x, y, z)]
            + self.w[self.w_index(x, y, z + 1)]
            - self.w[self.w_index(x, y, z)])
            / h
    }

    fn project(&mut self, dt: f64) -> Result<ProjectionStats, MacError> {
        let [nx, ny, nz] = self.dims();
        let n = self.fraction.len();
        let h2 = self.config.cell_size_m.powi(2);
        // Ghost-fluid pressure samples lie inside the reconstructed liquid.
        // Air-centred samples have p=0; their water fractions still participate
        // in transport unchanged. See Bridson, Fluid Simulation notes, sec 4.5.
        let surfaces = self.freely_displaced_air.then(|| self.reconstruct_planes());
        let mut liquid = vec![false; n];
        for (i, cell_is_liquid) in liquid.iter_mut().enumerate() {
            // Legacy single-phase rows follow C > 0. The opt-in two-phase
            // model solves water and ambient air, with density-weighted face
            // coefficients; no water fraction or pressure row is discarded.
            *cell_is_liquid =
                (self.ambient_density_kg_m3.is_some() || self.fraction[i] > 0.0) && !self.solid[i];
            if let Some(planes) = &surfaces
                && let Some(plane) = planes[i]
            {
                *cell_is_liquid = plane.normal.iter().sum::<f64>() * 0.5 < plane.alpha;
            }
        }
        let liquid_indices: Vec<usize> = liquid
            .iter()
            .enumerate()
            .filter_map(|(i, wet)| wet.then_some(i))
            .collect();
        let pressure_cells_below_1e6 = liquid_indices
            .iter()
            .filter(|&&i| self.fraction[i] < 1.0e-6)
            .count();
        let pressure_cells_below_1e3 = liquid_indices
            .iter()
            .filter(|&&i| self.fraction[i] < 1.0e-3)
            .count();
        let (entering_cells, leaving_cells, warm_start_nonzero_cells) =
            if self.config.pressure_diagnostics {
                (
                    liquid
                        .iter()
                        .zip(&self.previous_liquid)
                        .filter(|(now, was)| **now && !**was)
                        .count(),
                    liquid
                        .iter()
                        .zip(&self.previous_liquid)
                        .filter(|(now, was)| !**now && **was)
                        .count(),
                    liquid_indices
                        .iter()
                        .filter(|&&i| self.pressure_pa[i] != 0.0)
                        .count(),
                )
            } else {
                (0, 0, 0)
            };
        // A fixed boundary/configuration plus the exact active mask determines
        // this seven-point matrix and its ascending linear-cell row ordering.
        let matrix_active_set_unchanged =
            self.config.pressure_diagnostics && liquid == self.previous_liquid;
        let stage_start = Instant::now();
        let (components, mut anchored) = self.label_components(&liquid);
        let component_label_us = stage_start.elapsed().as_micros() as u64;
        let comp_count = anchored.len();
        let mut component_mean_sums = vec![0.0; comp_count];
        let mut component_mean_counts = vec![0usize; comp_count];
        let mut rhs = vec![0.0; n];
        let mut diag = vec![0.0; n];
        let mut phase_diagonal = self.phase_predictor.as_ref().map(|_| vec![0.0; n]);
        let mut div_before: f64 = 0.0;
        for &i in &liquid_indices {
            let x = i % nx;
            let y = (i / nx) % ny;
            let z = i / (nx * ny);
            let div = self.divergence(x, y, z);
            div_before = div_before.max(div.abs());
            rhs[i] = -self.config.density_kg_m3 / dt * div;
            for axis in 0..3 {
                for dir in [-1isize, 1] {
                    let mut p = [x as isize, y as isize, z as isize];
                    p[axis] += dir;
                    if p[0] >= 0
                        && p[1] >= 0
                        && p[2] >= 0
                        && p[0] < nx as isize
                        && p[1] < ny as isize
                        && p[2] < nz as isize
                    {
                        let j = self.cell_index(p[0] as usize, p[1] as usize, p[2] as usize);
                        if self.solid[j] {
                            continue;
                        }
                        // Current MAC approximation anchors pressure at the
                        // liquid/empty cell face. For a partial liquid cell
                        // (0 < C < 1) this is not its reconstructed in-cell
                        // interface; C does not alter the boundary distance.
                        diag[i] += (if liquid[j] {
                            self.relative_inverse_face_density(i, j)
                        } else {
                            self.surface_face_factor(i, j, axis, &liquid, surfaces.as_deref())
                        }) / h2;
                        // Empty-cell pressure is atmospheric at the face;
                        // the half-cell distance doubles the coefficient.
                    } else if axis == 1 && dir > 0 && self.config.open_top {
                        let extra = 2.0 * self.relative_inverse_face_density(i, i) / h2;
                        diag[i] += extra;
                        if let Some(d) = &mut phase_diagonal {
                            d[i] += extra;
                        }
                    }
                }
            }
        }
        if self.compressible_enclosed_air {
            // Isothermal sealed air: div(u) = -(1-C)(p - p_n)/(P_abs dt). In
            // this system's units that is a diagonal (and matching RHS) term
            // rho_w (1-C) / (P_abs dt^2); a region containing it is no longer
            // singular, so its pressure level is physical, not a gauge.
            let sealed = self.sealed_air_fractions();
            for &i in &liquid_indices {
                if sealed[i] > 0.0 {
                    let absolute = (ATMOSPHERIC_PRESSURE_PA + self.pressure_pa[i])
                        .max(0.1 * ATMOSPHERIC_PRESSURE_PA);
                    let k = sealed[i] * self.config.density_kg_m3 / (absolute * dt * dt);
                    diag[i] += k;
                    if let Some(d) = &mut phase_diagonal {
                        d[i] += k;
                    }
                    rhs[i] += k * self.pressure_pa[i];
                    if let Some(c) = components[i] {
                        anchored[c] = true;
                    }
                }
            }
        }
        let matrix_setup_us = stage_start.elapsed().as_micros() as u64 - component_label_us;
        let matrix_coefficients_unchanged = self.ambient_density_kg_m3.is_none()
            && matrix_active_set_unchanged
            && self
                .previous_pressure_diagonal
                .as_ref()
                .is_some_and(|previous| {
                    liquid_indices
                        .iter()
                        .all(|&i| diag[i].to_bits() == previous[i].to_bits())
                });
        let component_partition_unchanged = matrix_active_set_unchanged
            && self
                .previous_component_labels
                .as_ref()
                .is_some_and(|previous| components == *previous)
            && self
                .previous_component_anchors
                .as_ref()
                .is_some_and(|previous| anchored == *previous);
        let gauge_treatment_unchanged = component_partition_unchanged
            && self.previous_pressure_preconditioner == Some(self.pressure_preconditioner);
        let matrix_exactly_unchanged = matrix_coefficients_unchanged
            && component_partition_unchanged
            && gauge_treatment_unchanged;
        // Enclosed all-Neumann components have one pressure null mode. Project
        // their RHS and all Krylov vectors onto the zero-mean subspace.
        let mut component_sum = vec![0.0; comp_count];
        let mut component_size = vec![0usize; comp_count];
        for &i in &liquid_indices {
            if let Some(c) = components[i] {
                component_sum[c] += rhs[i];
                component_size[c] += 1;
            }
        }
        for c in 0..comp_count {
            if !anchored[c] && component_size[c] > 0 {
                let mean = component_sum[c] / component_size[c] as f64;
                if mean.abs() > 1.0e-7 {
                    return Err(MacError::IncompatibleEnclosedPressureRegion);
                }
                for &i in &liquid_indices {
                    if components[i] == Some(c) {
                        rhs[i] -= mean;
                    }
                }
            }
        }
        let factor_start = Instant::now();
        let ic0 = match self.pressure_preconditioner {
            PressurePreconditioner::Jacobi => None,
            PressurePreconditioner::Ic0 => Some(Ic0Factor::build(
                self,
                &liquid_indices,
                &components,
                &anchored,
                &diag,
                h2,
            )?),
            PressurePreconditioner::Mic0 => Some(Ic0Factor::build_modified(
                self,
                &liquid_indices,
                &components,
                &anchored,
                &diag,
                h2,
            )?),
            PressurePreconditioner::Multigrid => None,
        };
        let mut multigrid = (self.pressure_preconditioner == PressurePreconditioner::Multigrid)
            .then(|| Multigrid::build(self, &liquid, &diag));
        let factor_setup_us = factor_start.elapsed().as_micros() as u64;
        let mut p = self.pressure_pa.clone();
        zero_nonliquid(&mut p, &liquid);
        project_component_means(
            &mut p,
            &components,
            &anchored,
            comp_count,
            &liquid_indices,
            &mut component_mean_sums,
            &mut component_mean_counts,
        );
        let operator = PressureOperator::build(self, &liquid, &liquid_indices, &diag);
        let mut ap = vec![0.0; n];
        let mut matrix_application_us = 0u64;
        let mut preconditioner_application_us = 0u64;
        let mut vector_operations_us = 0u64;
        let timer = Instant::now();
        operator.apply(&p, &mut ap);
        if self.config.pressure_diagnostics {
            matrix_application_us += timer.elapsed().as_micros() as u64;
        }
        let mut r = vec![0.0; n];
        let mut zvec = vec![0.0; n];
        let mut preconditioner_work = ic0.as_ref().map(|factor| vec![0.0; factor.diagonal.len()]);
        let timer = Instant::now();
        for &i in &liquid_indices {
            r[i] = rhs[i] - ap[i];
        }
        // Freeze acceptance against the original warm residual. Coarse guesses
        // must never relax the fine solver's divergence/residual requirement.
        let mut baseline_residual = None;
        let mut phase_rows = 0;
        let mut phase_reused = false;
        let mut phase_micros = 0;
        let mut balanced = None;
        if let Some(state) = &self.phase_predictor {
            let start = Instant::now();
            project_component_means(
                &mut r,
                &components,
                &anchored,
                comp_count,
                &liquid_indices,
                &mut component_mean_sums,
                &mut component_mean_counts,
            );
            baseline_residual = Some(l2_norm_indices(&r, &liquid_indices));
            if self.ambient_density_kg_m3.is_none()
                || self
                    .solid
                    .iter()
                    .enumerate()
                    .any(|(i, &s)| s != state.phase.geometry().component_at_index(i).is_none())
            {
                return Err(MacError::BoundaryMismatch);
            }
            let phase = state
                .phase
                .with_fractions(self.fraction.clone())
                .map_err(|e| MacError::PhaseGraph(GraphError::Phase(e)))?;
            let (graph, reused) = state
                .graph
                .refresh(&phase, state.limits)
                .map_err(MacError::PhaseGraph)?;
            let weights: Vec<_> = phase
                .faces()
                .iter()
                .map(|f| self.relative_inverse_face_density(f.lower, f.upper) / h2)
                .collect();
            let coarse = graph
                .pressure_operator_with_diagonal(&phase, &weights, phase_diagonal.as_ref().unwrap())
                .map_err(MacError::PhaseGraph)?;
            if state.iterative {
                balanced = Some(BalancedPhasePressure::new(
                    &graph,
                    coarse.smoother().map_err(MacError::PhaseGraph)?,
                    n,
                    state.sweeps,
                ));
            } else {
                let mut source = vec![0.0; graph.rows().len()];
                for &i in &liquid_indices {
                    source[graph.row_at_index(i).unwrap() as usize] += r[i];
                }
                let correction = coarse
                    .smooth(&source, state.sweeps)
                    .map_err(MacError::PhaseGraph)?;
                for &i in &liquid_indices {
                    p[i] += correction[graph.row_at_index(i).unwrap() as usize];
                }
                project_component_means(
                    &mut p,
                    &components,
                    &anchored,
                    comp_count,
                    &liquid_indices,
                    &mut component_mean_sums,
                    &mut component_mean_counts,
                );
                operator.apply(&p, &mut ap);
                for &i in &liquid_indices {
                    r[i] = rhs[i] - ap[i];
                }
            }
            phase_rows = graph.rows().len();
            phase_reused = reused;
            self.phase_predictor = Some(PhasePredictor {
                phase,
                graph,
                limits: state.limits,
                sweeps: state.sweeps,
                iterative: state.iterative,
            });
            phase_micros = start.elapsed().as_micros() as u64;
        }
        let mut base = BasePressurePreconditioner {
            ic0: ic0.as_ref(),
            multigrid: multigrid.as_mut(),
            work: preconditioner_work.as_mut(),
            active: &liquid_indices,
            diag: &diag,
        };
        let mut phase_applications = 0;
        let mut phase_apply_micros = 0;
        let phase_scratch_bytes = balanced
            .as_ref()
            .map_or(0, BalancedPhasePressure::array_storage_bytes);
        if let Some(b) = &mut balanced {
            let start = Instant::now();
            b.apply(&r, &mut zvec, &operator, &mut base)?;
            phase_applications += 1;
            phase_apply_micros += start.elapsed().as_micros() as u64;
        } else {
            base.apply(&r, &mut zvec);
        }
        if self.config.pressure_diagnostics {
            preconditioner_application_us += timer.elapsed().as_micros() as u64;
        }
        let timer = Instant::now();
        project_component_means(
            &mut r,
            &components,
            &anchored,
            comp_count,
            &liquid_indices,
            &mut component_mean_sums,
            &mut component_mean_counts,
        );
        project_component_means(
            &mut zvec,
            &components,
            &anchored,
            comp_count,
            &liquid_indices,
            &mut component_mean_sums,
            &mut component_mean_counts,
        );
        let residual_initial = l2_norm_indices(&r, &liquid_indices);
        if self.config.pressure_diagnostics {
            vector_operations_us += timer.elapsed().as_micros() as u64;
        }
        let target = self.config.pressure_absolute_tolerance.max(
            self.config.pressure_relative_tolerance * baseline_residual.unwrap_or(residual_initial),
        );
        let mut d = zvec.clone();
        let mut rz = dot_indices(&r, &zvec, &liquid_indices);
        let mut residual_final = residual_initial;
        let mut iterations: usize = 0;
        let mut converged = residual_initial <= target;
        let mut residual_history = if self.config.pressure_diagnostics {
            vec![residual_initial]
        } else {
            Vec::new()
        };
        let krylov_start = Instant::now();
        for iteration in 0..self.config.pressure_max_iterations {
            if converged {
                break;
            }
            let timer = Instant::now();
            operator.apply(&d, &mut ap);
            if self.config.pressure_diagnostics {
                matrix_application_us += timer.elapsed().as_micros() as u64;
            }
            let timer = Instant::now();
            let denom = dot_indices(&d, &ap, &liquid_indices);
            if denom <= 0.0 || !denom.is_finite() {
                break;
            }
            let alpha = rz / denom;
            for &i in &liquid_indices {
                p[i] += alpha * d[i];
                r[i] -= alpha * ap[i];
            }
            // With compatible RHS, zero-mean p/r remain in the constrained
            // subspace under A. Projecting them every iteration only repeats
            // full active-cell reductions; preconditioned z and search d are
            // still explicitly projected below.
            residual_final = l2_norm_indices(&r, &liquid_indices);
            if self.config.pressure_diagnostics {
                residual_history.push(residual_final);
            }
            iterations = (iteration + 1) as usize;
            converged = residual_final <= target;
            if converged {
                break;
            }
            let preconditioner_timer = Instant::now();
            if let Some(b) = &mut balanced {
                let start = Instant::now();
                b.apply(&r, &mut zvec, &operator, &mut base)?;
                phase_applications += 1;
                phase_apply_micros += start.elapsed().as_micros() as u64;
            } else {
                base.apply(&r, &mut zvec);
            }
            let preconditioner_elapsed = if self.config.pressure_diagnostics {
                let elapsed = preconditioner_timer.elapsed().as_micros() as u64;
                preconditioner_application_us += elapsed;
                elapsed
            } else {
                0
            };
            project_component_means(
                &mut zvec,
                &components,
                &anchored,
                comp_count,
                &liquid_indices,
                &mut component_mean_sums,
                &mut component_mean_counts,
            );
            let rz_new = dot_indices(&r, &zvec, &liquid_indices);
            let beta = rz_new / rz;
            for &i in &liquid_indices {
                d[i] = zvec[i] + beta * d[i];
            }
            project_component_means(
                &mut d,
                &components,
                &anchored,
                comp_count,
                &liquid_indices,
                &mut component_mean_sums,
                &mut component_mean_counts,
            );
            rz = rz_new;
            if self.config.pressure_diagnostics {
                vector_operations_us += timer.elapsed().as_micros() as u64 - preconditioner_elapsed;
            }
        }
        let krylov_us = krylov_start.elapsed().as_micros() as u64;
        let correction_start = Instant::now();
        project_component_means(
            &mut p,
            &components,
            &anchored,
            comp_count,
            &liquid_indices,
            &mut component_mean_sums,
            &mut component_mean_counts,
        );
        self.pressure_pa = p;
        self.correct_faces(&liquid, dt, surfaces.as_deref());
        if self.freely_displaced_air {
            if self.experimental_surface_films {
                self.accelerate_unresolved_surface_films(&liquid, dt);
            }
            self.extrapolate_surface_velocities(&liquid);
        }
        let mut div_after: f64 = 0.0;
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let i = self.cell_index(x, y, z);
                    if liquid[i] {
                        div_after = div_after.max(self.divergence(x, y, z).abs());
                    }
                }
            }
        }
        let correction_us = correction_start.elapsed().as_micros() as u64;
        if self.config.pressure_diagnostics {
            self.previous_liquid.clone_from(&liquid);
            if let Some(previous) = &mut self.previous_pressure_diagonal {
                previous.clone_from(&diag);
            }
            if let Some(previous) = &mut self.previous_component_labels {
                previous.clone_from(&components);
            }
            if let Some(previous) = &mut self.previous_component_anchors {
                previous.clone_from(&anchored);
            }
            self.previous_pressure_preconditioner = Some(self.pressure_preconditioner);
        }
        let residual_verification_us;
        if self.config.pressure_diagnostics {
            let verification_start = Instant::now();
            apply_pressure_matrix(
                self,
                &self.pressure_pa,
                &liquid,
                &liquid_indices,
                &diag,
                &mut ap,
            );
            for &i in &liquid_indices {
                r[i] = rhs[i] - ap[i];
            }
            project_component_means(
                &mut r,
                &components,
                &anchored,
                comp_count,
                &liquid_indices,
                &mut component_mean_sums,
                &mut component_mean_counts,
            );
            let true_residual = l2_norm_indices(&r, &liquid_indices);
            residual_verification_us = verification_start.elapsed().as_micros() as u64;
            let rhs_norm = l2_norm_indices(&rhs, &liquid_indices);
            let mut component_sizes = vec![0usize; comp_count];
            for &i in &liquid_indices {
                if let Some(c) = components[i] {
                    component_sizes[c] += 1;
                }
            }
            println!(
                "{{\"type\":\"pressure_substep\",\"preconditioner\":\"{}\",\"ic0_factor_setup_us\":{},\"ic0_factor_bytes\":{},\"active_liquid_cells\":{},\"active_cells_below_C_1e-6\":{},\"active_cells_below_C_1e-3\":{},\"cells_entering_liquid_set\":{},\"cells_leaving_liquid_set\":{},\"matrix_exactly_unchanged_from_previous_projection\":{},\"matrix_active_set_unchanged\":{},\"matrix_row_ordering_unchanged\":{},\"matrix_face_pattern_unchanged\":{},\"matrix_coefficients_unchanged\":{},\"component_partition_unchanged\":{},\"gauge_treatment_unchanged\":{},\"matrix_identity_basis\":\"fixed geometry/config plus exact active mask; row-major ordering and face stencil\",\"warm_start_nonzero_cells\":{},\"component_sizes\":{:?},\"atmospheric_components\":{},\"enclosed_components\":{},\"residual_norm\":\"L2 over liquid cells, Pa/m^2\",\"right_hand_side_norm\":{:.12e},\"warm_start_initial_residual_ratio\":{:.12e},\"residual_initial\":{:.12e},\"residual_final_recursive\":{:.12e},\"residual_final_recomputed\":{:.12e},\"stopping_threshold_pa_per_m2_l2\":{:.12e},\"iterations\":{},\"residual_history\":{:?},\"component_label_us\":{},\"matrix_and_rhs_setup_us\":{},\"matrix_application_us\":{},\"preconditioner_application_us\":{},\"vector_operations_us\":{},\"krylov_us\":{},\"correct_faces_and_divergence_us\":{},\"residual_verification_us\":{}}}",
                match self.pressure_preconditioner {
                    PressurePreconditioner::Jacobi => "Jacobi",
                    PressurePreconditioner::Ic0 => "IC(0)",
                    PressurePreconditioner::Mic0 => "MIC(0)",
                    PressurePreconditioner::Multigrid => "multigrid",
                },
                factor_setup_us,
                ic0.as_ref().map_or(0, Ic0Factor::allocated_bytes),
                liquid_indices.len(),
                pressure_cells_below_1e6,
                pressure_cells_below_1e3,
                entering_cells,
                leaving_cells,
                matrix_exactly_unchanged,
                matrix_active_set_unchanged,
                matrix_active_set_unchanged,
                matrix_active_set_unchanged,
                matrix_coefficients_unchanged,
                component_partition_unchanged,
                gauge_treatment_unchanged,
                warm_start_nonzero_cells,
                component_sizes,
                anchored.iter().filter(|a| **a).count(),
                anchored.iter().filter(|a| !**a).count(),
                rhs_norm,
                residual_initial / rhs_norm.max(f64::MIN_POSITIVE),
                residual_initial,
                residual_final,
                true_residual,
                target,
                iterations,
                residual_history,
                component_label_us,
                matrix_setup_us,
                matrix_application_us,
                preconditioner_application_us,
                vector_operations_us,
                krylov_us,
                correction_us,
                residual_verification_us
            );
        }
        Ok(ProjectionStats {
            phase_applications,
            phase_apply_micros,
            phase_scratch_bytes,
            phase_rows,
            phase_reused,
            phase_micros,
            iterations,
            active_cells: liquid_indices.len(),
            residual_initial,
            residual_final,
            divergence_before: div_before,
            divergence_after: div_after,
            converged,
        })
    }

    fn label_components(&self, liquid: &[bool]) -> (Vec<Option<usize>>, Vec<bool>) {
        let [nx, ny, nz] = self.dims();
        let mut labels = vec![None; liquid.len()];
        let mut anchored = Vec::new();
        for seed in 0..liquid.len() {
            if !liquid[seed] || labels[seed].is_some() {
                continue;
            }
            let label = anchored.len();
            anchored.push(false);
            labels[seed] = Some(label);
            let mut queue = VecDeque::from([seed]);
            while let Some(i) = queue.pop_front() {
                let x = i % nx;
                let y = (i / nx) % ny;
                let z = i / (nx * ny);
                for axis in 0..3 {
                    for dir in [-1isize, 1] {
                        let mut q = [x as isize, y as isize, z as isize];
                        q[axis] += dir;
                        if q[0] < 0
                            || q[1] < 0
                            || q[2] < 0
                            || q[0] >= nx as isize
                            || q[1] >= ny as isize
                            || q[2] >= nz as isize
                        {
                            if axis == 1 && dir > 0 && self.config.open_top {
                                anchored[label] = true;
                            }
                            continue;
                        }
                        let j = self.cell_index(q[0] as usize, q[1] as usize, q[2] as usize);
                        if self.solid[j] {
                            continue;
                        }
                        if liquid[j] {
                            if labels[j].is_none() {
                                labels[j] = Some(label);
                                queue.push_back(j);
                            }
                        } else {
                            anchored[label] = true;
                        }
                    }
                }
            }
        }
        (labels, anchored)
    }

    /// Reciprocal liquid-centre to atmospheric-interface distance in cell
    /// units. Use the PLIC plane's actual segment intersection where present;
    /// full/dry neighbours use the existing half-cell free surface. The 0.01
    /// minimum theta is the explicit ghost-fluid conditioning floor, not a
    /// liquid-fraction cutoff or an added pressure anchor.
    fn surface_face_factor(
        &self,
        a: usize,
        b: usize,
        axis: usize,
        liquid: &[bool],
        surfaces: Option<&[Option<InterfacePlane>]>,
    ) -> f64 {
        let Some(planes) = surfaces else {
            return 2.0;
        };
        let (wet, dry) = if liquid[a] { (a, b) } else { (b, a) };
        let direction = if dry > wet { 1.0 } else { -1.0 };
        let intersection = |plane: InterfacePlane, offset: f64| {
            let slope = plane.normal[axis] * direction;
            if slope > 0.0 {
                Some(offset + (plane.alpha - 0.5 * plane.normal.iter().sum::<f64>()) / slope)
            } else {
                None
            }
        };
        let valid = |t: &f64| t.is_finite() && *t >= 0.0 && *t <= 1.0;
        // The air-centred cell describes the atmospheric boundary seen from
        // this pressure segment. A nearly saturated wet cell can also acquire
        // a PLIC plane through roundoff; that internal reconstruction must not
        // replace the surface on the air side. Reject each invalid candidate
        // before falling back, so one out-of-segment plane cannot mask the
        // other cell's valid boundary.
        let theta = planes[dry]
            .and_then(|p| intersection(p, 1.0))
            .filter(valid)
            .or_else(|| planes[wet].and_then(|p| intersection(p, 0.0)).filter(valid))
            .unwrap_or(0.5);
        1.0 / theta.max(0.01)
    }

    fn correct_faces(
        &mut self,
        liquid: &[bool],
        dt: f64,
        surfaces: Option<&[Option<InterfacePlane>]>,
    ) {
        let [nx, ny, nz] = self.dims();
        let h = self.config.cell_size_m;
        let k = dt / (self.config.density_kg_m3 * h);
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..=nx {
                    let l = if x > 0 {
                        Some(self.cell_index(x - 1, y, z))
                    } else {
                        None
                    };
                    let r = if x < nx {
                        Some(self.cell_index(x, y, z))
                    } else {
                        None
                    };
                    if let (Some(li), Some(ri)) = (l, r)
                        && !self.solid[li]
                        && !self.solid[ri]
                        && (liquid[li] || liquid[ri])
                    {
                        let fi = x + (nx + 1) * (y + ny * z);
                        let interface_factor = if liquid[li] != liquid[ri] {
                            self.surface_face_factor(li, ri, 0, liquid, surfaces)
                        } else {
                            self.relative_inverse_face_density(li, ri)
                        };
                        self.u[fi] -=
                            interface_factor * k * (self.pressure_pa[ri] - self.pressure_pa[li]);
                    }
                }
            }
        }
        for z in 0..nz {
            for y in 0..=ny {
                for x in 0..nx {
                    let b = if y > 0 {
                        Some(self.cell_index(x, y - 1, z))
                    } else {
                        None
                    };
                    let t = if y < ny {
                        Some(self.cell_index(x, y, z))
                    } else {
                        None
                    };
                    match (b, t) {
                        (Some(bi), Some(ti))
                            if !self.solid[bi] && !self.solid[ti] && (liquid[bi] || liquid[ti]) =>
                        {
                            let fi = x + nx * (y + (ny + 1) * z);
                            let interface_factor = if liquid[bi] != liquid[ti] {
                                self.surface_face_factor(bi, ti, 1, liquid, surfaces)
                            } else {
                                self.relative_inverse_face_density(bi, ti)
                            };
                            self.v[fi] -= interface_factor
                                * k
                                * (self.pressure_pa[ti] - self.pressure_pa[bi]);
                        }
                        (Some(bi), None)
                            if self.config.open_top && liquid[bi] && !self.solid[bi] =>
                        {
                            let fi = x + nx * (y + (ny + 1) * z);
                            self.v[fi] -= 2.0
                                * self.relative_inverse_face_density(bi, bi)
                                * k
                                * (0.0 - self.pressure_pa[bi]);
                        }
                        _ => {}
                    }
                }
            }
        }
        for z in 0..=nz {
            for y in 0..ny {
                for x in 0..nx {
                    let a = if z > 0 {
                        Some(self.cell_index(x, y, z - 1))
                    } else {
                        None
                    };
                    let b = if z < nz {
                        Some(self.cell_index(x, y, z))
                    } else {
                        None
                    };
                    if let (Some(ai), Some(bi)) = (a, b)
                        && !self.solid[ai]
                        && !self.solid[bi]
                        && (liquid[ai] || liquid[bi])
                    {
                        let fi = x + nx * (y + ny * z);
                        let interface_factor = if liquid[ai] != liquid[bi] {
                            self.surface_face_factor(ai, bi, 2, liquid, surfaces)
                        } else {
                            self.relative_inverse_face_density(ai, bi)
                        };
                        self.w[fi] -=
                            interface_factor * k * (self.pressure_pa[bi] - self.pressure_pa[ai]);
                    }
                }
            }
        }
        self.enforce_wall_velocities();
    }

    /// A VOF film below the pressure sample still has a hydrostatic head. On
    /// horizontal faces without a bulk pressure sample, integrate the shallow
    /// layer pressure (rho*g*d^2/2): division by the mean face depth gives
    /// -g*(d_right-d_left)/h. The existing bounded PLIC transfers move its mass.
    /// This closure is for downward vertical gravity; bulk faces continue to
    /// use the three-dimensional ghost-fluid pressure projection.
    fn accelerate_unresolved_surface_films(&mut self, liquid: &[bool], dt: f64) {
        let [gx, gy, gz] = self.config.gravity_m_s2;
        if gx != 0.0 || gz != 0.0 || gy >= 0.0 {
            return;
        }
        let [nx, ny, nz] = self.dims();
        for axis in [0, 2] {
            let mut ext = [nx, ny, nz];
            ext[axis] += 1;
            for z in 0..nz {
                for y in 0..ny {
                    for x in 0..nx {
                        let p = [x, y, z];
                        if p[axis] == 0 {
                            continue;
                        }
                        let mut low = p;
                        low[axis] -= 1;
                        let a = self.cell_index(low[0], low[1], low[2]);
                        let b = self.cell_index(x, y, z);
                        if self.solid[a] || self.solid[b] || liquid[a] || liquid[b] {
                            continue;
                        }
                        let acceleration = gy * (self.fraction[b] - self.fraction[a]);
                        let fi = x + ext[0] * (y + ext[1] * z);
                        let faces = if axis == 0 { &mut self.u } else { &mut self.w };
                        faces[fi] += dt * acceleration;
                    }
                }
            }
        }
    }

    /// Ghost-fluid advection needs a narrow velocity extension outside liquid,
    /// not independently accelerating dry-space velocities. Three face layers
    /// cover the bounded CFL backtrace. Solids/closed exterior are never donors.
    fn extrapolate_surface_velocities(&mut self, liquid: &[bool]) {
        let dims = self.dims();
        for axis in 0..3 {
            let mut ext = dims;
            ext[axis] += 1;
            let count = ext.iter().product();
            let mut known = vec![false; count];
            let mut blocked = vec![false; count];
            for i in 0..count {
                let p = [i % ext[0], i / ext[0] % ext[1], i / (ext[0] * ext[1])];
                let mut low = p;
                let lower = if p[axis] > 0 {
                    low[axis] -= 1;
                    Some(self.cell_index(low[0], low[1], low[2]))
                } else {
                    None
                };
                let upper = (p[axis] < dims[axis]).then(|| self.cell_index(p[0], p[1], p[2]));
                let open = axis == 1 && p[axis] == dims[axis] && self.config.open_top;
                blocked[i] = lower.is_some_and(|c| self.solid[c])
                    || upper.is_some_and(|c| self.solid[c])
                    || ((lower.is_none() || upper.is_none()) && !open);
                known[i] = !blocked[i]
                    && (lower.is_some_and(|c| liquid[c])
                        || upper.is_some_and(|c| liquid[c])
                        // Shallow films have no cell-centre pressure sample.
                        // Keep horizontal momentum and downward outlet velocity;
                        // an empty face above a film is not a gravity donor.
                        || (self.experimental_surface_films
                            && ((axis != 1 && lower.is_some_and(|c| self.fraction[c] > 0.0))
                                || upper.is_some_and(|c| self.fraction[c] > 0.0))));
            }
            let faces = match axis {
                0 => &mut self.u,
                1 => &mut self.v,
                _ => &mut self.w,
            };
            for (i, velocity) in faces.iter_mut().enumerate() {
                if !known[i] {
                    *velocity = 0.0;
                }
            }
            let strides = [1, ext[0], ext[0] * ext[1]];
            for _ in 0..3 {
                let source = faces.clone();
                let previous = known.clone();
                for i in 0..count {
                    if blocked[i] || previous[i] {
                        continue;
                    }
                    let p = [i % ext[0], i / ext[0] % ext[1], i / (ext[0] * ext[1])];
                    let (mut sum, mut donors) = (0.0, 0);
                    for a in 0..3 {
                        for positive in [false, true] {
                            let j = if positive && p[a] + 1 < ext[a] {
                                Some(i + strides[a])
                            } else if !positive && p[a] > 0 {
                                Some(i - strides[a])
                            } else {
                                None
                            };
                            if let Some(j) = j
                                && previous[j]
                            {
                                sum += source[j];
                                donors += 1;
                            }
                        }
                    }
                    if donors > 0 {
                        faces[i] = sum / f64::from(donors);
                        known[i] = true;
                    }
                }
            }
        }
        self.enforce_wall_velocities();
    }

    fn reconstruct_planes(&self) -> Vec<Option<InterfacePlane>> {
        let [nx, ny, nz] = self.dims();
        let mut planes = vec![None; self.fraction.len()];
        // Each cell writes only its own plane: identical for any thread count.
        planes.par_iter_mut().enumerate().for_each(|(i, plane)| {
            let (x, y, z) = (i % nx, (i / nx) % ny, i / (nx * ny));
            let c = self.fraction[i];
            if self.solid[i] || c <= 0.0 || c >= 1.0 {
                return;
            }
            let p = [x as isize, y as isize, z as isize];
            let sample = |q: [isize; 3]| {
                if q.iter()
                    .zip([nx, ny, nz])
                    .any(|(&v, n)| v < 0 || v >= n as isize)
                {
                    return c;
                }
                let j = self.cell_index(q[0] as usize, q[1] as usize, q[2] as usize);
                if self.solid[j] { c } else { self.fraction[j] }
            };
            let mut normal = [0.0; 3];
            // A bottom-supported film has a height function even when the
            // generic fraction gradient places its plane away from the lip.
            // Reconstruct its surface slope from neighbouring layer depths;
            // otherwise bounded swept slabs can never reach that dry face.
            if self.experimental_surface_films
                && self.config.gravity_m_s2[0] == 0.0
                && self.config.gravity_m_s2[2] == 0.0
                && self.config.gravity_m_s2[1] < 0.0
                && c < 0.5
                && y > 0
                && self.solid[self.cell_index(x, y - 1, z)]
            {
                normal[1] = 1.0;
                for axis in [0, 2] {
                    let mut lo = p;
                    let mut hi = p;
                    lo[axis] -= 1;
                    hi[axis] += 1;
                    normal[axis] = 0.5 * (sample(lo) - sample(hi));
                }
                *plane = Some(InterfacePlane::from_fraction(normal, c));
                return;
            }
            for axis in 0..3 {
                let b = (axis + 1) % 3;
                let d = (axis + 2) % 3;
                for j in -1isize..=1 {
                    for k in -1isize..=1 {
                        let mut lo = p;
                        lo[axis] -= 1;
                        lo[b] += j;
                        lo[d] += k;
                        let mut hi = lo;
                        hi[axis] += 2;
                        let weight =
                            if j == 0 { 2.0 } else { 1.0 } * if k == 0 { 2.0 } else { 1.0 };
                        normal[axis] += weight * (sample(lo) - sample(hi));
                    }
                }
            }
            *plane = Some(InterfacePlane::from_fraction(normal, c));
        });
        planes
    }

    fn advect_fraction_fct(
        &mut self,
        dt: f64,
        tracked_region: Option<&[bool]>,
    ) -> Result<(f64, f64, f64, StrictTransferMetrics), MacError> {
        let [nx, ny, nz] = self.dims();
        let h = self.config.cell_size_m;
        let old = self.fraction.clone();
        let planes = (self.ambient_density_kg_m3.is_some() || self.freely_displaced_air)
            .then(|| self.reconstruct_planes());
        let n = old.len();
        let face_flux = |axis: usize, xf: usize, yf: usize, zf: usize| {
            let f = [xf, yf, zf];
            let (left, right, vel) = match axis {
                0 => (
                    if xf > 0 {
                        Some(self.cell_index(xf - 1, yf, zf))
                    } else {
                        None
                    },
                    if xf < nx {
                        Some(self.cell_index(xf, yf, zf))
                    } else {
                        None
                    },
                    self.u[self.u_index(xf, yf, zf)],
                ),
                1 => (
                    if yf > 0 {
                        Some(self.cell_index(xf, yf - 1, zf))
                    } else {
                        None
                    },
                    if yf < ny {
                        Some(self.cell_index(xf, yf, zf))
                    } else {
                        None
                    },
                    self.v[self.v_index(xf, yf, zf)],
                ),
                _ => (
                    if zf > 0 {
                        Some(self.cell_index(xf, yf, zf - 1))
                    } else {
                        None
                    },
                    if zf < nz {
                        Some(self.cell_index(xf, yf, zf))
                    } else {
                        None
                    },
                    self.w[self.w_index(xf, yf, zf)],
                ),
            };
            let exterior_open = axis == 1 && yf == ny && self.config.open_top;
            if left.is_none() && right.is_none() {
                return None;
            }
            if let (Some(a), Some(b)) = (left, right)
                && (self.solid[a] || self.solid[b])
            {
                return None;
            }
            if let Some(a) = left
                && right.is_none()
                && self.solid[a]
            {
                return None;
            }
            if let Some(b) = right
                && left.is_none()
                && self.solid[b]
            {
                return None;
            }
            if (left.is_none() || right.is_none()) && !exterior_open {
                return None;
            }
            // A face between two dry cells (or a dry cell and the
            // exterior) carries exactly zero donor and high-order
            // flux, so skipping it cannot change the result.
            if !self.conservative_momentum
                && left.is_none_or(|i| old[i] == 0.0)
                && right.is_none_or(|i| old[i] == 0.0)
            {
                return None;
            }
            let upstream = if vel >= 0.0 { left } else { right };
            let c_up = upstream.map(|i| old[i]).unwrap_or(0.0);
            let courant = vel * dt / h;
            let donor = courant * c_up;
            let reconstructed = upstream
                .map(|i| {
                    let mut q = [f[0] as isize, f[1] as isize, f[2] as isize];
                    // Select the upwind cell on this face axis; the
                    // transverse cell coordinates stay unchanged.
                    let axis_coord = match axis {
                        0 => f[0] as isize - isize::from(vel >= 0.0),
                        1 => f[1] as isize - isize::from(vel >= 0.0),
                        _ => f[2] as isize - isize::from(vel >= 0.0),
                    };
                    q[axis] = axis_coord;
                    let c0 = old[i];
                    let mut qm = q;
                    qm[axis] -= 1;
                    let mut qp = q;
                    qp[axis] += 1;
                    let sample = |p: [isize; 3]| -> f64 {
                        if p[0] < 0
                            || p[1] < 0
                            || p[2] < 0
                            || p[0] >= nx as isize
                            || p[1] >= ny as isize
                            || p[2] >= nz as isize
                        {
                            0.0
                        } else {
                            let j = self.cell_index(p[0] as usize, p[1] as usize, p[2] as usize);
                            if self.solid[j] { 0.0 } else { old[j] }
                        }
                    };
                    let slope = minmod(c0 - sample(qm), sample(qp) - c0);
                    (c0 + 0.5 * if vel >= 0.0 { slope } else { -slope }).clamp(0.0, 1.0)
                })
                .unwrap_or(0.0);
            let high = if let Some(planes) = &planes {
                upstream.map_or(0.0, |i| {
                    planes[i].map_or(courant * old[i], |plane| {
                        if self.experimental_surface_films
                            && axis == 1
                            && plane.normal.iter().sum::<f64>() * 0.5 >= plane.alpha
                        {
                            // Vertically unresolved films use positive upwind
                            // transport; momentum follows this accepted mass.
                            donor
                        } else {
                            plane.swept_volume(axis, courant)
                        }
                    })
                })
            } else {
                courant * reconstructed
            };
            Some((left, right, donor, high, courant))
        };
        let mut raw_fluxes: Vec<(Option<usize>, Option<usize>, f64, f64)> = Vec::new();
        let mut full_fluxes = Vec::new();
        for axis in 0..3 {
            let ext = match axis {
                0 => [nx + 1, ny, nz],
                1 => [nx, ny + 1, nz],
                _ => [nx, ny, nz + 1],
            };
            // Rayon's collect preserves sequential order, so the limiter below
            // sees exactly the serial z/y/x face list.
            let faces: Vec<_> = (0..ext[1] * ext[2])
                .into_par_iter()
                .flat_map_iter(|row| {
                    let (yf, zf) = (row % ext[1], row / ext[1]);
                    (0..ext[0]).filter_map(move |xf| face_flux(axis, xf, yf, zf))
                })
                .collect();
            for (left, right, donor, high, full) in faces {
                raw_fluxes.push((left, right, donor, high));
                if self.conservative_momentum {
                    full_fluxes.push(full);
                }
            }
        }
        // Only cells with a non-trivial face can change. Every limiter pass and
        // the FCT bounds below work on this list: an earlier version swept the
        // whole domain (and re-allocated four domain-sized vectors) up to 64
        // times per substep, which cost more than the pressure solve once the
        // projection residual was loosened. Cells off the list keep `old`
        // (inside [0, 1] by invariant), so results are identical.
        let mut touched: Vec<usize> = Vec::with_capacity(raw_fluxes.len() * 2);
        for (left, right, _, _) in &raw_fluxes {
            touched.extend(left.iter().copied());
            touched.extend(right.iter().copied());
        }
        touched.sort_unstable();
        touched.dedup();
        // Shared-face donor flux limiter. Divergence errors at newly-wetted
        // cells may compress the low-order update; rejected inflow remains in
        // its source cell because one common face flux is scaled on both sides.
        let mut low_fluxes: Vec<f64> = raw_fluxes.iter().map(|(_, _, donor, _)| *donor).collect();
        let mut low = old.clone();
        let mut outgoing = vec![0.0; n];
        let mut incoming = vec![0.0; n];
        let mut source_scale = vec![1.0; n];
        let mut destination_scale = vec![1.0; n];
        for _ in 0..64 {
            for &i in &touched {
                low[i] = old[i];
                outgoing[i] = 0.0;
                incoming[i] = 0.0;
                source_scale[i] = 1.0;
                destination_scale[i] = 1.0;
            }
            for (edge, (left, right, _, _)) in raw_fluxes.iter().enumerate() {
                let flux = low_fluxes[edge];
                let (source, destination) = if flux >= 0.0 {
                    (*left, *right)
                } else {
                    (*right, *left)
                };
                if let Some(i) = source {
                    outgoing[i] += flux.abs();
                }
                if let Some(i) = destination {
                    incoming[i] += flux.abs();
                }
                if let Some(i) = left {
                    low[*i] -= flux;
                }
                if let Some(i) = right {
                    low[*i] += flux;
                }
            }
            let mut changed = false;
            // Only scale genuine CFL/divergence overshoots. Roundoff-level
            // excursions (pressure residual on full cells, ~1e-13) are far
            // inside the hard 1e-10 bound check below; chasing them used to
            // exhaust all 64 passes every substep without changing physics.
            for &i in &touched {
                if low[i] < -DONOR_LIMIT_TOLERANCE && outgoing[i] > 0.0 {
                    source_scale[i] = (old[i] / outgoing[i]).clamp(0.0, 1.0);
                    changed = true;
                }
                if low[i] > 1.0 + DONOR_LIMIT_TOLERANCE && incoming[i] > 0.0 {
                    destination_scale[i] =
                        ((1.0 - old[i] + outgoing[i]) / incoming[i]).clamp(0.0, 1.0);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
            for (edge, (left, right, _, _)) in raw_fluxes.iter().enumerate() {
                let flux = low_fluxes[edge];
                let (source, destination) = if flux >= 0.0 {
                    (*left, *right)
                } else {
                    (*right, *left)
                };
                if let Some(i) = source {
                    low_fluxes[edge] *= source_scale[i];
                }
                if let Some(i) = destination {
                    low_fluxes[edge] *= destination_scale[i];
                }
            }
        }
        for &i in &touched {
            low[i] = old[i];
        }
        let mut outflow_fraction = 0.0;
        let mut region_outflow_fraction = 0.0;
        let mut region_inflow_fraction = 0.0;
        let mut anti: Vec<(Option<usize>, Option<usize>, f64, bool)> =
            Vec::with_capacity(raw_fluxes.len());
        for (edge, (left, right, _, high)) in raw_fluxes.into_iter().enumerate() {
            let donor = low_fluxes[edge];
            accumulate_region_flux(
                left,
                right,
                donor,
                tracked_region,
                &mut region_outflow_fraction,
                &mut region_inflow_fraction,
            );
            if let Some(i) = left {
                low[i] -= donor;
            } else if donor < 0.0 {
                outflow_fraction += -donor;
            }
            if let Some(i) = right {
                low[i] += donor;
            } else if donor > 0.0 {
                outflow_fraction += donor;
            }
            anti.push((left, right, high - donor, false));
        }
        let (low_min, low_max) = touched
            .iter()
            .map(|&i| low[i])
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                (lo.min(v), hi.max(v))
            });
        if !touched.is_empty() && (low_min < -1.0e-10 || low_max > 1.0 + 1.0e-10) {
            return Err(MacError::LowOrderCflViolation {
                minimum: low_min,
                maximum: low_max,
            });
        }
        // FCT/Zalesak bounds are computed from all shared face antidiffusive fluxes.
        // The accumulators reuse the limiter's scratch vectors (reset on `touched`).
        let (mut p_plus, mut p_minus) = (outgoing, incoming);
        for &i in &touched {
            p_plus[i] = 0.0;
            p_minus[i] = 0.0;
        }
        for (left, right, a, _) in &anti {
            if let Some(i) = left {
                let delta = -*a;
                if delta > 0.0 {
                    p_plus[*i] += delta
                } else {
                    p_minus[*i] += delta
                }
            }
            if let Some(i) = right {
                let delta = *a;
                if delta > 0.0 {
                    p_plus[*i] += delta
                } else {
                    p_minus[*i] += delta
                }
            }
        }
        let (mut r_plus, mut r_minus) = (source_scale, destination_scale);
        for &i in &touched {
            r_plus[i] = 1.0;
            r_minus[i] = 1.0;
            if p_plus[i] > 0.0 {
                r_plus[i] = ((1.0 - low[i]) / p_plus[i]).clamp(0.0, 1.0)
            }
            if p_minus[i] < 0.0 {
                r_minus[i] = ((-low[i]) / p_minus[i]).clamp(0.0, 1.0)
            }
        }
        let mut corrected_outflow = 0.0;
        for (edge, &(left, right, a, _)) in anti.iter().enumerate() {
            let limiter = match (left, right) {
                (Some(l), Some(r)) if a >= 0.0 => r_minus[l].min(r_plus[r]),
                (Some(l), Some(r)) => r_plus[l].min(r_minus[r]),
                (Some(l), None) if a >= 0.0 => r_minus[l],
                (Some(_), None) => 1.0,
                (None, Some(_r)) if a >= 0.0 => 1.0,
                (None, Some(r)) => r_minus[r],
                _ => 1.0,
            };
            let flux = a * limiter;
            if self.strict_phase_bounds {
                low_fluxes[edge] += flux;
            }
            accumulate_region_flux(
                left,
                right,
                flux,
                tracked_region,
                &mut region_outflow_fraction,
                &mut region_inflow_fraction,
            );
            if let Some(l) = left {
                low[l] -= flux
            }
            if let Some(r) = right {
                low[r] += flux
            } else if left.is_some() {
                // The exterior is on the right at the open top. Signed
                // anti-flux must reduce donor outflow when it is negative.
                corrected_outflow += flux;
            } else {
                corrected_outflow -= flux;
            }
        }
        // The low-order flux is CFL bounded. Any failure indicates an algorithmic
        // defect; do not hide it with clamping or renormalization.
        let (minimum, maximum) = touched
            .iter()
            .map(|&i| low[i])
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                (lo.min(v), hi.max(v))
            });
        // Strict mode validates the final paired transfers below against exact
        // [0,1] bounds. Its preliminary FCT candidate is not yet accepted;
        // rejecting before that limiter prevents it doing its intended work.
        if !self.strict_phase_bounds
            && !touched.is_empty()
            && (minimum < -1.0e-10 || maximum > 1.0 + 1.0e-10)
        {
            return Err(MacError::TransportBoundsViolation { minimum, maximum });
        }
        let volume = self.cell_volume();
        let mut net_open_outflow_fraction = outflow_fraction + corrected_outflow;
        let mut strict = StrictTransferMetrics::default();
        if self.strict_phase_bounds {
            // Re-limit the actually accepted donor+PLIC face transfers, never
            // cell amounts. Reuse FCT scratch and recompute all boundary/region
            // ledgers from exactly the final paired transfers.
            strict = strict_transfer_bounds(
                &old,
                &anti,
                &mut low_fluxes,
                &touched,
                &mut p_plus,
                &mut p_minus,
                &mut low,
            )?;
            net_open_outflow_fraction = 0.0;
            region_outflow_fraction = 0.0;
            region_inflow_fraction = 0.0;
            for ((left, right, _, _), &flux) in anti.iter().zip(&low_fluxes) {
                accumulate_region_flux(
                    *left,
                    *right,
                    flux,
                    tracked_region,
                    &mut region_outflow_fraction,
                    &mut region_inflow_fraction,
                );
                if right.is_none() {
                    net_open_outflow_fraction += flux;
                } else if left.is_none() {
                    net_open_outflow_fraction -= flux;
                }
            }
        }
        if !net_open_outflow_fraction.is_finite() || net_open_outflow_fraction < 0.0 {
            return Err(MacError::NegativeOpenBoundaryFlux {
                volume_fraction: net_open_outflow_fraction,
            });
        }
        if self.conservative_momentum || self.experimental_surface_films {
            let candidate =
                momentum::transport(self, &old, &low, &anti, &low_fluxes, &full_fluxes)?;
            [self.u, self.v, self.w] = candidate.velocity;
            strict.momentum = candidate.metrics;
            strict.momentum.bytes += full_fluxes.capacity() * size_of::<f64>();
        }
        self.fraction = low;
        Ok((
            net_open_outflow_fraction * volume,
            region_outflow_fraction * volume,
            region_inflow_fraction * volume,
            strict,
        ))
    }
}

/// Absolute pressure corresponding to zero stored gauge pressure.
const ATMOSPHERIC_PRESSURE_PA: f64 = 101_325.0;

/// Fraction excursion below which the shared-face donor limiter leaves fluxes
/// unscaled. Must stay well inside the 1e-10 transport bound check.
const DONOR_LIMIT_TOLERANCE: f64 = 1.0e-12;

#[derive(Debug, Default)]
struct StrictTransferMetrics {
    momentum: momentum::MomentumMetrics,
    path_repairs: u64,
    scratch_bytes: usize,
}

fn strict_transfer_bounds(
    old: &[f64],
    edges: &[(Option<usize>, Option<usize>, f64, bool)],
    transfers: &mut [f64],
    touched: &[usize],
    incoming: &mut [f64],
    outgoing: &mut [f64],
    candidate: &mut [f64],
) -> Result<StrictTransferMetrics, MacError> {
    for pass in 0..=64 {
        for &i in touched {
            incoming[i] = 0.0;
            outgoing[i] = 0.0;
        }
        for (&(left, right, _, _), &flux) in edges.iter().zip(transfers.iter()) {
            let (donor, receiver) = if flux >= 0.0 {
                (left, right)
            } else {
                (right, left)
            };
            if let Some(i) = donor {
                outgoing[i] += flux.abs();
            }
            if let Some(i) = receiver {
                incoming[i] += flux.abs();
            }
        }
        let mut valid = true;
        for &i in touched {
            // Form the signed flux difference first: a balanced full-cell
            // cycle stays exactly full instead of acquiring an ulp from
            // adding a transfer to 1 before subtracting its matching outflow.
            let v = old[i] + (incoming[i] - outgoing[i]);
            candidate[i] = v;
            valid &= v.is_finite() && (0.0..=1.0).contains(&v);
        }
        if valid {
            return Ok(StrictTransferMetrics::default());
        }
        if pass == 64 {
            return repair_strict_transfer_paths(
                old, edges, transfers, touched, incoming, outgoing, candidate,
            );
        }
        // Gauss-Seidel face corrections use updated neighbour sums immediately.
        // Jacobi scaling propagated a roundoff defect only one edge per pass
        // and could chase it around a saturated loop indefinitely. Alternate
        // traversal so both orientations of saturated paths are resolved.
        for offset in 0..edges.len() {
            let edge = if pass % 2 == 0 {
                edges.len() - 1 - offset
            } else {
                offset
            };
            let (left, right, _, _) = edges[edge];
            let flux = transfers[edge];
            let (donor, receiver) = if flux >= 0.0 {
                (left, right)
            } else {
                (right, left)
            };
            let mut reduction = 0.0_f64;
            for (cell, lower) in [(donor, true), (receiver, false)] {
                if let Some(i) = cell {
                    let v = old[i] + (incoming[i] - outgoing[i]);
                    let violation = if lower { -v } else { v - 1.0 };
                    if violation > 0.0 {
                        reduction = reduction.max(violation);
                    }
                }
            }
            let magnitude = flux.abs();
            // Round an actual correction inward in transfer units. A broad
            // fraction-sized safety margin creates fresh capacity violations
            // at full neighbours and amplifies around saturated cycles.
            let accepted = if reduction > 0.0 {
                (magnitude - reduction).next_down().max(0.0)
            } else {
                magnitude
            };
            let removed = magnitude - accepted;
            transfers[edge] = accepted.copysign(flux);
            if let Some(i) = donor {
                outgoing[i] -= removed;
            }
            if let Some(i) = receiver {
                incoming[i] -= removed;
            }
        }
    }
    unreachable!("bounded strict transfer loop returns")
}

/// Route a reduction back along an incoming path (overflow), or forward along
/// an outgoing path (underflow), to a cell with actual capacity. Intermediate
/// full cells receive the same paired correction on both incident faces.
/// Unlike local sweeps, this does not send a capacity defect around a loop.
fn repair_strict_transfer_paths(
    old: &[f64],
    edges: &[(Option<usize>, Option<usize>, f64, bool)],
    transfers: &mut [f64],
    touched: &[usize],
    incoming: &mut [f64],
    outgoing: &mut [f64],
    candidate: &mut [f64],
) -> Result<StrictTransferMetrics, MacError> {
    let n = touched.len();
    let slot = |cell| {
        touched
            .binary_search(&cell)
            .expect("edge endpoint is touched")
    };
    let mut offsets = vec![0usize; n + 1];
    for &(l, r, _, _) in edges {
        for i in [l, r].into_iter().flatten() {
            offsets[slot(i) + 1] += 1;
        }
    }
    for i in 1..=n {
        offsets[i] += offsets[i - 1];
    }
    let mut adjacency = vec![0usize; offsets[n]];
    let mut cursors = offsets[..n].to_vec();
    for (edge, &(l, r, _, _)) in edges.iter().enumerate() {
        for i in [l, r].into_iter().flatten() {
            let i = slot(i);
            adjacency[cursors[i]] = edge;
            cursors[i] += 1;
        }
    }
    // Reuse the cursor allocation for BFS parents.
    let mut parents = cursors;
    parents.fill(usize::MAX);
    let mut queue = Vec::with_capacity(touched.len());
    let mut metrics = StrictTransferMetrics {
        momentum: momentum::MomentumMetrics::default(),
        path_repairs: 0,
        scratch_bytes: (offsets.capacity()
            + adjacency.capacity()
            + parents.capacity()
            + queue.capacity())
            * size_of::<usize>(),
    };
    // Every correction either resolves a violated endpoint or exhausts a path
    // edge. Keep an explicit finite budget even for floating-point degeneracy.
    let budget = 64.min(touched.len().saturating_add(edges.len()));
    for repair in 0..=budget {
        for &i in touched {
            incoming[i] = 0.0;
            outgoing[i] = 0.0;
        }
        for (&(l, r, _, _), &f) in edges.iter().zip(transfers.iter()) {
            let (d, r) = if f >= 0.0 { (l, r) } else { (r, l) };
            if let Some(i) = d {
                outgoing[i] += f.abs();
            }
            if let Some(i) = r {
                incoming[i] += f.abs();
            }
        }
        for &i in touched {
            candidate[i] = old[i] + (incoming[i] - outgoing[i]);
        }
        let Some(&root) = touched
            .iter()
            .find(|&&i| !(0.0..=1.0).contains(&candidate[i]))
        else {
            return Ok(metrics);
        };
        if !candidate[root].is_finite() || repair == budget {
            break;
        }
        let overflow = candidate[root] > 1.0;
        let needed = if overflow {
            candidate[root] - 1.0
        } else {
            -candidate[root]
        };
        for &i in &queue {
            parents[i] = usize::MAX;
        }
        queue.clear();
        let root = slot(root);
        parents[root] = 0;
        queue.push(root);
        let mut target = None;
        let mut boundary_edge = None;
        let mut best_capacity = 0.0_f64;
        let mut best = None;
        let mut cursor = 0;
        'search: while cursor < queue.len() {
            let cell = queue[cursor];
            cursor += 1;
            if cell != root {
                let capacity = if overflow {
                    1.0 - candidate[touched[cell]]
                } else {
                    candidate[touched[cell]]
                };
                if capacity >= needed {
                    target = Some(cell);
                    break;
                }
                if capacity > best_capacity {
                    best_capacity = capacity;
                    best = Some(cell);
                }
            }
            for &edge in &adjacency[offsets[cell]..offsets[cell + 1]] {
                let (l, r, _, _) = edges[edge];
                let f = transfers[edge];
                if f == 0.0 {
                    continue;
                }
                let (d, r) = if f >= 0.0 { (l, r) } else { (r, l) };
                let next = if overflow && r == Some(touched[cell]) {
                    d
                } else if !overflow && d == Some(touched[cell]) {
                    r
                } else {
                    continue;
                };
                if next.is_none() {
                    target = Some(cell);
                    boundary_edge = Some(edge);
                    break 'search;
                }
                if let Some(next) = next {
                    let next = slot(next);
                    if parents[next] == usize::MAX {
                        parents[next] = edge;
                        queue.push(next);
                    }
                }
            }
        }
        let Some(target) = target.or(best) else {
            break;
        };
        let capacity = if boundary_edge.is_some() {
            f64::INFINITY
        } else if overflow {
            1.0 - candidate[touched[target]]
        } else {
            candidate[touched[target]]
        };
        let mut amount = needed.min(capacity);
        if let Some(edge) = boundary_edge {
            amount = amount.min(transfers[edge].abs());
        }
        let mut cell = target;
        while cell != root {
            let edge = parents[cell];
            amount = amount.min(transfers[edge].abs());
            let (l, r, _, _) = edges[edge];
            let (d, r) = if transfers[edge] >= 0.0 {
                (l, r)
            } else {
                (r, l)
            };
            cell = slot(if overflow { r } else { d }.expect("BFS path has an interior parent"));
        }
        if amount <= 0.0 {
            break;
        }
        cell = target;
        let mut changed = false;
        if let Some(edge) = boundary_edge {
            let f = transfers[edge];
            transfers[edge] = (f.abs() - amount).max(0.0).copysign(f);
            changed |= transfers[edge] != f;
        }
        while cell != root {
            let edge = parents[cell];
            let f = transfers[edge];
            let (l, r, _, _) = edges[edge];
            let (d, r) = if f >= 0.0 { (l, r) } else { (r, l) };
            transfers[edge] = (f.abs() - amount).max(0.0).copysign(f);
            changed |= transfers[edge] != f;
            cell = slot(if overflow { r } else { d }.expect("BFS path has an interior parent"));
        }
        if !changed {
            break;
        }
        metrics.path_repairs += 1;
    }
    Err(MacError::TransportBoundsViolation {
        minimum: touched.iter().map(|&i| candidate[i]).fold(1.0, f64::min),
        maximum: touched.iter().map(|&i| candidate[i]).fold(0.0, f64::max),
    })
}

/// Plane n.x <= alpha inside the unit cell [0,1]^3. The normal is a
/// Youngs-style smoothed VOF gradient; alpha is inverted from the stored volume.
#[derive(Clone, Copy)]
struct InterfacePlane {
    normal: [f64; 3],
    alpha: f64,
}

impl InterfacePlane {
    fn from_fraction(mut normal: [f64; 3], fraction: f64) -> Self {
        let norm: f64 = normal.iter().map(|n| n.abs()).sum();
        if norm == 0.0 {
            normal = [0.0, 1.0, 0.0];
        } else {
            for n in &mut normal {
                *n /= norm;
            }
        }
        // Volume is monotone in alpha on [lo, hi] (0 at lo, 1 at hi). The
        // Illinois variant of regula falsi keeps that bracket and converges
        // superlinearly; plain bisection needed 48 volume evaluations.
        let mut lo: f64 = normal.iter().map(|&n| n.min(0.0)).sum();
        let mut hi: f64 = normal.iter().map(|&n| n.max(0.0)).sum();
        let (mut g_lo, mut g_hi) = (-fraction, 1.0 - fraction);
        let mut side = 0i8;
        let mut alpha = 0.5 * (lo + hi);
        for _ in 0..64 {
            alpha = (lo * g_hi - hi * g_lo) / (g_hi - g_lo);
            if !(alpha > lo && alpha < hi) {
                alpha = 0.5 * (lo + hi);
            }
            let g = plane_cube_fraction(normal, alpha) - fraction;
            if g.abs() <= 1.0e-15 || hi - lo <= 1.0e-14 {
                break;
            }
            if g < 0.0 {
                lo = alpha;
                g_lo = g;
                if side == -1 {
                    g_hi *= 0.5;
                }
                side = -1;
            } else {
                hi = alpha;
                g_hi = g;
                if side == 1 {
                    g_lo *= 0.5;
                }
                side = 1;
            }
        }
        Self { normal, alpha }
    }

    fn swept_volume(self, axis: usize, courant: f64) -> f64 {
        let width = courant.abs().min(1.0);
        if width == 0.0 {
            return 0.0;
        }
        let start = if courant > 0.0 { 1.0 - width } else { 0.0 };
        let mut normal = self.normal;
        let alpha = self.alpha - normal[axis] * start;
        normal[axis] *= width;
        courant.signum() * width * plane_cube_fraction(normal, alpha)
    }
}

/// Inclusion/exclusion integral for a half-space over a unit cube. Reflect
/// negative axes and use complement symmetry to avoid cancellation near full.
fn plane_cube_fraction(normal: [f64; 3], mut alpha: f64) -> f64 {
    let mut weights = [0.0; 3];
    let mut dimension = 0;
    let scale: f64 = normal.iter().map(|n| n.abs()).sum();
    for n in normal {
        if n < 0.0 {
            alpha -= n;
        }
        if n.abs() > scale * 1e-12 {
            weights[dimension] = n.abs();
            dimension += 1;
        }
    }
    weights.sort_by(|a, b| b.total_cmp(a));
    let [a, b, c] = weights;
    let total = a + b + c;
    if alpha <= 0.0 {
        return 0.0;
    }
    if alpha >= total {
        return 1.0;
    }
    let complement = alpha > total * 0.5;
    if complement {
        alpha = total - alpha;
    }
    let volume = match dimension {
        1 => alpha / a,
        2 if alpha >= b => (alpha - 0.5 * b) / a,
        2 => alpha * alpha / (2.0 * a * b),
        _ if alpha >= b + c => (alpha - 0.5 * (b + c)) / a,
        _ => {
            // Integrate the shortest axis analytically before inclusion /
            // exclusion. Factoring t^3-(t-c)^3 avoids cancellation as c -> 0.
            let cubic_difference = |t: f64| {
                if t <= 0.0 {
                    0.0
                } else if t <= c {
                    t * t * t
                } else {
                    c * (3.0 * t * (t - c) + c * c)
                }
            };
            (cubic_difference(alpha) - cubic_difference(alpha - a) - cubic_difference(alpha - b)
                + cubic_difference(alpha - a - b))
                / (6.0 * a * b * c)
        }
    }
    .clamp(0.0, 1.0);
    if complement { 1.0 - volume } else { volume }
}

fn accumulate_region_flux(
    left: Option<usize>,
    right: Option<usize>,
    flux: f64,
    region: Option<&[bool]>,
    outward: &mut f64,
    inward: &mut f64,
) {
    let Some(region) = region else { return };
    let (source, destination) = if flux >= 0.0 {
        (left, right)
    } else {
        (right, left)
    };
    let source_inside = source.is_some_and(|i| region[i]);
    let destination_inside = destination.is_some_and(|i| region[i]);
    if source_inside && !destination_inside {
        *outward += flux.abs();
    } else if !source_inside && destination_inside {
        *inward += flux.abs();
    }
}

/// Linear standing-wave tank: 6 m long, 3 m tall, 0.5 m wide, closed sides,
/// open top, still depth 1 m, first mode eta = a cos(pi x / L) with a = 0.1 m.
/// Inviscid linear theory: omega^2 = g k tanh(k H), T = 4.00 s, no decay.
/// Twin of `docs/reports/ENG-103-evidence/reference-standing-wave.c`.
pub struct StandingWaveFixture {
    grid: MacGridWorld,
}

impl StandingWaveFixture {
    pub const LENGTH_M: f64 = 6.0;
    pub const DEPTH_M: f64 = 1.0;
    pub const AMPLITUDE_M: f64 = 0.1;

    pub fn new(refinement: u32, ambient_density: f64) -> Result<Self, Box<dyn std::error::Error>> {
        let r = refinement.max(1);
        let h = 0.25 / f64::from(r);
        let dims = [24 * r, 12 * r, 2 * r];
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), dims, 1_000_000)?;
        let mut volume = spall_voxel::Volume::new(
            spall_core::VolumeId::new(980).expect("nonzero volume id"),
            spall_core::CellSizeCode::Quarter,
        );
        for bz in 0..dims[2].div_ceil(32) as i64 {
            for by in 0..dims[1].div_ceil(32) as i64 {
                for bx in 0..dims[0].div_ceil(32) as i64 {
                    volume.insert_brick(
                        spall_core::BrickCoord::new(bx, by, bz),
                        spall_voxel::Brick::uniform(
                            spall_core::MaterialId::AIR,
                            spall_core::Revision(1),
                        ),
                    )?;
                }
            }
        }
        let boundary = SolidBoundary::capture(&volume, spec)?;
        let mut grid = MacGridWorld::new(
            &boundary,
            MacConfig {
                cell_size_m: h,
                ..MacConfig::default()
            },
        )?;
        grid.set_ambient_density(ambient_density)?;
        let k = std::f64::consts::PI / Self::LENGTH_M;
        let [nx, ny, nz] = grid.dims();
        for x in 0..nx {
            // Exact cell average of the cosine surface over the column.
            let (x0, x1) = (x as f64 * h, (x + 1) as f64 * h);
            let eta = Self::AMPLITUDE_M * ((k * x1).sin() - (k * x0).sin()) / (k * h);
            for y in 0..ny {
                let c = ((Self::DEPTH_M + eta - y as f64 * h) / h).clamp(0.0, 1.0);
                if c > 0.0 {
                    for z in 0..nz {
                        let i = grid.cell_index(x, y, z);
                        grid.fraction[i] = c;
                    }
                }
            }
        }
        Ok(Self { grid })
    }

    pub fn grid(&self) -> &MacGridWorld {
        &self.grid
    }

    pub fn grid_mut(&mut self) -> &mut MacGridWorld {
        &mut self.grid
    }

    pub fn analytic_period_s(&self) -> f64 {
        let k = std::f64::consts::PI / Self::LENGTH_M;
        let g = self.grid.config.gravity_m_s2[1].abs();
        2.0 * std::f64::consts::PI / (g * k * (k * Self::DEPTH_M).tanh()).sqrt()
    }

    /// Half the difference of the two end columns' mean water heights. The
    /// first mode is antisymmetric, so this rejects symmetric harmonics.
    pub fn end_elevation_m(&self) -> f64 {
        let [nx, _, _] = self.grid.dims();
        0.5 * (self.column_height_m(0) - self.column_height_m(nx - 1))
    }

    fn column_height_m(&self, x: usize) -> f64 {
        let [_, ny, nz] = self.grid.dims();
        let mut total = 0.0;
        for z in 0..nz {
            for y in 0..ny {
                total += self.grid.fraction[self.grid.cell_index(x, y, z)];
            }
        }
        total * self.grid.config.cell_size_m / nz as f64
    }
}

/// Peaks of |signal| above `floor`, as (time, amplitude).
pub fn oscillation_peaks(samples: &[(f64, f64)], floor: f64) -> Vec<(f64, f64)> {
    samples
        .windows(3)
        .filter_map(|w| {
            let (a, b, c) = (w[0].1.abs(), w[1].1.abs(), w[2].1.abs());
            (b >= a && b > c && b > floor).then_some((w[1].0, b))
        })
        .collect()
}

/// Mean period from linear-interpolated zero crossings (half periods).
pub fn zero_crossing_period(samples: &[(f64, f64)]) -> Option<f64> {
    let crossings: Vec<f64> = samples
        .windows(2)
        .filter(|w| w[0].1 * w[1].1 < 0.0)
        .map(|w| w[0].0 + (w[1].0 - w[0].0) * w[0].1 / (w[0].1 - w[1].1))
        .collect();
    (crossings.len() >= 3).then(|| {
        2.0 * (crossings[crossings.len() - 1] - crossings[0]) / (crossings.len() - 1) as f64
    })
}

/// Amplitude ratio per `period` from a log-linear least-squares fit of peaks.
pub fn amplitude_ratio_per_period(peaks: &[(f64, f64)], period: f64) -> Option<f64> {
    if peaks.len() < 2 {
        return None;
    }
    let n = peaks.len() as f64;
    let (mx, my) = peaks
        .iter()
        .fold((0.0, 0.0), |(sx, sy), (t, a)| (sx + t / n, sy + a.ln() / n));
    let (num, den) = peaks.iter().fold((0.0, 0.0), |(num, den), (t, a)| {
        (num + (t - mx) * (a.ln() - my), den + (t - mx).powi(2))
    });
    Some((num / den * period).exp())
}

/// Grid water on the ENG-103 finite-reservoir voxel scene
/// (`fixtures::ReservoirScene`), seeded with the scene's reference volume.
pub struct GridReservoirFixture {
    volume: spall_voxel::Volume,
    grid: MacGridWorld,
    scale: u32,
    refinement: u32,
    initial_volume_m3: f64,
    upper_pool_region: Vec<bool>,
    last_boundary_update_micros: u64,
}

impl GridReservoirFixture {
    pub fn new(scale: u32, open_top: bool) -> Result<Self, Box<dyn std::error::Error>> {
        Self::configured(scale, open_top, false, 1)
    }

    pub fn new_tunnel(scale: u32) -> Result<Self, Box<dyn std::error::Error>> {
        Self::configured(scale, false, true, 1)
    }

    pub fn new_tunnel_refined(
        scale: u32,
        refinement: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::configured(scale, false, true, refinement)
    }

    pub fn new_basin(scale: u32) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_basin_refined(scale, 1)
    }

    /// Separate equilibrium fixture: same basin walls and water volume, but
    /// fill the complete left compartment. The historical basin deliberately
    /// remains unchanged; its two dry side strips cause initial spreading.
    pub fn new_equilibrium_basin(
        scale: u32,
        refinement: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut fixture = Self::new_basin_refined(scale, refinement)?;
        let s = i64::from(scale) * i64::from(refinement);
        fixture.grid.fraction.fill(0.0);
        fill_region_uniform_columns(
            &mut fixture.grid,
            [
                i64::from(refinement),
                12 * s,
                i64::from(refinement),
                8 * s - i64::from(refinement),
                i64::from(refinement),
                10 * s,
            ],
            fixture.initial_volume_m3,
        )?;
        Ok(fixture)
    }

    pub fn new_basin_refined(
        scale: u32,
        refinement: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let source = crate::fixtures::ReservoirScene::new_stability_basin(scale)?;
        let volume = source.volume().clone();
        let spec = source.domain();
        let initial_volume_m3 = source.reference_volume_m3();
        let base_boundary = SolidBoundary::capture(&volume, spec)?;
        let boundary = base_boundary.refined(refinement, 2_000_000)?;
        let config = MacConfig {
            cell_size_m: 0.25 / f64::from(refinement),
            ..MacConfig::default()
        };
        let mut grid = MacGridWorld::new(&boundary, config)?;
        let s = i64::from(scale) * i64::from(refinement);
        fill_region_uniform_columns(
            &mut grid,
            [2 * s, 11 * s, s, 7 * s, i64::from(refinement), 4 * s],
            initial_volume_m3,
        )?;
        Ok(Self {
            upper_pool_region: upper_pool_mask(&grid, scale, refinement, false),
            volume,
            grid,
            scale,
            refinement,
            initial_volume_m3,
            last_boundary_update_micros: 0,
        })
    }

    fn configured(
        scale: u32,
        open_top: bool,
        tunnel_pool: bool,
        refinement: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let source = if tunnel_pool {
            crate::fixtures::ReservoirScene::new_tunnel_under_separate_pool(scale)?
        } else {
            crate::fixtures::ReservoirScene::new_scaled(scale)?
        };
        let volume = source.volume().clone();
        let spec = source.domain();
        let initial_volume_m3 = source.reference_volume_m3();
        let base_boundary = SolidBoundary::capture(&volume, spec)?;
        let boundary = base_boundary.refined(refinement, 2_000_000)?;
        let config = MacConfig {
            cell_size_m: 0.25 / f64::from(refinement),
            open_top,
            ..MacConfig::default()
        };
        let mut grid = MacGridWorld::new(&boundary, config)?;
        let base_x = i64::from(scale) * i64::from(refinement);
        let region_left = [
            3 * base_x,
            12 * base_x,
            2 * base_x,
            6 * base_x,
            1,
            5 * base_x,
        ];
        let region_right = [
            14 * base_x,
            21 * base_x,
            2 * base_x,
            6 * base_x,
            1,
            4 * base_x,
        ];
        if tunnel_pool {
            fill_region_volume(&mut grid, region_left, initial_volume_m3 * 0.65)?;
            fill_region_volume(&mut grid, region_right, initial_volume_m3 * 0.17)?;
            let upper = [
                14 * base_x + 1,
                20 * base_x,
                base_x + 1,
                7 * base_x,
                5 * base_x,
                8 * base_x,
            ];
            fill_region_volume(&mut grid, upper, initial_volume_m3 * 0.18)?;
        } else {
            fill_region_volume(&mut grid, region_left, initial_volume_m3 * 0.70)?;
            fill_region_volume(&mut grid, region_right, initial_volume_m3 * 0.30)?;
        }
        Ok(Self {
            upper_pool_region: upper_pool_mask(&grid, scale, refinement, tunnel_pool),
            volume,
            grid,
            scale,
            refinement,
            initial_volume_m3,
            last_boundary_update_micros: 0,
        })
    }

    pub fn volume(&self) -> &spall_voxel::Volume {
        &self.volume
    }
    pub fn grid(&self) -> &MacGridWorld {
        &self.grid
    }
    pub fn grid_mut(&mut self) -> &mut MacGridWorld {
        &mut self.grid
    }
    pub fn step(&mut self, dt_s: f64) -> Result<MacStepMetrics, MacError> {
        self.grid
            .step_tracking_region(dt_s, Some(&self.upper_pool_region))
    }
    pub fn initial_volume_m3(&self) -> f64 {
        self.initial_volume_m3
    }
    pub fn scale(&self) -> u32 {
        self.scale
    }

    pub fn refinement(&self) -> u32 {
        self.refinement
    }

    fn cell_scale(&self) -> i64 {
        i64::from(self.scale) * i64::from(self.refinement)
    }

    pub fn excavate_canal(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let s = self.cell_scale();
        let x = 12 * s;
        let z = (self.grid.spec.dimensions()[2] as i64) / 2;
        let mut edit = spall_voxel::EditPlan::new(self.volume.id());
        for y in s..2 * s {
            for zz in z - s..z + s {
                edit.set(GlobalCell::new(x, y, zz), spall_core::MaterialId::AIR);
            }
        }
        self.apply_staged_edit(&edit)
    }

    pub fn breach_dam(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let s = self.cell_scale();
        let x = 12 * s;
        let z = (self.grid.spec.dimensions()[2] as i64) / 2;
        let mut edit = spall_voxel::EditPlan::new(self.volume.id());
        for y in s..5 * s {
            for zz in z - 2 * s..z + 2 * s {
                edit.set(GlobalCell::new(x, y, zz), spall_core::MaterialId::AIR);
            }
        }
        self.apply_staged_edit(&edit)
    }

    /// Closes the original canal. If water has entered any proposed solid
    /// cell, placement is rejected and the voxel and grid snapshots remain live.
    pub fn close_canal(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let s = self.cell_scale();
        let x = 12 * s;
        let z = (self.grid.spec.dimensions()[2] as i64) / 2;
        let mut edit = spall_voxel::EditPlan::new(self.volume.id());
        for y in s..2 * s {
            for zz in z - s..z + s {
                edit.set(GlobalCell::new(x, y, zz), spall_core::MaterialId(1));
            }
        }
        self.apply_staged_edit(&edit)
    }

    pub fn prepare_boundary_edit(
        &self,
        edit: &spall_voxel::EditPlan,
    ) -> Result<(spall_voxel::Volume, Vec<bool>), Box<dyn std::error::Error>> {
        if self.refinement != 1 {
            return Err(
                "voxel edits are disabled for the static refined comparison fixture".into(),
            );
        }
        let mut staged = self.volume.clone();
        staged.apply_edit(edit)?;
        let boundary = SolidBoundary::capture(&staged, self.grid.spec)?;
        let prepared = self.grid.prepare_boundary(&boundary)?;
        Ok((staged, prepared))
    }

    fn apply_staged_edit(
        &mut self,
        edit: &spall_voxel::EditPlan,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let start = Instant::now();
        let (staged_volume, prepared) = self.prepare_boundary_edit(edit)?;
        let mut staged_grid = self.grid.clone();
        staged_grid.commit_boundary(prepared)?;
        // Exclusive mutable access means observers can see either full state,
        // never the staged voxel volume paired with a stale boundary.
        self.volume = staged_volume;
        self.grid = staged_grid;
        self.last_boundary_update_micros = start.elapsed().as_micros() as u64;
        Ok(())
    }

    pub fn last_boundary_update_micros(&self) -> u64 {
        self.last_boundary_update_micros
    }

    pub fn downstream_volume_m3(&self) -> f64 {
        let dam = 12 * self.cell_scale();
        self.grid
            .fraction
            .iter()
            .enumerate()
            .filter(|(i, _)| self.grid.spec.cell_at(*i).x > self.grid.spec.origin().x + dam)
            .map(|(_, c)| *c * self.grid.cell_volume())
            .sum()
    }

    pub fn p95_level_difference_m(&self) -> f64 {
        let dam = self.grid.spec.origin().x + 12 * self.cell_scale();
        let left = self.region_column_heights(
            3 * self.cell_scale()..dam,
            2 * self.cell_scale()..6 * self.cell_scale(),
        );
        let right = self.region_column_heights(
            dam + 2 * self.cell_scale()..21 * self.cell_scale(),
            2 * self.cell_scale()..6 * self.cell_scale(),
        );
        (percentile95(left) - percentile95(right)).abs()
    }

    pub fn basin_surface_p95_m(&self) -> f64 {
        let s = self.cell_scale();
        percentile95(self.region_column_heights(2 * s..11 * s, s..7 * s))
    }

    pub fn upper_pool_volume_m3(&self) -> f64 {
        self.grid
            .fraction
            .iter()
            .enumerate()
            .filter(|(i, _)| self.upper_pool_region[*i])
            .map(|(_, f)| *f * self.grid.cell_volume())
            .sum()
    }

    /// Water at or above the top of the pool's x/z containment walls. The
    /// value is diagnostic: this region is air above/around the reservoir and
    /// its water indicates overtopping, not transfer through the tunnel roof.
    pub fn upper_pool_above_wall_volume_m3(&self) -> f64 {
        let s = self.cell_scale();
        self.grid
            .fraction
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                let c = self.grid.spec.cell_at(*i);
                c.x >= 14 * s && c.x < 21 * s && c.y >= 10 * s && c.z >= s && c.z < 7 * s
            })
            .map(|(_, f)| *f * self.grid.cell_volume())
            .sum()
    }

    pub fn lower_tunnel_volume_m3(&self) -> f64 {
        let s = self.cell_scale();
        self.grid
            .fraction
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                let c = self.grid.spec.cell_at(*i);
                c.x > 14 * s && c.x < 20 * s && c.y < 4 * s && c.z > s && c.z < 7 * s
            })
            .map(|(_, f)| *f * self.grid.cell_volume())
            .sum()
    }

    pub fn tunnel_roof_normal_speed_max_m_s(&self) -> f64 {
        let s = self.cell_scale() as usize;
        let mut maximum: f64 = 0.0;
        for z in s..7 * s {
            for x in 14 * s..21 * s {
                // The roof occupies voxel layer y=4s. Its lower and upper
                // staggered faces are y=4s and y=5s respectively.
                maximum = maximum
                    .max(self.grid.v[self.grid.v_index(x, 4 * s, z)].abs())
                    .max(self.grid.v[self.grid.v_index(x, 4 * s + 1, z)].abs());
            }
        }
        maximum
    }

    pub fn eastward_momentum_kg_m_s(&self) -> f64 {
        let [nx, ny, nz] = self.grid.dims();
        let s = self.cell_scale();
        let dam = self.grid.spec.origin().x + 12 * s;
        let mut total = 0.0;
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let i = self.grid.cell_index(x, y, z);
                    if self.grid.fraction[i] == 0.0 {
                        continue;
                    }
                    let cell_x = self.grid.spec.origin().x + x as i64;
                    if cell_x <= dam {
                        continue;
                    }
                    let vx = 0.5
                        * (self.grid.u[self.grid.u_index(x, y, z)]
                            + self.grid.u[self.grid.u_index(x + 1, y, z)]);
                    total += self.grid.config.density_kg_m3
                        * self.grid.fraction[i]
                        * self.grid.cell_volume()
                        * vx;
                }
            }
        }
        total
    }

    pub fn discharge_through_dam_m3_s(&self) -> f64 {
        let s = self.cell_scale();
        let x = (12 * s + 1) as usize;
        let zmid = (self.grid.spec.dimensions()[2] / 2) as usize;
        let mut q = 0.0;
        for z in (zmid.saturating_sub(s as usize))..(zmid + s as usize).min(self.grid.dims()[2]) {
            for y in s as usize..(2 * s) as usize {
                let l = self.grid.cell_index(x.saturating_sub(1), y, z);
                let r = self.grid.cell_index(x, y, z);
                let frac = 0.5 * (self.grid.fraction[l] + self.grid.fraction[r]);
                q += self.grid.u[self.grid.u_index(x, y, z)].max(0.0)
                    * frac
                    * self.grid.config.cell_size_m.powi(2);
            }
        }
        q
    }

    fn region_column_heights(
        &self,
        xr: std::ops::Range<i64>,
        zr: std::ops::Range<i64>,
    ) -> Vec<f64> {
        let [nx, ny, nz] = self.grid.dims();
        let mut heights = Vec::new();
        for z in 0..nz {
            for x in 0..nx {
                let gx = self.grid.spec.origin().x + x as i64;
                let gz = self.grid.spec.origin().z + z as i64;
                if !xr.contains(&gx) || !zr.contains(&gz) {
                    continue;
                }
                let mut h = 0.0;
                for y in 0..ny {
                    h += self.grid.fraction[self.grid.cell_index(x, y, z)]
                        * self.grid.config.cell_size_m;
                }
                heights.push(h);
            }
        }
        heights
    }
}

fn upper_pool_mask(grid: &MacGridWorld, scale: u32, refinement: u32, enabled: bool) -> Vec<bool> {
    let s = i64::from(scale) * i64::from(refinement);
    (0..grid.fraction.len())
        .map(|i| {
            if !enabled {
                return false;
            }
            let c = grid.spec.cell_at(i);
            c.x > 14 * s && c.x < 20 * s && c.y > 4 * s && c.z > s && c.z < 7 * s
        })
        .collect()
}

fn fill_region_volume(
    grid: &mut MacGridWorld,
    region: [i64; 6],
    mut volume: f64,
) -> Result<(), MacError> {
    let [x0, x1, z0, z1, y0, y1] = region;
    let h3 = grid.cell_volume();
    for y in y0..y1 {
        for z in z0..z1 {
            for x in x0..x1 {
                if volume <= 0.0 {
                    return Ok(());
                }
                let cell = GlobalCell::new(x, y, z);
                let Some(i) = grid.cell_index_global(cell) else {
                    continue;
                };
                if grid.solid[i] {
                    continue;
                }
                let fraction = (volume / h3).min(1.0);
                grid.fraction[i] = fraction;
                volume -= fraction * h3;
            }
        }
    }
    if volume > 1.0e-9 {
        return Err(MacError::InvalidWaterFraction);
    }
    Ok(())
}

/// Initialize a level, volume-conserving water layer over a rectangular
/// wetted footprint. Each vertical column receives the same water depth;
/// the final layer is represented by its VOF fraction instead of filling
/// whole rows in x/z order and accidentally creating a stepped surface.
fn fill_region_uniform_columns(
    grid: &mut MacGridWorld,
    region: [i64; 6],
    volume: f64,
) -> Result<(), MacError> {
    let [x0, x1, z0, z1, y0, y1] = region;
    if x0 >= x1 || z0 >= z1 || y0 >= y1 || !volume.is_finite() || volume < 0.0 {
        return Err(MacError::InvalidWaterFraction);
    }
    let footprint = ((x1 - x0) * (z1 - z0)) as f64;
    let column_depth = volume / (footprint * grid.config.cell_size_m.powi(2));
    let mut unapplied_depth = 0.0_f64;
    for z in z0..z1 {
        for x in x0..x1 {
            let mut remaining = column_depth;
            for y in y0..y1 {
                let cell = GlobalCell::new(x, y, z);
                let Some(i) = grid.cell_index_global(cell) else {
                    return Err(MacError::BoundaryMismatch);
                };
                if grid.solid[i] {
                    return Err(MacError::WaterOverlapsSolid(cell));
                }
                let c = (remaining / grid.config.cell_size_m).clamp(0.0, 1.0);
                grid.fraction[i] = c;
                remaining -= c * grid.config.cell_size_m;
            }
            unapplied_depth = unapplied_depth.max(remaining);
        }
    }
    if unapplied_depth > 1.0e-10 {
        return Err(MacError::InvalidWaterFraction);
    }
    Ok(())
}

fn percentile95(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) * 95) / 100]
}

fn json_number(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.12e}")
    } else {
        "null".to_owned()
    }
}

#[derive(Default)]
struct ProjectionStats {
    phase_applications: u32,
    phase_apply_micros: u64,
    phase_scratch_bytes: usize,
    phase_rows: usize,
    phase_reused: bool,
    phase_micros: u64,
    iterations: usize,
    active_cells: usize,
    residual_initial: f64,
    residual_final: f64,
    divergence_before: f64,
    divergence_after: f64,
    converged: bool,
}

#[derive(Debug, Clone)]
struct PhasePredictor {
    phase: PhaseWater,
    graph: PhaseGraph,
    limits: GraphLimits,
    sweeps: u32,
    iterative: bool,
}

struct BasePressurePreconditioner<'a> {
    ic0: Option<&'a Ic0Factor>,
    multigrid: Option<&'a mut Multigrid>,
    work: Option<&'a mut Vec<f64>>,
    active: &'a [usize],
    diag: &'a [f64],
}
impl BasePressurePreconditioner<'_> {
    fn apply(&mut self, source: &[f64], target: &mut [f64]) {
        if let Some(factor) = self.ic0 {
            factor.apply(source, target, self.active, self.work.as_mut().unwrap());
        } else if let Some(mg) = &mut self.multigrid {
            mg.apply(source, target);
        } else {
            for &i in self.active {
                target[i] = if self.diag[i] > 0.0 {
                    source[i] / self.diag[i]
                } else {
                    0.0
                };
            }
        }
    }
}

/// Balanced B = Q + (I-QA) S (I-AQ), Q=P B_c P^T.
/// Q is symmetric positive semidefinite, S is the existing symmetric fine
/// preconditioner. Thus x^T B x = x^T Q x + y^T S y with y=(I-AQ)x.
/// Owning CG retains its component-mean projection; no new gauge pins.
struct BalancedPhasePressure {
    labels: std::sync::Arc<Vec<u32>>,
    coarse: PressureSmoother,
    row_source: Vec<f64>,
    row_q: Vec<f64>,
    row_z: Vec<f64>,
    temporary: Vec<f64>,
    applied: Vec<f64>,
    sweeps: u32,
}
impl BalancedPhasePressure {
    fn new(graph: &PhaseGraph, coarse: PressureSmoother, n: usize, sweeps: u32) -> Self {
        let m = graph.rows().len();
        Self {
            labels: graph.shared_labels(),
            coarse,
            row_source: vec![0.0; m],
            row_q: vec![0.0; m],
            row_z: vec![0.0; m],
            temporary: vec![0.0; n],
            applied: vec![0.0; n],
            sweeps,
        }
    }
    fn apply(
        &mut self,
        source: &[f64],
        target: &mut [f64],
        operator: &PressureOperator,
        base: &mut BasePressurePreconditioner<'_>,
    ) -> Result<(), MacError> {
        self.row_source.fill(0.0);
        for &i in base.active {
            self.row_source[self.labels[i] as usize] += source[i];
        }
        self.coarse
            .apply(&self.row_source, &mut self.row_q, self.sweeps)
            .map_err(MacError::PhaseGraph)?;
        for &i in base.active {
            self.temporary[i] = self.row_q[self.labels[i] as usize];
        }
        operator.apply(&self.temporary, &mut self.applied);
        for &i in base.active {
            self.temporary[i] = source[i] - self.applied[i];
        }
        base.apply(&self.temporary, target);
        operator.apply(target, &mut self.applied);
        self.row_source.fill(0.0);
        for &i in base.active {
            self.row_source[self.labels[i] as usize] += self.applied[i];
        }
        self.coarse
            .apply(&self.row_source, &mut self.row_z, self.sweeps)
            .map_err(MacError::PhaseGraph)?;
        for &i in base.active {
            let row = self.labels[i] as usize;
            target[i] += self.row_q[row] - self.row_z[row];
        }
        if base.active.iter().any(|&i| !target[i].is_finite()) {
            return Err(MacError::PhaseGraph(GraphError::InvalidState));
        }
        Ok(())
    }
    fn array_storage_bytes(&self) -> usize {
        self.coarse.array_storage_bytes()
            + (self.row_source.capacity()
                + self.row_q.capacity()
                + self.row_z.capacity()
                + self.temporary.capacity()
                + self.applied.capacity())
                * size_of::<f64>()
    }
}

/// Zero-fill incomplete Cholesky of the active seven-point pressure matrix.
/// Enclosed components use their lowest linear cell as a factor-only gauge pin;
/// the physical operator and zero-mean Krylov projection remain unchanged.
struct Ic0Factor {
    rows: FlatRows,
    diagonal: Vec<f64>,
    upper: FlatRows,
    pins: Vec<bool>,
    active_to_full: Vec<usize>,
}

/// Compressed sparse rows over active indices; built once per projection so
/// the triangular solves stream contiguous memory.
struct FlatRows {
    start: Vec<u32>,
    entries: Vec<(u32, f64)>,
}

impl FlatRows {
    fn from_nested(nested: Vec<Vec<(usize, f64)>>) -> Self {
        let mut start = Vec::with_capacity(nested.len() + 1);
        let mut entries = Vec::with_capacity(nested.iter().map(Vec::len).sum());
        start.push(0);
        for row in nested {
            entries.extend(row.into_iter().map(|(j, l)| (j as u32, l)));
            start.push(entries.len() as u32);
        }
        Self { start, entries }
    }

    fn row(&self, i: usize) -> &[(u32, f64)] {
        &self.entries[self.start[i] as usize..self.start[i + 1] as usize]
    }

    fn allocated_bytes(&self) -> usize {
        std::mem::size_of::<u32>() * self.start.capacity()
            + std::mem::size_of::<(u32, f64)>() * self.entries.capacity()
    }
}

/// Assembled seven-point pressure operator in CSR form. Coefficients match
/// `apply_pressure_matrix`, which remains the reference implementation.
struct PressureOperator {
    rows: Vec<usize>,
    diagonal: Vec<f64>,
    start: Vec<u32>,
    entries: Vec<(u32, f64)>,
}

impl PressureOperator {
    fn build(grid: &MacGridWorld, liquid: &[bool], active: &[usize], diag: &[f64]) -> Self {
        let [nx, ny, nz] = grid.dims();
        let h2 = grid.config.cell_size_m.powi(2);
        let plane = nx * ny;
        let mut start = Vec::with_capacity(active.len() + 1);
        let mut entries = Vec::with_capacity(active.len() * 6);
        let mut diagonal = Vec::with_capacity(active.len());
        start.push(0);
        for &i in active {
            let ix = i % nx;
            let iy = (i / nx) % ny;
            let iz = i / plane;
            diagonal.push(diag[i]);
            for (present, j) in [
                (ix > 0, i.wrapping_sub(1)),
                (ix + 1 < nx, i + 1),
                (iy > 0, i.wrapping_sub(nx)),
                (iy + 1 < ny, i + nx),
                (iz > 0, i.wrapping_sub(plane)),
                (iz + 1 < nz, i + plane),
            ] {
                if present && liquid[j] {
                    entries.push((j as u32, grid.relative_inverse_face_density(i, j) / h2));
                }
            }
            start.push(entries.len() as u32);
        }
        Self {
            rows: active.to_vec(),
            diagonal,
            start,
            entries,
        }
    }

    fn apply(&self, x: &[f64], out: &mut [f64]) {
        for (k, &i) in self.rows.iter().enumerate() {
            let mut value = self.diagonal[k] * x[i];
            for &(j, c) in &self.entries[self.start[k] as usize..self.start[k + 1] as usize] {
                value -= c * x[j as usize];
            }
            out[i] = value;
        }
    }
}

impl Ic0Factor {
    fn allocated_bytes(&self) -> usize {
        self.rows.allocated_bytes()
            + self.upper.allocated_bytes()
            + std::mem::size_of::<f64>() * self.diagonal.capacity()
            + std::mem::size_of::<bool>() * self.pins.capacity()
            + std::mem::size_of::<usize>() * self.active_to_full.capacity()
    }
    fn build(
        grid: &MacGridWorld,
        active: &[usize],
        components: &[Option<usize>],
        anchored: &[bool],
        matrix_diagonal: &[f64],
        h2: f64,
    ) -> Result<Self, MacError> {
        Self::build_with(
            grid,
            active,
            components,
            anchored,
            matrix_diagonal,
            h2,
            None,
        )
    }

    fn build_modified(
        grid: &MacGridWorld,
        active: &[usize],
        components: &[Option<usize>],
        anchored: &[bool],
        matrix_diagonal: &[f64],
        h2: f64,
    ) -> Result<Self, MacError> {
        Self::build_with(
            grid,
            active,
            components,
            anchored,
            matrix_diagonal,
            h2,
            Some(0.97),
        )
    }

    /// `modification`: None is plain IC(0) with no pivot fallback; Some(tau)
    /// is MIC(0), subtracting tau times each row's dropped fill from its
    /// pivot so the factor preserves row sums on smooth modes.
    fn build_with(
        grid: &MacGridWorld,
        active: &[usize],
        components: &[Option<usize>],
        anchored: &[bool],
        matrix_diagonal: &[f64],
        h2: f64,
        modification: Option<f64>,
    ) -> Result<Self, MacError> {
        let [nx, ny, nz] = grid.dims();
        let mut full_to_active = vec![usize::MAX; grid.fraction.len()];
        for (k, &full) in active.iter().enumerate() {
            full_to_active[full] = k;
        }
        let mut pins = vec![false; active.len()];
        let mut component_seen = vec![false; anchored.len()];
        for (k, &full) in active.iter().enumerate() {
            if let Some(c) = components[full] {
                if !anchored[c] && !component_seen[c] {
                    pins[k] = true;
                }
                component_seen[c] = true;
            }
        }
        let mut rows: Vec<Vec<(usize, f64)>> = vec![Vec::new(); active.len()];
        let mut diagonal = vec![0.0; active.len()];
        let mut upper = vec![Vec::new(); active.len()];
        for (i, &full) in active.iter().enumerate() {
            if pins[i] {
                diagonal[i] = 1.0;
                continue;
            }
            let x = full % nx;
            let y = (full / nx) % ny;
            let z = full / (nx * ny);
            let mut prior = Vec::with_capacity(3);
            if x > 0 {
                prior.push(full - 1);
            }
            if y > 0 {
                prior.push(full - nx);
            }
            if z > 0 {
                prior.push(full - nx * ny);
            }
            prior.sort_unstable();
            let mut row_sum = 0.0;
            let mut dropped_fill = 0.0;
            for neighbor in prior {
                let j = full_to_active[neighbor];
                if j == usize::MAX || pins[j] || components[full] != components[neighbor] {
                    continue;
                }
                let mut common = 0.0;
                let mut a = 0;
                let mut b = 0;
                while a < rows[i].len() && b < rows[j].len() {
                    match rows[i][a].0.cmp(&rows[j][b].0) {
                        std::cmp::Ordering::Less => a += 1,
                        std::cmp::Ordering::Greater => b += 1,
                        std::cmp::Ordering::Equal => {
                            common += rows[i][a].1 * rows[j][b].1;
                            a += 1;
                            b += 1;
                        }
                    }
                }
                let lij = (-grid.relative_inverse_face_density(full, neighbor) / h2 - common)
                    / diagonal[j];
                row_sum += lij * lij;
                rows[i].push((j, lij));
                upper[j].push((i, lij));
                if modification.is_some() {
                    // Eliminating j couples i with j's other forward
                    // neighbors k; IC(0) drops that (i, k) fill entry.
                    let (jx, jy, jz) = (neighbor % nx, (neighbor / nx) % ny, neighbor / (nx * ny));
                    for (in_bounds, k) in [
                        (jx + 1 < nx, neighbor + 1),
                        (jy + 1 < ny, neighbor + nx),
                        (jz + 1 < nz, neighbor + nx * ny),
                    ] {
                        if !in_bounds || k == full {
                            continue;
                        }
                        let kk = full_to_active[k];
                        if kk == usize::MAX || pins[kk] || components[k] != components[neighbor] {
                            continue;
                        }
                        let lkj =
                            -grid.relative_inverse_face_density(neighbor, k) / h2 / diagonal[j];
                        dropped_fill += lij * lkj;
                    }
                }
            }
            let mut pivot = matrix_diagonal[full] - row_sum;
            if let Some(tau) = modification {
                pivot -= tau * dropped_fill;
                if pivot < 0.25 * matrix_diagonal[full] {
                    pivot = matrix_diagonal[full];
                }
            }
            if !pivot.is_finite() || pivot <= matrix_diagonal[full].abs().max(1.0) * 1.0e-14 {
                return Err(MacError::Ic0NonPositivePivot { cell: full, pivot });
            }
            diagonal[i] = pivot.sqrt();
        }
        Ok(Self {
            rows: FlatRows::from_nested(rows),
            diagonal,
            upper: FlatRows::from_nested(upper),
            pins,
            active_to_full: active.to_vec(),
        })
    }

    fn apply(&self, source: &[f64], target: &mut [f64], active: &[usize], work: &mut [f64]) {
        // Forward solve L y = r, then back-solve L^T z = y in place: row i's
        // y is consumed before z_i overwrites it, and z_j (j > i) is final.
        for i in 0..self.diagonal.len() {
            if self.pins[i] {
                work[i] = 0.0;
                continue;
            }
            let mut value = source[self.active_to_full[i]];
            for &(j, l) in self.rows.row(i) {
                value -= l * work[j as usize];
            }
            work[i] = value / self.diagonal[i];
        }
        for i in (0..self.diagonal.len()).rev() {
            if self.pins[i] {
                work[i] = 0.0;
                continue;
            }
            let mut value = work[i];
            for &(j, l) in self.upper.row(i) {
                value -= l * work[j as usize];
            }
            work[i] = value / self.diagonal[i];
        }
        for (k, &full) in self.active_to_full.iter().enumerate() {
            target[full] = if work[k].is_finite() { work[k] } else { 0.0 };
        }
        debug_assert_eq!(active.len(), self.active_to_full.len());
    }
}

/// One level of the multigrid hierarchy on a dense box. `coupling[a][i]` is
/// the positive off-diagonal magnitude between cell i and its +axis-a
/// neighbor; zero means no coupling (inactive, solid, or disconnected).
struct MgLevel {
    dims: [usize; 3],
    active: Vec<usize>,
    diag: Vec<f64>,
    coupling: [Vec<f64>; 3],
    x: Vec<f64>,
    b: Vec<f64>,
    r: Vec<f64>,
    /// Per active row (same order as `active`): inverse diagonal, stencil
    /// range, and coarse parent. Built once so sweeps avoid index math.
    inverse_diag: Vec<f64>,
    start: Vec<u32>,
    stencil: Vec<(u32, f64)>,
    parents: Vec<u32>,
}

impl MgLevel {
    fn new(dims: [usize; 3], active: Vec<usize>, diag: Vec<f64>, coupling: [Vec<f64>; 3]) -> Self {
        let n = dims[0] * dims[1] * dims[2];
        let mut level = Self {
            dims,
            active,
            diag,
            coupling,
            x: vec![0.0; n],
            b: vec![0.0; n],
            r: vec![0.0; n],
            inverse_diag: Vec::new(),
            start: Vec::new(),
            stencil: Vec::new(),
            parents: Vec::new(),
        };
        let [nx, ny, nz] = dims;
        let strides = [1, nx, nx * ny];
        let extents = [nx, ny, nz];
        let coarse = [nx.div_ceil(2), ny.div_ceil(2)];
        level.start.push(0);
        for &i in &level.active {
            let coords = [i % nx, (i / nx) % ny, i / (nx * ny)];
            for axis in 0..3 {
                if coords[axis] + 1 < extents[axis] && level.coupling[axis][i] != 0.0 {
                    level
                        .stencil
                        .push(((i + strides[axis]) as u32, level.coupling[axis][i]));
                }
                if coords[axis] > 0 {
                    let j = i - strides[axis];
                    if level.coupling[axis][j] != 0.0 {
                        level.stencil.push((j as u32, level.coupling[axis][j]));
                    }
                }
            }
            level.start.push(level.stencil.len() as u32);
            level.inverse_diag.push(1.0 / level.diag[i]);
            level.parents.push(
                (coords[0] / 2 + coarse[0] * (coords[1] / 2 + coarse[1] * (coords[2] / 2))) as u32,
            );
        }
        level
    }

    fn row_sum(&self, x: &[f64], k: usize) -> f64 {
        let mut sum = 0.0;
        for &(j, c) in &self.stencil[self.start[k] as usize..self.start[k + 1] as usize] {
            sum += c * x[j as usize];
        }
        sum
    }

    #[cfg(test)]
    fn neighbor_sum(&self, x: &[f64], i: usize) -> f64 {
        let [nx, ny, nz] = self.dims;
        let strides = [1, nx, nx * ny];
        let coords = [i % nx, (i / nx) % ny, i / (nx * ny)];
        let extents = [nx, ny, nz];
        let mut sum = 0.0;
        for axis in 0..3 {
            if coords[axis] + 1 < extents[axis] {
                sum += self.coupling[axis][i] * x[i + strides[axis]];
            }
            if coords[axis] > 0 {
                let j = i - strides[axis];
                sum += self.coupling[axis][j] * x[j];
            }
        }
        sum
    }

    fn gauss_seidel(&mut self, forward: bool) {
        let count = self.active.len();
        for step in 0..count {
            let k = if forward { step } else { count - 1 - step };
            let i = self.active[k];
            let sum = self.row_sum(&self.x, k);
            self.x[i] = (self.b[i] + sum) * self.inverse_diag[k];
        }
    }

    fn residual(&mut self) {
        for k in 0..self.active.len() {
            let i = self.active[k];
            let sum = self.row_sum(&self.x, k);
            self.r[i] = self.b[i] - (self.diag[i] * self.x[i] - sum);
        }
    }

    fn parent_index(&self, coarse_dims: [usize; 3], i: usize) -> usize {
        let [nx, ny, _] = self.dims;
        let (x, y, z) = (i % nx, (i / nx) % ny, i / (nx * ny));
        x / 2 + coarse_dims[0] * (y / 2 + coarse_dims[1] * (z / 2))
    }

    /// Galerkin P^T A P for piecewise-constant 2x2x2 aggregation.
    fn coarsen(&self) -> Self {
        let [nx, ny, nz] = self.dims;
        let dims = [nx.div_ceil(2), ny.div_ceil(2), nz.div_ceil(2)];
        let n = dims[0] * dims[1] * dims[2];
        let strides = [1, nx, nx * ny];
        let extents = [nx, ny, nz];
        let mut is_active = vec![false; n];
        let mut diag = vec![0.0; n];
        let mut coupling = [vec![0.0; n], vec![0.0; n], vec![0.0; n]];
        for &i in &self.active {
            let pi = self.parent_index(dims, i);
            is_active[pi] = true;
            diag[pi] += self.diag[i];
            let coords = [i % nx, (i / nx) % ny, i / (nx * ny)];
            for axis in 0..3 {
                let c = self.coupling[axis][i];
                if c == 0.0 || coords[axis] + 1 >= extents[axis] {
                    continue;
                }
                let pj = self.parent_index(dims, i + strides[axis]);
                if pj == pi {
                    // Both orderings of an internal pair cancel diagonal mass.
                    diag[pi] -= 2.0 * c;
                } else {
                    coupling[axis][pi] += c;
                }
            }
        }
        let active: Vec<usize> = (0..n).filter(|&i| is_active[i]).collect();
        Self::new(dims, active, diag, coupling)
    }
}

/// Symmetric V-cycle preconditioner. Forward Gauss-Seidel before and
/// backward after each coarse correction, with restriction equal to the
/// transpose of prolongation, keeps the operator symmetric for CG.
/// Enclosed all-Neumann regions stay singular; CG projects their means.
struct Multigrid {
    levels: Vec<MgLevel>,
}

impl Multigrid {
    const SMOOTHING_SWEEPS: usize = 1;
    const COARSEST_SWEEPS: usize = 16;
    const COARSEST_CELLS: usize = 64;

    fn build(grid: &MacGridWorld, liquid: &[bool], diag: &[f64]) -> Self {
        let dims = grid.dims();
        let [nx, ny, nz] = dims;
        let n = nx * ny * nz;
        let h2 = grid.config.cell_size_m.powi(2);
        let strides = [1, nx, nx * ny];
        let extents = [nx, ny, nz];
        let mut coupling = [vec![0.0; n], vec![0.0; n], vec![0.0; n]];
        let active: Vec<usize> = (0..n).filter(|&i| liquid[i]).collect();
        for &i in &active {
            let coords = [i % nx, (i / nx) % ny, i / (nx * ny)];
            for axis in 0..3 {
                if coords[axis] + 1 < extents[axis] && liquid[i + strides[axis]] {
                    coupling[axis][i] =
                        grid.relative_inverse_face_density(i, i + strides[axis]) / h2;
                }
            }
        }
        let mut levels = vec![MgLevel::new(dims, active, diag.to_vec(), coupling)];
        loop {
            let last = levels.last().unwrap();
            if last.active.len() <= Self::COARSEST_CELLS || last.dims.iter().all(|&d| d <= 2) {
                break;
            }
            let next = last.coarsen();
            levels.push(next);
        }
        Self { levels }
    }

    fn apply(&mut self, source: &[f64], target: &mut [f64]) {
        let fine = &mut self.levels[0];
        for k in 0..fine.active.len() {
            let i = fine.active[k];
            fine.b[i] = source[i];
        }
        self.cycle(0);
        let fine = &self.levels[0];
        for &i in &fine.active {
            target[i] = fine.x[i];
        }
    }

    fn cycle(&mut self, level: usize) {
        {
            let current = &mut self.levels[level];
            for k in 0..current.active.len() {
                let i = current.active[k];
                current.x[i] = 0.0;
            }
        }
        if level + 1 == self.levels.len() {
            let current = &mut self.levels[level];
            for _ in 0..Self::COARSEST_SWEEPS {
                current.gauss_seidel(true);
                current.gauss_seidel(false);
            }
            return;
        }
        {
            let (fine_levels, coarse_levels) = self.levels.split_at_mut(level + 1);
            let fine = &mut fine_levels[level];
            let coarse = &mut coarse_levels[0];
            for _ in 0..Self::SMOOTHING_SWEEPS {
                fine.gauss_seidel(true);
            }
            fine.residual();
            for k in 0..coarse.active.len() {
                let i = coarse.active[k];
                coarse.b[i] = 0.0;
            }
            for (k, &i) in fine.active.iter().enumerate() {
                coarse.b[fine.parents[k] as usize] += fine.r[i];
            }
        }
        self.cycle(level + 1);
        let (fine_levels, coarse_levels) = self.levels.split_at_mut(level + 1);
        let fine = &mut fine_levels[level];
        let coarse = &coarse_levels[0];
        for k in 0..fine.active.len() {
            let i = fine.active[k];
            fine.x[i] += coarse.x[fine.parents[k] as usize];
        }
        for _ in 0..Self::SMOOTHING_SWEEPS {
            fine.gauss_seidel(false);
        }
    }
}

fn minmod(a: f64, b: f64) -> f64 {
    if a * b <= 0.0 {
        0.0
    } else {
        a.signum() * a.abs().min(b.abs())
    }
}
fn dot_indices(a: &[f64], b: &[f64], indices: &[usize]) -> f64 {
    indices.iter().map(|&i| a[i] * b[i]).sum()
}
fn l2_norm_indices(values: &[f64], indices: &[usize]) -> f64 {
    dot_indices(values, values, indices).sqrt()
}
fn zero_nonliquid(v: &mut [f64], liquid: &[bool]) {
    for (x, yes) in v.iter_mut().zip(liquid) {
        if !yes {
            *x = 0.0;
        }
    }
}
fn project_component_means(
    v: &mut [f64],
    labels: &[Option<usize>],
    anchored: &[bool],
    count: usize,
    active_indices: &[usize],
    sums: &mut [f64],
    component_sizes: &mut [usize],
) {
    if anchored.iter().all(|is_anchored| *is_anchored) {
        return;
    }
    debug_assert_eq!(sums.len(), count);
    debug_assert_eq!(component_sizes.len(), count);
    sums.fill(0.0);
    component_sizes.fill(0);
    for &i in active_indices {
        if let Some(c) = labels[i]
            && !anchored[c]
        {
            sums[c] += v[i];
            component_sizes[c] += 1;
        }
    }
    for &i in active_indices {
        if let Some(c) = labels[i]
            && !anchored[c]
            && component_sizes[c] > 0
        {
            v[i] -= sums[c] / component_sizes[c] as f64;
        }
    }
}
fn apply_pressure_matrix(
    grid: &MacGridWorld,
    x: &[f64],
    liquid: &[bool],
    liquid_indices: &[usize],
    diag: &[f64],
    out: &mut [f64],
) {
    let [nx, ny, nz] = grid.dims();
    let h2 = grid.config.cell_size_m.powi(2);
    let plane = nx * ny;
    for &i in liquid_indices {
        let ix = i % nx;
        let iy = (i / nx) % ny;
        let iz = i / plane;
        let mut value = diag[i] * x[i];
        if ix > 0 && liquid[i - 1] {
            value -= grid.relative_inverse_face_density(i, i - 1) * x[i - 1] / h2;
        }
        if ix + 1 < nx && liquid[i + 1] {
            value -= grid.relative_inverse_face_density(i, i + 1) * x[i + 1] / h2;
        }
        if iy > 0 && liquid[i - nx] {
            value -= grid.relative_inverse_face_density(i, i - nx) * x[i - nx] / h2;
        }
        if iy + 1 < ny && liquid[i + nx] {
            value -= grid.relative_inverse_face_density(i, i + nx) * x[i + nx] / h2;
        }
        if iz > 0 && liquid[i - plane] {
            value -= grid.relative_inverse_face_density(i, i - plane) * x[i - plane] / h2;
        }
        if iz + 1 < nz && liquid[i + plane] {
            value -= grid.relative_inverse_face_density(i, i + plane) * x[i + plane] / h2;
        }
        out[i] = value;
    }
}

fn sample_component(values: &[f64], dims: [usize; 3], p: [f64; 3]) -> f64 {
    let mut base = [0usize; 3];
    let mut t = [0.0; 3];
    for a in 0..3 {
        let q = p[a].clamp(0.0, dims[a].saturating_sub(1) as f64);
        base[a] = q.floor() as usize;
        t[a] = q - base[a] as f64;
    }
    let mut sum = 0.0;
    for dz in 0..=1 {
        for dy in 0..=1 {
            for dx in 0..=1 {
                let ix = (base[0] + dx).min(dims[0] - 1);
                let iy = (base[1] + dy).min(dims[1] - 1);
                let iz = (base[2] + dz).min(dims[2] - 1);
                let i = ix + dims[0] * (iy + dims[1] * iz);
                sum += values[i]
                    * (if dx == 0 { 1.0 - t[0] } else { t[0] })
                    * (if dy == 0 { 1.0 - t[1] } else { t[1] })
                    * (if dz == 0 { 1.0 - t[2] } else { t[2] });
            }
        }
    }
    sum
}
fn sample_velocity(u: &[f64], v: &[f64], w: &[f64], dims: [usize; 3], p: [f64; 3]) -> [f64; 3] {
    [
        sample_component(
            u,
            [dims[0] + 1, dims[1], dims[2]],
            [p[0], p[1] - 0.5, p[2] - 0.5],
        ),
        sample_component(
            v,
            [dims[0], dims[1] + 1, dims[2]],
            [p[0] - 0.5, p[1], p[2] - 0.5],
        ),
        sample_component(
            w,
            [dims[0], dims[1], dims[2] + 1],
            [p[0] - 0.5, p[1] - 0.5, p[2]],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{BrickCoord, CellSizeCode, MaterialId, Revision, VolumeId};
    use spall_voxel::{Brick, Volume};

    #[test]
    fn strict_phase_transfers_preserve_saturated_cycles_and_signed_boundary_accounting() {
        let old = [1.0, 1.0, 1.0];
        let edges = [
            (Some(0), Some(1), 0.0, false),
            (Some(1), Some(2), 0.0, false),
            (Some(0), Some(2), 0.0, false),
        ];
        let mut transfers = [0.1, 0.1, -0.1];
        let mut incoming = [0.0; 3];
        let mut outgoing = [0.0; 3];
        let mut candidate = old;
        strict_transfer_bounds(
            &old,
            &edges,
            &mut transfers,
            &[0, 1, 2],
            &mut incoming,
            &mut outgoing,
            &mut candidate,
        )
        .unwrap();
        assert_eq!(transfers, [0.1, 0.1, -0.1]);
        assert_eq!(candidate, old);

        // Negative orientation: water exits the left boundary from cell 0,
        // while a full neighbour receives a limited, paired internal transfer.
        let old = [0.2, 1.0, 0.0];
        let edges = [(None, Some(0), 0.0, true), (Some(0), Some(1), 0.0, false)];
        let mut transfers = [-0.1, 0.1];
        candidate = old;
        strict_transfer_bounds(
            &old,
            &edges,
            &mut transfers,
            &[0, 1],
            &mut incoming,
            &mut outgoing,
            &mut candidate,
        )
        .unwrap();
        assert_eq!(transfers[1], 0.0);
        assert!(candidate.iter().all(|v| (0.0..=1.0).contains(v)));
        assert!(
            (candidate.iter().sum::<f64>() - old.iter().sum::<f64>() - transfers[0]).abs() < 1e-15
        );
    }

    #[test]
    fn strict_phase_transfers_resolve_both_saturated_chain_orientations() {
        let old = [1.0; 108];
        let edges: Vec<_> = (0..107)
            .map(|i| (Some(i), Some(i + 1), 0.0, false))
            .collect();
        let mut incoming = [0.0; 108];
        let mut outgoing = [0.0; 108];
        let mut candidate = old;
        for flux in [0.1, -0.1] {
            let mut transfers = vec![flux; edges.len()];
            strict_transfer_bounds(
                &old,
                &edges,
                &mut transfers,
                &(0..108).collect::<Vec<_>>(),
                &mut incoming,
                &mut outgoing,
                &mut candidate,
            )
            .unwrap();
            assert_eq!(candidate, old);
            assert!(transfers.iter().all(|&f| f == 0.0));
        }
    }

    #[test]
    fn strict_phase_transfers_keep_throughflow_when_full_cell_fluxes_differ_by_ulps() {
        let old = [0.5, 1.0, 1.0, 1.0, 0.5];
        let edges: Vec<_> = (0..4).map(|i| (Some(i), Some(i + 1), 0.0, false)).collect();
        let mut incoming = [0.0; 5];
        let mut outgoing = [0.0; 5];
        let mut candidate = old;
        for sign in [1.0, -1.0] {
            let mut transfers = vec![
                0.01 + 3.0 * f64::EPSILON,
                0.01 + 2.0 * f64::EPSILON,
                0.01 + f64::EPSILON,
                0.01,
            ];
            if sign < 0.0 {
                transfers.reverse();
            }
            transfers.iter_mut().for_each(|f| *f *= sign);
            strict_transfer_bounds(
                &old,
                &edges,
                &mut transfers,
                &[0, 1, 2, 3, 4],
                &mut incoming,
                &mut outgoing,
                &mut candidate,
            )
            .unwrap();
            assert!(candidate.iter().all(|v| (0.0..=1.0).contains(v)));
            assert!((candidate.iter().sum::<f64>() - old.iter().sum::<f64>()).abs() < 1e-14);
            assert!(
                transfers.iter().all(|f| (f.abs() - 0.01).abs() < 1e-12),
                "roundoff repair destroyed throughflow: {transfers:?}"
            );
        }
    }

    #[test]
    fn strict_phase_transfers_repair_a_path_longer_than_the_local_sweep_budget() {
        let old = [1.0; 512];
        // Two interleaved parity groups prevent either traversal from resolving
        // the entire directed path in one sweep. The global path repair must
        // return water through the connected chain without extra local passes.
        let edges: Vec<_> = (0..511)
            .step_by(2)
            .chain((1..511).step_by(2))
            .map(|i| (Some(i), Some(i + 1), 0.0, false))
            .collect();
        let mut transfers = vec![0.1; edges.len()];
        let mut incoming = [0.0; 512];
        let mut outgoing = [0.0; 512];
        let mut candidate = old;
        let metrics = strict_transfer_bounds(
            &old,
            &edges,
            &mut transfers,
            &(0..512).collect::<Vec<_>>(),
            &mut incoming,
            &mut outgoing,
            &mut candidate,
        )
        .unwrap();
        assert!(
            metrics.path_repairs > 0 && metrics.path_repairs <= (old.len() + edges.len()) as u64
        );
        assert_eq!(candidate, old);
        assert!(transfers.iter().all(|&f| f.abs() < 1e-14));
        assert!(metrics.scratch_bytes > 0);
    }

    #[test]
    fn strict_path_repair_rejects_a_correction_that_cannot_change_a_face() {
        let flux = 0.4_f64;
        let old = [flux.next_up() - flux - 1e-25, 0.0, 0.5];
        let edges = [
            (Some(2), Some(0), 0.0, false),
            (Some(0), Some(1), 0.0, false),
        ];
        let initial = [flux, flux.next_up()];
        let mut transfers = initial;
        let mut incoming = [0.0; 3];
        let mut outgoing = [0.0; 3];
        let mut candidate = old;
        // The amount deficit is smaller than either transfer's ulp. Do not
        // repeatedly "repair" unchanged faces or clip the cell fraction.
        assert!(matches!(
            repair_strict_transfer_paths(
                &old,
                &edges,
                &mut transfers,
                &[0, 1, 2],
                &mut incoming,
                &mut outgoing,
                &mut candidate
            ),
            Err(MacError::TransportBoundsViolation { .. })
        ));
        assert_eq!(transfers, initial);
        assert!(candidate[0] < 0.0);
    }

    #[test]
    fn strict_path_repair_refunds_open_outflow_in_both_face_orientations() {
        for (l, r, flux) in [(Some(0), None, 0.2), (None, Some(0), -0.2)] {
            let old = [0.1];
            let edges = [(l, r, 0.0, true)];
            let mut transfers = [flux];
            let mut incoming = [0.0];
            let mut outgoing = [0.0];
            let mut candidate = old;
            let metrics = repair_strict_transfer_paths(
                &old,
                &edges,
                &mut transfers,
                &[0],
                &mut incoming,
                &mut outgoing,
                &mut candidate,
            )
            .unwrap();
            assert_eq!(metrics.path_repairs, 1);
            assert_eq!(candidate, [0.0]);
            assert!((candidate[0] + transfers[0].abs() - old[0]).abs() < 1e-15);
        }
    }

    #[test]
    fn strict_path_repair_rejects_when_its_finite_augmentation_budget_is_exhausted() {
        let old: Vec<_> = (0..65).flat_map(|_| [0.5, 1.0]).collect();
        let edges: Vec<_> = (0..65)
            .map(|i| (Some(2 * i), Some(2 * i + 1), 0.0, false))
            .collect();
        let mut transfers = vec![0.1; 65];
        let mut incoming = vec![0.0; old.len()];
        let mut outgoing = vec![0.0; old.len()];
        let mut candidate = old.clone();
        let touched: Vec<_> = (0..old.len()).collect();
        assert!(matches!(
            repair_strict_transfer_paths(
                &old,
                &edges,
                &mut transfers,
                &touched,
                &mut incoming,
                &mut outgoing,
                &mut candidate
            ),
            Err(MacError::TransportBoundsViolation { .. })
        ));
        assert!(candidate.iter().any(|&f| f > 1.0));
        assert_eq!(transfers[64], 0.1);
    }

    #[test]
    fn geometric_plane_matches_analytic_volumes_and_translates_sharp_interfaces() {
        assert!((plane_cube_fraction([1.0, 1.0, 1.0], 1.0) - 1.0 / 6.0).abs() < 1e-14);
        assert!((plane_cube_fraction([1.0, 1.0, 0.0], 1.0) - 0.5).abs() < 1e-14);
        for normal in [
            [1.0, 0.0, 0.0],
            [0.0, -1.0, 0.0],
            [1.0, 2.0, 3.0],
            [1.0, 1e-8, 1e-11],
        ] {
            for fraction in [0.001, 0.1, 0.4, 0.9, 0.999] {
                let p = InterfacePlane::from_fraction(normal, fraction);
                assert!((plane_cube_fraction(p.normal, p.alpha) - fraction).abs() < 1e-10);
            }
        }
        let p = InterfacePlane::from_fraction([0.0, 1.0, 0.0], 0.4);
        assert_eq!(
            p.swept_volume(1, 0.1),
            0.0,
            "air at the upper face must not export water"
        );
        assert!((p.swept_volume(1, -0.1) + 0.1).abs() < 1e-12);
        assert!((p.swept_volume(0, 0.1) - 0.04).abs() < 1e-12);
    }

    #[test]
    fn two_phase_partial_depth_equilibrium_survives_thirty_seconds() {
        for top_fraction in [0.2, 0.8] {
            let mut grid = all_air_grid([4, 6, 3], MacConfig::default());
            grid.set_ambient_density(1.2).unwrap();
            grid.set_pressure_preconditioner(PressurePreconditioner::Ic0);
            for z in 0..3 {
                for x in 0..4 {
                    grid.set_fraction(GlobalCell::new(x, 0, z), 1.0).unwrap();
                    grid.set_fraction(GlobalCell::new(x, 1, z), top_fraction)
                        .unwrap();
                }
            }
            let initial = grid.fraction.clone();
            let mass = grid.water_volume_m3();
            for _ in 0..1800 {
                let step = grid.step(1.0 / 60.0).unwrap();
                assert_eq!(step.pressure_converged_substeps, step.substeps);
                assert!(step.conservation_error_m3 < 1e-12);
            }
            assert!((grid.water_volume_m3() - mass).abs() < 1e-10);
            assert!(grid.cumulative_open_outflow_m3() < 1e-12);
            assert!(grid.kinetic_energy_j() < 1e-8);
            assert!(
                grid.fraction
                    .iter()
                    .zip(initial)
                    .all(|(a, b)| (a - b).abs() < 1e-6)
            );
        }
    }

    #[test]
    fn two_phase_pressure_matches_density_weighted_hydrostatic_column() {
        let mut grid = all_air_grid([1, 4, 1], MacConfig::default());
        grid.set_ambient_density(1.2).unwrap();
        grid.set_fraction(GlobalCell::new(0, 0, 0), 1.0).unwrap();
        grid.set_fraction(GlobalCell::new(0, 1, 0), 0.3).unwrap();
        grid.step(1.0 / 60.0).unwrap();
        let rho = [1000.0, 1.2 + 0.3 * (1000.0 - 1.2), 1.2, 1.2];
        let mut expected = 0.5 * 0.25 * 9.81 * rho[3];
        assert!((grid.pressure_pa[3] - expected).abs() < 1e-6);
        for y in (0..3).rev() {
            expected += 0.25 * 9.81 * 0.5 * (rho[y] + rho[y + 1]);
            assert!((grid.pressure_pa[y] - expected).abs() < 1e-6);
        }
        assert!(grid.max_face_component_velocity_m_s() < 1e-8);
    }

    #[test]
    fn two_phase_variable_density_operator_is_symmetric_and_ic0_converges() {
        let mut grid = all_air_grid([3, 1, 1], quiet_config(false));
        grid.set_ambient_density(1.2).unwrap();
        grid.set_pressure_preconditioner(PressurePreconditioner::Ic0);
        grid.fraction.copy_from_slice(&[1.0, 0.3, 0.0]);
        let c01 = grid.relative_inverse_face_density(0, 1) / 0.25_f64.powi(2);
        let c12 = grid.relative_inverse_face_density(1, 2) / 0.25_f64.powi(2);
        let diag = [c01, c01 + c12, c12];
        let x = [1.0, 0.0, -1.0];
        let y = [1.0, -2.0, 1.0];
        let mut ax = [0.0; 3];
        let mut ay = [0.0; 3];
        apply_pressure_matrix(&grid, &x, &[true; 3], &[0, 1, 2], &diag, &mut ax);
        apply_pressure_matrix(&grid, &y, &[true; 3], &[0, 1, 2], &diag, &mut ay);
        assert!(
            (dot_indices(&x, &ay, &[0, 1, 2]) - dot_indices(&y, &ax, &[0, 1, 2])).abs() < 1e-12
        );
        assert!(dot_indices(&x, &ax, &[0, 1, 2]) > 0.0);
        let face = grid.u_index(1, 0, 0);
        grid.u[face] = 0.5;
        let projection = grid.project(1.0 / 60.0).unwrap();
        assert!(projection.converged);
        assert!(projection.divergence_after < 1e-8);
    }

    #[test]
    fn two_phase_canal_breach_closure_and_tunnel_preserve_gameplay_invariants() {
        for (scenario, preconditioner) in ["canal", "breach", "closure", "tunnel"]
            .into_iter()
            .flat_map(|s| {
                [
                    (s, PressurePreconditioner::Ic0),
                    (s, PressurePreconditioner::Multigrid),
                ]
            })
        {
            let mut fixture = if scenario == "tunnel" {
                GridReservoirFixture::new_tunnel(1).unwrap()
            } else {
                GridReservoirFixture::new(1, true).unwrap()
            };
            fixture.grid.set_ambient_density(1.2).unwrap();
            fixture.grid.set_pressure_preconditioner(preconditioner);
            let initial = fixture.grid.water_volume_m3();
            let downstream = fixture.downstream_volume_m3();
            let level = fixture.p95_level_difference_m();
            let pool = fixture.upper_pool_volume_m3();
            match scenario {
                "canal" => fixture.excavate_canal().unwrap(),
                "breach" => fixture.breach_dam().unwrap(),
                "closure" => {
                    fixture.excavate_canal().unwrap();
                    fixture.close_canal().unwrap();
                }
                _ => {}
            }
            let mut momentum: f64 = 0.0;
            for _ in 0..120 {
                let step = fixture.step(1.0 / 60.0).unwrap();
                assert_eq!(step.pressure_converged_substeps, step.substeps);
                assert!(step.conservation_error_m3 < 1e-10);
                momentum = momentum.max(fixture.eastward_momentum_kg_m_s());
            }
            assert!(
                (initial
                    - fixture.grid.water_volume_m3()
                    - fixture.grid.cumulative_open_outflow_m3())
                .abs()
                    < 1e-9
            );
            if scenario == "canal" || scenario == "breach" {
                assert!(fixture.downstream_volume_m3() > downstream + 0.03);
                assert!(fixture.p95_level_difference_m() < level - 0.03);
                assert!(momentum > 0.01);
            } else if scenario == "closure" {
                assert!((fixture.downstream_volume_m3() - downstream).abs() < 1e-9);
            } else {
                assert_eq!(fixture.tunnel_roof_normal_speed_max_m_s(), 0.0);
                assert!((fixture.upper_pool_volume_m3() - pool).abs() < 1e-9);
            }
        }
    }

    /// Inverted bell (walls x=4,7 and roof y=10, cells) in a 12x16x2 open
    /// tank filled to y=14 (3.5 m). The 2x4x2-cell interior (1 m air column,
    /// 1.5-2.5 m) starts dry at atmospheric pressure. Returns the water
    /// volume inside the bell interior after each tick.
    fn diving_bell_run(air: Option<bool>, ticks: usize) -> (Vec<f64>, f64) {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [12, 16, 2], 4096).unwrap();
        let mut volume = Volume::new(VolumeId::new(901).unwrap(), CellSizeCode::Quarter);
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        let mut plan = spall_voxel::EditPlan::new(VolumeId::new(901).unwrap());
        for z in 0..2 {
            for y in 6..=10 {
                plan.set(GlobalCell::new(4, y, z), MaterialId(1));
                plan.set(GlobalCell::new(7, y, z), MaterialId(1));
            }
            for x in 4..=7 {
                plan.set(GlobalCell::new(x, 10, z), MaterialId(1));
            }
        }
        volume.apply_edit(&plan).unwrap();
        let boundary = SolidBoundary::capture(&volume, spec).unwrap();
        let mut grid = MacGridWorld::new(&boundary, MacConfig::default()).unwrap();
        grid.set_pressure_preconditioner(PressurePreconditioner::Multigrid);
        if let Some(compressible) = air {
            grid.set_ambient_density(1.2).unwrap();
            grid.set_compressible_enclosed_air(compressible).unwrap();
        } else {
            grid.set_freely_displaced_air().unwrap();
        }
        let bell = |x: usize, y: usize| (5..=6).contains(&x) && (6..=9).contains(&y);
        for z in 0..2 {
            for y in 0..14 {
                for x in 0..12 {
                    let i = grid.cell_index(x, y, z);
                    if !grid.solid[i] && !bell(x, y) {
                        grid.fraction[i] = 1.0;
                    }
                }
            }
        }
        let mass = grid.water_volume_m3();
        let inside = |grid: &MacGridWorld| {
            let mut total = 0.0;
            for z in 0..2 {
                for y in 6..=9 {
                    for x in 5..=6 {
                        total += grid.fraction[grid.cell_index(x, y, z)];
                    }
                }
            }
            total * grid.cell_volume()
        };
        let series = (0..ticks)
            .map(|_| {
                let step = grid.step(1.0 / 60.0).unwrap();
                assert_eq!(step.pressure_converged_substeps, step.substeps);
                inside(&grid)
            })
            .collect();
        (series, grid.water_volume_m3() - mass)
    }

    #[test]
    fn compressible_sealed_air_matches_isothermal_diving_bell_equilibrium() {
        // Solve P0 L = (P0 + rho g head) (L - d) with head =
        // (3.5 - d A_in/A_out) - (1.5 + d), A_in/A_out = 0.25/1.5.
        let (p0, rho_g, l) = (ATMOSPHERIC_PRESSURE_PA, 1000.0 * 9.81, 1.0);
        let mut d: f64 = 0.0;
        for _ in 0..50 {
            let head = (3.5 - d / 6.0) - (1.5 + d);
            d = l - p0 * l / (p0 + rho_g * head);
        }
        let expected_volume = d * 0.25;
        assert!((0.15..0.17).contains(&d), "analytic rise {d}");

        let late_mean = |series: &[f64]| {
            let tail = &series[series.len() / 2..];
            tail.iter().sum::<f64>() / tail.len() as f64
        };
        let (compressible, mass_change) = diving_bell_run(Some(true), 1200);
        assert!(mass_change.abs() < 1e-10);
        let measured = late_mean(&compressible);
        assert!(
            (measured / expected_volume - 1.0).abs() < 0.25,
            "measured {measured} m^3 vs isothermal {expected_volume} m^3"
        );
        let (rigid, _) = diving_bell_run(Some(false), 1200);
        assert!(
            late_mean(&rigid) < 0.1 * expected_volume,
            "incompressible air should keep water out"
        );
    }

    #[test]
    fn freely_displaced_air_fills_diving_bell_without_creating_water() {
        let (series, mass_change) = diving_bell_run(None, 600);
        let late = &series[series.len() / 2..];
        let mean = late.iter().sum::<f64>() / late.len() as f64;
        // Full 2x4x2 interior is 0.25 m3. Ignoring air pressure must let the
        // bell flood rather than retain a rigid or compressed gas pocket.
        assert!(mean > 0.225, "bell retained an air pocket: {mean}");
        assert!(
            mass_change.abs() < 1e-10,
            "water mass changed: {mass_change}"
        );
    }

    #[test]
    fn freely_displaced_air_uses_plic_surface_pressure_and_preserves_partial_water() {
        for top in [0.25, 0.75] {
            let mut grid = all_air_grid([1, 5, 1], MacConfig::default());
            grid.set_freely_displaced_air().unwrap();
            for y in 0..3 {
                grid.set_fraction(GlobalCell::new(0, y, 0), 1.0).unwrap();
            }
            grid.set_fraction(GlobalCell::new(0, 3, 0), top).unwrap();
            let volume = grid.water_volume_m3();
            let rows = if top > 0.5 { 4 } else { 3 };
            for _ in 0..20 {
                let step = grid.step(1.0 / 60.0).unwrap();
                assert_eq!(step.pressure_active_rows_total, rows);
                assert_eq!(step.pressure_converged_substeps, step.substeps);
                assert!((grid.water_volume_m3() - volume).abs() < 1e-12);
                assert!(
                    grid.max_face_speed_l1().0 < 1e-7,
                    "top={top}, v={:?}",
                    grid.v
                );
            }
            let surface_y = 3.0 + top;
            let sample_y = f64::from(rows as u32) - 0.5;
            let expected =
                grid.config.density_kg_m3 * 9.81 * grid.config.cell_size_m * (surface_y - sample_y);
            assert!((grid.pressure_pa[rows as usize - 1] - expected).abs() < 1e-5);
            assert!(grid.fraction[3] > 0.0, "air-centred water was discarded");
        }
    }

    #[test]
    fn atmospheric_surface_pressure_is_stable_with_nearly_saturated_neighbours() {
        for h in [0.5, 1.0] {
            for dt in [0.025, 0.05] {
                let mut grid = all_air_grid(
                    [12, 6, 2],
                    MacConfig {
                        cell_size_m: h,
                        ..MacConfig::default()
                    },
                );
                grid.set_pressure_preconditioner(PressurePreconditioner::Multigrid);
                grid.set_freely_displaced_air().unwrap();
                for z in 0..2 {
                    for x in 0..12 {
                        // Keep the actual tiny deficits. No snapping to one or
                        // liquid-volume adjustment is part of the correction.
                        let full = if (x + z) % 2 == 0 {
                            1.0
                        } else {
                            1.0 - f64::EPSILON
                        };
                        grid.set_fraction(GlobalCell::new(x, 0, z), full).unwrap();
                        grid.set_fraction(GlobalCell::new(x, 1, z), 0.25).unwrap();
                    }
                }
                let mass = grid.water_volume_m3();
                let expected_pressure = 1000.0 * 9.81 * h * 0.75;
                let (mut peak_speed, mut peak_pressure_error) = (0.0_f64, 0.0_f64);
                for _ in 0..600 {
                    let step = grid.step(dt).unwrap();
                    assert_eq!(step.pressure_converged_substeps, step.substeps);
                    peak_speed = peak_speed.max(grid.max_face_component_velocity_m_s());
                    assert!(peak_speed < 1e-7, "h={h}, dt={dt}, speed={peak_speed}");
                    assert!((grid.water_volume_m3() - mass).abs() < 1e-10);
                    for z in 0..2 {
                        for x in 0..12 {
                            let i = grid.cell_index(x, 0, z);
                            peak_pressure_error = peak_pressure_error
                                .max((grid.pressure_pa[i] - expected_pressure).abs());
                        }
                    }
                    assert!(
                        peak_pressure_error < 1e-5,
                        "pressure error={peak_pressure_error}"
                    );
                }
                println!(
                    "{{\"scenario\":\"nearly_saturated_surface_rest\",\"h_m\":{h},\"dt_s\":{dt},\"steps\":600,\"peak_speed_m_s\":{peak_speed},\"peak_pressure_error_pa\":{peak_pressure_error},\"water_error_m3\":{}}}",
                    grid.water_volume_m3() - mass
                );
            }
        }
    }

    #[test]
    fn parallel_step_is_bit_identical_for_any_thread_count() {
        let run = |threads: usize| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| {
                    let mut fixture = GridReservoirFixture::new(1, true).unwrap();
                    fixture.grid.set_ambient_density(1.2).unwrap();
                    fixture
                        .grid
                        .set_pressure_preconditioner(PressurePreconditioner::Multigrid);
                    fixture.breach_dam().unwrap();
                    for _ in 0..30 {
                        fixture.step(1.0 / 60.0).unwrap();
                    }
                    let g = fixture.grid;
                    (g.fraction, g.u, g.v, g.w, g.pressure_pa)
                })
        };
        let serial = run(1);
        let parallel = run(4);
        let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&serial.0), bits(&parallel.0));
        assert_eq!(bits(&serial.1), bits(&parallel.1));
        assert_eq!(bits(&serial.2), bits(&parallel.2));
        assert_eq!(bits(&serial.3), bits(&parallel.3));
        assert_eq!(bits(&serial.4), bits(&parallel.4));
    }

    #[test]
    fn standing_wave_keeps_linear_period_without_numerical_damping_or_growth() {
        let mut wave = StandingWaveFixture::new(1, 1.2).unwrap();
        wave.grid_mut()
            .set_pressure_preconditioner(PressurePreconditioner::Multigrid);
        let mass = wave.grid().water_volume_m3();
        let mut samples = vec![(0.0, wave.end_elevation_m())];
        for tick in 1..=1200 {
            let step = wave.grid_mut().step(1.0 / 60.0).unwrap();
            assert_eq!(step.pressure_converged_substeps, step.substeps);
            samples.push((f64::from(tick) / 60.0, wave.end_elevation_m()));
        }
        assert!((wave.grid().water_volume_m3() - mass).abs() < 1e-10);
        let analytic = wave.analytic_period_s();
        assert!((analytic - 4.0).abs() < 0.01, "{analytic}");
        let period = zero_crossing_period(&samples).unwrap();
        assert!((period / analytic - 1.0).abs() < 0.03, "period {period}");
        let peaks = oscillation_peaks(&samples, 0.01);
        assert!(peaks.len() >= 8, "{peaks:?}");
        // Over 15 periods semi-Lagrangian measured 0.994/period, while the
        // rejected MacCormack variants measured 1.046-1.111/period.
        let ratio = amplitude_ratio_per_period(&peaks, analytic).unwrap();
        assert!(
            (0.96..=1.02).contains(&ratio),
            "ratio {ratio} peaks {peaks:?}"
        );
        assert!(peaks.iter().all(|(_, a)| *a < 0.14), "{peaks:?}");
    }

    fn all_air_grid(dims: [u32; 3], config: MacConfig) -> MacGridWorld {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), dims, 4096).unwrap();
        let mut volume = Volume::new(VolumeId::new(900).unwrap(), CellSizeCode::Quarter);
        let brick_dims = dims.map(|d| d.div_ceil(32));
        for z in 0..brick_dims[2] as i64 {
            for y in 0..brick_dims[1] as i64 {
                for x in 0..brick_dims[0] as i64 {
                    volume
                        .insert_brick(
                            BrickCoord::new(x, y, z),
                            Brick::uniform(MaterialId::AIR, Revision(1)),
                        )
                        .unwrap();
                }
            }
        }
        let boundary = SolidBoundary::capture(&volume, spec).unwrap();
        MacGridWorld::new(&boundary, config).unwrap()
    }

    fn quiet_config(open_top: bool) -> MacConfig {
        MacConfig {
            gravity_m_s2: [0.0; 3],
            open_top,
            ..MacConfig::default()
        }
    }

    fn phase_for_grid(grid: &MacGridWorld) -> PhaseWater {
        use crate::cut_cell::{CutCellGeometry, GeometryLimits};
        use crate::phase_water::PhaseLimits;
        let boundary = SolidBoundary {
            spec: grid.spec,
            solid: grid.solid.clone(),
        };
        let geometry = std::sync::Arc::new(
            CutCellGeometry::build(
                &boundary,
                3,
                GeometryLimits {
                    max_fine_cells: 4096,
                    max_components: 4096,
                    max_portals: 12_288,
                },
            )
            .unwrap(),
        );
        PhaseWater::new(
            geometry,
            &grid.fraction,
            grid.config.cell_size_m,
            PhaseLimits {
                max_fine_cells: 4096,
                max_faces: 12_288,
                max_basins: 4096,
            },
        )
        .unwrap()
    }

    #[test]
    fn phase_predictor_matches_physical_fine_projection_with_open_and_sealed_air() {
        for open in [false, true] {
            for compressible in [false, true] {
                let mut grid = all_air_grid([6, 6, 3], quiet_config(open));
                grid.set_ambient_density(1.2).unwrap();
                grid.set_compressible_enclosed_air(compressible).unwrap();
                grid.set_pressure_preconditioner(PressurePreconditioner::Ic0);
                for i in 0..grid.fraction.len() {
                    let y = i / 6 % 6;
                    grid.fraction[i] = (2.3 - y as f64).clamp(0.0, 1.0);
                }
                let u = grid.u_index(3, 1, 1);
                let v = grid.v_index(2, 3, 1);
                grid.u[u] = 0.07;
                grid.v[v] = -0.04;
                let phase = phase_for_grid(&grid);
                let mut predicted = grid.clone();
                predicted
                    .set_phase_pressure_predictor(
                        &phase,
                        GraphLimits {
                            max_fine_cells: 4096,
                            max_rows: 4096,
                            max_connections: 12_288,
                        },
                        8,
                    )
                    .unwrap();
                let plain = grid.project(0.01).unwrap();
                let coarse = predicted.project(0.01).unwrap();
                assert!(
                    plain.converged && coarse.converged,
                    "open={open} gas={compressible}"
                );
                assert!(coarse.phase_rows < coarse.active_cells && coarse.phase_reused);
                for (a, b) in grid
                    .u
                    .iter()
                    .chain(&grid.v)
                    .chain(&grid.w)
                    .zip(predicted.u.iter().chain(&predicted.v).chain(&predicted.w))
                {
                    assert!(
                        (a - b).abs() < 1e-7,
                        "physical face difference {}",
                        (a - b).abs()
                    );
                }
                for (a, b) in grid.pressure_pa.iter().zip(&predicted.pressure_pa) {
                    assert!(
                        (a - b).abs() < 1e-5,
                        "physical pressure difference {}",
                        (a - b).abs()
                    );
                }
                // A boundary mutation cannot keep using the old geometry mapping.
                predicted.solid[0] = true;
                assert!(matches!(
                    predicted.project(0.01),
                    Err(MacError::BoundaryMismatch)
                ));
            }
        }
    }

    #[test]
    fn phase_galerkin_includes_density_top_and_gas_diagonals_without_losing_symmetry() {
        let mut grid = all_air_grid([6, 6, 3], quiet_config(true));
        grid.set_ambient_density(1.2).unwrap();
        for i in 0..grid.fraction.len() {
            grid.fraction[i] = (2.3 - (i / 6 % 6) as f64).clamp(0.0, 1.0);
        }
        let phase = phase_for_grid(&grid);
        let graph = PhaseGraph::build(
            &phase,
            GraphLimits {
                max_fine_cells: 4096,
                max_rows: 4096,
                max_connections: 12_288,
            },
        )
        .unwrap();
        let h2 = grid.config.cell_size_m.powi(2);
        let weights: Vec<_> = phase
            .faces()
            .iter()
            .map(|f| grid.relative_inverse_face_density(f.lower, f.upper) / h2)
            .collect();
        let mut extra = vec![0.0; grid.fraction.len()];
        // Synthetic sealed-gas compliance at every partial/air cell tests the
        // same diagonal formula even though this all-air top is vented.
        for (i, v) in extra.iter_mut().enumerate() {
            *v = (1.0 - grid.fraction[i]) * grid.config.density_kg_m3
                / (ATMOSPHERIC_PRESSURE_PA * 0.01_f64.powi(2));
            if i / 6 % 6 == 5 {
                *v += 2.0 * grid.relative_inverse_face_density(i, i) / h2;
            }
        }
        let coarse = graph
            .pressure_operator_with_diagonal(&phase, &weights, &extra)
            .unwrap();
        let mut diag = extra.clone();
        for (f, &w) in phase.faces().iter().zip(&weights) {
            diag[f.lower] += w;
            diag[f.upper] += w;
        }
        let p: Vec<_> = (0..graph.rows().len())
            .map(|i| (i % 7) as f64 - 3.0)
            .collect();
        let lifted: Vec<_> = (0..diag.len())
            .map(|i| p[graph.row_at_index(i).unwrap() as usize])
            .collect();
        let active: Vec<_> = (0..diag.len()).collect();
        let mut applied = vec![0.0; diag.len()];
        apply_pressure_matrix(
            &grid,
            &lifted,
            &vec![true; diag.len()],
            &active,
            &diag,
            &mut applied,
        );
        let mut restricted = vec![0.0; p.len()];
        for (i, v) in applied.iter().enumerate() {
            restricted[graph.row_at_index(i).unwrap() as usize] += v;
        }
        for (a, b) in coarse.apply(&p).unwrap().iter().zip(restricted) {
            assert!((a - b).abs() < 1e-8);
        }
        let q: Vec<_> = p.iter().map(|v| v * v + 0.2).collect();
        let (mp, mq) = (coarse.smooth(&p, 8).unwrap(), coarse.smooth(&q, 8).unwrap());
        let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
        assert!((dot(&p, &mq) - dot(&q, &mp)).abs() < 1e-12);
        assert!(dot(&p, &mp) > 0.0 && dot(&q, &mq) > 0.0);
    }

    #[test]
    fn balanced_phase_pressure_is_symmetric_positive_and_reuses_scratch() {
        for anchored in [false, true] {
            let mut grid = all_air_grid([6, 6, 3], quiet_config(anchored));
            grid.set_ambient_density(1.2).unwrap();
            for i in 0..grid.fraction.len() {
                grid.solid[i] = i % 6 == 2;
                grid.fraction[i] = if grid.solid[i] {
                    0.0
                } else {
                    (2.3 - (i / 6 % 6) as f64).clamp(0.0, 1.0)
                };
            }
            let phase = phase_for_grid(&grid);
            let graph = PhaseGraph::build(
                &phase,
                GraphLimits {
                    max_fine_cells: 4096,
                    max_rows: 4096,
                    max_connections: 12_288,
                },
            )
            .unwrap();
            let weights: Vec<_> = phase
                .faces()
                .iter()
                .map(|f| grid.relative_inverse_face_density(f.lower, f.upper) / 0.25_f64.powi(2))
                .collect();
            let extra: Vec<_> = (0..grid.fraction.len())
                .map(|i| if anchored && !grid.solid[i] { 3.0 } else { 0.0 })
                .collect();
            let coarse = graph
                .pressure_operator_with_diagonal(&phase, &weights, &extra)
                .unwrap();
            let mut diag = extra;
            for (f, &w) in phase.faces().iter().zip(weights.iter()) {
                diag[f.lower] += w;
                diag[f.upper] += w;
            }
            let liquid: Vec<_> = grid.solid.iter().map(|s| !s).collect();
            let active: Vec<_> = (0..liquid.len()).filter(|&i| liquid[i]).collect();
            let operator = PressureOperator::build(&grid, &liquid, &active, &diag);
            let mut balanced =
                BalancedPhasePressure::new(&graph, coarse.smoother().unwrap(), diag.len(), 8);
            let mut base = BasePressurePreconditioner {
                ic0: None,
                multigrid: None,
                work: None,
                active: &active,
                diag: &diag,
            };
            let (labels, mut anchors) = grid.label_components(&liquid);
            anchors.fill(anchored);
            let mut sums = vec![0.0; anchors.len()];
            let mut counts = vec![0; anchors.len()];
            let mut x: Vec<_> = (0..diag.len()).map(|i| (i % 7) as f64 - 3.0).collect();
            let mut y: Vec<_> = (0..diag.len()).map(|i| (i % 11) as f64 - 5.0).collect();
            for v in [&mut x, &mut y] {
                project_component_means(
                    v,
                    &labels,
                    &anchors,
                    anchors.len(),
                    &active,
                    &mut sums,
                    &mut counts,
                );
            }
            let (mut bx, mut by) = (vec![0.0; diag.len()], vec![0.0; diag.len()]);
            let pointers = (
                balanced.temporary.as_ptr(),
                balanced.applied.as_ptr(),
                balanced.row_source.as_ptr(),
                balanced.row_q.as_ptr(),
                balanced.row_z.as_ptr(),
            );
            let bytes = balanced.array_storage_bytes();
            balanced.apply(&x, &mut bx, &operator, &mut base).unwrap();
            balanced.apply(&y, &mut by, &operator, &mut base).unwrap();
            assert_eq!(
                pointers,
                (
                    balanced.temporary.as_ptr(),
                    balanced.applied.as_ptr(),
                    balanced.row_source.as_ptr(),
                    balanced.row_q.as_ptr(),
                    balanced.row_z.as_ptr()
                )
            );
            assert_eq!(bytes, balanced.array_storage_bytes());
            for v in [&mut bx, &mut by] {
                project_component_means(
                    v,
                    &labels,
                    &anchors,
                    anchors.len(),
                    &active,
                    &mut sums,
                    &mut counts,
                );
            }
            let dot = |a: &[f64], b: &[f64]| dot_indices(a, b, &active);
            let (xby, ybx) = (dot(&x, &by), dot(&y, &bx));
            assert!((xby - ybx).abs() < 1e-10 * xby.abs().max(ybx.abs()).max(1.0));
            assert!(dot(&x, &bx) > 0.0 && dot(&y, &by) > 0.0);
            let mut predicted = grid.clone();
            grid.set_pressure_preconditioner(PressurePreconditioner::Ic0);
            predicted.set_pressure_preconditioner(PressurePreconditioner::Ic0);
            predicted
                .set_phase_pressure_preconditioner(
                    &phase,
                    GraphLimits {
                        max_fine_cells: 4096,
                        max_rows: 4096,
                        max_connections: 12_288,
                    },
                    8,
                )
                .unwrap();
            let u = grid.u_index(4, 1, 1);
            grid.u[u] = 0.07;
            predicted.u[u] = 0.07;
            let plain = grid.project(0.01).unwrap();
            let result = predicted.project(0.01).unwrap();
            assert!(plain.converged && result.converged);
            assert!(result.phase_applications > 0 && result.phase_scratch_bytes > 0);
            for (a, b) in grid
                .u
                .iter()
                .chain(&grid.v)
                .chain(&grid.w)
                .zip(predicted.u.iter().chain(&predicted.v).chain(&predicted.w))
            {
                assert!((a - b).abs() < 1e-7);
            }
        }
    }

    #[test]
    fn fraction_diagnostics_keep_tiny_cells_in_solver_and_report_bridging() {
        let mut grid = all_air_grid([3, 1, 1], quiet_config(false));
        grid.set_fraction(GlobalCell::new(0, 0, 0), 0.5).unwrap();
        grid.set_fraction(GlobalCell::new(1, 0, 0), 1.0e-9).unwrap();
        grid.set_fraction(GlobalCell::new(2, 0, 0), 0.5).unwrap();

        let bands = grid.fraction_band_diagnostics();
        assert_eq!(bands.iter().map(|b| b.cells).sum::<usize>(), 3);
        assert_eq!(bands[0].cells, 0);
        assert_eq!(bands[1].pressure_active_cells, 1);
        assert_eq!(bands[7].pressure_active_cells, 2);
        assert!((bands[1].water_volume_m3 - 1.0e-9 * 0.25_f64.powi(3)).abs() < 1e-20);
        let components = grid.pressure_component_counts_at_fraction_thresholds();
        assert_eq!(components[0], (0.0, 1));
        assert_eq!(components[1], (1.0e-8, 2));
        assert!(grid.fraction.iter().filter(|c| **c > 0.0).count() == 3);
    }

    #[test]
    fn speed_and_energy_diagnostics_use_vector_velocity_and_gravity_reference() {
        let mut grid = all_air_grid(
            [1, 1, 1],
            MacConfig {
                cell_size_m: 0.5,
                gravity_m_s2: [0.0, -2.0, 0.0],
                ..quiet_config(false)
            },
        );
        grid.set_fraction(GlobalCell::new(0, 0, 0), 1.0).unwrap();
        let u0 = grid.u_index(0, 0, 0);
        let u1 = grid.u_index(1, 0, 0);
        let v0 = grid.v_index(0, 0, 0);
        let v1 = grid.v_index(0, 1, 0);
        grid.u[u0] = 3.0;
        grid.u[u1] = 3.0;
        grid.v[v0] = 4.0;
        grid.v[v1] = 4.0;
        assert!((grid.max_liquid_speed_m_s() - 5.0).abs() < 1.0e-12);
        let expected_ke = 0.5 * 1000.0 * 0.125 * 25.0;
        assert!((grid.kinetic_energy_j() - expected_ke).abs() < 1.0e-10);
        let expected_pe = 1000.0 * 0.125 * 2.0 * 0.25;
        assert!((grid.gravitational_potential_energy_j() - expected_pe).abs() < 1.0e-10);
    }

    #[test]
    fn tracked_region_volume_change_matches_final_applied_face_fluxes() {
        let mut grid = all_air_grid([2, 1, 1], quiet_config(false));
        grid.set_fraction(GlobalCell::new(0, 0, 0), 1.0).unwrap();
        let transfer_face = grid.u_index(1, 0, 0);
        grid.u[transfer_face] = 0.25;
        let region = [true, false];
        let before = grid.water_volume_m3();
        let upper_before = grid.fraction[grid.cell_index(0, 0, 0)] * grid.cell_volume();
        let (open_outflow, region_out, region_in, _) =
            grid.advect_fraction_fct(0.1, Some(&region)).unwrap();
        let upper_after = grid.fraction[grid.cell_index(0, 0, 0)] * grid.cell_volume();
        assert_eq!(open_outflow, 0.0);
        assert_eq!(region_in, 0.0);
        assert!(region_out > 0.0);
        assert!((upper_before - upper_after - region_out).abs() < 1.0e-14);
        assert!((grid.water_volume_m3() - before).abs() < 1.0e-14);
    }

    #[test]
    fn refined_boundary_preserves_world_extent_and_exact_aligned_solids() {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [2, 2, 2], 64).unwrap();
        let mut volume = Volume::new(VolumeId::new(980).unwrap(), CellSizeCode::Quarter);
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        let mut edit = spall_voxel::EditPlan::new(volume.id());
        edit.set(GlobalCell::new(1, 0, 1), MaterialId(1));
        volume.apply_edit(&edit).unwrap();
        let coarse_boundary = SolidBoundary::capture(&volume, spec).unwrap();
        let fine_boundary = coarse_boundary.refined(2, 64).unwrap();
        assert_eq!(fine_boundary.spec().dimensions(), [4, 4, 4]);
        assert_eq!(fine_boundary.solid_cell_count(), 8);
        for z in 0..4 {
            for y in 0..4 {
                for x in 0..4 {
                    let expected = (x / 2 == 1) && (y / 2 == 0) && (z / 2 == 1);
                    assert_eq!(
                        fine_boundary.is_solid(GlobalCell::new(x as i64, y as i64, z as i64)),
                        Some(expected)
                    );
                }
            }
        }
    }

    #[test]
    fn pressure_projection_matches_closed_two_cell_reference_and_nullspace() {
        let mut grid = all_air_grid([2, 1, 1], quiet_config(false));
        grid.set_fraction(GlobalCell::new(0, 0, 0), 1.0).unwrap();
        grid.set_fraction(GlobalCell::new(1, 0, 0), 1.0).unwrap();
        let face = grid.u_index(1, 0, 0);
        grid.u[face] = 1.0;

        let stats = grid.project(0.1).unwrap();

        assert!(
            stats.converged,
            "pressure residual {}",
            stats.residual_final
        );
        assert_eq!(stats.iterations, 1);
        assert!((grid.pressure_pa[1] - grid.pressure_pa[0] - 2500.0).abs() < 1.0e-6);
        assert!((grid.pressure_pa.iter().sum::<f64>()).abs() < 1.0e-10);
        assert!(stats.divergence_after < 1.0e-10);
        assert_eq!(grid.u[grid.u_index(1, 0, 0)], 0.0);
    }

    #[test]
    fn ic0_converges_on_anchored_component_and_rebuilds_for_each_projection() {
        let mut grid = all_air_grid([3, 2, 1], quiet_config(true));
        grid.set_pressure_preconditioner(PressurePreconditioner::Ic0);
        for x in 0..3 {
            grid.set_fraction(GlobalCell::new(x, 0, 0), 1.0).unwrap();
        }
        let face = grid.u_index(1, 0, 0);
        grid.u[face] = 0.2;
        let first = grid.project(1.0 / 60.0).unwrap();
        let second = grid.project(1.0 / 60.0).unwrap();
        assert!(first.converged && second.converged);
        assert!(first.residual_final < first.residual_initial);
    }

    #[test]
    fn assembled_pressure_operator_matches_reference_stencil() {
        let mut fixture = GridReservoirFixture::new_tunnel(1).unwrap();
        let grid = fixture.grid_mut();
        grid.set_ambient_density(1.2).unwrap();
        let liquid: Vec<bool> = grid.solid.iter().map(|s| !s).collect();
        let active: Vec<usize> = (0..liquid.len()).filter(|&i| liquid[i]).collect();
        let diag: Vec<f64> = (0..liquid.len()).map(|i| 3.0 + (i % 7) as f64).collect();
        let x: Vec<f64> = (0..liquid.len())
            .map(|i| ((i * 31) % 17) as f64 - 8.0)
            .collect();
        let mut expected = vec![0.0; liquid.len()];
        let mut actual = vec![0.0; liquid.len()];
        apply_pressure_matrix(grid, &x, &liquid, &active, &diag, &mut expected);
        PressureOperator::build(grid, &liquid, &active, &diag).apply(&x, &mut actual);
        for &i in &active {
            assert!((expected[i] - actual[i]).abs() <= 1e-9 * expected[i].abs().max(1.0));
        }
    }

    #[test]
    fn mic0_and_multigrid_converge_to_the_ic0_projection_on_a_closed_two_phase_tank() {
        // Incompressible air exercises the singular enclosed-region path
        // (divergence-free); sealed compressible gas makes the region
        // definite and intentionally divergent in its air cells.
        let solve = |preconditioner, compressible: bool| {
            let mut grid = all_air_grid([8, 8, 4], quiet_config(false));
            grid.set_ambient_density(1.2).unwrap();
            grid.set_compressible_enclosed_air(compressible).unwrap();
            grid.set_pressure_preconditioner(preconditioner);
            for z in 0..4 {
                for y in 0..3 {
                    for x in 0..8 {
                        grid.set_fraction(GlobalCell::new(x, y, z), 1.0).unwrap();
                    }
                }
            }
            for (i, u) in grid.u.iter_mut().enumerate() {
                *u = ((i * 37) % 11) as f64 * 0.01 - 0.05;
            }
            grid.enforce_wall_velocities();
            let stats = grid.project(1.0 / 60.0).unwrap();
            assert!(stats.converged);
            assert!(compressible || stats.divergence_after < 1e-6);
            grid.u
        };
        // Iteration savings are fixture-dependent (smooth modes on larger
        // grids); only the converged projection is asserted here.
        for compressible in [false, true] {
            let ic0_u = solve(PressurePreconditioner::Ic0, compressible);
            for other in [
                PressurePreconditioner::Mic0,
                PressurePreconditioner::Multigrid,
            ] {
                let other_u = solve(other, compressible);
                let max_difference = ic0_u
                    .iter()
                    .zip(&other_u)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0, f64::max);
                assert!(
                    max_difference < 1e-6,
                    "{other:?} compressible={compressible}: {max_difference}"
                );
            }
        }
    }

    #[test]
    fn multigrid_preconditioner_is_symmetric_positive_on_two_phase_geometry() {
        let mut fixture = GridReservoirFixture::new_tunnel(1).unwrap();
        let grid = fixture.grid_mut();
        grid.set_ambient_density(1.2).unwrap();
        let liquid: Vec<bool> = grid.solid.iter().map(|s| !s).collect();
        let n = liquid.len();
        // Open-top style Dirichlet contribution keeps the operator definite.
        let mut diag = vec![0.0; n];
        let [nx, ny, nz] = grid.dims();
        let h2 = grid.config.cell_size_m.powi(2);
        for i in (0..n).filter(|&i| liquid[i]) {
            let (x, y, z) = (i % nx, (i / nx) % ny, i / (nx * ny));
            diag[i] = 1.0 / h2;
            for (present, j) in [
                (x > 0, i.wrapping_sub(1)),
                (x + 1 < nx, i + 1),
                (y > 0, i.wrapping_sub(nx)),
                (y + 1 < ny, i + nx),
                (z > 0, i.wrapping_sub(nx * ny)),
                (z + 1 < nz, i + nx * ny),
            ] {
                if present && liquid[j] {
                    diag[i] += grid.relative_inverse_face_density(i, j) / h2;
                }
            }
        }
        let mut mg = Multigrid::build(grid, &liquid, &diag);
        assert!(mg.levels.len() > 1);
        let a: Vec<f64> = (0..n).map(|i| ((i * 29) % 13) as f64 - 6.0).collect();
        let b: Vec<f64> = (0..n).map(|i| ((i * 17) % 7) as f64 - 3.0).collect();
        let (mut ma, mut mb) = (vec![0.0; n], vec![0.0; n]);
        mg.apply(&a, &mut ma);
        mg.apply(&b, &mut mb);
        let dot = |x: &[f64], y: &[f64]| -> f64 {
            (0..n).filter(|&i| liquid[i]).map(|i| x[i] * y[i]).sum()
        };
        let (bma, amb) = (dot(&b, &ma), dot(&a, &mb));
        assert!(
            (bma - amb).abs() <= 1e-9 * bma.abs().max(amb.abs()),
            "{bma} vs {amb}"
        );
        assert!(dot(&a, &ma) > 0.0 && dot(&b, &mb) > 0.0);
        // Galerkin coarse rows of a definite operator stay diagonally dominant.
        for level in &mg.levels {
            for &i in &level.active {
                let off = level.neighbor_sum(&vec![1.0; level.x.len()], i);
                assert!(level.diag[i] >= off - 1e-9 * level.diag[i]);
            }
        }
    }

    #[test]
    fn ic0_handles_disconnected_enclosed_single_cell_nullspaces() {
        let mut config = quiet_config(false);
        config.gravity_m_s2 = [0.0; 3];
        let mut grid = all_air_grid([3, 1, 1], config);
        grid.set_pressure_preconditioner(PressurePreconditioner::Ic0);
        grid.set_fraction(GlobalCell::new(0, 0, 0), 1.0).unwrap();
        grid.set_fraction(GlobalCell::new(2, 0, 0), 1.0).unwrap();
        grid.solid[1] = true;
        let stats = grid.project(1.0 / 60.0).unwrap();
        assert!(stats.converged);
        assert_eq!(stats.iterations, 0);
        assert!(grid.pressure_pa.iter().all(|p| *p == 0.0));
    }

    #[test]
    fn ic0_reports_non_positive_factor_pivots_without_fallback() {
        let grid = all_air_grid([1, 1, 1], quiet_config(true));
        let result = Ic0Factor::build(&grid, &[0], &[Some(0)], &[true], &[-1.0], 0.25_f64.powi(2));
        assert!(matches!(result, Err(MacError::Ic0NonPositivePivot { .. })));
    }

    #[test]
    fn committed_geometry_edit_invalidates_pressure_warm_start() {
        let mut grid = all_air_grid([2, 1, 1], quiet_config(false));
        grid.pressure_pa.copy_from_slice(&[3.0, -3.0]);
        grid.previous_liquid.fill(true);
        grid.commit_boundary(vec![false; 2]).unwrap();
        assert_eq!(grid.pressure_pa, vec![0.0, 0.0]);
        assert_eq!(grid.previous_liquid, vec![false, false]);
    }

    #[test]
    fn closed_pressure_operator_is_symmetric_positive_on_zero_mean_subspace() {
        let grid = all_air_grid([3, 1, 1], quiet_config(false));
        let liquid = [true, true, true];
        let active = [0, 1, 2];
        let coefficient = 1.0 / grid.config.cell_size_m.powi(2);
        let diagonal = [coefficient, 2.0 * coefficient, coefficient];
        let x = [1.0, 0.0, -1.0];
        let y = [1.0, -2.0, 1.0];
        let mut ax = vec![0.0; 3];
        let mut ay = vec![0.0; 3];
        apply_pressure_matrix(&grid, &x, &liquid, &active, &diagonal, &mut ax);
        apply_pressure_matrix(&grid, &y, &liquid, &active, &diagonal, &mut ay);
        let xay = dot_indices(&x, &ay, &active);
        let yax = dot_indices(&y, &ax, &active);
        let xax = dot_indices(&x, &ax, &active);
        assert!((xay - yax).abs() < 1.0e-12);
        assert!(xax > 0.0);
        assert!(diagonal.iter().all(|d| *d > 0.0));
    }

    #[test]
    fn hydrostatic_column_cancels_gravity_at_a_free_surface() {
        let mut grid = all_air_grid(
            [1, 2, 1],
            MacConfig {
                max_substeps: 4,
                ..MacConfig::default()
            },
        );
        grid.set_fraction(GlobalCell::new(0, 0, 0), 1.0).unwrap();
        grid.set_fraction(GlobalCell::new(0, 1, 0), 1.0).unwrap();
        let metrics = grid.step(1.0 / 60.0).unwrap();

        assert_eq!(metrics.pressure_converged_substeps, metrics.substeps);
        assert!(metrics.divergence_after_max_s < 1.0e-8);
        assert!(grid.v[grid.v_index(0, 1, 0)].abs() < 1.0e-8);
        assert!(grid.v[grid.v_index(0, 2, 0)].abs() < 1.0e-8);
        let expected_surface_cell_pressure =
            grid.config.density_kg_m3 * 9.81 * grid.config.cell_size_m * 0.5;
        assert!((grid.pressure_pa[1] - expected_surface_cell_pressure).abs() < 1.0e-5);
        assert!((metrics.conservation_error_m3) < 1.0e-12);
    }

    #[test]
    fn hydrostatic_projection_is_consistent_at_two_cell_resolutions() {
        let make_column = |cells: u32, h: f64| {
            let mut config = MacConfig {
                cell_size_m: h,
                ..MacConfig::default()
            };
            config.open_top = true;
            let mut grid = all_air_grid([1, cells, 1], config);
            for y in 0..cells {
                grid.set_fraction(GlobalCell::new(0, y as i64, 0), 1.0)
                    .unwrap();
            }
            grid.step(1.0 / 120.0).unwrap();
            grid
        };
        let coarse = make_column(2, 0.25);
        let fine = make_column(4, 0.125);
        let mean_pressure =
            |g: &MacGridWorld| g.pressure_pa.iter().sum::<f64>() / g.pressure_pa.len() as f64;
        assert!(
            (mean_pressure(&coarse) - mean_pressure(&fine)).abs() < 5.0,
            "coarse/fine mean pressure {} vs {}",
            mean_pressure(&coarse),
            mean_pressure(&fine)
        );
        assert!(coarse.max_face_component_velocity_m_s() < 1.0e-8);
        assert!(fine.max_face_component_velocity_m_s() < 1.0e-8);
    }

    #[test]
    fn hydrostatic_measurements_are_consistent_at_two_time_steps() {
        let mut full_step = all_air_grid([1, 2, 1], MacConfig::default());
        let mut half_step = all_air_grid([1, 2, 1], MacConfig::default());
        for y in 0..2 {
            full_step
                .set_fraction(GlobalCell::new(0, y, 0), 1.0)
                .unwrap();
            half_step
                .set_fraction(GlobalCell::new(0, y, 0), 1.0)
                .unwrap();
        }
        full_step.step(1.0 / 60.0).unwrap();
        half_step.step(1.0 / 120.0).unwrap();
        half_step.step(1.0 / 120.0).unwrap();
        assert!((full_step.water_volume_m3() - half_step.water_volume_m3()).abs() < 1.0e-14);
        assert!((full_step.kinetic_energy_j() - half_step.kinetic_energy_j()).abs() < 1.0e-10);
        assert!((full_step.pressure_pa[0] - half_step.pressure_pa[0]).abs() < 1.0e-5);
        assert!(full_step.max_face_component_velocity_m_s() < 1.0e-8);
        assert!(half_step.max_face_component_velocity_m_s() < 1.0e-8);
    }

    #[test]
    fn fct_shared_flux_is_conservative_and_keeps_fraction_bounds() {
        let mut grid = all_air_grid([4, 1, 1], quiet_config(false));
        grid.set_fraction(GlobalCell::new(0, 0, 0), 1.0).unwrap();
        grid.set_fraction(GlobalCell::new(1, 0, 0), 0.25).unwrap();
        let face = grid.u_index(1, 0, 0);
        grid.u[face] = 0.5;
        let before = grid.water_volume_m3();

        let outflow = grid.advect_fraction_fct(0.1, None).unwrap().0;

        assert_eq!(outflow, 0.0);
        assert!((grid.water_volume_m3() - before).abs() < 1.0e-14);
        assert!(grid.fraction.iter().all(|c| (0.0..=1.0).contains(c)));
        assert!(grid.fraction[0] < 1.0);
        assert!(grid.fraction[1] > 0.25);
    }

    #[test]
    fn open_top_outflow_is_explicit_and_closes_the_volume_balance() {
        let mut grid = all_air_grid([1, 1, 1], quiet_config(true));
        grid.set_fraction(GlobalCell::new(0, 0, 0), 0.5).unwrap();
        let before = grid.water_volume_m3();
        let face = grid.v_index(0, 1, 0);
        grid.v[face] = 1.0;

        let outflow = grid.advect_fraction_fct(0.1, None).unwrap().0;

        assert!((before - grid.water_volume_m3() - outflow).abs() < 1.0e-14);
        assert!((outflow - 0.2 * grid.cell_volume()).abs() < 1.0e-14);
    }

    #[test]
    fn open_top_accounting_subtracts_applied_negative_antiflux() {
        let mut grid = all_air_grid([3, 2, 1], quiet_config(true));
        grid.set_fraction(GlobalCell::new(1, 0, 0), 1.0).unwrap();
        grid.set_fraction(GlobalCell::new(1, 1, 0), 0.5).unwrap();
        let top_face = grid.v_index(1, 2, 0);
        grid.v[top_face] = 0.1;
        let before = grid.water_volume_m3();

        let outflow = grid.advect_fraction_fct(0.1, None).unwrap().0;

        assert!((before - grid.water_volume_m3() - outflow).abs() < 1.0e-14);
        assert!(
            outflow < 0.02 * grid.cell_volume(),
            "outflow={outflow}, cell volume={}, fractions={:?}",
            grid.cell_volume(),
            grid.fraction
        );
    }

    #[test]
    fn intact_wall_on_brick_seam_blocks_face_transfer() {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [34, 4, 4], 1024).unwrap();
        let mut volume = Volume::new(VolumeId::new(902).unwrap(), CellSizeCode::Quarter);
        for bx in 0..=1 {
            volume
                .insert_brick(
                    BrickCoord::new(bx, 0, 0),
                    Brick::uniform(MaterialId::AIR, Revision(1)),
                )
                .unwrap();
        }
        let mut edit = spall_voxel::EditPlan::new(volume.id());
        for z in 0..4 {
            for y in 0..4 {
                edit.set(GlobalCell::new(31, y, z), MaterialId(1));
            }
        }
        volume.apply_edit(&edit).unwrap();
        let boundary = SolidBoundary::capture(&volume, spec).unwrap();
        let mut grid = MacGridWorld::new(&boundary, quiet_config(false)).unwrap();
        grid.set_fraction(GlobalCell::new(30, 1, 1), 1.0).unwrap();
        let wall_cell = GlobalCell::new(31, 1, 1);
        assert!(matches!(
            grid.set_fraction(wall_cell, 0.1),
            Err(MacError::WaterOverlapsSolid(_))
        ));
        let left_face = grid.u_index(31, 1, 1);
        let right_face = grid.u_index(32, 1, 1);
        grid.u[left_face] = 4.0;
        grid.u[right_face] = 4.0;
        grid.enforce_wall_velocities();
        let before = grid.water_volume_m3();
        let _ = grid.advect_fraction_fct(0.1, None).unwrap();

        assert_eq!(grid.u[left_face], 0.0);
        assert_eq!(grid.u[right_face], 0.0);
        assert_eq!(grid.fraction[grid.cell_index(32, 1, 1)], 0.0);
        assert_eq!(grid.water_volume_m3(), before);
    }

    #[test]
    fn fixed_tick_overload_does_not_advance_or_discard_time() {
        let mut grid = all_air_grid(
            [2, 1, 1],
            MacConfig {
                max_substeps: 1,
                ..quiet_config(false)
            },
        );
        grid.set_fraction(GlobalCell::new(0, 0, 0), 1.0).unwrap();
        let face = grid.u_index(1, 0, 0);
        grid.u[face] = 20.0;
        let before_volume = grid.water_volume_m3();
        let before_u = grid.u.clone();
        let result = grid.step(1.0 / 60.0);

        assert!(matches!(
            result,
            Err(MacError::SubstepBudgetExceeded { .. })
        ));
        assert_eq!(grid.water_volume_m3(), before_volume);
        assert_eq!(grid.u, before_u);
    }

    #[test]
    fn geometry_boundary_rejects_liquid_overlap_without_mutating_grid() {
        let mut volume = Volume::new(VolumeId::new(901).unwrap(), CellSizeCode::Quarter);
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [2, 1, 1], 8).unwrap();
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        let boundary = SolidBoundary::capture(&volume, spec).unwrap();
        let mut grid = MacGridWorld::new(&boundary, quiet_config(false)).unwrap();
        grid.set_fraction(GlobalCell::new(0, 0, 0), 0.5).unwrap();
        let old_grid = grid.clone();

        let mut staged_volume = volume.clone();
        let mut edit = spall_voxel::EditPlan::new(volume.id());
        edit.set(GlobalCell::new(0, 0, 0), MaterialId(1));
        staged_volume.apply_edit(&edit).unwrap();
        let staged_boundary = SolidBoundary::capture(&staged_volume, spec).unwrap();
        let rejected = grid.prepare_boundary(&staged_boundary);

        assert_eq!(
            rejected,
            Err(MacError::WaterOverlapsSolid(GlobalCell::new(0, 0, 0)))
        );
        assert_eq!(grid.fraction, old_grid.fraction);
        assert_eq!(grid.solid, old_grid.solid);
        assert_eq!(
            volume.sample(GlobalCell::new(0, 0, 0)).unwrap(),
            spall_voxel::Sample::Empty { modified: false }
        );
        assert_eq!(
            staged_volume.sample(GlobalCell::new(0, 0, 0)).unwrap(),
            spall_voxel::Sample::Filled(MaterialId(1))
        );
    }

    #[test]
    fn boundary_edit_displaces_fully_submerged_cell_without_losing_water() {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [4, 3, 3], 128).unwrap();
        let mut volume = Volume::new(VolumeId::new(903).unwrap(), CellSizeCode::Quarter);
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        let initial_boundary = SolidBoundary::capture(&volume, spec).unwrap();
        let mut grid = MacGridWorld::new(&initial_boundary, quiet_config(false)).unwrap();
        let source = GlobalCell::new(1, 1, 1);
        grid.set_fraction(source, 1.0).unwrap();
        let initial_volume = grid.water_volume_m3();

        let mut edit = spall_voxel::EditPlan::new(volume.id());
        edit.set(source, MaterialId(1));
        volume.apply_edit(&edit).unwrap();
        let edited_boundary = SolidBoundary::capture(&volume, spec).unwrap();
        let displaced = grid.refresh_boundary_displacing(&edited_boundary).unwrap();

        assert!((grid.water_volume_m3() - initial_volume).abs() < 1.0e-12);
        assert!((displaced - initial_volume).abs() < 1.0e-12);
        assert_eq!(grid.fraction_at(source), Some(0.0));
        assert_eq!(grid.active_cells(), 1);
    }

    #[test]
    fn boundary_edit_displaces_water_within_a_sealed_pocket() {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [4, 3, 3], 128).unwrap();
        let mut volume = Volume::new(VolumeId::new(904).unwrap(), CellSizeCode::Quarter);
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId(1), Revision(1)),
            )
            .unwrap();
        let source = GlobalCell::new(1, 1, 1);
        let reservoir = GlobalCell::new(2, 1, 1);
        let mut opening = spall_voxel::EditPlan::new(volume.id());
        opening.set(source, MaterialId::AIR);
        opening.set(reservoir, MaterialId::AIR);
        volume.apply_edit(&opening).unwrap();
        let boundary = SolidBoundary::capture(&volume, spec).unwrap();
        let mut grid = MacGridWorld::new(&boundary, quiet_config(false)).unwrap();
        grid.set_fraction(source, 0.75).unwrap();
        grid.set_fraction(reservoir, 0.25).unwrap();
        let initial_volume = grid.water_volume_m3();

        let mut placement = spall_voxel::EditPlan::new(volume.id());
        placement.set(source, MaterialId(1));
        volume.apply_edit(&placement).unwrap();
        let edited_boundary = SolidBoundary::capture(&volume, spec).unwrap();
        grid.refresh_boundary_displacing(&edited_boundary).unwrap();

        assert!((grid.water_volume_m3() - initial_volume).abs() < 1.0e-12);
        assert_eq!(grid.fraction_at(source), Some(0.0));
        assert_eq!(grid.fraction_at(reservoir), Some(1.0));
    }

    #[test]
    fn boundary_displacement_cannot_teleport_through_solid_walls() {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [4, 3, 3], 128).unwrap();
        let mut volume = Volume::new(VolumeId::new(905).unwrap(), CellSizeCode::Quarter);
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId(1), Revision(1)),
            )
            .unwrap();
        let source = GlobalCell::new(1, 1, 1);
        let isolated_cavity = GlobalCell::new(3, 1, 1);
        let mut open = spall_voxel::EditPlan::new(volume.id());
        open.set(source, MaterialId::AIR);
        open.set(isolated_cavity, MaterialId::AIR);
        volume.apply_edit(&open).unwrap();
        let initial = SolidBoundary::capture(&volume, spec).unwrap();
        let mut grid = MacGridWorld::new(&initial, quiet_config(false)).unwrap();
        grid.set_fraction(source, 0.75).unwrap();
        let before_fraction = grid.fraction.clone();
        let before_solid = grid.solid.clone();

        let mut placement = spall_voxel::EditPlan::new(volume.id());
        placement.set(source, MaterialId(1));
        volume.apply_edit(&placement).unwrap();
        let edited = SolidBoundary::capture(&volume, spec).unwrap();
        assert!(matches!(
            grid.refresh_boundary_displacing(&edited),
            Err(MacError::WaterDisplacementCapacityExceeded { cell }) if cell == source
        ));
        assert_eq!(grid.fraction, before_fraction);
        assert_eq!(grid.solid, before_solid);
    }

    #[test]
    fn boundary_retains_sealed_water_and_releases_only_connected_capacity() {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [4, 3, 3], 128).unwrap();
        let mut volume = Volume::new(VolumeId::new(905).unwrap(), CellSizeCode::Quarter);
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId(1), Revision(1)),
            )
            .unwrap();
        let source = GlobalCell::new(1, 1, 1);
        let other = GlobalCell::new(3, 1, 1);
        let mut edit = spall_voxel::EditPlan::new(volume.id());
        edit.set(source, MaterialId::AIR);
        edit.set(other, MaterialId::AIR);
        volume.apply_edit(&edit).unwrap();
        let mut grid = MacGridWorld::new(
            &SolidBoundary::capture(&volume, spec).unwrap(),
            quiet_config(false),
        )
        .unwrap();
        grid.set_fraction(source, 0.75).unwrap();
        let initial = grid.water_volume_m3();
        edit.set(source, MaterialId(1));
        volume.apply_edit(&edit).unwrap();
        grid.refresh_boundary_retaining(&SolidBoundary::capture(&volume, spec).unwrap())
            .unwrap();
        assert_eq!(grid.water_volume_m3(), initial);
        assert_eq!(grid.trapped_volume_m3(), initial);
        assert_eq!(grid.fraction_at(other), Some(0.0));
        for _ in 0..4 {
            grid.step(1.0 / 60.0).unwrap();
        }
        assert_eq!(grid.water_volume_m3(), initial);
        edit.set(source, MaterialId::AIR);
        volume.apply_edit(&edit).unwrap();
        grid.refresh_boundary_retaining(&SolidBoundary::capture(&volume, spec).unwrap())
            .unwrap();
        assert_eq!(grid.water_volume_m3(), initial);
        assert_eq!(grid.trapped_volume_m3(), 0.0);
        assert_eq!(grid.fraction_at(source), Some(0.75));
        assert_eq!(grid.fraction_at(other), Some(0.0));
    }

    #[test]
    fn rejected_fixture_edit_preserves_voxels_and_all_grid_arrays() {
        let mut fixture = GridReservoirFixture::new(1, true).unwrap();
        let cell = GlobalCell::new(3, 1, 2);
        assert!(fixture.grid.fraction[fixture.grid.cell_index_global(cell).unwrap()] > 0.0);
        let before_sample = fixture.volume.sample(cell).unwrap();
        let before_solid = fixture.grid.solid.clone();
        let before_fraction = fixture.grid.fraction.clone();
        let before_pressure = fixture.grid.pressure_pa.clone();
        let before_u = fixture.grid.u.clone();
        let before_v = fixture.grid.v.clone();
        let before_w = fixture.grid.w.clone();
        let mut edit = spall_voxel::EditPlan::new(fixture.volume.id());
        edit.set(cell, MaterialId(1));

        assert!(fixture.apply_staged_edit(&edit).is_err());
        assert_eq!(fixture.volume.sample(cell).unwrap(), before_sample);
        assert_eq!(fixture.grid.solid, before_solid);
        assert_eq!(fixture.grid.fraction, before_fraction);
        assert_eq!(fixture.grid.pressure_pa, before_pressure);
        assert_eq!(fixture.grid.u, before_u);
        assert_eq!(fixture.grid.v, before_v);
        assert_eq!(fixture.grid.w, before_w);
    }

    #[test]
    fn voxel_reservoir_initialization_matches_reference_volume_and_canal_moves_water() {
        let mut fixture = GridReservoirFixture::new(1, true).unwrap();
        assert!((fixture.grid.water_volume_m3() - fixture.initial_volume_m3).abs() < 1.0e-10);
        let initial_difference = fixture.p95_level_difference_m();
        let initial_downstream = fixture.downstream_volume_m3();
        fixture.excavate_canal().unwrap();
        let mut max_discharge = 0.0f64;
        for _ in 0..120 {
            fixture.step(1.0 / 60.0).unwrap();
            max_discharge = max_discharge.max(fixture.discharge_through_dam_m3_s());
        }
        assert!(fixture.downstream_volume_m3() > initial_downstream + 0.03);
        assert!(fixture.p95_level_difference_m() < initial_difference - 0.03);
        assert!(max_discharge > 0.01, "max discharge {max_discharge}");
        assert!(
            fixture
                .grid
                .fraction
                .iter()
                .all(|c| (0.0..=1.0).contains(c))
        );
    }

    #[test]
    fn basin_initialization_spreads_vof_depth_over_the_wetted_footprint() {
        let mut fixture = GridReservoirFixture::new_basin(1).unwrap();
        let grid = &fixture.grid;
        let h = grid.config.cell_size_m;
        let mut depths = Vec::new();
        for z in 1..7 {
            for x in 2..11 {
                let depth: f64 = (1..4)
                    .map(|y| grid.fraction[grid.cell_index(x, y, z)] * h)
                    .sum();
                depths.push(depth);
            }
        }
        assert_eq!(depths.len(), 54);
        let expected_depth = fixture.initial_volume_m3() / (54.0 * h.powi(2));
        assert!(
            depths
                .iter()
                .all(|depth| (*depth - expected_depth).abs() < 1.0e-12)
        );
        assert!((grid.water_volume_m3() - fixture.initial_volume_m3()).abs() < 1.0e-12);
        assert_eq!(grid.active_cells(), 54 * 3);
        assert_eq!(
            grid.fraction
                .iter()
                .filter(|c| **c > 0.0 && **c < 1.0)
                .count(),
            54
        );
        let bands = grid.fraction_band_diagnostics();
        assert_eq!(
            bands
                .iter()
                .map(|band| band.pressure_active_cells)
                .sum::<usize>(),
            grid.active_cells(),
            "every conservative water-bearing cell, including partial cells, owns one pressure row"
        );
        let initial_rows = grid.active_cells();
        let first_step = fixture.grid_mut().step(1.0 / 60.0).unwrap();
        assert_eq!(first_step.pressure_active_rows_total as usize, initial_rows);
    }

    #[test]
    fn fully_filled_basin_settles_without_losing_water_or_excess_surface_drift() {
        let mut fixture = GridReservoirFixture::new_basin(1).unwrap();
        let initial_volume = fixture.grid.water_volume_m3();
        let initial_surface = fixture.basin_surface_p95_m();
        let mut max_late_speed = 0.0f64;
        let mut eligible_volume = 0.0;
        let mut eligible_fast_volume = 0.0;
        let mut eligible_speed_histogram = vec![0.0; 400];
        for tick in 0..180 {
            fixture.grid.step(1.0 / 60.0).unwrap();
            if tick >= 120 {
                max_late_speed = max_late_speed.max(fixture.grid.max_liquid_speed_m_s());
                for sample in fixture.grid.liquid_speed_samples() {
                    if sample.fraction >= 1.0e-3 {
                        let volume = sample.fraction * fixture.grid.cell_volume();
                        eligible_volume += volume;
                        if sample.speed_m_s > 0.5 {
                            eligible_fast_volume += volume;
                        }
                        eligible_speed_histogram
                            [((sample.speed_m_s / 0.05).floor() as usize).min(399)] += volume;
                    }
                }
            }
        }
        let drift = (fixture.basin_surface_p95_m() - initial_surface).abs();
        let balance = (initial_volume
            - fixture.grid.water_volume_m3()
            - fixture.grid.cumulative_open_outflow_m3())
        .abs();
        assert!(balance < 1.0e-8, "basin conservation error {balance}");
        assert!(drift < 0.15, "basin surface drift {drift} m");
        let eligible_target = eligible_volume * 0.95;
        let mut cumulative = 0.0;
        let mut eligible_p95 = 0.0;
        for (bucket, volume) in eligible_speed_histogram.iter().enumerate() {
            cumulative += volume;
            if cumulative >= eligible_target {
                eligible_p95 = bucket as f64 * 0.05;
                break;
            }
        }
        let fast_share = eligible_fast_volume / eligible_volume.max(f64::MIN_POSITIVE);
        assert!(
            eligible_p95 <= 0.5,
            "eligible water weighted p95 {eligible_p95} m/s; raw max {max_late_speed} m/s"
        );
        assert!(
            fast_share <= 0.01,
            "eligible water share above 0.5 m/s {fast_share}; raw max {max_late_speed} m/s"
        );
        assert!(drift < 0.15, "basin surface drift {drift} m");
    }

    #[test]
    fn dam_breach_creates_downstream_surge_momentum() {
        let mut fixture = GridReservoirFixture::new(1, true).unwrap();
        fixture.breach_dam().unwrap();
        let mut peak_momentum = 0.0f64;
        let mut peak_discharge = 0.0f64;
        for _ in 0..30 {
            fixture.grid.step(1.0 / 60.0).unwrap();
            peak_momentum = peak_momentum.max(fixture.eastward_momentum_kg_m_s());
            peak_discharge = peak_discharge.max(fixture.discharge_through_dam_m3_s());
        }
        assert!(
            peak_momentum > 0.1,
            "peak downstream momentum {peak_momentum}"
        );
        assert!(peak_discharge > 0.001, "peak discharge {peak_discharge}");
        assert!(fixture.downstream_volume_m3() > 0.0);
    }

    #[test]
    fn unoccupied_canal_closure_stops_subsequent_exchange() {
        let mut fixture = GridReservoirFixture::new(1, true).unwrap();
        fixture.excavate_canal().unwrap();
        // The staged opening remains dry, so this is a declared unoccupied
        // construction location. Close before allowing water into the gap.
        fixture.close_canal().unwrap();
        let downstream = fixture.downstream_volume_m3();
        for _ in 0..30 {
            fixture.grid.step(1.0 / 60.0).unwrap();
        }
        assert!((fixture.downstream_volume_m3() - downstream).abs() < 1.0e-12);
    }

    #[test]
    fn stacked_pool_floor_blocks_flux_while_lower_tunnel_moves_water() {
        let mut fixture = GridReservoirFixture::new_tunnel(1).unwrap();
        fixture.excavate_canal().unwrap();
        let upper_before = fixture.upper_pool_volume_m3();
        let lower_before = fixture.lower_tunnel_volume_m3();
        let mut upper_outflow = 0.0;
        let mut upper_inflow = 0.0;
        for _ in 0..30 {
            let metrics = fixture.step(1.0 / 60.0).unwrap();
            upper_outflow += metrics.tracked_region_outflow_m3;
            upper_inflow += metrics.tracked_region_inflow_m3;
        }
        assert!(
            (fixture.upper_pool_volume_m3() - upper_before + upper_outflow - upper_inflow).abs()
                < 1.0e-10,
            "upper-pool change is not explained by applied face fluxes"
        );
        let mut roof_normal_velocity_max: f64 = 0.0;
        for z in 1..7 {
            for x in 14..21 {
                roof_normal_velocity_max = roof_normal_velocity_max
                    .max(fixture.grid.v[fixture.grid.v_index(x, 4, z)].abs())
                    .max(fixture.grid.v[fixture.grid.v_index(x, 5, z)].abs());
                let roof = fixture.grid.cell_index(x, 4, z);
                if fixture.grid.solid[roof] {
                    assert_eq!(fixture.grid.fraction[roof], 0.0);
                }
            }
        }
        assert_eq!(
            roof_normal_velocity_max, 0.0,
            "upper-pool roof allowed normal fluid flux"
        );
        assert!(
            (fixture.upper_pool_volume_m3() - upper_before).abs() < 1.0e-9,
            "upper pool volume changed despite the intact roof and side walls: {} m3",
            fixture.upper_pool_volume_m3() - upper_before
        );
        let lower_after = fixture.lower_tunnel_volume_m3();
        assert!(
            lower_after - lower_before <= 1.0e-9,
            "upper pool added water below its solid roof: {} m3",
            lower_after - lower_before
        );

        let y = 2usize;
        let lower_downstream_before: f64 = fixture
            .grid
            .fraction
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                let c = fixture.grid.spec.cell_at(*i);
                c.x > 20 && c.y < 4
            })
            .map(|(_, f)| *f * fixture.grid.cell_volume())
            .sum();
        for x in 14..=21 {
            for z in 2..6 {
                let face = fixture.grid.u_index(x, y, z);
                fixture.grid.u[face] = 0.25;
            }
        }
        let mut peak_exit_discharge = 0.0f64;
        for _ in 0..30 {
            fixture.grid.step(1.0 / 60.0).unwrap();
            for z in 2..6 {
                let face = fixture.grid.u_index(21, y, z);
                let left = fixture.grid.cell_index(20, y, z);
                let right = fixture.grid.cell_index(21, y, z);
                let frac = 0.5 * (fixture.grid.fraction[left] + fixture.grid.fraction[right]);
                peak_exit_discharge = peak_exit_discharge.max(
                    fixture.grid.u[face].max(0.0) * frac * fixture.grid.config.cell_size_m.powi(2),
                );
            }
        }
        assert!(
            fixture
                .grid
                .fraction
                .iter()
                .all(|c| (0.0..=1.0).contains(c))
        );
        let lower_downstream_after: f64 = fixture
            .grid
            .fraction
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                let c = fixture.grid.spec.cell_at(*i);
                c.x > 20 && c.y < 4
            })
            .map(|(_, f)| *f * fixture.grid.cell_volume())
            .sum();
        assert!(
            lower_downstream_after > lower_downstream_before + 1.0e-5
                && peak_exit_discharge > 1.0e-5,
            "lower tunnel transfer {}, peak exit discharge {peak_exit_discharge}",
            lower_downstream_after - lower_downstream_before,
        );
    }

    #[test]
    fn scaled_tunnel_side_walls_meet_the_roof_without_an_air_course() {
        for scale in [1usize, 2] {
            let fixture = GridReservoirFixture::new_tunnel(scale as u32).unwrap();
            let s = scale;
            for y in (4 * s + 1)..10 * s {
                for z in s..7 * s {
                    assert!(fixture.grid.solid[fixture.grid.cell_index(14 * s, y, z)]);
                    assert!(fixture.grid.solid[fixture.grid.cell_index(20 * s, y, z)]);
                }
                for x in 14 * s..21 * s {
                    assert!(fixture.grid.solid[fixture.grid.cell_index(x, y, s)]);
                    assert!(fixture.grid.solid[fixture.grid.cell_index(x, y, 7 * s)]);
                }
            }
        }
    }
}

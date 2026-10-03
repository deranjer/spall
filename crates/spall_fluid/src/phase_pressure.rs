//! Experimental fine-grid reference coupling for ENG-122 phase placement.
//! Reuses the existing two-phase MAC/PLIC backend at the actual voxel size.
//! This is not the coarsened component solver or a production activation.
//! MAC velocity advection is semi-Lagrangian: this adapter does not establish
//! the conservative staggered-momentum accuracy gate of component_fluid.

use crate::SolidBoundary;
use crate::grid_mac::{MacConfig, MacError, MacGridWorld, MacStepMetrics, PressurePreconditioner};
use crate::phase_water::{PhaseError, PhaseWater};

#[derive(Debug, Clone, Copy)]
pub struct PhasePressureConfig {
    pub mac: MacConfig,
    pub air_density_kg_m3: f64,
    pub preconditioner: PressurePreconditioner,
    /// Retained phase + solver array cap, not transient pressure/clone/RSS cap.
    pub max_retained_array_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PhasePressureError {
    #[error("phase placement: {0}")]
    Phase(#[from] PhaseError),
    #[error("fine MAC reference: {0}")]
    Mac(#[from] MacError),
    #[error("phase/pressure cell size mismatch or nonfinite gravity")]
    InvalidConfig,
    #[error("pressure converged in {converged} of {substeps} substeps; residual {residual}")]
    PressureNotConverged {
        converged: u32,
        substeps: u32,
        residual: f64,
    },
    #[error("retained phase/solver arrays {requested} bytes exceed limit {limit}")]
    ArrayLimit { requested: usize, limit: usize },
}

#[derive(Debug, Clone)]
pub struct PhasePressureWorld {
    phase: PhaseWater,
    solver: MacGridWorld,
    max_retained_array_bytes: usize,
}

impl PhasePressureWorld {
    pub fn new(phase: PhaseWater, config: PhasePressureConfig) -> Result<Self, PhasePressureError> {
        if config.mac.cell_size_m.to_bits() != phase.voxel_size_m().to_bits()
            || config.mac.gravity_m_s2.iter().any(|g| !g.is_finite())
        {
            return Err(PhasePressureError::InvalidConfig);
        }
        let spec = phase.geometry().fine_spec();
        let dims = spec.dimensions().map(u128::from);
        // Check dense initial array storage before allocating the MAC copy.
        let face_slots = (dims[0] + 1) * dims[1] * dims[2]
            + dims[0] * (dims[1] + 1) * dims[2]
            + dims[0] * dims[1] * (dims[2] + 1);
        let initial_solver_bytes = spec.cell_count() as u128
            * (2 * size_of::<bool>() + 3 * size_of::<f64>()) as u128
            + face_slots * size_of::<f64>() as u128;
        let initial_bytes =
            usize::try_from(phase.array_storage_bytes() as u128 + initial_solver_bytes)
                .map_err(|_| PhasePressureError::InvalidConfig)?;
        check_bytes(initial_bytes, config.max_retained_array_bytes)?;
        let boundary = SolidBoundary {
            spec,
            solid: (0..spec.cell_count())
                .map(|i| phase.geometry().component_at(spec.cell_at(i)).is_none())
                .collect(),
        };
        let mut solver = MacGridWorld::new(&boundary, config.mac)?;
        solver.set_ambient_density(config.air_density_kg_m3)?;
        solver.set_pressure_preconditioner(config.preconditioner);
        solver.set_strict_phase_bounds();
        solver.restore_fractions(phase.fractions())?;
        let world = Self {
            phase,
            solver,
            max_retained_array_bytes: config.max_retained_array_bytes,
        };
        check_bytes(world.retained_array_bytes(), world.max_retained_array_bytes)?;
        Ok(world)
    }

    pub fn phase(&self) -> &PhaseWater {
        &self.phase
    }
    pub fn solver(&self) -> &MacGridWorld {
        &self.solver
    }
    /// Excludes shared exact geometry and transient solve/candidate allocations.
    /// Includes the canonical phase fractions and MAC's working fraction copy.
    pub fn retained_array_bytes(&self) -> usize {
        self.phase.array_storage_bytes() + self.solver.allocated_bytes()
    }

    /// Clone the numerical candidate to keep even late pressure, transport,
    /// phase-fragmentation and array-budget failures completely atomic.
    pub fn step(&mut self, dt: f64) -> Result<MacStepMetrics, PhasePressureError> {
        if !dt.is_finite() || dt <= 0.0 {
            return Err(MacError::InvalidTimeStep.into());
        }
        let mut solver = self.solver.clone();
        let metrics = solver.step(dt)?;
        if metrics.pressure_converged_substeps != metrics.substeps {
            return Err(PhasePressureError::PressureNotConverged {
                converged: metrics.pressure_converged_substeps,
                substeps: metrics.substeps,
                residual: metrics.pressure_residual_final_max,
            });
        }
        let phase = self.phase.with_fractions(solver.fractions().to_vec())?;
        check_bytes(
            phase.array_storage_bytes() + solver.allocated_bytes(),
            self.max_retained_array_bytes,
        )?;
        self.phase = phase;
        self.solver = solver;
        Ok(metrics)
    }
}

fn check_bytes(requested: usize, limit: usize) -> Result<(), PhasePressureError> {
    if requested > limit {
        Err(PhasePressureError::ArrayLimit { requested, limit })
    } else {
        Ok(())
    }
}

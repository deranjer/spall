//! Experimental fine-grid reference coupling for ENG-122 phase placement.
//! Reuses the existing two-phase MAC/PLIC backend at the actual voxel size.
//! This is not the coarsened component solver or a production activation.
//! Velocity advection defaults to semi-Lagrangian. An opt-in first-order
//! conservative staggered mixture transport is available; sealed-gas mass and
//! full interface accuracy remain acceptance gates.

use crate::SolidBoundary;
use crate::grid_mac::{MacConfig, MacError, MacGridWorld, MacStepMetrics, PressurePreconditioner};
use crate::phase_graph::GraphLimits;
use crate::phase_water::{PhaseError, PhaseWater};

#[derive(Debug, Clone, Copy)]
pub struct PhasePressureConfig {
    pub mac: MacConfig,
    pub air_density_kg_m3: f64,
    pub preconditioner: PressurePreconditioner,
    /// Retained phase + solver array upper-bound cap, not transient/clone/RSS.
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
        Self::new_with_air(phase, config, true)
    }

    /// Water-only comparison: dry space has atmospheric pressure even when
    /// enclosed. No gas mass, pressure rows or compression; air density is unused.
    /// Retains geometric PLIC/FCT water transport without an ambient fluid.
    pub fn new_water_only(
        phase: PhaseWater,
        config: PhasePressureConfig,
    ) -> Result<Self, PhasePressureError> {
        Self::new_with_air(phase, config, false)
    }

    fn new_with_air(
        phase: PhaseWater,
        config: PhasePressureConfig,
        two_phase: bool,
    ) -> Result<Self, PhasePressureError> {
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
        if two_phase {
            solver.set_ambient_density(config.air_density_kg_m3)?;
        } else {
            solver.set_freely_displaced_air()?;
        }
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

    /// ENG-122 diagnostic support model; reserve its retained face-flux arrays
    /// before enabling. This does not activate it in authoritative gameplay.
    pub fn enable_cut_surface_support(&mut self) -> Result<(), PhasePressureError> {
        if self.solver.cut_surface_support_enabled() {
            return Ok(());
        }
        let [nx, ny, nz] = self
            .phase
            .geometry()
            .fine_spec()
            .dimensions()
            .map(|v| v as usize);
        let slots = (nx + 1) * ny * nz + nx * (ny + 1) * nz + nx * ny * (nz + 1);
        let bytes = self
            .retained_array_bytes()
            .checked_add(slots * size_of::<f64>())
            .ok_or(PhasePressureError::InvalidConfig)?;
        check_bytes(bytes, self.max_retained_array_bytes)?;
        self.solver.set_cut_surface_support()?;
        Ok(())
    }

    /// Opt into accepted-flux staggered mixture momentum transport followed by
    /// a fine projection on the new phase. First-order experimental advection;
    /// no production/save/wire activation or full interface accuracy claim.
    pub fn enable_conservative_momentum(&mut self) -> Result<(), PhasePressureError> {
        self.solver.set_conservative_momentum()?;
        Ok(())
    }

    /// Existing fine-reference air-model comparison. Explicitly selecting
    /// incompressible air is not acceptance of the sealed-gas model.
    pub fn set_compressible_enclosed_air(
        &mut self,
        enabled: bool,
    ) -> Result<(), PhasePressureError> {
        self.solver.set_compressible_enclosed_air(enabled)?;
        Ok(())
    }

    /// Experimental physical Galerkin pressure predictor. Fine pressure
    /// correction, convergence, geometry and strict transport remain required.
    /// Enabling is atomic, including graph and retained-array budget failure.
    pub fn enable_pressure_predictor(
        &mut self,
        limits: GraphLimits,
        sweeps: u32,
    ) -> Result<(), PhasePressureError> {
        let mut solver = self.solver.clone();
        solver.set_phase_pressure_predictor(&self.phase, limits, sweeps)?;
        check_bytes(
            self.phase.array_storage_bytes() + solver.allocated_bytes(),
            self.max_retained_array_bytes,
        )?;
        self.solver = solver;
        Ok(())
    }
    /// Experimental balanced phase correction in every Krylov iteration.
    /// Reuses fine convergence/transport gates; extra work is per projection.
    pub fn enable_pressure_preconditioner(
        &mut self,
        limits: GraphLimits,
        sweeps: u32,
    ) -> Result<(), PhasePressureError> {
        let mut solver = self.solver.clone();
        solver.set_phase_pressure_preconditioner(&self.phase, limits, sweeps)?;
        check_bytes(
            self.phase.array_storage_bytes() + solver.allocated_bytes(),
            self.max_retained_array_bytes,
        )?;
        self.solver = solver;
        Ok(())
    }
    /// Excludes shared exact geometry and transient solve/candidate allocations.
    /// Includes the canonical phase fractions and MAC's working fraction copy.
    /// With a predictor, conservatively counts its fraction snapshot even when
    /// it aliases canonical phase. Shared packed faces are counted only once.
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

#[cfg(test)]
mod support_tests {
    use super::*;
    use crate::phase_fixtures::PhaseReferenceScene;
    #[test]
    fn support_activation_respects_array_budget_atomically_and_is_idempotent() {
        let phase = PhaseReferenceScene::LowDam.build().unwrap();
        let config = PhasePressureConfig {
            mac: MacConfig::default(),
            air_density_kg_m3: 1.2,
            preconditioner: PressurePreconditioner::Multigrid,
            max_retained_array_bytes: 100_000_000,
        };
        let mut world = PhasePressureWorld::new_water_only(phase, config).unwrap();
        world.max_retained_array_bytes = world.retained_array_bytes();
        let before = format!("{:?}", world.solver);
        assert!(matches!(
            world.enable_cut_surface_support(),
            Err(PhasePressureError::ArrayLimit { .. })
        ));
        assert_eq!(format!("{:?}", world.solver), before);
        world.max_retained_array_bytes = 100_000_000;
        world.enable_cut_surface_support().unwrap();
        world.step(0.01).unwrap();
        world.max_retained_array_bytes = world.retained_array_bytes();
        world.enable_cut_surface_support().unwrap();
        assert!(world.solver.cut_surface_support_enabled());
    }
}

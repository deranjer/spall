//! Salva DFSPH adapter for bounded CPU feasibility experiments.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use salva3d::LiquidWorld;
use salva3d::kernel::{CubicSplineKernel, Kernel};
use salva3d::math::Vector;
use salva3d::object::interaction_groups::InteractionGroups;
use salva3d::object::{Boundary, BoundaryHandle, Fluid, FluidHandle};
use salva3d::solver::DFSPHSolver;
use spall_core::GlobalCell;

use crate::{DomainSpec, SolidBoundary};

/// Fixed physical parameters for a bounded SPH feasibility run.
#[derive(Debug, Clone, Copy)]
pub struct SphConfig {
    pub particle_radius_m: f32,
    pub smoothing_factor: f32,
    pub density_kg_m3: f32,
    pub gravity_m_s2: [f32; 3],
    pub fixed_step_seconds: f32,
    pub fixed_substeps_per_tick: u32,
}

impl Default for SphConfig {
    fn default() -> Self {
        Self {
            particle_radius_m: 0.0625,
            smoothing_factor: 2.0,
            density_kg_m3: 1_000.0,
            gravity_m_s2: [0.0, -9.81, 0.0],
            fixed_step_seconds: 1.0 / 60.0,
            fixed_substeps_per_tick: 1,
        }
    }
}

/// A bounded fluid state using Salva DFSPH and voxel-derived static boundary
/// particles. This wrapper deliberately exposes snapshots/metrics rather than
/// Salva handles or storage types to the rest of Spall.
pub struct SphWorld {
    world: LiquidWorld,
    fluid: FluidHandle,
    boundary_handle: BoundaryHandle,
    domain: DomainSpec,
    solid_boundary: SolidBoundary,
    config: SphConfig,
    solid_cell_size_m: f32,
    domain_world_origin_m: [f32; 3],
    initial_particles: usize,
    initial_volume_m3: f64,
    elapsed: Duration,
    substeps: u64,
    cumulative_permitted_outflow_m3: f64,
    permitted_exit_particles: HashSet<usize>,
    first_invalid_crossing: Option<InvalidCrossing>,
    last_backend_call_time: Duration,
    last_diagnostic_time: Duration,
    last_boundary_rebuild_time: Duration,
    last_step_solid_penetration_particles: HashSet<usize>,
}

/// Report from one bounded advance operation.
#[derive(Debug, Clone, Copy)]
pub struct StepMetrics {
    /// Complete time spent in Salva's `LiquidWorld::step` call (all internal substeps).
    pub solver_call_time: Duration,
    /// Wrapper diagnostics and bookkeeping after the solver call.
    pub wrapper_diagnostic_time: Duration,
    pub wall_time: Duration,
    pub substeps: u64,
    pub particle_count: usize,
    pub particle_volume_m3: f64,
    pub volume_in_domain_m3: f64,
    /// Current particle volume outside the rectangular domain, including open top and invalid exits.
    pub current_outside_domain_volume_m3: f64,
    /// Cumulative volume of particles that first crossed the permitted open top.
    pub cumulative_permitted_outflow_m3: f64,
    pub solid_penetration_count: usize,
    pub intact_wall_penetration_count: usize,
    pub open_top_particle_count: usize,
    pub retained_particle_mass_kg: f64,
    pub absolute_balance_error_m3: f64,
    pub relative_balance_error: f64,
    pub particles_outside_domain: usize,
    pub active_cells: usize,
    pub max_speed_m_s: f64,
    pub speed_p95_m_s: f64,
    pub mean_absolute_density_error: f64,
    pub max_absolute_density_error: f64,
    pub density_sampled: bool,
    pub kinetic_energy_j: f64,
    pub surface_level_p95_m: f64,
    pub first_invalid_crossing: Option<InvalidCrossing>,
    pub pressure_iterations_per_tick: u64,
    pub divergence_iterations_per_tick: u64,
    pub pressure_converged_substeps: u32,
    pub divergence_converged_substeps: u32,
    pub pressure_residual_last: f64,
    pub divergence_residual_last: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossingKind {
    OpenTop,
    SolidPenetration,
    ClosedDomainExit,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InvalidCrossing {
    pub tick: u64,
    pub particle_index: usize,
    pub kind: CrossingKind,
    pub position_m: [f32; 3],
    pub velocity_m_s: [f32; 3],
    pub affected_voxel: GlobalCell,
}

/// Fully validated replacement boundary. Building it has no effect on the live Salva world.
pub struct PreparedBoundary {
    boundary: SolidBoundary,
    particles: Vec<Vector<f32>>,
    solid_cell_size_m: f32,
    domain_world_origin_m: [f32; 3],
}

impl SphWorld {
    /// Builds a fluid world from an immutable voxel boundary capture and an
    /// explicit set of initial water-particle positions in world metres.
    pub fn new(
        boundary: &SolidBoundary,
        solid_cell_size_m: f32,
        domain_world_origin_m: [f32; 3],
        particles: Vec<[f32; 3]>,
        config: SphConfig,
    ) -> Result<Self, SphError> {
        validate_config(config)?;
        if !solid_cell_size_m.is_finite() || solid_cell_size_m <= 0.0 {
            return Err(SphError::InvalidConfig);
        }
        if particles.is_empty() {
            return Err(SphError::NoWaterParticles);
        }
        if particles.iter().flatten().any(|value| !value.is_finite()) {
            return Err(SphError::InvalidConfig);
        }
        let boundary_particles = voxel_boundary_particles(
            boundary,
            solid_cell_size_m,
            domain_world_origin_m,
            config.particle_radius_m,
        );
        if boundary_particles.is_empty() {
            return Err(SphError::NoBoundaryParticles);
        }

        let r = config.particle_radius_m;
        let solver = DFSPHSolver::<CubicSplineKernel, CubicSplineKernel>::new();
        let mut world = LiquidWorld::new(solver, r, config.smoothing_factor, 0.0);
        let boundary_handle = world.add_boundary(Boundary::new(
            boundary_particles,
            InteractionGroups::default(),
        ));
        let water = particles
            .into_iter()
            .map(|p| Vector::new(p[0], p[1], p[2]))
            .collect::<Vec<_>>();
        let fluid = world.add_fluid(Fluid::new(
            water,
            r,
            config.density_kg_m3,
            InteractionGroups::default(),
        ));
        let particle_count = world
            .fluids()
            .get(fluid)
            .expect("new Salva handle")
            .num_particles();
        let initial_volume_m3 = world
            .fluids()
            .get(fluid)
            .expect("new Salva handle")
            .volumes
            .iter()
            .map(|v| f64::from(*v))
            .sum();

        Ok(Self {
            world,
            fluid,
            boundary_handle,
            domain: boundary.spec(),
            solid_boundary: boundary.clone(),
            config,
            solid_cell_size_m,
            domain_world_origin_m,
            initial_particles: particle_count,
            initial_volume_m3,
            elapsed: Duration::ZERO,
            substeps: 0,
            cumulative_permitted_outflow_m3: 0.0,
            permitted_exit_particles: HashSet::new(),
            first_invalid_crossing: None,
            last_backend_call_time: Duration::ZERO,
            last_diagnostic_time: Duration::ZERO,
            last_boundary_rebuild_time: Duration::ZERO,
            last_step_solid_penetration_particles: HashSet::new(),
        })
    }

    pub fn advance(&mut self) -> StepMetrics {
        let overall_started = Instant::now();
        let before = self.position_snapshot();
        let solver_started = Instant::now();
        let gravity = Vector::new(
            self.config.gravity_m_s2[0],
            self.config.gravity_m_s2[1],
            self.config.gravity_m_s2[2],
        );
        let n = self.config.fixed_substeps_per_tick.max(1);
        let mut pressure_iterations = 0u64;
        let mut divergence_iterations = 0u64;
        let mut pressure_converged = 0u32;
        let mut divergence_converged = 0u32;
        let mut pressure_residual = f64::NAN;
        let mut divergence_residual = f64::NAN;
        for _ in 0..n {
            self.world
                .step(self.config.fixed_step_seconds / n as f32, &gravity);
            self.substeps += self.world.counters.nsubsteps as u64;
            pressure_iterations += self.world.counters.solver.pressure_iterations as u64;
            divergence_iterations += self.world.counters.solver.divergence_iterations as u64;
            pressure_converged += u32::from(self.world.counters.solver.pressure_converged);
            divergence_converged += u32::from(self.world.counters.solver.divergence_converged);
            pressure_residual = f64::from(self.world.counters.solver.pressure_error);
            divergence_residual = f64::from(self.world.counters.solver.divergence_error);
        }
        let solver_call_time = solver_started.elapsed();
        self.elapsed += solver_call_time;
        self.record_crossings(&before);
        let mut metrics = self.collect_metrics(solver_call_time, false);
        let wrapper_diagnostic_time = overall_started.elapsed().saturating_sub(solver_call_time);
        self.last_backend_call_time = solver_call_time;
        self.last_diagnostic_time = wrapper_diagnostic_time;
        metrics.solver_call_time = solver_call_time;
        metrics.wrapper_diagnostic_time = wrapper_diagnostic_time;
        metrics.wall_time = solver_call_time + wrapper_diagnostic_time;
        metrics.substeps = u64::from(n);
        metrics.pressure_iterations_per_tick = pressure_iterations;
        metrics.divergence_iterations_per_tick = divergence_iterations;
        metrics.pressure_converged_substeps = pressure_converged;
        metrics.divergence_converged_substeps = divergence_converged;
        metrics.pressure_residual_last = pressure_residual;
        metrics.divergence_residual_last = divergence_residual;
        metrics
    }

    /// Atomically replaces static voxel boundary particles after the owning
    /// thread has committed a geometry edit and captured its new revision.
    /// The bounds must stay fixed, and a new solid may not overlap existing
    /// water particles; caller policy must explicitly displace or reject it.
    pub fn prepare_boundary(
        &self,
        boundary: &SolidBoundary,
        solid_cell_size_m: f32,
        domain_world_origin_m: [f32; 3],
    ) -> Result<PreparedBoundary, SphError> {
        if boundary.spec() != self.domain
            || !solid_cell_size_m.is_finite()
            || solid_cell_size_m <= 0.0
            || domain_world_origin_m.iter().any(|v| !v.is_finite())
        {
            return Err(SphError::InvalidConfig);
        }
        let positions = self
            .world
            .fluids()
            .get(self.fluid)
            .expect("fluid remains installed")
            .positions
            .clone();
        for position in positions {
            let cell = world_to_cell(
                [position.x, position.y, position.z],
                self.domain,
                solid_cell_size_m,
                domain_world_origin_m,
            );
            if boundary.is_solid(cell) == Some(true) {
                return Err(SphError::WaterInSolidCell(cell));
            }
        }
        let particles = voxel_boundary_particles(
            boundary,
            solid_cell_size_m,
            domain_world_origin_m,
            self.config.particle_radius_m,
        );
        if particles.is_empty() {
            return Err(SphError::NoBoundaryParticles);
        }
        Ok(PreparedBoundary {
            boundary: boundary.clone(),
            particles,
            solid_cell_size_m,
            domain_world_origin_m,
        })
    }

    /// Publishes a boundary that passed `prepare_boundary`; this has no failure path.
    pub fn commit_boundary(
        &mut self,
        prepared: PreparedBoundary,
        staged_build_time: Duration,
    ) -> Duration {
        let started = Instant::now();
        let new_handle = self.world.add_boundary(Boundary::new(
            prepared.particles,
            InteractionGroups::default(),
        ));
        self.world
            .remove_boundary(self.boundary_handle)
            .expect("current boundary remains installed");
        self.boundary_handle = new_handle;
        self.solid_boundary = prepared.boundary;
        self.solid_cell_size_m = prepared.solid_cell_size_m;
        self.domain_world_origin_m = prepared.domain_world_origin_m;
        self.last_boundary_rebuild_time = staged_build_time + started.elapsed();
        self.last_boundary_rebuild_time
    }

    pub const fn last_boundary_rebuild_time(&self) -> Duration {
        self.last_boundary_rebuild_time
    }

    pub fn metrics(&self, wall_time: Duration) -> StepMetrics {
        self.collect_metrics(wall_time, true)
    }

    fn collect_metrics(&self, wall_time: Duration, include_density: bool) -> StepMetrics {
        let fluid = self
            .world
            .fluids()
            .get(self.fluid)
            .expect("fluid remains installed");
        let particle_cell_size = f64::from(self.config.particle_radius_m * 2.0);
        let mut occupied = HashSet::with_capacity(fluid.positions.len());
        let mut particles_outside_domain = 0;
        let mut open_top_particle_count = 0;
        let mut speeds = Vec::with_capacity(fluid.velocities.len());
        let mut kinetic_energy_j = 0.0;
        let mut ys = Vec::with_capacity(fluid.positions.len());
        let mut mass = 0.0;
        let mut outside_volume = 0.0;
        for (index, (position, velocity)) in
            fluid.positions.iter().zip(&fluid.velocities).enumerate()
        {
            let cell = world_to_cell(
                [position.x, position.y, position.z],
                self.domain,
                self.solid_cell_size_m,
                self.domain_world_origin_m,
            );
            let inside = cell_in_spec(self.domain, cell);
            if !inside {
                particles_outside_domain += 1;
                outside_volume += f64::from(fluid.volumes[index]);
                if is_open_top_exit(self.domain, cell) {
                    open_top_particle_count += 1;
                }
            }
            occupied.insert((
                ((f64::from(position.x) - f64::from(self.domain_world_origin_m[0]))
                    / particle_cell_size)
                    .floor() as i64,
                ((f64::from(position.y) - f64::from(self.domain_world_origin_m[1]))
                    / particle_cell_size)
                    .floor() as i64,
                ((f64::from(position.z) - f64::from(self.domain_world_origin_m[2]))
                    / particle_cell_size)
                    .floor() as i64,
            ));
            let speed = f64::from(velocity.norm());
            speeds.push(speed);
            ys.push(f64::from(position.y));
            let particle_mass =
                f64::from(fluid.volumes[index]) * f64::from(self.config.density_kg_m3);
            mass += particle_mass;
            kinetic_energy_j += 0.5 * particle_mass * speed * speed;
        }
        speeds.sort_by(f64::total_cmp);
        let max_speed_m_s = speeds.last().copied().unwrap_or(0.0);
        let speed_p95_m_s = speeds
            .get(
                ((speeds.len().saturating_sub(1) as f64 * 0.95).ceil() as usize)
                    .min(speeds.len().saturating_sub(1)),
            )
            .copied()
            .unwrap_or(0.0);
        let particle_volume_m3 = fluid.volumes.iter().map(|v| f64::from(*v)).sum::<f64>();
        let solid_penetration_count = self.last_step_solid_penetration_particles.len();
        let intact_wall_penetration_count = solid_penetration_count;
        let volume_in_domain_m3 = particle_volume_m3 - outside_volume;
        ys.sort_by(f64::total_cmp);
        let surface_level_p95_m = ys
            .get(
                ((ys.len().saturating_sub(1) as f64 * 0.95).ceil() as usize)
                    .min(ys.len().saturating_sub(1)),
            )
            .copied()
            .unwrap_or(0.0);
        let (mean_absolute_density_error, max_absolute_density_error) = if include_density {
            self.density_errors()
        } else {
            (f64::NAN, f64::NAN)
        };
        let absolute_balance_error_m3 = (self.initial_volume_m3 - particle_volume_m3).abs();
        StepMetrics {
            solver_call_time: self.last_backend_call_time,
            wrapper_diagnostic_time: self.last_diagnostic_time,
            wall_time,
            substeps: self.world.counters.nsubsteps as u64,
            particle_count: fluid.positions.len(),
            particle_volume_m3,
            volume_in_domain_m3,
            current_outside_domain_volume_m3: outside_volume,
            cumulative_permitted_outflow_m3: self.cumulative_permitted_outflow_m3,
            solid_penetration_count,
            intact_wall_penetration_count,
            open_top_particle_count,
            retained_particle_mass_kg: mass,
            absolute_balance_error_m3,
            relative_balance_error: absolute_balance_error_m3 / self.initial_volume_m3,
            particles_outside_domain,
            active_cells: occupied.len(),
            max_speed_m_s,
            speed_p95_m_s,
            mean_absolute_density_error,
            max_absolute_density_error,
            density_sampled: include_density,
            kinetic_energy_j,
            surface_level_p95_m,
            first_invalid_crossing: self.first_invalid_crossing,
            pressure_iterations_per_tick: 0,
            divergence_iterations_per_tick: 0,
            pressure_converged_substeps: 0,
            divergence_converged_substeps: 0,
            pressure_residual_last: f64::NAN,
            divergence_residual_last: f64::NAN,
        }
    }

    fn record_crossings(&mut self, before: &[[f32; 3]]) {
        self.last_step_solid_penetration_particles.clear();
        let fluid = self
            .world
            .fluids()
            .get(self.fluid)
            .expect("fluid remains installed");
        for (i, p) in fluid.positions.iter().enumerate() {
            let now = [p.x, p.y, p.z];
            let cell = world_to_cell(
                now,
                self.domain,
                self.solid_cell_size_m,
                self.domain_world_origin_m,
            );
            let prior = before.get(i).copied().unwrap_or(now);
            let prior_cell = world_to_cell(
                prior,
                self.domain,
                self.solid_cell_size_m,
                self.domain_world_origin_m,
            );
            let kind = if !cell_in_spec(self.domain, cell) && is_open_top_exit(self.domain, cell) {
                if self.permitted_exit_particles.insert(i) {
                    self.cumulative_permitted_outflow_m3 += f64::from(fluid.volumes[i]);
                }
                continue;
            } else if !cell_in_spec(self.domain, cell) {
                Some(CrossingKind::ClosedDomainExit)
            } else if self.solid_boundary.is_solid(cell) == Some(true) {
                self.last_step_solid_penetration_particles.insert(i);
                Some(CrossingKind::SolidPenetration)
            } else {
                None
            };
            if let Some(kind) = kind {
                let had_invalid = !cell_in_spec(self.domain, prior_cell)
                    || self.solid_boundary.is_solid(prior_cell) == Some(true);
                if !had_invalid && self.first_invalid_crossing.is_none() {
                    self.first_invalid_crossing = Some(InvalidCrossing {
                        tick: self.substeps,
                        particle_index: i,
                        kind,
                        position_m: now,
                        velocity_m_s: [
                            fluid.velocities[i].x,
                            fluid.velocities[i].y,
                            fluid.velocities[i].z,
                        ],
                        affected_voxel: cell,
                    });
                }
            }
            // Sample the complete swept segment so a particle cannot tunnel through a thin
            // intact wall and become invisible to endpoint occupancy checks.
            let dx = now[0] - prior[0];
            let dy = now[1] - prior[1];
            let dz = now[2] - prior[2];
            let distance = (dx * dx + dy * dy + dz * dz).sqrt();
            let sample_step = (self.solid_cell_size_m * 0.25).max(0.001);
            let sample_count = (distance / sample_step).ceil().max(1.0) as usize;
            for sample in 1..sample_count {
                let t = sample as f32 / sample_count as f32;
                let swept = [prior[0] + dx * t, prior[1] + dy * t, prior[2] + dz * t];
                let swept_cell = world_to_cell(
                    swept,
                    self.domain,
                    self.solid_cell_size_m,
                    self.domain_world_origin_m,
                );
                if self.solid_boundary.is_solid(swept_cell) == Some(true) {
                    self.last_step_solid_penetration_particles.insert(i);
                    if self.first_invalid_crossing.is_none() {
                        self.first_invalid_crossing = Some(InvalidCrossing {
                            tick: self.substeps,
                            particle_index: i,
                            kind: CrossingKind::SolidPenetration,
                            position_m: swept,
                            velocity_m_s: [
                                fluid.velocities[i].x,
                                fluid.velocities[i].y,
                                fluid.velocities[i].z,
                            ],
                            affected_voxel: swept_cell,
                        });
                    }
                    break;
                }
            }
        }
    }

    /// Independent post-step density diagnostic using Salva's configured
    /// cubic-spline kernel. This estimates the positive density error used by
    /// DFSPH's stopping rule; Salva does not expose the actual iteration count.
    pub fn density_errors(&self) -> (f64, f64) {
        let Some(fluid) = self.world.fluids().get(self.fluid) else {
            return (f64::NAN, f64::NAN);
        };
        let Some((_, boundary)) = self.world.boundaries().iter().next() else {
            return (f64::NAN, f64::NAN);
        };
        let h = self.world.h();
        let rest_density = self.config.density_kg_m3;
        let mut sum = 0.0_f64;
        let mut max = 0.0_f64;
        for position in &fluid.positions {
            let mut density = 0.0_f64;
            for (j, other) in fluid.positions.iter().enumerate() {
                let distance = (position - other).norm();
                if distance <= h {
                    density += f64::from(fluid.volumes[j] * rest_density)
                        * f64::from(CubicSplineKernel::scalar_apply(distance, h));
                }
            }
            for (j, other) in boundary.positions.iter().enumerate() {
                let distance = (position - other).norm();
                if distance <= h {
                    density += f64::from(boundary.volumes[j] * rest_density)
                        * f64::from(CubicSplineKernel::scalar_apply(distance, h));
                }
            }
            let error = (density / f64::from(rest_density) - 1.0).abs();
            sum += error;
            max = max.max(error);
        }
        let count = fluid.positions.len().max(1) as f64;
        (sum / count, max)
    }

    pub const fn initial_particles(&self) -> usize {
        self.initial_particles
    }

    pub const fn initial_volume_m3(&self) -> f64 {
        self.initial_volume_m3
    }

    pub const fn world_particle_spacing_m(&self) -> f32 {
        self.config.particle_radius_m * 2.0
    }

    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    pub const fn total_substeps(&self) -> u64 {
        self.substeps
    }

    /// Lower-bound estimate for retained particle/boundary arrays. Salva's
    /// internal spatial and pressure-solver work buffers are intentionally
    /// excluded and must be included by process-level profiling.
    pub fn particle_array_bytes(&self) -> usize {
        let fluid = self
            .world
            .fluids()
            .get(self.fluid)
            .expect("fluid remains installed");
        let boundary = self
            .world
            .boundaries()
            .get(self.boundary_handle)
            .expect("boundary remains installed");
        fluid.positions.capacity() * size_of::<Vector<f32>>()
            + fluid.velocities.capacity() * size_of::<Vector<f32>>()
            + fluid.accelerations.capacity() * size_of::<Vector<f32>>()
            + fluid.volumes.capacity() * size_of::<f32>()
            + fluid.deleted_particles.capacity() * size_of::<bool>()
            + boundary.positions.capacity() * size_of::<Vector<f32>>()
            + boundary.velocities.capacity() * size_of::<Vector<f32>>()
            + boundary.volumes.capacity() * size_of::<f32>()
    }

    pub fn position_snapshot(&self) -> Vec<[f32; 3]> {
        self.world
            .fluids()
            .get(self.fluid)
            .expect("fluid remains installed")
            .positions
            .iter()
            .map(|p| [p.x, p.y, p.z])
            .collect()
    }

    pub fn maximum_x_velocity_m_s(&self) -> f32 {
        self.world
            .fluids()
            .get(self.fluid)
            .expect("fluid remains installed")
            .velocities
            .iter()
            .map(|v| v.x)
            .fold(f32::NEG_INFINITY, f32::max)
    }

    pub fn solid_boundary(&self) -> &SolidBoundary {
        &self.solid_boundary
    }

    pub fn eastward_momentum_kg_m_s(&self) -> f64 {
        let fluid = self
            .world
            .fluids()
            .get(self.fluid)
            .expect("fluid remains installed");
        let dam_x = self.domain.origin().x as f32
            + self.domain.dimensions()[0] as f32 * self.solid_cell_size_m * 0.5;
        fluid
            .positions
            .iter()
            .zip(&fluid.velocities)
            .enumerate()
            .filter(|(_, (p, v))| p.x > dam_x && v.x > 0.0)
            .map(|(i, (_, v))| f64::from(fluid.volumes[i] * self.config.density_kg_m3 * v.x))
            .sum()
    }

    pub fn reservoir_p95_levels_m(&self, dam_x_cell: i64) -> (f64, f64) {
        let fluid = self
            .world
            .fluids()
            .get(self.fluid)
            .expect("fluid remains installed");
        let mut west = Vec::new();
        let mut east = Vec::new();
        let dam_x_m = dam_x_cell as f32 * self.solid_cell_size_m;
        for p in &fluid.positions {
            if p.x < dam_x_m {
                west.push(f64::from(p.y));
            } else {
                east.push(f64::from(p.y));
            }
        }
        fn p95(v: &mut [f64]) -> f64 {
            v.sort_by(f64::total_cmp);
            v.get(
                ((v.len().saturating_sub(1) as f64 * 0.95).ceil() as usize)
                    .min(v.len().saturating_sub(1)),
            )
            .copied()
            .unwrap_or(0.0)
        }
        (p95(&mut west), p95(&mut east))
    }

    pub fn kernel_support_radius_m(&self) -> f32 {
        self.world.h()
    }

    pub fn dfsph_parameters(&self) -> Option<salva3d::solver::DfsphParameters> {
        self.world.dfsph_parameters()
    }

    pub fn set_fixed_substeps_per_tick(&mut self, substeps: u32) -> Result<(), SphError> {
        if substeps == 0 {
            return Err(SphError::InvalidConfig);
        }
        self.config.fixed_substeps_per_tick = substeps;
        Ok(())
    }

    pub const fn configured_tick_seconds(&self) -> f32 {
        self.config.fixed_step_seconds
    }
    pub const fn fixed_substeps_per_tick(&self) -> u32 {
        self.config.fixed_substeps_per_tick
    }

    /// Salva's per-stage timers currently report zero because timer clock calls are disabled upstream.
    pub fn salva_solver_timer_seconds(&self) -> f64 {
        self.world.counters.stages.solver_time.time()
    }
}

/// Places a regular particle lattice within a half-open cell-aligned box,
/// excluding captured solids. Particle centers are offset by half a spacing
/// and use the same global origin as the boundary snapshot.
pub fn water_lattice_in_cells(
    boundary: &SolidBoundary,
    cell_size_m: f32,
    world_origin_m: [f32; 3],
    cell_min: GlobalCell,
    cell_max_exclusive: GlobalCell,
    particle_radius_m: f32,
) -> Result<Vec<[f32; 3]>, SphError> {
    if !cell_size_m.is_finite()
        || cell_size_m <= 0.0
        || !particle_radius_m.is_finite()
        || particle_radius_m <= 0.0
        || world_origin_m.iter().any(|v| !v.is_finite())
    {
        return Err(SphError::InvalidConfig);
    }
    let spec = boundary.spec();
    if cell_min.x >= cell_max_exclusive.x
        || cell_min.y >= cell_max_exclusive.y
        || cell_min.z >= cell_max_exclusive.z
        || !cell_in_spec(spec, cell_min)
        || !cell_in_spec(
            spec,
            GlobalCell::new(
                cell_max_exclusive.x - 1,
                cell_max_exclusive.y - 1,
                cell_max_exclusive.z - 1,
            ),
        )
    {
        return Err(SphError::WaterBoxOutsideDomain);
    }
    let spacing = particle_radius_m * 2.0;
    let per_cell = (cell_size_m / spacing).floor() as usize;
    if per_cell == 0 {
        return Err(SphError::InvalidConfig);
    }
    let mut particles = Vec::new();
    for z in cell_min.z..cell_max_exclusive.z {
        for y in cell_min.y..cell_max_exclusive.y {
            for x in cell_min.x..cell_max_exclusive.x {
                let cell = GlobalCell::new(x, y, z);
                if boundary.is_solid(cell) != Some(false) {
                    continue;
                }
                for iz in 0..per_cell {
                    for iy in 0..per_cell {
                        for ix in 0..per_cell {
                            let half = spacing * 0.5;
                            let p = [
                                world_origin_m[0]
                                    + x as f32 * cell_size_m
                                    + half
                                    + ix as f32 * spacing,
                                world_origin_m[1]
                                    + y as f32 * cell_size_m
                                    + half
                                    + iy as f32 * spacing,
                                world_origin_m[2]
                                    + z as f32 * cell_size_m
                                    + half
                                    + iz as f32 * spacing,
                            ];
                            particles.push(p);
                        }
                    }
                }
            }
        }
    }
    if particles.is_empty() {
        return Err(SphError::NoWaterParticles);
    }
    Ok(particles)
}

fn voxel_boundary_particles(
    boundary: &SolidBoundary,
    cell_size_m: f32,
    world_origin_m: [f32; 3],
    particle_radius_m: f32,
) -> Vec<Vector<f32>> {
    let spec = boundary.spec();
    let dims = spec.dimensions();
    let origin = spec.origin();
    let per_cell = (cell_size_m / (2.0 * particle_radius_m)).ceil() as usize;
    let face_spacing = cell_size_m / per_cell as f32;
    let mut points = Vec::new();
    let mut seen = HashSet::new();
    for z in 0..dims[2] {
        for y in 0..dims[1] {
            for x in 0..dims[0] {
                let cell = GlobalCell::new(
                    origin.x + i64::from(x),
                    origin.y + i64::from(y),
                    origin.z + i64::from(z),
                );
                if boundary.is_solid(cell) != Some(true) {
                    continue;
                }
                for (axis, direction) in [
                    (0usize, [-1i64, 0, 0]),
                    (0, [1, 0, 0]),
                    (1, [0, -1, 0]),
                    (1, [0, 1, 0]),
                    (2, [0, 0, -1]),
                    (2, [0, 0, 1]),
                ] {
                    let neighbor = GlobalCell::new(
                        cell.x + direction[0],
                        cell.y + direction[1],
                        cell.z + direction[2],
                    );
                    if boundary.is_solid(neighbor) != Some(false) {
                        continue;
                    }
                    let sign = if direction[axis] < 0 { 0.0 } else { 1.0 };
                    let other_axes = match axis {
                        0 => [1, 2],
                        1 => [0, 2],
                        _ => [0, 1],
                    };
                    for v in 0..per_cell {
                        for u in 0..per_cell {
                            let mut p = [0.0; 3];
                            let coord = [cell.x, cell.y, cell.z];
                            p[axis] = world_origin_m[axis]
                                + coord[axis] as f32 * cell_size_m
                                + sign * cell_size_m;
                            for (sample_axis, sample_index) in
                                [(other_axes[0], v), (other_axes[1], u)]
                            {
                                p[sample_axis] = world_origin_m[sample_axis]
                                    + coord[sample_axis] as f32 * cell_size_m
                                    + sample_index as f32 * face_spacing
                                    + face_spacing * 0.5;
                            }
                            let key = (
                                (p[0] * 1_000_000.0).round() as i64,
                                (p[1] * 1_000_000.0).round() as i64,
                                (p[2] * 1_000_000.0).round() as i64,
                            );
                            if seen.insert(key) {
                                points.push(Vector::new(p[0], p[1], p[2]));
                            }
                        }
                    }
                }
            }
        }
    }
    points
}

fn cell_in_spec(spec: DomainSpec, cell: GlobalCell) -> bool {
    let origin = spec.origin();
    let dims = spec.dimensions();
    cell.x
        .checked_sub(origin.x)
        .is_some_and(|x| x >= 0 && x < i64::from(dims[0]))
        && cell
            .y
            .checked_sub(origin.y)
            .is_some_and(|y| y >= 0 && y < i64::from(dims[1]))
        && cell
            .z
            .checked_sub(origin.z)
            .is_some_and(|z| z >= 0 && z < i64::from(dims[2]))
}

fn is_open_top_exit(spec: DomainSpec, cell: GlobalCell) -> bool {
    let origin = spec.origin();
    let dims = spec.dimensions();
    cell.y >= origin.y + i64::from(dims[1])
        && cell.x >= origin.x
        && cell.x < origin.x + i64::from(dims[0])
        && cell.z >= origin.z
        && cell.z < origin.z + i64::from(dims[2])
}

fn world_to_cell(
    position: [f32; 3],
    spec: DomainSpec,
    cell_size_m: f32,
    world_origin_m: [f32; 3],
) -> GlobalCell {
    let origin = spec.origin();
    GlobalCell::new(
        origin.x.saturating_add(
            ((f64::from(position[0]) - f64::from(world_origin_m[0])) / f64::from(cell_size_m))
                .floor() as i64,
        ),
        origin.y.saturating_add(
            ((f64::from(position[1]) - f64::from(world_origin_m[1])) / f64::from(cell_size_m))
                .floor() as i64,
        ),
        origin.z.saturating_add(
            ((f64::from(position[2]) - f64::from(world_origin_m[2])) / f64::from(cell_size_m))
                .floor() as i64,
        ),
    )
}

fn validate_config(config: SphConfig) -> Result<(), SphError> {
    if !config.particle_radius_m.is_finite()
        || config.particle_radius_m <= 0.0
        || !config.smoothing_factor.is_finite()
        || config.smoothing_factor < 1.0
        || !config.density_kg_m3.is_finite()
        || config.density_kg_m3 <= 0.0
        || !config.fixed_step_seconds.is_finite()
        || config.fixed_step_seconds <= 0.0
        || config.fixed_substeps_per_tick == 0
        || config.gravity_m_s2.iter().any(|v| !v.is_finite())
    {
        return Err(SphError::InvalidConfig);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SphError {
    #[error("invalid SPH configuration or non-finite coordinate")]
    InvalidConfig,
    #[error("water particle set is empty")]
    NoWaterParticles,
    #[error("voxel-derived boundary has no fluid-facing surface particles")]
    NoBoundaryParticles,
    #[error("requested water box lies outside the captured fluid domain")]
    WaterBoxOutsideDomain,
    #[error("committed solid overlaps an existing water particle at {0:?}")]
    WaterInSolidCell(GlobalCell),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DomainSpec;
    use spall_core::{BrickCoord, CellSizeCode, MaterialId, Revision, VolumeId};
    use spall_voxel::{Brick, EditPlan, Volume};

    #[test]
    fn dfsph_keeps_water_particles_and_respects_voxel_box_boundaries() {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [12, 8, 8], 2_000).unwrap();
        let volume_id = VolumeId::new(103).unwrap();
        let mut volume = Volume::new(volume_id, CellSizeCode::Quarter);
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        let mut walls = EditPlan::new(volume_id);
        for z in 0..8 {
            for x in 0..12 {
                walls.set(GlobalCell::new(x, 0, z), MaterialId(1));
                if z == 0 || z == 7 || x == 0 || x == 11 {
                    for y in 1..8 {
                        walls.set(GlobalCell::new(x, y, z), MaterialId(1));
                    }
                }
            }
        }
        volume.apply_edit(&walls).unwrap();
        let boundary = crate::SolidBoundary::capture(&volume, spec).unwrap();
        let config = SphConfig {
            fixed_step_seconds: 1.0 / 120.0,
            ..SphConfig::default()
        };
        let particles = water_lattice_in_cells(
            &boundary,
            0.25,
            [0.0; 3],
            GlobalCell::new(2, 1, 2),
            GlobalCell::new(6, 4, 6),
            config.particle_radius_m,
        )
        .unwrap();
        let mut fluid = SphWorld::new(&boundary, 0.25, [0.0; 3], particles, config).unwrap();
        let count = fluid.initial_particles();
        let initial_y = fluid
            .position_snapshot()
            .iter()
            .map(|p| f64::from(p[1]))
            .sum::<f64>()
            / count as f64;
        let mut final_metrics = fluid.metrics(Duration::ZERO);
        for _ in 0..24 {
            final_metrics = fluid.advance();
        }
        let final_y = fluid
            .position_snapshot()
            .iter()
            .map(|p| f64::from(p[1]))
            .sum::<f64>()
            / count as f64;

        assert_eq!(final_metrics.particle_count, count);
        assert_eq!(final_metrics.particles_outside_domain, 0);
        assert!(final_metrics.active_cells > 0);
        assert!(final_metrics.pressure_iterations_per_tick > 0);
        assert!(final_metrics.divergence_iterations_per_tick > 0);
        assert!(final_metrics.pressure_residual_last.is_finite());
        assert!(final_metrics.divergence_residual_last.is_finite());
        assert!(
            final_y < initial_y,
            "gravity must move the finite water down"
        );
        assert!((final_metrics.particle_volume_m3 - fluid.initial_volume_m3()).abs() < 1.0e-9);
    }
}

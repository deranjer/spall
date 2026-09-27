//! Bounded finite-reservoir fixtures built from authoritative Spall voxels.

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_voxel::{Brick, EditError, EditPlan, Volume};

use crate::sph::{SphConfig, SphError, SphWorld, water_lattice_in_cells};
use crate::{BoundaryError, DomainError, DomainSpec, SolidBoundary};

pub const RESERVOIR_DIMENSIONS: [u32; 3] = [24, 12, 8];
pub const RESERVOIR_CELL_SIZE_M: f32 = 0.25;
pub const RESERVOIR_PARTICLE_RADIUS_M: f32 = 0.0625;
const SOLID_MATERIAL: MaterialId = MaterialId(1);

/// Finite two-reservoir scene with a voxel-authored dam, floor, and outer
/// side/end walls. Geometry edits are committed to `Volume` before the
/// Salva boundary snapshot is replaced.
pub struct TwoReservoirFixture {
    volume: Volume,
    spec: DomainSpec,
    fluid: SphWorld,
}

impl TwoReservoirFixture {
    pub fn new() -> Result<Self, FixtureError> {
        Self::new_scaled(1, 1.0 / 60.0)
    }

    pub fn new_scaled(scale: u32, fixed_step_seconds: f32) -> Result<Self, FixtureError> {
        Self::new_configured(scale, fixed_step_seconds, false, false)
    }

    /// Flat, pre-filled basin used to measure stability independently of block collapse.
    pub fn new_stability_basin(scale: u32, fixed_step_seconds: f32) -> Result<Self, FixtureError> {
        Self::new_configured(scale, fixed_step_seconds, true, false)
    }

    pub fn new_tunnel_under_separate_pool(
        scale: u32,
        fixed_step_seconds: f32,
    ) -> Result<Self, FixtureError> {
        Self::new_configured(scale, fixed_step_seconds, false, true)
    }

    fn new_configured(
        scale: u32,
        fixed_step_seconds: f32,
        stability_basin: bool,
        tunnel_pool: bool,
    ) -> Result<Self, FixtureError> {
        if scale == 0 || !fixed_step_seconds.is_finite() || fixed_step_seconds <= 0.0 {
            return Err(FixtureError::InvalidParameters);
        }
        let dimensions: [u32; 3] = RESERVOIR_DIMENSIONS
            .map(|v| v.checked_mul(scale).ok_or(FixtureError::InvalidParameters))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .map_err(|_| FixtureError::InvalidParameters)?;
        let spec = DomainSpec::new(
            GlobalCell::new(0, 0, 0),
            dimensions,
            dimensions.iter().map(|v| *v as usize).product(),
        )?;
        let volume_id = VolumeId::new(0x103).expect("nonzero fixture volume id");
        let mut volume = Volume::new(volume_id, CellSizeCode::Quarter);
        let brick_dims = dimensions.map(|v| v.div_ceil(32));
        for z in 0..brick_dims[2] as i64 {
            for y in 0..brick_dims[1] as i64 {
                for x in 0..brick_dims[0] as i64 {
                    volume.insert_brick(
                        BrickCoord::new(x, y, z),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )?;
                }
            }
        }
        let mut solids = EditPlan::new(volume_id);
        for z in 0..dimensions[2] as i64 {
            for x in 0..dimensions[0] as i64 {
                solids.set(GlobalCell::new(x, 0, z), SOLID_MATERIAL);
            }
            for y in 1..dimensions[1] as i64 {
                solids.set(GlobalCell::new(0, y, z), SOLID_MATERIAL);
                solids.set(
                    GlobalCell::new(dimensions[0] as i64 - 1, y, z),
                    SOLID_MATERIAL,
                );
            }
        }
        for x in 0..dimensions[0] as i64 {
            for y in 1..dimensions[1] as i64 {
                solids.set(GlobalCell::new(x, y, 0), SOLID_MATERIAL);
                solids.set(
                    GlobalCell::new(x, y, dimensions[2] as i64 - 1),
                    SOLID_MATERIAL,
                );
            }
        }
        let dam_x = 12 * scale as i64;
        for y in 1..(10 * scale) as i64 {
            for z in 1..dimensions[2] as i64 - 1 {
                solids.set(GlobalCell::new(dam_x, y, z), SOLID_MATERIAL);
            }
        }
        if tunnel_pool {
            for z in scale as i64..dimensions[2] as i64 - scale as i64 {
                for x in 14 * scale as i64..21 * scale as i64 {
                    solids.set(GlobalCell::new(x, 4 * scale as i64, z), SOLID_MATERIAL);
                }
            }
            // Start the side walls immediately above the tunnel roof. Scaling
            // the roof layer by itself leaves an air-course gap at scale > 1.
            for y in 4 * scale as i64 + 1..10 * scale as i64 {
                for z in scale as i64..7 * scale as i64 {
                    solids.set(GlobalCell::new(14 * scale as i64, y, z), SOLID_MATERIAL);
                    solids.set(GlobalCell::new(20 * scale as i64, y, z), SOLID_MATERIAL);
                }
                for x in 14 * scale as i64..21 * scale as i64 {
                    solids.set(GlobalCell::new(x, y, scale as i64), SOLID_MATERIAL);
                    solids.set(GlobalCell::new(x, y, 7 * scale as i64), SOLID_MATERIAL);
                }
            }
        }
        volume.apply_edit(&solids)?;
        let boundary = SolidBoundary::capture(&volume, spec)?;
        let config = SphConfig {
            particle_radius_m: RESERVOIR_PARTICLE_RADIUS_M,
            fixed_step_seconds,
            ..SphConfig::default()
        };
        let z_min = 2 * scale as i64;
        let z_max = 6 * scale as i64;
        let left_min = if stability_basin {
            GlobalCell::new(2 * scale as i64, 1, scale as i64)
        } else {
            GlobalCell::new(3 * scale as i64, 1, z_min)
        };
        let left_max = if stability_basin {
            GlobalCell::new(
                11 * scale as i64,
                4 * scale as i64,
                dimensions[2] as i64 - 1,
            )
        } else {
            GlobalCell::new(11 * scale as i64, 5 * scale as i64, z_max)
        };
        let mut particles = water_lattice_in_cells(
            &boundary,
            RESERVOIR_CELL_SIZE_M,
            [0.0; 3],
            left_min,
            left_max,
            config.particle_radius_m,
        )?;
        if !stability_basin {
            particles.extend(water_lattice_in_cells(
                &boundary,
                RESERVOIR_CELL_SIZE_M,
                [0.0; 3],
                GlobalCell::new(14 * scale as i64, 1, z_min),
                GlobalCell::new(21 * scale as i64, 3 * scale as i64, z_max),
                config.particle_radius_m,
            )?);
        }
        if tunnel_pool {
            particles.extend(water_lattice_in_cells(
                &boundary,
                RESERVOIR_CELL_SIZE_M,
                [0.0; 3],
                GlobalCell::new(15 * scale as i64, 5 * scale as i64, 2 * scale as i64),
                GlobalCell::new(20 * scale as i64, 7 * scale as i64, 6 * scale as i64),
                config.particle_radius_m,
            )?);
        }
        let fluid = SphWorld::new(
            &boundary,
            RESERVOIR_CELL_SIZE_M,
            [0.0; 3],
            particles,
            config,
        )?;
        Ok(Self {
            volume,
            spec,
            fluid,
        })
    }

    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// Captured grid bounds used by the finite-reservoir fixture.
    pub const fn domain(&self) -> DomainSpec {
        self.spec
    }

    pub fn fluid(&self) -> &SphWorld {
        &self.fluid
    }

    pub fn fluid_mut(&mut self) -> &mut SphWorld {
        &mut self.fluid
    }

    /// Opens a low 0.5 m canal across the dam. Two cells are excavated from
    /// the existing voxel dam, then the immutable Salva boundary is rebuilt.
    pub fn excavate_canal(&mut self) -> Result<(), FixtureError> {
        let scale = (self.spec.dimensions()[0] / RESERVOIR_DIMENSIONS[0]) as i64;
        let dam_x = 12 * scale;
        let mid_z = self.spec.dimensions()[2] as i64 / 2;
        self.commit_opening(dam_x, 1..2 * scale, mid_z - scale..mid_z + scale)
    }

    /// Makes a larger breach spanning 1.0 m in height and width for the
    /// momentum/surge scenario.
    pub fn break_dam(&mut self) -> Result<(), FixtureError> {
        let scale = (self.spec.dimensions()[0] / RESERVOIR_DIMENSIONS[0]) as i64;
        let dam_x = 12 * scale;
        let mid_z = self.spec.dimensions()[2] as i64 / 2;
        self.commit_opening(dam_x, 1..5 * scale, mid_z - 2 * scale..mid_z + 2 * scale)
    }

    pub fn close_canal(&mut self) -> Result<(), FixtureError> {
        let rebuild_started = std::time::Instant::now();
        let scale = (self.spec.dimensions()[0] / RESERVOIR_DIMENSIONS[0]) as i64;
        let dam_x = 12 * scale;
        let mid_z = self.spec.dimensions()[2] as i64 / 2;
        let mut edit = EditPlan::new(self.volume.id());
        for y in 1..2 * scale {
            for z in mid_z - scale..mid_z + scale {
                edit.set(GlobalCell::new(dam_x, y, z), SOLID_MATERIAL);
            }
        }
        let mut staged = self.volume.clone();
        staged.apply_edit(&edit)?;
        let boundary = SolidBoundary::capture(&staged, self.spec)?;
        let prepared = self
            .fluid
            .prepare_boundary(&boundary, RESERVOIR_CELL_SIZE_M, [0.0; 3])?;
        self.fluid
            .commit_boundary(prepared, rebuild_started.elapsed());
        self.volume = staged;
        Ok(())
    }

    fn commit_opening(
        &mut self,
        dam_x: i64,
        y_range: std::ops::Range<i64>,
        z_range: std::ops::Range<i64>,
    ) -> Result<(), FixtureError> {
        let rebuild_started = std::time::Instant::now();
        let mut edit = EditPlan::new(self.volume.id());
        for y in y_range {
            for z in z_range.clone() {
                edit.set(GlobalCell::new(dam_x, y, z), MaterialId::AIR);
            }
        }
        // Stage both authoritative voxel data and the Salva boundary before publishing either.
        let mut staged_volume = self.volume.clone();
        staged_volume.apply_edit(&edit)?;
        let boundary = SolidBoundary::capture(&staged_volume, self.spec)?;
        let prepared = self
            .fluid
            .prepare_boundary(&boundary, RESERVOIR_CELL_SIZE_M, [0.0; 3])?;
        self.fluid
            .commit_boundary(prepared, rebuild_started.elapsed());
        self.volume = staged_volume;
        Ok(())
    }

    pub fn particles_east_of_dam(&self) -> usize {
        let dam_x_m = (self.spec.dimensions()[0] / RESERVOIR_DIMENSIONS[0]) as f32 * 3.0;
        self.fluid
            .position_snapshot()
            .iter()
            .filter(|p| p[0] > dam_x_m)
            .count()
    }

    pub fn maximum_eastward_speed(&self) -> f32 {
        self.fluid.maximum_x_velocity_m_s()
    }

    pub fn elevated_pool_particle_count(&self) -> usize {
        self.fluid
            .position_snapshot()
            .iter()
            .filter(|p| p[0] > 3.5 && p[1] > 1.25)
            .count()
    }

    pub fn eastward_momentum_kg_m_s(&self) -> f64 {
        self.fluid.eastward_momentum_kg_m_s()
    }

    pub fn reservoir_p95_levels_m(&self) -> (f64, f64) {
        self.fluid.reservoir_p95_levels_m(
            (12 * (self.spec.dimensions()[0] / RESERVOIR_DIMENSIONS[0])) as i64,
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("scale must be nonzero and tick duration finite/positive")]
    InvalidParameters,
    #[error("invalid bounded fixture domain: {0}")]
    Domain(#[from] DomainError),
    #[error("voxel storage failed: {0}")]
    VoxelAccess(#[from] spall_voxel::AccessError),
    #[error("voxel edit failed: {0}")]
    Edit(#[from] EditError),
    #[error("voxel boundary capture failed: {0}")]
    Boundary(#[from] BoundaryError),
    #[error("fluid initialization failed: {0}")]
    Sph(#[from] SphError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intact_dam_keeps_two_finite_reservoirs_separated() {
        let mut fixture = TwoReservoirFixture::new().unwrap();
        let initial_east = fixture.particles_east_of_dam();
        for _ in 0..30 {
            let metrics = fixture.fluid_mut().advance();
            assert_eq!(metrics.particles_outside_domain, 0);
            assert_eq!(metrics.particle_count, fixture.fluid().initial_particles());
        }
        assert_eq!(fixture.particles_east_of_dam(), initial_east);
        assert!(fixture.fluid().density_errors().0.is_finite());
    }

    #[test]
    fn committed_canal_edit_transfers_finite_water_between_reservoirs() {
        let mut fixture = TwoReservoirFixture::new().unwrap();
        let initial_east = fixture.particles_east_of_dam();
        fixture.excavate_canal().unwrap();
        let (west0, east0) = fixture.reservoir_p95_levels_m();
        for _ in 0..120 {
            let metrics = fixture.fluid_mut().advance();
            assert_eq!(metrics.particles_outside_domain, 0);
        }
        let metrics = fixture.fluid().metrics(std::time::Duration::ZERO);
        let (west1, east1) = fixture.reservoir_p95_levels_m();
        assert!(
            fixture.particles_east_of_dam() > initial_east,
            "canal must transfer particles downstream"
        );
        assert!(
            west0 - east0 > west1 - east1 + 0.03,
            "water level difference must decline: before={}, after={}",
            west0 - east0,
            west1 - east1
        );
        assert!(matches!(
            fixture.volume().sample(GlobalCell::new(12, 1, 3)),
            Ok(spall_voxel::Sample::Empty { .. })
        ));
        assert_eq!(metrics.solid_penetration_count, 0);
        assert_eq!(metrics.particle_count, fixture.fluid().initial_particles());
        assert!(metrics.absolute_balance_error_m3 < 1.0e-10);
        assert!(metrics.max_absolute_density_error.is_finite());
    }

    #[test]
    fn dam_break_moves_a_surge_downstream_without_creating_water() {
        let mut fixture = TwoReservoirFixture::new().unwrap();
        fixture.break_dam().unwrap();
        for _ in 0..10 {
            let metrics = fixture.fluid_mut().advance();
            assert_eq!(metrics.particles_outside_domain, 0);
        }
        let metrics = fixture.fluid().metrics(std::time::Duration::ZERO);
        assert!(
            fixture.eastward_momentum_kg_m_s() > 0.1,
            "breach must produce downstream momentum"
        );
        assert_eq!(metrics.solid_penetration_count, 0);
        assert_eq!(metrics.particle_count, fixture.fluid().initial_particles());
        assert!(metrics.absolute_balance_error_m3 < 1.0e-10);
    }

    #[test]
    fn closed_canal_blocks_exchange_after_rebuild() {
        let mut fixture = TwoReservoirFixture::new().unwrap();
        let east_initial = fixture.particles_east_of_dam();
        fixture.excavate_canal().unwrap();
        fixture.close_canal().unwrap();
        for _ in 0..30 {
            let m = fixture.fluid_mut().advance();
            assert_eq!(m.solid_penetration_count, 0);
        }
        assert_eq!(fixture.particles_east_of_dam(), east_initial);
        assert!(matches!(
            fixture.volume().sample(GlobalCell::new(12, 1, 3)),
            Ok(spall_voxel::Sample::Filled(_))
        ));
    }

    #[test]
    #[ignore = "Known failing gate of the rejected Salva backend: 28/320 particles cross the intact roof                 (docs/reports/ENG-103.md). The two-phase MAC grid is the chosen solver; run with --ignored."]
    fn flooded_tunnel_transfers_under_a_separate_pool_roof() {
        let mut fixture =
            TwoReservoirFixture::new_tunnel_under_separate_pool(1, 1.0 / 60.0).unwrap();
        let upper_pool_particles = fixture.elevated_pool_particle_count();
        let initial_east = fixture.particles_east_of_dam();
        fixture.excavate_canal().unwrap();
        for _ in 0..120 {
            fixture.fluid_mut().advance();
        }
        let metrics = fixture.fluid().metrics(std::time::Duration::ZERO);
        assert!(fixture.particles_east_of_dam() > initial_east);
        assert_eq!(
            fixture.elevated_pool_particle_count(),
            upper_pool_particles,
            "separate pool lost particles beneath its roof; crossing={:?}",
            metrics.first_invalid_crossing
        );
        assert_eq!(metrics.solid_penetration_count, 0);
        assert!(metrics.current_outside_domain_volume_m3 < 1.0e-9);
    }

    #[test]
    fn rejected_voxel_edit_preserves_volume_and_fluid_boundary() {
        let fixture = TwoReservoirFixture::new().unwrap();
        let cell = GlobalCell::new(3, 2, 3);
        let before_sample = fixture.volume.sample(cell).unwrap();
        let before_positions = fixture.fluid.position_snapshot();
        let before_boundary = fixture.fluid.solid_boundary().clone();
        let before_solid_count = fixture.fluid.solid_boundary().solid_cell_count();
        let mut edit = EditPlan::new(fixture.volume.id());
        edit.set(cell, SOLID_MATERIAL);
        let mut staged = fixture.volume.clone();
        staged.apply_edit(&edit).unwrap();
        let boundary = SolidBoundary::capture(&staged, fixture.spec).unwrap();
        assert!(
            fixture
                .fluid
                .prepare_boundary(&boundary, RESERVOIR_CELL_SIZE_M, [0.0; 3])
                .is_err()
        );
        assert_eq!(fixture.volume.sample(cell).unwrap(), before_sample);
        assert_eq!(fixture.fluid.position_snapshot(), before_positions);
        assert_eq!(fixture.fluid.solid_boundary(), &before_boundary);
        assert_eq!(
            fixture.fluid.solid_boundary().solid_cell_count(),
            before_solid_count
        );
    }

    #[test]
    fn flat_basin_stability_measurements_expose_baseline_floor_penetration() {
        let mut fixture = TwoReservoirFixture::new_stability_basin(1, 1.0 / 60.0).unwrap();
        let initial = fixture.fluid.metrics(std::time::Duration::ZERO);
        let mut late_speeds = Vec::new();
        let mut late_energies = Vec::new();
        let mut first_penetration = None;
        let mut max_penetrations = 0;
        for tick in 0..180 {
            let m = fixture.fluid_mut().advance();
            max_penetrations = max_penetrations.max(m.solid_penetration_count);
            first_penetration = first_penetration.or(m.first_invalid_crossing);
            if tick >= 120 {
                late_speeds.push(m.speed_p95_m_s);
                late_energies.push(m.kinetic_energy_j);
            }
        }
        let final_metrics = fixture.fluid.metrics(std::time::Duration::ZERO);
        late_speeds.sort_by(f64::total_cmp);
        let late_speed_p95 = late_speeds
            .get(
                ((late_speeds.len().saturating_sub(1) as f64 * 0.95).ceil() as usize)
                    .min(late_speeds.len().saturating_sub(1)),
            )
            .copied()
            .unwrap_or(0.0);
        let energy_max = late_energies.into_iter().fold(0.0_f64, f64::max);
        let stable = late_speed_p95 < 0.5
            && energy_max <= initial.kinetic_energy_j + 1.0
            && (final_metrics.surface_level_p95_m - initial.surface_level_p95_m).abs() < 0.15
            && final_metrics.max_absolute_density_error < 0.02
            && max_penetrations == 0;
        assert!(
            !stable,
            "baseline unexpectedly passed all predeclared stability gates"
        );
        assert!(
            first_penetration.is_some(),
            "baseline floor penetration should be reproducible"
        );
        assert!(max_penetrations > 0);
        assert!(final_metrics.max_absolute_density_error.is_finite());
    }
}

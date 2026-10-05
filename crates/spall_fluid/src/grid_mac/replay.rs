//! Version 1 numerical replay for the ENG-122 reconstructed-support diagnostic.
//! Not a save, wire DTO, authority snapshot or gameplay restore path.
//! Warm physical pressure and retained interface predictors must survive replay.
use super::{MacConfig, MacGridWorld};
use crate::{DomainSpec, SolidBoundary};
use spall_core::GlobalCell;
use std::io::{self, Read, Write};

const MAGIC: &[u8; 8] = b"SPWREP01";
const MAX_CELLS: usize = 1_000_000;
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid reconstructed-water replay",
    )
}
fn scalar(r: &mut impl Read) -> io::Result<f64> {
    let mut bytes = [0; 8];
    r.read_exact(&mut bytes)?;
    let value = f64::from_le_bytes(bytes);
    if !value.is_finite() {
        return Err(invalid());
    }
    Ok(value)
}
fn flag(r: &mut impl Read) -> io::Result<bool> {
    let mut bytes = [0];
    r.read_exact(&mut bytes)?;
    match bytes[0] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid()),
    }
}
fn integers<const N: usize>(r: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    r.read_exact(&mut bytes)?;
    Ok(bytes)
}
impl MacGridWorld {
    /// The exact unfiltered outflow sum used by the CFL gate and its cell.
    /// This diagnostic never changes solver participation or the gate.
    pub fn cfl_limiting_cell_diagnostic(&self) -> (GlobalCell, f64) {
        let (speed, i) = self.max_face_speed_l1();
        (self.spec.cell_at(i), speed)
    }

    /// Query the unchanged CFL/gravity decision without projecting or advancing.
    pub fn substeps_required_diagnostic(&self, dt: f64) -> Result<u32, super::MacError> {
        if !dt.is_finite() || dt <= 0.0 {
            return Err(super::MacError::InvalidTimeStep);
        }
        let (advective, gravity) = self.stability_time_limits(self.max_face_speed_l1().0);
        let required = (dt / advective.min(gravity)).ceil().max(1.0) as u32;
        if required > self.config.max_substeps {
            return Err(super::MacError::SubstepBudgetExceeded {
                required,
                maximum: self.config.max_substeps,
            });
        }
        Ok(required)
    }

    /// Write the exact input of a subsequent reconstructed-support step.
    /// V1 is explicitly little-endian, bounded to one million cells and accepts
    /// only strict, freely displaced air with reconstructed surface support.
    /// Rebuilt cut fluxes and unused legacy pressure caches are omitted.
    /// This diagnostic is deliberately separate from canonical save/wire state.
    pub fn write_reconstructed_replay(&self, mut w: impl Write) -> io::Result<()> {
        if !self.cut_surface_support
            || !self.freely_displaced_air
            || !self.strict_phase_bounds
            || self.ambient_density_kg_m3.is_some()
            || self.experimental_surface_films
            || self.conservative_momentum
            || self.compressible_enclosed_air
            || self.phase_predictor.is_some()
            || self.spec.cell_count() > MAX_CELLS
        {
            return Err(invalid());
        }
        w.write_all(MAGIC)?;
        let p = self.spec.origin();
        for v in [p.x, p.y, p.z] {
            w.write_all(&v.to_le_bytes())?;
        }
        for v in self.spec.dimensions() {
            w.write_all(&v.to_le_bytes())?;
        }
        let c = self.config;
        for v in [
            c.cell_size_m,
            c.density_kg_m3,
            c.gravity_m_s2[0],
            c.gravity_m_s2[1],
            c.gravity_m_s2[2],
            c.cfl_limit,
            c.pressure_relative_tolerance,
            c.pressure_absolute_tolerance,
            self.cumulative_open_outflow_m3,
        ] {
            w.write_all(&v.to_le_bytes())?;
        }
        for v in [c.max_substeps, c.pressure_max_iterations] {
            w.write_all(&v.to_le_bytes())?;
        }
        for v in [
            c.open_top,
            c.reconstructed_surface_support,
            c.pressure_diagnostics,
            self.stage_diagnostics,
            !self.cut_surface_velocity[0].is_empty(),
        ] {
            w.write_all(&[u8::from(v)])?;
        }
        w.write_all(&self.diagnostic_outer_step.to_le_bytes())?;
        for &v in &self.solid {
            w.write_all(&[u8::from(v)])?;
        }
        for array in [
            &self.fraction,
            &self.trapped,
            &self.pressure_pa,
            &self.u,
            &self.v,
            &self.w,
        ]
        .into_iter()
        .chain(self.cut_surface_velocity.iter())
        {
            for v in array {
                w.write_all(&v.to_le_bytes())?;
            }
        }
        w.flush()?;
        Ok(())
    }

    /// Load V1 diagnostic replay. Invalid dimensions, fractions, flags,
    /// nonfinite numerical inputs, truncation and trailing bytes are rejected.
    /// Does not install state into a game, owner thread or canonical save.
    pub fn read_reconstructed_replay(mut r: impl Read) -> io::Result<Self> {
        if &integers::<8>(&mut r)? != MAGIC {
            return Err(invalid());
        }
        let origin = GlobalCell::new(
            i64::from_le_bytes(integers(&mut r)?),
            i64::from_le_bytes(integers(&mut r)?),
            i64::from_le_bytes(integers(&mut r)?),
        );
        let dims = [
            u32::from_le_bytes(integers(&mut r)?),
            u32::from_le_bytes(integers(&mut r)?),
            u32::from_le_bytes(integers(&mut r)?),
        ];
        let spec = DomainSpec::new(origin, dims, MAX_CELLS).map_err(|_| invalid())?;
        let cell_size_m = scalar(&mut r)?;
        let density_kg_m3 = scalar(&mut r)?;
        let gravity_m_s2 = [scalar(&mut r)?, scalar(&mut r)?, scalar(&mut r)?];
        let cfl_limit = scalar(&mut r)?;
        let pressure_relative_tolerance = scalar(&mut r)?;
        let pressure_absolute_tolerance = scalar(&mut r)?;
        let outflow = scalar(&mut r)?;
        if outflow < 0.0 {
            return Err(invalid());
        }
        let config = MacConfig {
            cell_size_m,
            density_kg_m3,
            gravity_m_s2,
            cfl_limit,
            pressure_relative_tolerance,
            pressure_absolute_tolerance,
            max_substeps: u32::from_le_bytes(integers(&mut r)?),
            pressure_max_iterations: u32::from_le_bytes(integers(&mut r)?),
            open_top: flag(&mut r)?,
            reconstructed_surface_support: flag(&mut r)?,
            pressure_diagnostics: flag(&mut r)?,
        };
        let stage = flag(&mut r)?;
        let has_interface = flag(&mut r)?;
        let outer = u64::from_le_bytes(integers(&mut r)?);
        let solid: Vec<_> = (0..spec.cell_count())
            .map(|_| flag(&mut r))
            .collect::<io::Result<_>>()?;
        let mut grid = Self::new(&SolidBoundary { spec, solid }, config).map_err(|_| invalid())?;
        // These fixed model flags do not reset the saved numerical warm state.
        grid.freely_displaced_air = true;
        grid.strict_phase_bounds = true;
        grid.cut_surface_support = true;
        grid.stage_diagnostics = stage;
        grid.diagnostic_outer_step = outer;
        grid.cumulative_open_outflow_m3 = outflow;
        if has_interface {
            grid.cut_surface_velocity = std::array::from_fn(|_| vec![0.0; spec.cell_count()]);
        }
        for array in [
            &mut grid.fraction,
            &mut grid.trapped,
            &mut grid.pressure_pa,
            &mut grid.u,
            &mut grid.v,
            &mut grid.w,
        ]
        .into_iter()
        .chain(grid.cut_surface_velocity.iter_mut())
        {
            for v in array {
                *v = scalar(&mut r)?;
            }
        }
        if grid
            .fraction
            .iter()
            .zip(&grid.solid)
            .any(|(&c, &s)| !(0.0..=1.0).contains(&c) || (s && c != 0.0))
            || grid.trapped.iter().any(|&c| c < 0.0)
        {
            return Err(invalid());
        }
        let mut trailing = [0];
        if r.read(&mut trailing)? != 0 {
            return Err(invalid());
        }
        Ok(grid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn grid() -> MacGridWorld {
        let spec = DomainSpec::new(GlobalCell::new(-7, 2, 11), [6, 4, 3], MAX_CELLS).unwrap();
        let mut grid = MacGridWorld::new(
            &SolidBoundary {
                spec,
                solid: vec![false; spec.cell_count()],
            },
            MacConfig {
                cell_size_m: 0.75,
                ..MacConfig::default()
            },
        )
        .unwrap();
        grid.set_freely_displaced_air().unwrap();
        grid.set_cut_surface_support().unwrap();
        for i in 0..spec.cell_count() {
            grid.fraction[i] = if i % 24 < 12 { 0.25 } else { 0.0 };
        }
        grid.fraction[71] = 1e-300;
        grid
    }
    fn bytes(g: &MacGridWorld) -> Vec<u8> {
        let mut bytes = Vec::new();
        g.write_reconstructed_replay(&mut bytes).unwrap();
        bytes
    }
    #[test]
    fn replay_preserves_warm_surface_state_and_next_accepted_transfers_bitwise() {
        let mut original = grid();
        original.step(0.01).unwrap();
        original.trapped[0] = 0.125;
        let encoded = bytes(&original);
        let mut replay = MacGridWorld::read_reconstructed_replay(encoded.as_slice()).unwrap();
        assert_eq!(bytes(&replay), encoded);
        let a = original.step(0.01).unwrap();
        let b = replay.step(0.01).unwrap();
        assert_eq!(a.substeps, b.substeps);
        assert_eq!(a.pressure_iterations, b.pressure_iterations);
        assert_eq!(
            a.conservation_error_m3.to_bits(),
            b.conservation_error_m3.to_bits()
        );
        assert_eq!(bytes(&original), bytes(&replay));
        assert_eq!(original.cut_surface_flux, replay.cut_surface_flux);
    }
    #[test]
    fn replay_preserves_rejected_cfl_state_without_advancing() {
        let mut original = grid();
        let face = original.u_index(3, 1, 1);
        original.u[face] = 59.0;
        let encoded = bytes(&original);
        let mut replay = MacGridWorld::read_reconstructed_replay(encoded.as_slice()).unwrap();
        assert_eq!(
            original.step(0.05).unwrap_err(),
            replay.step(0.05).unwrap_err()
        );
        assert_eq!(bytes(&original), encoded);
        assert_eq!(bytes(&replay), encoded);
    }
    /// Original captured crop, including its open top. Preserve total water
    /// by accounting for permitted exterior transfers, as in the full trench.
    #[test]
    fn captured_trench_crop_advances_without_velocity_instability() {
        let input = include_bytes!("../../fixtures/eng122-trench-crop-v1.water-replay");
        let mut grid = MacGridWorld::read_reconstructed_replay(input.as_slice()).unwrap();
        let initial = grid.water_volume_m3();
        let mut exterior = 0.0;
        for _ in 0..100 {
            let metrics = grid.step(0.05).unwrap();
            assert_eq!(metrics.pressure_converged_substeps, metrics.substeps);
            assert!(metrics.permitted_outflow_m3 >= 0.0);
            exterior += metrics.permitted_outflow_m3;
            assert!((grid.water_volume_m3() + exterior - initial).abs() < 1e-10);
            assert!(grid.fraction.iter().all(|c| (0.0..=1.0).contains(c)));
        }
    }
    /// Separate sealed variant: no exterior transfer or raw volume loss is
    /// allowed. Its new top wall is explicit, not original trench acceptance.
    #[test]
    fn sealed_trench_crop_retains_all_water_and_remains_stable() {
        let input = include_bytes!("../../fixtures/eng122-trench-crop-v1.water-replay");
        let mut grid = MacGridWorld::read_reconstructed_replay(input.as_slice()).unwrap();
        grid.config.open_top = false;
        let initial = grid.water_volume_m3();
        for _ in 0..100 {
            let metrics = grid.step(0.05).unwrap();
            assert_eq!(metrics.pressure_converged_substeps, metrics.substeps);
            assert_eq!(metrics.permitted_outflow_m3, 0.0);
            assert!((grid.water_volume_m3() - initial).abs() < 1e-10);
        }
    }

    #[test]
    fn replay_rejects_unknown_version_oversized_truncated_and_nonfinite_data() {
        let encoded = bytes(&grid());
        for n in [0, 7, 48, encoded.len() - 1] {
            assert!(MacGridWorld::read_reconstructed_replay(&encoded[..n]).is_err());
        }
        let mut bad = encoded.clone();
        bad[7] = b'2';
        assert!(MacGridWorld::read_reconstructed_replay(bad.as_slice()).is_err());
        let mut bad = encoded.clone();
        bad[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(MacGridWorld::read_reconstructed_replay(bad.as_slice()).is_err());
        let mut bad = encoded.clone();
        bad[44..52].copy_from_slice(&f64::NAN.to_le_bytes());
        assert!(MacGridWorld::read_reconstructed_replay(bad.as_slice()).is_err());
        let mut bad = encoded;
        bad.push(0);
        assert!(MacGridWorld::read_reconstructed_replay(bad.as_slice()).is_err());
    }
}

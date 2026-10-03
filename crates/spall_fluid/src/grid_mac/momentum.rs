//! First-order half-cell dual mass/momentum transport from accepted VOF fluxes.
use super::{MacError, MacGridWorld};
pub(super) type WaterEdge = (Option<usize>, Option<usize>, f64, bool);
#[derive(Debug, Default)]
pub(super) struct MomentumMetrics {
    pub wall: [f64; 3],
    pub exterior: [f64; 3],
    pub error: [f64; 3],
    pub mass_defect: f64,
    pub sweeps: u32,
    pub bytes: usize,
}
pub(super) struct Candidate {
    pub velocity: [Vec<f64>; 3],
    pub metrics: MomentumMetrics,
}
pub(super) fn transport(
    grid: &MacGridWorld,
    old: &[f64],
    new: &[f64],
    edges: &[WaterEdge],
    water: &[f64],
    full: &[f64],
) -> Result<Candidate, MacError> {
    if edges.len() != water.len() || (!grid.freely_displaced_air && edges.len() != full.len()) {
        return Err(MacError::InvalidConfig);
    }
    let air = if grid.freely_displaced_air {
        0.0
    } else {
        grid.ambient_density_kg_m3.ok_or(MacError::InvalidConfig)?
    };
    let liquid = grid.config.density_kg_m3;
    let volume = grid.cell_volume();
    let mut result = Candidate {
        velocity: [Vec::new(), Vec::new(), Vec::new()],
        metrics: MomentumMetrics::default(),
    };
    for (axis, velocity) in [&grid.u, &grid.v, &grid.w].into_iter().enumerate() {
        let n = velocity.len();
        let mut mass = vec![0.0; n];
        let mut expected = vec![0.0; n];
        let mut outgoing = vec![0.0; n];
        let mut blocked = vec![false; n];
        for i in 0..old.len() {
            if grid.solid[i] {
                continue;
            }
            for face in cell_faces(grid, i, axis) {
                mass[face] += 0.5 * volume * (air + (liquid - air) * old[i]);
                expected[face] += 0.5 * volume * (air + (liquid - air) * new[i]);
                blocked[face] = is_wall(grid, face, axis);
            }
        }
        // The two half-cell lanes give averaged primal mass fluxes on dual
        // faces. Combine opposing contributions before selecting a donor.
        let mut lanes = Vec::with_capacity(edges.len() * 2);
        for (j, (&(l, r, _, _), &f)) in edges.iter().zip(water).enumerate() {
            let q = full.get(j).copied().unwrap_or(0.0);
            let m = volume * (air * q + (liquid - air) * f) * 0.5;
            if !m.is_finite() {
                return Err(MacError::MomentumInvalidState);
            }
            let a = l.map(|i| cell_faces(grid, i, axis));
            let b = r.map(|i| cell_faces(grid, i, axis));
            for side in 0..2 {
                lanes.push((a.map(|v| v[side]), b.map(|v| v[side]), m));
            }
        }
        lanes.sort_by_key(|&(a, b, _)| (a, b));
        let mut count = 0;
        for j in 0..lanes.len() {
            if count > 0 && (lanes[count - 1].0, lanes[count - 1].1) == (lanes[j].0, lanes[j].1) {
                lanes[count - 1].2 += lanes[j].2;
            } else {
                lanes[count] = lanes[j];
                count += 1;
            }
        }
        lanes.truncate(count);
        if grid.freely_displaced_air {
            let mut transported = mass.clone();
            for &(a, b, m) in &lanes {
                if let Some(i) = a {
                    transported[i] -= m;
                }
                if let Some(i) = b {
                    transported[i] += m;
                }
            }
            result.metrics.mass_defect = result.metrics.mass_defect.max(
                transported
                    .iter()
                    .zip(&expected)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0, f64::max),
            );
            drop(transported);
            let (next, wall, exterior, error, sweeps) =
                water_only_upwind(&mass, &expected, velocity, &blocked, &lanes)?;
            result.metrics.wall[axis] = wall;
            result.metrics.exterior[axis] = exterior;
            result.metrics.error[axis] = error;
            result.metrics.sweeps = result.metrics.sweeps.max(sweeps);
            result.metrics.bytes = result.metrics.bytes.max(
                8 * n * size_of::<f64>()
                    + blocked.capacity() * size_of::<bool>()
                    + lanes.capacity() * size_of::<(Option<usize>, Option<usize>, f64)>(),
            );
            result.velocity[axis] = next;
            continue;
        }
        let mut transported = mass.clone();
        for &(a, b, m) in &lanes {
            if let Some(i) = a {
                transported[i] -= m;
            }
            if let Some(i) = b {
                transported[i] += m;
            }
            if let Some(i) = if m >= 0.0 { a } else { b } {
                outgoing[i] += m.abs();
            }
        }
        let mut sweeps = 1u32;
        for i in 0..n {
            if mass[i] == 0.0 && expected[i] == 0.0 {
                continue;
            }
            let defect = (transported[i] - expected[i]).abs();
            result.metrics.mass_defect = result.metrics.mass_defect.max(defect);
            // Finite pressure residual can leave a small air-mass defect.
            // Measure it explicitly. Never hide negative dual mass or clipping.
            if transported[i] <= 0.0
                || !transported[i].is_finite()
                || defect > 1e-8 * mass[i].max(expected[i])
            {
                return Err(MacError::MomentumMassMismatch {
                    face: i,
                    defect_kg: defect,
                });
            }
            // Subcycle only momentum over the SAME final water/mass fluxes.
            // This preserves convex upwinding even when a tiny gas dual volume
            // receives liquid throughflow during this accepted water step.
            let required = (outgoing[i] / mass[i].min(transported[i]) / 0.9).ceil();
            if required > 1024.0 {
                return Err(MacError::MomentumSubcycleBudget {
                    required: required as u64,
                });
            }
            sweeps = sweeps.max(required as u32);
        }
        result.metrics.sweeps = result.metrics.sweeps.max(sweeps);
        let mut p: Vec<_> = mass.iter().zip(velocity).map(|(m, u)| m * u).collect();
        let before: f64 = p.iter().sum();
        let mut next = p.clone();
        let mut next_mass = mass.clone();
        for _ in 0..sweeps {
            next.copy_from_slice(&p);
            next_mass.copy_from_slice(&mass);
            for &(a, b, m) in &lanes {
                let m = m / f64::from(sweeps);
                let donor = if m >= 0.0 { a } else { b };
                // Atmospheric inflow has zero prescribed velocity; walls have
                // zero normal velocity. Boundary reaction is a separate ledger.
                let u = donor.map_or(0.0, |i| if blocked[i] { 0.0 } else { p[i] / mass[i] });
                let flux = m * u;
                if let Some(i) = a {
                    next[i] -= flux;
                    next_mass[i] -= m;
                } else {
                    result.metrics.exterior[axis] -= flux;
                }
                if let Some(i) = b {
                    next[i] += flux;
                    next_mass[i] += m;
                } else {
                    result.metrics.exterior[axis] += flux;
                }
            }
            for i in 0..n {
                if blocked[i] {
                    result.metrics.wall[axis] += next[i];
                    next[i] = 0.0;
                }
            }
            std::mem::swap(&mut p, &mut next);
            std::mem::swap(&mut mass, &mut next_mass);
        }
        result.metrics.error[axis] =
            p.iter().sum::<f64>() + result.metrics.wall[axis] + result.metrics.exterior[axis]
                - before;
        // The new phase-derived dual density matches the pressure coefficients.
        // Converting p to velocity does not change accepted momentum.
        result.velocity[axis] = p
            .iter()
            .zip(&expected)
            .map(|(p, m)| if *m == 0.0 { 0.0 } else { p / m })
            .collect();
        if result.velocity[axis].iter().any(|u| !u.is_finite()) {
            return Err(MacError::MomentumInvalidState);
        }
        result.metrics.bytes = result.metrics.bytes.max(
            (mass.capacity()
                + expected.capacity()
                + outgoing.capacity()
                + transported.capacity()
                + p.capacity()
                + next.capacity()
                + next_mass.capacity())
                * size_of::<f64>()
                + blocked.capacity() * size_of::<bool>()
                + lanes.capacity() * size_of::<(Option<usize>, Option<usize>, f64)>(),
        );
    }
    result.metrics.bytes += result
        .velocity
        .iter()
        .map(|v| v.capacity() * size_of::<f64>())
        .sum::<usize>();
    Ok(result)
}

/// Implicit upwind dual momentum using exactly the accepted water mass fluxes.
/// (M_new + outgoing) u_new - incoming*u_donor = M_old*u_old.
/// Since M_new + outgoing = M_old + incoming, each velocity update is a
/// convex average, including faces that start empty or dry completely. No gas
/// inertia, mass floor, velocity clipping or division by a vanishing old mass.
/// Wall reaction and open momentum flux use the same converged velocities.
fn water_only_upwind(
    old: &[f64],
    new: &[f64],
    velocity: &[f64],
    blocked: &[bool],
    lanes: &[(Option<usize>, Option<usize>, f64)],
) -> Result<(Vec<f64>, f64, f64, f64, u32), MacError> {
    let n = old.len();
    let momentum: Vec<_> = old.iter().zip(velocity).map(|(m, u)| m * u).collect();
    let mut diagonal = old.to_vec();
    let mut transported = old.to_vec();
    for &(a, b, m) in lanes {
        if let Some(i) = a {
            transported[i] -= m;
        }
        if let Some(i) = b {
            transported[i] += m;
        }
        if let Some(i) = if m >= 0.0 { b } else { a } {
            diagonal[i] += m.abs();
        }
    }
    for i in 0..n {
        let defect = (transported[i] - new[i]).abs();
        if defect > 1e-10 * old[i].max(new[i]) + 1e-12 {
            return Err(MacError::MomentumMassMismatch {
                face: i,
                defect_kg: defect,
            });
        }
    }
    let tolerance = 1e-12 + 1e-11 * momentum.iter().map(|p| p.abs()).sum::<f64>();
    let mut u: Vec<_> = velocity
        .iter()
        .enumerate()
        .map(|(i, u)| {
            if blocked[i] || diagonal[i] == 0.0 {
                0.0
            } else {
                *u
            }
        })
        .collect();
    let mut rhs = vec![0.0; n];
    let mut converged = false;
    let mut sweeps = 0;
    for _ in 0..256 {
        rhs.copy_from_slice(&momentum);
        for &(a, b, m) in lanes {
            let (donor, receiver) = if m >= 0.0 { (a, b) } else { (b, a) };
            if let Some(i) = receiver {
                rhs[i] += m.abs() * donor.map_or(0.0, |j| u[j]);
            }
        }
        let mut change = 0.0;
        for i in 0..n {
            let next = if blocked[i] || diagonal[i] == 0.0 {
                0.0
            } else {
                rhs[i] / diagonal[i]
            };
            change += diagonal[i] * (next - u[i]).abs();
            u[i] = next;
        }
        sweeps += 1;
        if change <= tolerance {
            converged = true;
            break;
        }
    }
    if !converged || u.iter().any(|u| !u.is_finite()) {
        return Err(MacError::MomentumInvalidState);
    }
    let mut reaction = momentum
        .iter()
        .enumerate()
        .filter(|(i, _)| blocked[*i])
        .map(|(_, p)| p)
        .sum::<f64>();
    let mut exterior = 0.0;
    for &(a, b, m) in lanes {
        let (donor, receiver) = if m >= 0.0 { (a, b) } else { (b, a) };
        let flux = m.abs() * donor.map_or(0.0, |j| u[j]);
        if receiver.is_some_and(|i| blocked[i]) {
            reaction += flux;
        }
        if receiver.is_none() {
            exterior += flux;
        }
    }
    for i in 0..n {
        if new[i] == 0.0 {
            u[i] = 0.0;
        }
    }
    let error = new.iter().zip(&u).map(|(m, u)| m * u).sum::<f64>() + reaction + exterior
        - momentum.iter().sum::<f64>();
    if error.abs() > 2.0 * tolerance {
        return Err(MacError::MomentumInvalidState);
    }
    Ok((u, reaction, exterior, error, sweeps))
}
fn cell_faces(grid: &MacGridWorld, i: usize, axis: usize) -> [usize; 2] {
    let [nx, ny, _] = grid.dims();
    let (x, y, z) = (i % nx, i / nx % ny, i / (nx * ny));
    match axis {
        0 => [grid.u_index(x, y, z), grid.u_index(x + 1, y, z)],
        1 => [grid.v_index(x, y, z), grid.v_index(x, y + 1, z)],
        _ => [grid.w_index(x, y, z), grid.w_index(x, y, z + 1)],
    }
}
fn is_wall(grid: &MacGridWorld, face: usize, axis: usize) -> bool {
    let mut ext = grid.dims();
    ext[axis] += 1;
    let mut q = [
        face % ext[0],
        face / ext[0] % ext[1],
        face / (ext[0] * ext[1]),
    ];
    if q[axis] == 0 || (q[axis] == grid.dims()[axis] && !(axis == 1 && grid.config.open_top)) {
        return true;
    }
    if q[axis] < grid.dims()[axis] && grid.solid[grid.cell_index(q[0], q[1], q[2])] {
        return true;
    }
    q[axis] -= 1;
    grid.solid[grid.cell_index(q[0], q[1], q[2])]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid_mac::MacConfig;
    use crate::{DomainSpec, SolidBoundary};
    use spall_core::GlobalCell;

    #[test]
    fn water_only_momentum_handles_drying_and_empty_throughflow_without_air_mass() {
        let old = [1.0, 0.0, 0.0];
        let new = [0.0, 0.5, 0.5];
        let (u, wall, exterior, error, _) = water_only_upwind(
            &old,
            &new,
            &[2.0, 0.0, 0.0],
            &[false; 3],
            &[(Some(0), Some(1), 1.0), (Some(1), Some(2), 0.5)],
        )
        .unwrap();
        assert_eq!(u, [0.0, 2.0, 2.0]);
        assert_eq!((wall, exterior, error), (0.0, 0.0, 0.0));
        let energy: f64 = new.iter().zip(u).map(|(m, u)| 0.5 * m * u * u).sum();
        assert_eq!(energy, 2.0);
    }

    #[test]
    fn water_only_upwinding_dissipates_energy_and_accounts_for_wall_and_outflow() {
        let (u, _, _, error, _) = water_only_upwind(
            &[1.0, 1.0],
            &[1.0, 1.0],
            &[1.0, -1.0],
            &[false; 2],
            &[(Some(0), Some(1), 0.25), (Some(1), Some(0), 0.25)],
        )
        .unwrap();
        assert!(error.abs() < 1e-10);
        assert!(0.5 * (u[0] * u[0] + u[1] * u[1]) < 1.0);
        for wall in [false, true] {
            let destination = wall.then_some(1);
            let new = if wall { [0.0, 1.0] } else { [0.0, 0.0] };
            let (_, reaction, exterior, error, _) = water_only_upwind(
                &[1.0, 0.0],
                &new,
                &[2.0, 0.0],
                &[false, wall],
                &[(Some(0), destination, 1.0)],
            )
            .unwrap();
            assert_eq!(error, 0.0);
            assert_eq!(reaction + exterior, 2.0);
            assert_eq!(reaction, if wall { 2.0 } else { 0.0 });
        }
    }

    fn grid(dims: [u32; 3], open_top: bool) -> MacGridWorld {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), dims, 1000).unwrap();
        let mut grid = MacGridWorld::new(
            &SolidBoundary {
                spec,
                solid: vec![false; spec.cell_count()],
            },
            MacConfig {
                gravity_m_s2: [0.0; 3],
                open_top,
                ..MacConfig::default()
            },
        )
        .unwrap();
        grid.set_ambient_density(1.2).unwrap();
        grid.set_strict_phase_bounds();
        grid.set_conservative_momentum().unwrap();
        grid
    }

    #[test]
    fn conservative_momentum_preserves_uniform_transverse_velocity_and_energy() {
        let mut grid = grid([4, 4, 2], false);
        // A divergence-free primal circulation moves a sharp interface at
        // density ratio 833. Uniform transverse velocity must stay uniform.
        let mut edges = Vec::new();
        let mut water = Vec::new();
        let mut full = Vec::new();
        for z in 0..2 {
            let cells = [(1, 1), (2, 1), (2, 2), (1, 2)].map(|(x, y)| grid.cell_index(x, y, z));
            for (j, &i) in cells.iter().enumerate() {
                grid.fraction[i] = if j == 0 { 1.0 } else { 0.0 };
            }
            for j in 0..4 {
                let (a, b) = (cells[j], cells[(j + 1) % 4]);
                let (l, r, sign) = if a < b { (a, b, 1.0) } else { (b, a, -1.0) };
                edges.push((Some(l), Some(r), 0.0, false));
                full.push(sign * 0.2);
                water.push(sign * 0.2 * grid.fraction[a]);
            }
        }
        for y in 0..4 {
            for x in 0..4 {
                let i = grid.w_index(x, y, 1);
                grid.w[i] = 0.7;
            }
        }
        let old = grid.fraction.clone();
        let mut new = old.clone();
        for (&(a, b, _, _), &f) in edges.iter().zip(&water) {
            new[a.unwrap()] -= f;
            new[b.unwrap()] += f;
        }
        let result = transport(&grid, &old, &new, &edges, &water, &full).unwrap();
        assert!(result.metrics.sweeps >= 1);
        assert!(result.metrics.error.iter().all(|e| e.abs() < 1e-12));
        assert!(result.metrics.mass_defect < 1e-14);
        for y in 0..4 {
            for x in 0..4 {
                assert!((result.velocity[2][grid.w_index(x, y, 1)] - 0.7).abs() < 1e-12);
            }
        }
        // Nonuniform normal-component mixing must dissipate, not create KE.
        let i = grid.u_index(2, 1, 0);
        grid.u[i] = 0.3;
        let result = transport(&grid, &old, &new, &edges, &water, &full).unwrap();
        let energy = |phase: &[f64], velocity: &[f64]| {
            let mut mass = vec![0.0; velocity.len()];
            for (i, &c) in phase.iter().enumerate() {
                for f in cell_faces(&grid, i, 0) {
                    mass[f] += 0.5 * grid.cell_volume() * (1.2 + 998.8 * c);
                }
            }
            mass.iter()
                .zip(velocity)
                .map(|(m, u)| 0.5 * m * u * u)
                .sum::<f64>()
        };
        assert!(energy(&new, &result.velocity[0]) <= energy(&old, &grid.u) * (1.0 + 1e-12));
    }

    #[test]
    fn conservative_momentum_accounts_for_open_boundary_and_wall_impulse() {
        let mut grid = grid([2, 1, 1], true);
        grid.fraction.fill(1.0);
        let u = grid.u_index(1, 0, 0);
        grid.u[u] = 0.8;
        // Pure liquid outflow: full and water volumes match exactly.
        let edges = [
            (Some(0), None, 0.0, false),
            (Some(1), None, 0.0, false),
            (Some(0), Some(1), 0.0, false),
        ];
        let result = transport(
            &grid,
            &[1.0, 1.0],
            &[1.0, 0.9],
            &edges,
            &[0.1, 0.0, -0.1],
            &[0.1, -0.1, -0.1],
        )
        .unwrap();
        assert!(result.metrics.exterior[0] > 0.0);
        assert!(result.metrics.error.iter().all(|e| e.abs() < 1e-12));
        // Interior mass moves into a normal-velocity wall half-volume.
        let edges = [(Some(0), Some(1), 0.0, false)];
        let result = transport(&grid, &[1.0, 0.0], &[0.8, 0.2], &edges, &[0.2], &[0.0]).unwrap();
        assert!(result.metrics.wall[0] > 0.0);
        assert_eq!(result.velocity[0][grid.u_index(2, 0, 0)], 0.0);
        assert!(result.metrics.error.iter().all(|e| e.abs() < 1e-12));
    }

    #[test]
    fn conservative_momentum_uses_final_limited_fluxes_and_rejects_mass_defects() {
        let mut grid = grid([2, 1, 1], false);
        grid.fraction = [0.9, 1.0].to_vec();
        let i = grid.u_index(1, 0, 0);
        grid.u[i] = 0.5;
        let old = grid.fraction.clone();
        // A full receiving cell rejects this requested transfer. Momentum
        // consumes the accepted zero flux, never the requested water flux.
        let edges = [(Some(0), Some(1), 0.1, false)];
        let mut water = [0.1];
        let mut new = old.clone();
        super::super::strict_transfer_bounds(
            &old,
            &edges,
            &mut water,
            &[0, 1],
            &mut [0.0; 2],
            &mut [0.0; 2],
            &mut new,
        )
        .unwrap();
        assert_eq!(water, [0.0]);
        let result = transport(&grid, &old, &new, &edges, &water, &[0.0]).unwrap();
        assert_eq!(result.velocity[0], grid.u);
        assert!(matches!(
            transport(&grid, &old, &new, &edges, &[0.1], &[0.0]),
            Err(MacError::MomentumMassMismatch { .. })
        ));
        assert_eq!(grid.fraction, old);
        assert_eq!(grid.u[i], 0.5);
        let grid = self::grid([3, 1, 1], false);
        let edges = [
            (Some(0), Some(1), 0.0, false),
            (Some(1), Some(2), 0.0, false),
        ];
        let result = transport(
            &grid,
            &[1.0, 0.0, 0.0],
            &[0.8, 0.0, 0.2],
            &edges,
            &[0.2, 0.2],
            &[0.0, 0.0],
        )
        .unwrap();
        assert!(result.metrics.sweeps > 1);
        assert!(result.metrics.error.iter().all(|e| e.abs() < 1e-12));
        let mut extreme = self::grid([3, 1, 1], false);
        extreme.set_ambient_density(0.01).unwrap();
        assert!(matches!(
            transport(
                &extreme,
                &[1.0, 0.0, 0.0],
                &[0.8, 0.0, 0.2],
                &edges,
                &[0.2, 0.2],
                &[0.0, 0.0]
            ),
            Err(MacError::MomentumSubcycleBudget { .. })
        ));
    }
}

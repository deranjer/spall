//! Analytic mass, centre of mass, and inertia of a solid occupancy grid.
//!
//! This is the *reference* the collider representations are checked against: it
//! sums exact unit cubes, so if a native voxel collider and a merged-cuboid
//! compound both describe the same occupancy their Rapier-derived mass
//! properties must both match this to a tight tolerance.
//!
//! Frame: grid-local metres, with the origin at the `(0, 0, 0)` corner of grid
//! cell `(0, 0, 0)`. Cube `(x, y, z)` therefore has its centre at
//! `((x + 0.5) * s, (y + 0.5) * s, (z + 0.5) * s)` for cell edge `s`.

use spall_core::MaterialId;

use crate::occupancy::OccupancyGrid;

/// Rigid mass properties in a grid-local metre frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MassProperties {
    /// Total mass, kilograms.
    pub mass_kg: f64,
    /// Centre of mass, grid-local metres.
    pub com_m: [f64; 3],
    /// Inertia tensor about the centre of mass, `kg·m²`, row-major.
    pub inertia_com: [[f64; 3]; 3],
}

/// The physics-body-facing narrowing of [`MassProperties`]: `f32` mass, centre
/// of mass, and the full inertia tensor about the centre of mass, ready to
/// install verbatim into a rigid body **independently of its collision shape**.
///
/// The frame is the body-local metre frame the collider is built in — origin at
/// the `(0, 0, 0)` corner of grid cell `(0, 0, 0)` — so the installed centre of
/// mass and the collider geometry share one origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyMassProperties {
    /// Total mass, kilograms.
    pub mass_kg: f32,
    /// Centre of mass, body-local metres.
    pub local_com_m: [f32; 3],
    /// Inertia tensor about the centre of mass, `kg·m²`, row-major (symmetric).
    pub inertia_com_kg_m2: [[f32; 3]; 3],
}

impl MassProperties {
    /// The three diagonal entries of [`Self::inertia_com`].
    pub fn principal_diagonal(&self) -> [f64; 3] {
        [
            self.inertia_com[0][0],
            self.inertia_com[1][1],
            self.inertia_com[2][2],
        ]
    }

    /// Narrows to the `f32` [`BodyMassProperties`] the physics body carries. The
    /// analytic `f64` value stays the reference; this is what the solver
    /// integrates.
    pub fn to_body_properties(&self) -> BodyMassProperties {
        let n = |v: f64| v as f32;
        BodyMassProperties {
            mass_kg: n(self.mass_kg),
            local_com_m: [n(self.com_m[0]), n(self.com_m[1]), n(self.com_m[2])],
            inertia_com_kg_m2: [
                [
                    n(self.inertia_com[0][0]),
                    n(self.inertia_com[0][1]),
                    n(self.inertia_com[0][2]),
                ],
                [
                    n(self.inertia_com[1][0]),
                    n(self.inertia_com[1][1]),
                    n(self.inertia_com[1][2]),
                ],
                [
                    n(self.inertia_com[2][0]),
                    n(self.inertia_com[2][1]),
                    n(self.inertia_com[2][2]),
                ],
            ],
        }
    }

    /// Largest absolute per-entry difference between two tensors' plus mass and
    /// COM, scaled: returns `(mass_rel, com_abs_m, inertia_rel)`.
    pub fn compare(&self, other: &MassProperties) -> (f64, f64, f64) {
        let mass_rel = ((self.mass_kg - other.mass_kg) / self.mass_kg.max(1e-9)).abs();
        let com_abs = (0..3)
            .map(|i| (self.com_m[i] - other.com_m[i]).abs())
            .fold(0.0_f64, f64::max);
        let scale = self
            .principal_diagonal()
            .iter()
            .fold(0.0_f64, |m, &v| m.max(v.abs()))
            .max(1e-9);
        let inertia_rel = (0..3)
            .flat_map(|r| (0..3).map(move |c| (r, c)))
            .map(|(r, c)| (self.inertia_com[r][c] - other.inertia_com[r][c]).abs() / scale)
            .fold(0.0_f64, f64::max);
        (mass_rel, com_abs, inertia_rel)
    }
}

/// Computes [`MassProperties`] for `grid` with cell edge `cell_m` metres, using
/// `density` (kg/m³) per material.
pub fn analytic_mass_properties(
    grid: &OccupancyGrid,
    cell_m: f64,
    density: impl Fn(MaterialId) -> f64,
) -> MassProperties {
    let cube_vol = cell_m * cell_m * cell_m;

    // Pass 1: mass and first moments.
    let mut mass = 0.0_f64;
    let mut moment = [0.0_f64; 3];
    grid.for_each_solid(|x, y, z, mat| {
        let m = density(mat) * cube_vol;
        let c = [
            (x as f64 + 0.5) * cell_m,
            (y as f64 + 0.5) * cell_m,
            (z as f64 + 0.5) * cell_m,
        ];
        mass += m;
        moment[0] += m * c[0];
        moment[1] += m * c[1];
        moment[2] += m * c[2];
    });
    let com = if mass > 0.0 {
        [moment[0] / mass, moment[1] / mass, moment[2] / mass]
    } else {
        [0.0; 3]
    };

    // Pass 2: inertia about the COM. Each cube contributes its own solid-cube
    // inertia (`m·s²/6` on the diagonal) plus the parallel-axis term for the
    // offset `r` of its centre from the COM.
    let self_diag = |m: f64| m * cell_m * cell_m / 6.0;
    let mut inertia = [[0.0_f64; 3]; 3];
    grid.for_each_solid(|x, y, z, mat| {
        let m = density(mat) * cube_vol;
        let r = [
            (x as f64 + 0.5) * cell_m - com[0],
            (y as f64 + 0.5) * cell_m - com[1],
            (z as f64 + 0.5) * cell_m - com[2],
        ];
        let r2 = r[0] * r[0] + r[1] * r[1] + r[2] * r[2];
        let sd = self_diag(m);
        for a in 0..3 {
            for b in 0..3 {
                let kron = if a == b { 1.0 } else { 0.0 };
                inertia[a][b] += kron * sd; // own-cube inertia (diagonal)
                inertia[a][b] += m * (kron * r2 - r[a] * r[b]); // parallel axis
            }
        }
    });

    MassProperties {
        mass_kg: mass,
        com_m: com,
        inertia_com: inertia,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, GlobalCell, VolumeId};
    use spall_voxel::{EditPlan, Volume, fixtures};

    fn vid(n: u64) -> VolumeId {
        VolumeId::new(n).unwrap()
    }

    fn solid_box(min: GlobalCell, max: GlobalCell) -> OccupancyGrid {
        let mut v = Volume::new(vid(1), CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(vid(1), min, max, fixtures::STONE))
            .unwrap();
        OccupancyGrid::from_region(&v, min, max).unwrap()
    }

    #[test]
    fn a_uniform_cuboid_matches_the_closed_form() {
        // 8 x 4 x 6 cells at 0.25 m -> 2 x 1 x 1.5 m box.
        let grid = solid_box(GlobalCell::new(0, 0, 0), GlobalCell::new(7, 3, 5));
        let s = 0.25;
        let rho = 2600.0;
        let mp = analytic_mass_properties(&grid, s, |_| rho);

        let dims = [8.0 * s, 4.0 * s, 6.0 * s];
        let expected_mass = rho * dims[0] * dims[1] * dims[2];
        assert!((mp.mass_kg - expected_mass).abs() / expected_mass < 1e-9);

        // COM at the geometric centre.
        for (com, extent) in mp.com_m.iter().zip(dims) {
            assert!((com - extent / 2.0).abs() < 1e-9);
        }

        // Solid cuboid inertia: Ixx = m/12 (h² + d²), etc.
        let m = expected_mass;
        let ixx = m / 12.0 * (dims[1] * dims[1] + dims[2] * dims[2]);
        let iyy = m / 12.0 * (dims[0] * dims[0] + dims[2] * dims[2]);
        let izz = m / 12.0 * (dims[0] * dims[0] + dims[1] * dims[1]);
        let diag = mp.principal_diagonal();
        assert!((diag[0] - ixx).abs() / ixx < 1e-6, "{} vs {ixx}", diag[0]);
        assert!((diag[1] - iyy).abs() / iyy < 1e-6, "{} vs {iyy}", diag[1]);
        assert!((diag[2] - izz).abs() / izz < 1e-6, "{} vs {izz}", diag[2]);
        // Off-diagonal products of inertia vanish for an axis-aligned cuboid.
        assert!(mp.inertia_com[0][1].abs() / ixx < 1e-9);
    }

    #[test]
    fn mixed_material_shifts_the_centre_of_mass() {
        // Two 4³ blocks side by side on x; right block is 3x denser.
        let mut v = Volume::new(vid(2), CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            vid(2),
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(3, 3, 3),
            fixtures::STONE,
        ))
        .unwrap();
        v.apply_edit(&EditPlan::filled_box(
            vid(2),
            GlobalCell::new(4, 0, 0),
            GlobalCell::new(7, 3, 3),
            fixtures::DIRT,
        ))
        .unwrap();
        let grid =
            OccupancyGrid::from_region(&v, GlobalCell::new(0, 0, 0), GlobalCell::new(7, 3, 3))
                .unwrap();
        let mp = analytic_mass_properties(&grid, 0.25, |m| {
            if m == fixtures::STONE { 1000.0 } else { 3000.0 }
        });
        // 8 cells on x at 0.25 m: geometric centre is 1.0 m. The 3x-denser
        // right block pulls the COM to (0.5 + 3*1.5) / 4 = 1.25 m.
        assert!(mp.com_m[0] > 1.0, "com_x = {}", mp.com_m[0]);
        assert!(mp.com_m[0] < 1.5, "com_x = {}", mp.com_m[0]);
        assert!((mp.com_m[0] - 1.25).abs() < 1e-6, "com_x = {}", mp.com_m[0]);
    }
}

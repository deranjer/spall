//! Rigid volume-to-world transforms.
//!
//! A [`Volume`](crate::volume::Volume) stores geometry in integer **cell**
//! space. Terrain uses the identity transform; a detached body carries a rigid
//! [`Pose`] (rotation + `f64` metre translation). [`RigidXform`] converts
//! between a volume's local cell space and world metres so a world-space ray can
//! be traced against a transformed volume and the hit mapped back.
//!
//! The mapping is: `local_cell -> local_cell * cell_size_m -> rotate -> + translation`.
//! Because the scale is uniform, directions ignore `cell_size_m` (the ray code
//! renormalises), and the inverse uses the quaternion conjugate — exact for the
//! unit quaternion this type keeps.

use glam::{DQuat, DVec3};

use spall_core::units::ZeroQuaternion;
use spall_core::{CellSizeCode, Pose};

/// A rigid transform from one volume's local cell space to world metres.
///
/// The stored rotation is always normalised, so [`RigidXform::world_to_local_cell`]
/// and friends can use the conjugate as the exact inverse rotation.
#[derive(Debug, Clone, Copy)]
pub struct RigidXform {
    rotation: DQuat,
    translation_m: DVec3,
    cell_m: f64,
}

impl RigidXform {
    /// A transform with the given rotation (normalised on the way in),
    /// `f64` metre translation, and cell size.
    pub fn new(rotation: DQuat, translation_m: DVec3, cell_size: CellSizeCode) -> Self {
        Self {
            rotation: normalise_or_identity(rotation),
            translation_m,
            cell_m: cell_size.metres(),
        }
    }

    /// The identity transform for a volume of the given cell size — local cells
    /// map to world metres by a plain scale. This is the terrain transform.
    pub fn identity(cell_size: CellSizeCode) -> Self {
        Self {
            rotation: DQuat::IDENTITY,
            translation_m: DVec3::ZERO,
            cell_m: cell_size.metres(),
        }
    }

    /// Builds a transform from a replicated [`Pose`]. The pose's quantised
    /// quaternion is dequantised and renormalised; a degenerate (zero-norm)
    /// orientation is rejected.
    pub fn from_pose(pose: &Pose, cell_size: CellSizeCode) -> Result<Self, ZeroQuaternion> {
        let [x, y, z, w] = pose.rotation.to_unit()?;
        let rotation = DQuat::from_xyzw(f64::from(x), f64::from(y), f64::from(z), f64::from(w));
        if !rotation.is_finite() || rotation.length_squared() <= f64::EPSILON {
            return Err(ZeroQuaternion);
        }
        Ok(Self {
            rotation: rotation.normalize(),
            translation_m: DVec3::from_array(pose.translation_m),
            cell_m: cell_size.metres(),
        })
    }

    /// Edge length of one cell, in metres.
    pub fn cell_size_m(&self) -> f64 {
        self.cell_m
    }

    /// Local cell coordinate (fractional cells) to world position (metres).
    pub fn local_cell_to_world_m(&self, local_cell: DVec3) -> DVec3 {
        self.rotation * (local_cell * self.cell_m) + self.translation_m
    }

    /// World position (metres) to local cell coordinate (fractional cells).
    pub fn world_to_local_cell(&self, world_m: DVec3) -> DVec3 {
        (self.rotation.conjugate() * (world_m - self.translation_m)) / self.cell_m
    }

    /// Rotate a direction from local space into world space (no scale, no
    /// translation).
    pub fn local_dir_to_world(&self, dir: DVec3) -> DVec3 {
        self.rotation * dir
    }

    /// Rotate a direction from world space into local space.
    pub fn world_dir_to_local(&self, dir: DVec3) -> DVec3 {
        self.rotation.conjugate() * dir
    }
}

fn normalise_or_identity(q: DQuat) -> DQuat {
    if q.is_finite() && q.length_squared() > f64::EPSILON {
        q.normalize()
    } else {
        DQuat::IDENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::units::QuantizedQuat;

    fn close(a: DVec3, b: DVec3, tol: f64) -> bool {
        (a - b).length() <= tol
    }

    #[test]
    fn identity_transform_is_a_pure_cell_scale() {
        let x = RigidXform::identity(CellSizeCode::Quarter);
        assert!(close(
            x.local_cell_to_world_m(DVec3::new(4.0, -8.0, 2.0)),
            DVec3::new(1.0, -2.0, 0.5),
            1e-12,
        ));
        assert!(close(
            x.world_to_local_cell(DVec3::new(1.0, -2.0, 0.5)),
            DVec3::new(4.0, -8.0, 2.0),
            1e-12,
        ));
    }

    #[test]
    fn world_local_round_trips_for_rotated_translated_volumes() {
        // A 45-degree spin about an arbitrary axis, an off-origin translation,
        // and points at negative cell coordinates.
        let axis = DVec3::new(1.0, 2.0, -0.5).normalize();
        let rot = DQuat::from_axis_angle(axis, std::f64::consts::FRAC_PI_4);
        let x = RigidXform::new(rot, DVec3::new(-37.25, 12.0, 4.5), CellSizeCode::Sixteenth);

        for p in [
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(-3.5, 0.5, 0.5),
            DVec3::new(-40.0, -40.0, -40.0),
            DVec3::new(123.75, -9.0, 61.5),
        ] {
            let round = x.world_to_local_cell(x.local_cell_to_world_m(p));
            assert!(close(round, p, 1e-9), "cell round-trip {p} -> {round}");

            let d = DVec3::new(1.0, -2.0, 3.0);
            let dround = x.world_dir_to_local(x.local_dir_to_world(d));
            assert!(close(dround, d, 1e-9), "dir round-trip {d} -> {dround}");
        }
    }

    #[test]
    fn from_pose_round_trips_within_quantisation_tolerance() {
        let (qx, qy, qz, qw) = (0.0_f32, 0.382_683_4, 0.0, 0.923_879_5); // 45 deg about Y
        let pose = Pose {
            translation_m: [10.0, -4.0, 100.0],
            rotation: QuantizedQuat::from_unit(qx, qy, qz, qw).unwrap(),
        };
        let x = RigidXform::from_pose(&pose, CellSizeCode::Quarter).unwrap();
        let p = DVec3::new(-6.0, 3.0, 12.5);
        let round = x.world_to_local_cell(x.local_cell_to_world_m(p));
        // i16 quaternion quantisation, not lossless, but well under a cell.
        assert!(close(round, p, 1e-3), "{p} -> {round}");
    }

    #[test]
    fn from_pose_rejects_a_zero_orientation() {
        let pose = Pose {
            translation_m: [0.0, 0.0, 0.0],
            rotation: QuantizedQuat {
                x: 0,
                y: 0,
                z: 0,
                w: 0,
            },
        };
        assert!(matches!(
            RigidXform::from_pose(&pose, CellSizeCode::Quarter),
            Err(ZeroQuaternion)
        ));
    }
}

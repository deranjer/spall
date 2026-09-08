//! Baked vertex ambient occlusion and the merge rule that depends on it.
//!
//! Each quad corner gets an integer occlusion level `0..=3` from the three
//! cells that touch that corner in the plane one step outside the face
//! (`side_u`, `side_v`, and the diagonal `corner`). The classic rule is:
//!
//! ```text
//! level = if side_u && side_v { 0 } else { 3 - (side_u + side_v + corner) }
//! ```
//!
//! `0` is the darkest (fully occluded) corner, `3` is unoccluded.
//!
//! **Merge rule (fixed before AO is baked into vertices):** two coplanar unit
//! faces merge only when their material, facing, *and* full four-corner level
//! tuple are identical. A merged quad then carries one level tuple, and the
//! bilinear interpolation of AO across it is exactly what each unit face it
//! replaced would have produced. Any AO variation splits the quad instead of
//! being averaged away.

use crate::face::FaceDir;
use crate::sample::VolumeSampler;

/// The four corners of a face quad, in emission order:
/// `(u0,v0) (u1,v0) (u1,v1) (u0,v1)` where `u`/`v` are the face's tangent axes.
pub const CORNER_OFFSETS: [(i64, i64); 4] = [(0, 0), (1, 0), (1, 1), (0, 1)];

/// AO level for a single corner of `dir`'s face on the solid cell `cell`.
/// `du`/`dv` are `0` or `1` and select which corner (see [`CORNER_OFFSETS`]).
pub fn corner_level(
    sampler: &VolumeSampler<'_>,
    dir: FaceDir,
    cell: [i64; 3],
    du: i64,
    dv: i64,
) -> u8 {
    let (ua, va) = dir.tangent_axes();
    let outside = dir.neighbour(cell);

    // Step from the outside cell toward the corner: -1 when the corner is at the
    // low edge of the tangent axis, +1 at the high edge.
    let su = if du == 1 { 1 } else { -1 };
    let sv = if dv == 1 { 1 } else { -1 };

    let side_u = solid_at(sampler, outside, ua, su);
    let side_v = solid_at(sampler, outside, va, sv);

    if side_u && side_v {
        return 0;
    }
    let corner = {
        let mut c = outside;
        c[ua] += su;
        c[va] += sv;
        sampler.is_solid(c)
    };
    3 - (u8::from(side_u) + u8::from(side_v) + u8::from(corner))
}

/// All four corner levels for one face, in [`CORNER_OFFSETS`] order.
pub fn quad_levels(sampler: &VolumeSampler<'_>, dir: FaceDir, cell: [i64; 3]) -> [u8; 4] {
    let mut out = [0u8; 4];
    for (i, (du, dv)) in CORNER_OFFSETS.iter().enumerate() {
        out[i] = corner_level(sampler, dir, cell, *du, *dv);
    }
    out
}

#[inline]
fn solid_at(sampler: &VolumeSampler<'_>, mut cell: [i64; 3], axis: usize, step: i64) -> bool {
    cell[axis] += step;
    sampler.is_solid(cell)
}

/// Maps an occlusion level `0..=3` to a brightness multiplier. Level `3` is
/// unoccluded (`1.0`); level `0` keeps a floor of `0.35` so fully occluded
/// corners are dim but not black.
#[inline]
pub fn ao_factor(level: u8) -> f32 {
    const FLOOR: f32 = 0.35;
    FLOOR + (1.0 - FLOOR) * (level.min(3) as f32 / 3.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
    use spall_voxel::{EditPlan, Volume};

    const STONE: MaterialId = MaterialId(1);

    fn solid_cells(cells: &[[i64; 3]]) -> Volume {
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let mut plan = EditPlan::new(v.id());
        for c in cells {
            plan.set(GlobalCell::new(c[0], c[1], c[2]), STONE);
        }
        v.apply_edit(&plan).unwrap();
        v
    }

    #[test]
    fn open_face_has_all_corners_unoccluded() {
        let v = solid_cells(&[[0, 0, 0]]);
        let s = VolumeSampler::new(&v);
        assert_eq!(quad_levels(&s, FaceDir::PosY, [0, 0, 0]), [3, 3, 3, 3]);
    }

    #[test]
    fn two_side_neighbours_fully_occlude_the_shared_corner() {
        // Top face of (0,0,0). Its +u/+v corner (u=Z, v=X for PosY) touches the
        // cells above and beside at (1,1,0) and (0,1,1); fill both.
        let v = solid_cells(&[[0, 0, 0], [1, 1, 0], [0, 1, 1]]);
        let s = VolumeSampler::new(&v);
        let levels = quad_levels(&s, FaceDir::PosY, [0, 0, 0]);
        // Corner index 2 is (u1, v1).
        assert_eq!(levels[2], 0, "both side neighbours present => level 0");
        assert_eq!(levels[0], 3, "opposite corner still open");
    }

    #[test]
    fn ao_factor_is_monotonic_and_bounded() {
        assert!((ao_factor(3) - 1.0).abs() < 1e-6);
        assert!(ao_factor(0) >= 0.34 && ao_factor(0) < ao_factor(1));
        assert!(ao_factor(1) < ao_factor(2) && ao_factor(2) < ao_factor(3));
    }
}

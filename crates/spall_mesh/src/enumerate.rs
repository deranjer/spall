//! The single source of truth for "which cell faces are exposed": every solid
//! cell in a box, every direction whose neighbour is not solid.

use spall_core::MaterialId;

use crate::ao::quad_levels;
use crate::face::FACE_DIRS;
use crate::mesh::FaceQuad;
use crate::sample::{Occupancy, ResidentCells, VolumeSampler};

/// One exposed unit face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExposedFace {
    pub cell: [i64; 3],
    pub dir: crate::face::FaceDir,
    pub material: MaterialId,
    /// State of the neighbour cell across this face.
    pub neighbour: Occupancy,
    /// Corner occlusion levels `0..=3`.
    pub ao: [u8; 4],
}

impl ExposedFace {
    /// The 1x1 [`FaceQuad`] for this face.
    pub fn unit_quad(&self) -> FaceQuad {
        let (ua, va) = self.dir.tangent_axes();
        FaceQuad {
            dir: self.dir,
            material: self.material,
            plane: self.dir.plane(self.cell),
            u0: self.cell[ua],
            v0: self.cell[va],
            u_len: 1,
            v_len: 1,
            ao: self.ao,
        }
    }

    /// True when the face is exposed only because the neighbour brick is not
    /// resident (an unresolved halo dependency).
    pub fn is_unresolved(&self) -> bool {
        matches!(self.neighbour, Occupancy::Unknown(_))
    }
}

/// Visits every exposed face of every solid cell of every resident brick in
/// `cells`, in canonical brick order then `(z, y, x)` cell order then
/// [`FACE_DIRS`] order.
///
/// Only resident-brick cells are enumerated, so the cost scales with the
/// resident data, not with the bounding hull. Seam faces are still fully
/// resolved: each solid cell samples its six neighbours directly, reaching one
/// cell past any brick boundary.
pub fn for_each_exposed_face(
    sampler: &VolumeSampler<'_>,
    cells: &ResidentCells,
    mut visit: impl FnMut(ExposedFace),
) {
    for cell in cells.cells() {
        let Occupancy::Solid(material) = sampler.at(cell) else {
            continue;
        };
        for dir in FACE_DIRS {
            let neighbour = sampler.at(dir.neighbour(cell));
            if neighbour.is_solid() {
                continue;
            }
            visit(ExposedFace {
                cell,
                dir,
                material,
                neighbour,
                ao: quad_levels(sampler, dir, cell),
            });
        }
    }
}

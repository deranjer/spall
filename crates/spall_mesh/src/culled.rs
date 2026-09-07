//! The reference face emitter: one unit quad per exposed cell face.
//!
//! No merging happens here, so its output is the ground truth every other
//! strategy is checked against — same set of unit faces, same surface area.

use crate::enumerate::for_each_exposed_face;
use crate::mesh::FaceQuad;
use crate::sample::{CellBox, VolumeSampler};

/// Emit one [`FaceQuad`] per exposed face of every solid cell in `cell_box`,
/// sorted into the canonical order (facing, plane, `v`, `u`) that
/// [`emit_greedy`](crate::greedy::emit_greedy) also produces.
pub fn emit_culled(sampler: &VolumeSampler<'_>, cell_box: CellBox) -> Vec<FaceQuad> {
    let mut quads = Vec::new();
    for_each_exposed_face(sampler, cell_box, |face| quads.push(face.unit_quad()));
    quads.sort_by_key(|q| (q.dir.code(), q.plane, q.material.raw(), q.v0, q.u0));
    quads
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
    use spall_voxel::{EditPlan, Volume};

    const STONE: MaterialId = MaterialId(1);

    fn box_of(cells: &[[i64; 3]]) -> (Volume, CellBox) {
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let mut plan = EditPlan::new(v.id());
        for c in cells {
            plan.set(GlobalCell::new(c[0], c[1], c[2]), STONE);
        }
        v.apply_edit(&plan).unwrap();
        let cb = CellBox::of_resident(&v).unwrap();
        (v, cb)
    }

    #[test]
    fn a_lone_cell_has_six_faces() {
        let (v, cb) = box_of(&[[0, 0, 0]]);
        let quads = emit_culled(&VolumeSampler::new(&v), cb);
        assert_eq!(quads.len(), 6);
        assert!(quads.iter().all(|q| q.u_len == 1 && q.v_len == 1));
    }

    #[test]
    fn a_shared_face_between_two_cells_is_not_emitted() {
        let (v, cb) = box_of(&[[0, 0, 0], [1, 0, 0]]);
        let quads = emit_culled(&VolumeSampler::new(&v), cb);
        // 2 cells * 6 faces - 2 hidden faces on the shared boundary.
        assert_eq!(quads.len(), 10);
    }

    #[test]
    fn faces_come_out_in_canonical_order() {
        let (v, cb) = box_of(&[[0, 0, 0], [0, 0, 1]]);
        let quads = emit_culled(&VolumeSampler::new(&v), cb);
        let keys: Vec<(u8, i64, i64, i64)> = quads
            .iter()
            .map(|q| (q.dir.code(), q.plane, q.v0, q.u0))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }
}

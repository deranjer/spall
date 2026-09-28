//! Greedy meshing: merge coplanar exposed faces that share a material, a
//! facing, and a full four-corner AO tuple into rectangular runs.
//!
//! Faces are bucketed by `(facing, plane, material)`. Within a bucket the merge
//! is the standard scan: for each unvisited face grow along `u` while the AO
//! tuple matches, then grow whole rows along `v`, then mark the rectangle
//! visited. Requiring the *entire* AO tuple to match (not just the shared edge)
//! keeps a merged quad's bilinear AO identical to every unit face it replaced.
//!
//! Bucket iteration and the in-bucket scan both run in sorted order, so the
//! output is deterministic and comes out in the same canonical order as
//! [`emit_culled`](crate::culled::emit_culled).

use std::collections::BTreeMap;

use spall_core::MaterialId;

use crate::enumerate::for_each_exposed_face;
use crate::face::FaceDir;
use crate::mesh::FaceQuad;
use crate::sample::{ResidentCells, VolumeSampler};

type BucketKey = (u8, i64, u16);
/// Faces in one coplanar bucket, keyed by `(v, u)` so iteration is row-major.
type Bucket = BTreeMap<(i64, i64), [u8; 4]>;

/// Emit merged [`FaceQuad`]s for every exposed face of every solid cell in
/// `cells`.
pub fn emit_greedy(sampler: &VolumeSampler<'_>, cells: &ResidentCells) -> Vec<FaceQuad> {
    let mut buckets: BTreeMap<BucketKey, Bucket> = BTreeMap::new();
    for_each_exposed_face(sampler, cells, |face| {
        let (ua, va) = face.dir.tangent_axes();
        buckets
            .entry((
                face.dir.code(),
                face.dir.plane(face.cell),
                face.material.raw(),
            ))
            .or_default()
            .insert((face.cell[va], face.cell[ua]), face.ao);
    });

    let mut quads = Vec::new();
    for ((dir_code, plane, material), bucket) in buckets {
        let dir = FACE_DIR_BY_CODE[dir_code as usize];
        merge_bucket(dir, plane, MaterialId(material), &bucket, &mut quads);
    }
    quads
}

const FACE_DIR_BY_CODE: [FaceDir; 6] = crate::face::FACE_DIRS;

fn merge_bucket(
    dir: FaceDir,
    plane: i64,
    material: MaterialId,
    bucket: &Bucket,
    out: &mut Vec<FaceQuad>,
) {
    let mut visited: std::collections::BTreeSet<(i64, i64)> = std::collections::BTreeSet::new();

    for (&(v, u), &ao) in bucket {
        if visited.contains(&(v, u)) {
            continue;
        }

        // Grow along u while the face exists, is unvisited, and shares the tuple.
        let mut u_len = 1;
        while matches!(bucket.get(&(v, u + u_len)), Some(&other) if other == ao)
            && !visited.contains(&(v, u + u_len))
        {
            u_len += 1;
        }

        // Grow whole rows along v.
        let mut v_len = 1;
        'rows: loop {
            let vv = v + v_len;
            for uu in u..u + u_len {
                match bucket.get(&(vv, uu)) {
                    Some(&other) if other == ao && !visited.contains(&(vv, uu)) => {}
                    _ => break 'rows,
                }
            }
            v_len += 1;
        }

        for vv in v..v + v_len {
            for uu in u..u + u_len {
                visited.insert((vv, uu));
            }
        }

        out.push(FaceQuad {
            dir,
            material,
            plane,
            u0: u,
            v0: v,
            u_len,
            v_len,
            ao,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::culled::emit_culled;
    use spall_core::{CellSizeCode, GlobalCell, VolumeId};
    use spall_voxel::{EditPlan, Volume};

    const STONE: MaterialId = MaterialId(1);

    fn scene(cells: &[[i64; 3]]) -> (Volume, ResidentCells) {
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let mut plan = EditPlan::new(v.id());
        for c in cells {
            plan.set(GlobalCell::new(c[0], c[1], c[2]), STONE);
        }
        v.apply_edit(&plan).unwrap();
        let cells = ResidentCells::plan(&v, ResidentCells::DEFAULT_CELL_VISIT_BUDGET).unwrap();
        (v.clone(), cells)
    }

    fn flat_slab() -> Vec<[i64; 3]> {
        let mut c = Vec::new();
        for x in 0..4 {
            for z in 0..4 {
                c.push([x, 0, z]);
            }
        }
        c
    }

    #[test]
    fn a_flat_slab_top_merges_into_one_quad() {
        let (v, cb) = scene(&flat_slab());
        let s = VolumeSampler::new(&v);
        let greedy = emit_greedy(&s, &cb);
        let top: Vec<_> = greedy.iter().filter(|q| q.dir == FaceDir::PosY).collect();
        assert_eq!(top.len(), 1);
        assert_eq!((top[0].u_len, top[0].v_len), (4, 4));
    }

    #[test]
    fn greedy_covers_exactly_the_culled_unit_face_set() {
        for cells in [
            flat_slab(),
            vec![[0, 0, 0], [2, 0, 0], [0, 0, 2], [2, 2, 2]],
            vec![[-3, -3, -3], [-3, -2, -3], [-3, -3, -2]],
        ] {
            let (v, cb) = scene(&cells);
            let s = VolumeSampler::new(&v);
            let culled: std::collections::BTreeSet<_> = emit_culled(&s, &cb)
                .iter()
                .flat_map(|q| q.unit_faces().collect::<Vec<_>>())
                .collect();
            let greedy: std::collections::BTreeSet<_> = emit_greedy(&s, &cb)
                .iter()
                .flat_map(|q| q.unit_faces().collect::<Vec<_>>())
                .collect();
            assert_eq!(greedy, culled, "cells = {cells:?}");
        }
    }

    #[test]
    fn a_checkerboard_slab_cannot_merge_any_face() {
        // Alternating solid cells in a plane: no two exposed top faces touch.
        let mut cells = Vec::new();
        for x in 0..6 {
            for z in 0..6 {
                if (x + z) % 2 == 0 {
                    cells.push([x, 0, z]);
                }
            }
        }
        let (v, cb) = scene(&cells);
        let s = VolumeSampler::new(&v);
        let culled = emit_culled(&s, &cb);
        let greedy = emit_greedy(&s, &cb);
        assert_eq!(greedy.len(), culled.len());
        assert!(greedy.iter().all(|q| q.u_len == 1 && q.v_len == 1));
    }
}

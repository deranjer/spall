//! Deterministic collider policy for an authoritative body.
//!
//! Every body — terrain and detached — uses the
//! [`spall_physics::Representation::MergedCuboids`] compound selected in T06
//! (`docs/collision-decision.md`). This module adds the **primitive budget** that
//! decision left for `spall_sim` to enforce: if the greedy decomposition of a
//! body's occupancy would exceed [`PRIMITIVE_BUDGET`] boxes, the collider is
//! built from a **coarsened** occupancy instead — the body's own grid
//! downsampled by an integer factor `k` where a coarse cell is solid iff *any*
//! covered fine cell is solid.
//!
//! Coarsening only ever *adds* collision volume inside the body's own footprint;
//! it never removes gameplay matter and never becomes a convex hull. The fine
//! voxel grid stays authoritative for geometry and mass; `k` travels in the
//! body's collider revision so replication and persistence agree on what was
//! built.

use spall_physics::{OccupancyGrid, Representation, greedy_boxes};

/// Merged-cuboid budget per body (provisional, from `docs/collision-decision.md`).
pub const PRIMITIVE_BUDGET: usize = 4096;

/// Downsample factors tried in order once the budget is exceeded.
const COARSEN_STEPS: [u32; 2] = [2, 4];

/// The collider to build for one body.
#[derive(Debug, Clone)]
pub struct ColliderPlan {
    /// Always [`Representation::MergedCuboids`] for now; kept explicit so a
    /// future policy change is visible at the call site.
    pub representation: Representation,
    /// Integer downsample factor: `1` = exact fine grid, `2` / `4` = coarse
    /// fallback.
    pub coarsen_k: u32,
    /// The grid the collider is actually built from (fine grid when `k == 1`,
    /// otherwise the coarsened grid).
    pub grid: OccupancyGrid,
    /// Merged-box count of `grid` — the primitive count of the built compound.
    pub primitives: usize,
}

/// Chooses a collider representation for `fine` and returns the grid to build
/// from. Deterministic: the same occupancy always yields the same plan.
pub fn plan_collider(fine: &OccupancyGrid) -> ColliderPlan {
    let primitives = greedy_boxes(fine).len();
    if primitives <= PRIMITIVE_BUDGET {
        return ColliderPlan {
            representation: Representation::MergedCuboids,
            coarsen_k: 1,
            grid: fine.clone(),
            primitives,
        };
    }

    for &k in &COARSEN_STEPS {
        let coarse = coarsen(fine, k);
        let primitives = greedy_boxes(&coarse).len();
        if primitives <= PRIMITIVE_BUDGET {
            return ColliderPlan {
                representation: Representation::MergedCuboids,
                coarsen_k: k,
                grid: coarse,
                primitives,
            };
        }
    }

    // Even at the coarsest step the body is pathological; build it anyway (never
    // a hull, never dropped mass) and let the scale gate flag it.
    let k = *COARSEN_STEPS.last().unwrap();
    let coarse = coarsen(fine, k);
    let primitives = greedy_boxes(&coarse).len();
    ColliderPlan {
        representation: Representation::MergedCuboids,
        coarsen_k: k,
        grid: coarse,
        primitives,
    }
}

/// Coarse-downsamples `fine` by integer factor `k` and re-inflates it back to
/// the *same* resolution: a `k×k×k` block is solid iff any covered fine cell is
/// solid, and every fine cell of a solid block is then marked solid. The result
/// keeps `fine`'s dims, origin, and cell size (so the collider builds unchanged
/// through [`spall_physics`]), but the greedy decomposition sees fat aligned
/// blocks instead of speckle and produces far fewer boxes.
///
/// This only ever *adds* collision volume inside the body's footprint. A solid
/// cell's material is preserved; an inflated cell takes the block's first solid
/// material in canonical order.
fn coarsen(fine: &OccupancyGrid, k: u32) -> OccupancyGrid {
    let k = k.max(1);
    let dims = fine.dims();
    let count = dims[0] as usize * dims[1] as usize * dims[2] as usize;
    let cdims = [
        dims[0].div_ceil(k),
        dims[1].div_ceil(k),
        dims[2].div_ceil(k),
    ];
    let cidx = |x: u32, y: u32, z: u32| (x + cdims[0] * (y + cdims[1] * z)) as usize;

    // Pass 1: which coarse blocks contain any solid, and their material.
    let mut block_solid = vec![false; cdims[0] as usize * cdims[1] as usize * cdims[2] as usize];
    let mut block_mat = vec![spall_core::MaterialId::AIR; block_solid.len()];
    for fz in 0..dims[2] {
        for fy in 0..dims[1] {
            for fx in 0..dims[0] {
                if let Some(m) = fine.material(fx, fy, fz) {
                    let ci = cidx(fx / k, fy / k, fz / k);
                    if !block_solid[ci] {
                        block_solid[ci] = true;
                        block_mat[ci] = m;
                    }
                }
            }
        }
    }

    // Pass 2: inflate every solid block back to full resolution.
    let idx = |x: u32, y: u32, z: u32| (x + dims[0] * (y + dims[1] * z)) as usize;
    let mut solid = vec![false; count];
    let mut material = vec![spall_core::MaterialId::AIR; count];
    for fz in 0..dims[2] {
        for fy in 0..dims[1] {
            for fx in 0..dims[0] {
                let ci = cidx(fx / k, fy / k, fz / k);
                if block_solid[ci] {
                    let i = idx(fx, fy, fz);
                    solid[i] = true;
                    material[i] = fine.material(fx, fy, fz).unwrap_or(block_mat[ci]);
                }
            }
        }
    }

    OccupancyGrid::from_solid_mask(fine.origin(), dims, solid, material)
        .expect("inflated grid keeps the source dims")
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
    use spall_voxel::{EditPlan, Volume};

    fn vid(n: u64) -> VolumeId {
        VolumeId::new(n).unwrap()
    }

    fn checkerboard(edge: i64) -> OccupancyGrid {
        let mut v = Volume::new(vid(1), CellSizeCode::Quarter);
        let mut plan = EditPlan::new(v.id());
        for z in 0..edge {
            for y in 0..edge {
                for x in 0..edge {
                    if (x + y + z) % 2 == 0 {
                        plan.set(GlobalCell::new(x, y, z), MaterialId(1));
                    }
                }
            }
        }
        v.apply_edit(&plan).unwrap();
        OccupancyGrid::from_region(
            &v,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(edge - 1, edge - 1, edge - 1),
        )
        .unwrap()
    }

    #[test]
    fn a_compact_body_is_built_at_full_resolution() {
        let mut v = Volume::new(vid(1), CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            v.id(),
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(9, 9, 9),
            MaterialId(1),
        ))
        .unwrap();
        let grid = OccupancyGrid::from_volume(&v).unwrap().unwrap();
        let plan = plan_collider(&grid);
        assert_eq!(plan.coarsen_k, 1);
        assert!(plan.primitives <= PRIMITIVE_BUDGET);
    }

    #[test]
    fn a_fragmented_body_over_budget_is_coarsened_and_never_loses_mass() {
        // 32³ checkerboard: ~16k isolated solid cells -> ~16k boxes, over budget.
        let grid = checkerboard(32);
        assert!(greedy_boxes(&grid).len() > PRIMITIVE_BUDGET);

        let plan = plan_collider(&grid);
        assert!(plan.coarsen_k >= 2, "over-budget body must coarsen");
        assert!(plan.primitives <= PRIMITIVE_BUDGET);
        // Same resolution, same footprint, only ever more solid (never less).
        assert_eq!(plan.grid.dims(), grid.dims());
        assert!(plan.grid.solid_count() >= grid.solid_count());
    }
}

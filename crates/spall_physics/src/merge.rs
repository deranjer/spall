//! Deterministic greedy decomposition of a solid occupancy grid into a minimal
//! set of axis-aligned cell boxes.
//!
//! This is the geometry the *merged-cuboid compound* collider is built from —
//! the exact-occupied-space correctness baseline the architecture calls for. A
//! single convex hull over a hollow building would seal its rooms; a per-cell
//! cuboid compound is exact but wastes primitives. Greedy box merging keeps the
//! occupied set exact while collapsing solid slabs to a handful of boxes.
//!
//! Algorithm (stable, order-independent result):
//!
//! 1. Walk cells in canonical `(z, y, x)` order.
//! 2. At the first not-yet-consumed solid cell `(x0, y0, z0)`, grow a box:
//!    - extend `+x` while the next cell is solid and unconsumed → `x1`;
//!    - extend `+y`: accept row `y` only if the whole span `x0..=x1` at `y` is
//!      solid and unconsumed → `y1`;
//!    - extend `+z`: accept layer `z` only if the whole rectangle
//!      `x0..=x1 × y0..=y1` at `z` is solid and unconsumed → `z1`.
//! 3. Mark every cell of `[x0..=x1, y0..=y1, z0..=z1]` consumed and emit the box.
//!
//! Material is ignored: collision geometry does not need the visual palette, and
//! [`crate::mass`] computes mass per solid cell independently. The result covers
//! exactly the solid set with no overlap.

use crate::occupancy::OccupancyGrid;

/// An inclusive axis-aligned box in grid-cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxSpan {
    /// Inclusive minimum cell `[x, y, z]`.
    pub min: [u32; 3],
    /// Inclusive maximum cell `[x, y, z]`.
    pub max: [u32; 3],
}

impl BoxSpan {
    /// Cell counts on each axis.
    pub fn extent(&self) -> [u32; 3] {
        [
            self.max[0] - self.min[0] + 1,
            self.max[1] - self.min[1] + 1,
            self.max[2] - self.min[2] + 1,
        ]
    }

    /// Number of cells the box covers.
    pub fn volume(&self) -> u64 {
        let e = self.extent();
        e[0] as u64 * e[1] as u64 * e[2] as u64
    }
}

/// Greedily decomposes the solid set of `grid` into non-overlapping boxes that
/// exactly cover it. Boxes are returned in the order they are grown, which is
/// canonical `(z, y, x)` by their minimum corner.
pub fn greedy_boxes(grid: &OccupancyGrid) -> Vec<BoxSpan> {
    let dims = grid.dims();
    let (dx, dy, dz) = (dims[0], dims[1], dims[2]);
    let mut consumed = vec![false; (dx as usize) * (dy as usize) * (dz as usize)];
    let lin = |x: u32, y: u32, z: u32| (x + dx * (y + dy * z)) as usize;
    let mut boxes = Vec::new();

    for z0 in 0..dz {
        for y0 in 0..dy {
            for x0 in 0..dx {
                if consumed[lin(x0, y0, z0)] || !grid.is_solid(x0, y0, z0) {
                    continue;
                }

                // Grow +x.
                let mut x1 = x0;
                while x1 + 1 < dx && grid.is_solid(x1 + 1, y0, z0) && !consumed[lin(x1 + 1, y0, z0)]
                {
                    x1 += 1;
                }

                // Grow +y while the whole x-span stays solid and free.
                let mut y1 = y0;
                'y: while y1 + 1 < dy {
                    for x in x0..=x1 {
                        if !grid.is_solid(x, y1 + 1, z0) || consumed[lin(x, y1 + 1, z0)] {
                            break 'y;
                        }
                    }
                    y1 += 1;
                }

                // Grow +z while the whole x*y rectangle stays solid and free.
                let mut z1 = z0;
                'z: while z1 + 1 < dz {
                    for y in y0..=y1 {
                        for x in x0..=x1 {
                            if !grid.is_solid(x, y, z1 + 1) || consumed[lin(x, y, z1 + 1)] {
                                break 'z;
                            }
                        }
                    }
                    z1 += 1;
                }

                for z in z0..=z1 {
                    for y in y0..=y1 {
                        for x in x0..=x1 {
                            consumed[lin(x, y, z)] = true;
                        }
                    }
                }
                boxes.push(BoxSpan {
                    min: [x0, y0, z0],
                    max: [x1, y1, z1],
                });
            }
        }
    }
    boxes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::occupancy::OccupancyGrid;
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
    fn a_solid_box_collapses_to_one_span() {
        let grid = solid_box(GlobalCell::new(0, 0, 0), GlobalCell::new(9, 4, 6));
        let boxes = greedy_boxes(&grid);
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].min, [0, 0, 0]);
        assert_eq!(boxes[0].max, [9, 4, 6]);
    }

    #[test]
    fn boxes_exactly_cover_the_solid_set_without_overlap() {
        let v = fixtures::hollow_tower(vid(7));
        let grid = OccupancyGrid::from_volume(&v).unwrap().unwrap();
        let boxes = greedy_boxes(&grid);

        let covered: u64 = boxes.iter().map(BoxSpan::volume).sum();
        assert_eq!(covered, grid.solid_count(), "no overlap, full cover");

        // Every covered cell is solid; every solid cell is covered exactly once.
        let dims = grid.dims();
        let mut hits = vec![0u8; (dims[0] * dims[1] * dims[2]) as usize];
        for b in &boxes {
            for z in b.min[2]..=b.max[2] {
                for y in b.min[1]..=b.max[1] {
                    for x in b.min[0]..=b.max[0] {
                        assert!(grid.is_solid(x, y, z));
                        hits[(x + dims[0] * (y + dims[1] * z)) as usize] += 1;
                    }
                }
            }
        }
        grid.for_each_solid(|x, y, z, _| {
            assert_eq!(hits[(x + dims[0] * (y + dims[1] * z)) as usize], 1);
        });

        // The shell is far cheaper as merged boxes than as one cuboid per cell.
        assert!(
            (boxes.len() as u64) < grid.solid_count() / 8,
            "merged {} boxes for {} solid cells",
            boxes.len(),
            grid.solid_count()
        );
    }

    #[test]
    fn result_is_independent_of_a_shifted_origin() {
        let a = solid_box(GlobalCell::new(0, 0, 0), GlobalCell::new(5, 5, 5));
        let b = solid_box(GlobalCell::new(-40, 7, 100), GlobalCell::new(-35, 12, 105));
        assert_eq!(greedy_boxes(&a), greedy_boxes(&b));
    }
}

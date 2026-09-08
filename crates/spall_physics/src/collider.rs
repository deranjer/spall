//! Building a Rapier collider from a solid occupancy grid, two ways.
//!
//! - [`Representation::NativeVoxels`] — one `parry` voxel shape over the grid's
//!   solid cells. Concavity is native; a rebuild after an edit is one shape
//!   construction.
//! - [`Representation::MergedCuboids`] — a compound of the axis-aligned boxes
//!   from [`crate::merge::greedy_boxes`]. Exact occupied space, the correctness
//!   baseline, at the cost of more primitives and a slower rebuild.
//!
//! This module is one of only two that name Rapier types ([`crate::world`] is
//! the other); everything above it works in engine terms.

use std::time::{Duration, Instant};

use rapier3d::prelude::*;

use crate::merge::{BoxSpan, greedy_boxes};
use crate::occupancy::OccupancyGrid;

/// Which collision representation to build for a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Representation {
    /// A single native voxel shape.
    NativeVoxels,
    /// A compound of greedily merged cuboids.
    MergedCuboids,
}

impl Representation {
    /// Short stable label for reports.
    pub fn label(self) -> &'static str {
        match self {
            Self::NativeVoxels => "native_voxels",
            Self::MergedCuboids => "merged_cuboids",
        }
    }
}

/// A built collider plus the cost of building it.
pub struct ColliderBuild {
    /// The Rapier collider, ready to attach to a body.
    pub collider: Collider,
    /// Primitive count: `1` for the voxel shape, or the number of merged boxes.
    pub primitives: usize,
    /// Wall-clock time for the **complete** occupancy → collider construction:
    /// the native solid-index extraction (and its allocation) or the greedy box
    /// decomposition and every per-part shape allocation, *plus* the final
    /// Rapier shape wrapping. This is the figure the feasibility report and
    /// `docs/collision-decision.md` cite as build / rebuild cost — it is not a
    /// wrap-only measurement.
    pub build: Duration,
    /// The sub-interval of [`Self::build`] spent only inside the final Rapier
    /// shape construction (`ColliderBuilder::voxels` / `::compound`). Retained as
    /// a component timing so the wrapping cost stays visible next to the total.
    pub wrap: Duration,
    /// Rough resident size of the shape's geometry, bytes. An estimate for the
    /// feasibility report, not an allocator measurement.
    pub est_bytes: usize,
}

/// Builds a collider for `grid` (cell edge `cell_m` metres) in `rep`. The shape
/// is placed so grid cell `(0, 0, 0)`'s corner is the body-local origin.
pub fn build_collider(grid: &OccupancyGrid, cell_m: f32, rep: Representation) -> ColliderBuild {
    match rep {
        Representation::NativeVoxels => build_native(grid, cell_m),
        Representation::MergedCuboids => build_compound(grid, cell_m),
    }
}

fn build_native(grid: &OccupancyGrid, cell_m: f32) -> ColliderBuild {
    // The timer starts before the solid-index extraction: pulling every solid
    // cell out of the dense grid and allocating the index vector is part of the
    // occupancy → collider work, not setup that can be excluded.
    let start = Instant::now();
    let indices: Vec<IVector> = grid
        .solid_indices()
        .into_iter()
        .map(|[x, y, z]| IVector::new(x, y, z))
        .collect();

    let wrap_start = Instant::now();
    let collider = ColliderBuilder::voxels(Vector::splat(cell_m), &indices).build();
    let wrap = wrap_start.elapsed();
    let build = start.elapsed();

    // parry stores per-voxel state (occupancy + face flags) plus a spatial
    // index; a dense-array upper bound is one u32 per grid cell.
    let dims = grid.dims();
    let est_bytes = (dims[0] as usize * dims[1] as usize * dims[2] as usize) * 4 + 512;

    ColliderBuild {
        collider,
        primitives: 1,
        build,
        wrap,
        est_bytes,
    }
}

fn build_compound(grid: &OccupancyGrid, cell_m: f32) -> ColliderBuild {
    // The timer covers the whole decomposition: the greedy box merge over the
    // grid (the potentially dominant 128³ scan for a large body) and every
    // per-part cuboid/isometry allocation, then the Rapier compound wrapping.
    let start = Instant::now();
    let boxes = greedy_boxes(grid);
    let parts: Vec<(Pose, SharedShape)> = boxes
        .iter()
        .map(|b: &BoxSpan| {
            let ext = b.extent();
            let half = [
                ext[0] as f32 * cell_m * 0.5,
                ext[1] as f32 * cell_m * 0.5,
                ext[2] as f32 * cell_m * 0.5,
            ];
            let centre = Vector::new(
                b.min[0] as f32 * cell_m + half[0],
                b.min[1] as f32 * cell_m + half[1],
                b.min[2] as f32 * cell_m + half[2],
            );
            (
                Pose::from_translation(centre),
                SharedShape::cuboid(half[0], half[1], half[2]),
            )
        })
        .collect();
    let primitives = parts.len();

    let wrap_start = Instant::now();
    let collider = ColliderBuilder::compound(parts).build();
    let wrap = wrap_start.elapsed();
    let build = start.elapsed();

    // Per part: an isometry (~28 B) + a cuboid half-extents vector (12 B) + the
    // shared-shape Arc slot and tree node (~48 B).
    let est_bytes = primitives * 88 + 128;

    ColliderBuild {
        collider,
        primitives,
        build,
        wrap,
        est_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapier3d::parry::query::Ray;
    use spall_core::VolumeId;
    use spall_voxel::fixtures;

    fn vid(n: u64) -> VolumeId {
        VolumeId::new(n).unwrap()
    }

    fn hollow_grid() -> OccupancyGrid {
        let v = fixtures::hollow_tower(vid(1));
        OccupancyGrid::from_volume(&v).unwrap().unwrap()
    }

    #[test]
    fn build_time_covers_decomposition_not_just_the_rapier_wrap() {
        let grid = hollow_grid();
        for rep in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let built = build_collider(&grid, 0.25, rep);
            // The complete build must include, and so be at least as large as,
            // the wrap-only component it contains.
            assert!(
                built.build >= built.wrap,
                "{}: complete build {:?} < wrap-only {:?}",
                rep.label(),
                built.build,
                built.wrap
            );
            assert!(
                built.build.as_nanos() > 0,
                "{}: zero build time",
                rep.label()
            );
        }
    }

    #[test]
    fn both_representations_build_for_a_hollow_shell() {
        let grid = hollow_grid();
        let native = build_collider(&grid, 0.25, Representation::NativeVoxels);
        let compound = build_collider(&grid, 0.25, Representation::MergedCuboids);
        assert_eq!(native.primitives, 1);
        assert!(compound.primitives > 1);
        // The merged compound uses far fewer parts than one box per solid cell.
        assert!((compound.primitives as u64) < grid.solid_count() / 8);
    }

    #[test]
    fn the_hollow_interior_survives_both_representations() {
        // A convex-hull approximation would seal the tower. Fire a ray from the
        // interior centre outward: in a real hollow it travels a real gap
        // before hitting the inner wall face.
        let grid = hollow_grid();
        let s = 0.25_f32;
        let dims = grid.dims();
        let centre = Vector::new(
            dims[0] as f32 * s * 0.5,
            dims[1] as f32 * s * 0.5,
            dims[2] as f32 * s * 0.5,
        );
        for rep in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let built = build_collider(&grid, s, rep);
            let shape = built.collider.shape();
            let ray = Ray::new(centre, Vector::new(1.0, 0.0, 0.0));
            let toi = shape
                .cast_ray(&Pose::IDENTITY, &ray, 100.0, true)
                .unwrap_or(0.0);
            // Interior half-width is ~4 cells of void before the 2-cell shell.
            assert!(
                toi > 3.0 * s,
                "{}: interior clearance {toi} collapsed",
                rep.label()
            );
        }
    }
}

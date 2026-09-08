//! Deterministic **exact** collider policy for an authoritative body.
//!
//! Terrain and detached bodies are *active*: openings must block, deleted cells
//! must stop colliding, and a physics query must see exactly the authoritative
//! occupied space. So the collider is always built from the body's **fine**
//! occupancy grid — never a coarsened or inflated approximation. The primitive
//! budget selects the *representation*, never the *geometry*:
//!
//! - `greedy_boxes(fine).len() <= `[`PRIMITIVE_BUDGET`] — build the
//!   [`spall_physics::Representation::MergedCuboids`] compound selected in T06
//!   (`docs/collision-decision.md`): exact occupied space, a cheap bounded-edit
//!   rebuild, and working CCD.
//! - over budget — fall back to the exact
//!   [`spall_physics::Representation::NativeVoxels`] shape: one `parry` voxel
//!   shape over the *same* fine solid set, `O(1)` primitives. Every fine-grid
//!   air passage and every solid cell is preserved bit-for-bit; the cost is a
//!   heavier rebuild and weaker CCD for that one body (see the doc).
//! - over budget **and** the fine grid larger than
//!   [`MAX_ACTIVE_COLLIDER_CELLS`] — [`plan_collider`] returns
//!   [`ColliderInfeasible`]. A full native-voxel collider rebuild costs roughly
//!   41 ns per grid cell (release, measured — see `sweep_native_rebuild_cost`),
//!   and an active body rebuilds its whole collider on every accepted edit, so
//!   past this bound the exact per-tick-editable rebuild no longer fits the
//!   tick. The feasibility gate stays *failed* — the commit / spawn errors —
//!   rather than silently building an oversized or inflated collider.
//!
//! There is **no** coarsen / OR-downsample / "build it anyway" path: active
//! collision is never approximated, and a failed budget is never bypassed with
//! approximate topology.

use spall_physics::{OccupancyGrid, Representation, greedy_boxes};

/// Merged-cuboid budget per body: above this the exact native voxel shape is
/// used instead of the compound (provisional, from `docs/collision-decision.md`).
pub const PRIMITIVE_BUDGET: usize = 4096;

/// Largest fine-grid cell count for which the **native-voxel** exact fallback is
/// declared feasible: `1 << 17` = 131 072 cells (~50³).
///
/// A full native `parry` voxel collider rebuild scales ~linearly with total
/// grid cells — ~41 ns/cell in release for a maximally fragmented body
/// (`sweep_native_rebuild_cost`: 110 k cells → 4.5 ms, 262 k → 10.4 ms). At
/// this bound a worst-case rebuild is ~5.4 ms, about a third of the 16.7 ms
/// tick, leaving room for structure analysis, the commit and other bodies. An
/// active body needing an exact collider *larger and more fragmented* than this
/// is out of scope for per-tick editable collision (large-collapse / streaming,
/// T11 / T18) and is reported infeasible.
///
/// This gate applies **only** to the native fallback. A body within the
/// primitive budget builds a merged-cuboid compound whose rebuild cost is
/// bounded by the box count, not the cell count, so a large but simple body
/// (e.g. a solid slab) is always feasible.
pub const MAX_ACTIVE_COLLIDER_CELLS: u128 = 1 << 17;

/// Why an exact active collider cannot be planned for a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ColliderInfeasible {
    /// The body's greedy decomposition is over [`PRIMITIVE_BUDGET`] (so it needs
    /// the native-voxel fallback) **and** its fine grid is larger than
    /// [`MAX_ACTIVE_COLLIDER_CELLS`], so a per-tick exact collider rebuild does
    /// not fit the tick. The policy will not coarsen or inflate to get under
    /// the bound.
    #[error(
        "fragmented body occupancy is {cells} cells ({greedy_boxes} greedy boxes), over the \
         {limit}-cell exact native-collider budget and with no coarse fallback for active bodies"
    )]
    TooLarge {
        cells: u128,
        greedy_boxes: usize,
        limit: u128,
    },
}

/// The collider to build for one body. `grid` is **always** the exact fine
/// occupancy; only `representation` changes with the primitive budget.
#[derive(Debug, Clone)]
pub struct ColliderPlan {
    /// [`Representation::MergedCuboids`] within budget, else the exact
    /// [`Representation::NativeVoxels`] shape.
    pub representation: Representation,
    /// Always `1`: active bodies are never coarsened (ENG-42). Retained so the
    /// persisted collider revision keeps its shape; a future save-format
    /// revision can drop it.
    pub coarsen_k: u32,
    /// The exact fine grid the collider is built from — same dims, origin, cell
    /// size, and solid set as the body's authoritative occupancy.
    pub grid: OccupancyGrid,
    /// Primitive count of the built collider: the greedy-box count for the
    /// compound, or `1` for the native voxel shape.
    pub primitives: usize,
}

/// Chooses the exact collider representation for `fine`. Deterministic: the same
/// occupancy always yields the same plan. Returns [`ColliderInfeasible`] rather
/// than approximating when the fine grid is too large to support exactly.
pub fn plan_collider(fine: &OccupancyGrid) -> Result<ColliderPlan, ColliderInfeasible> {
    let greedy = greedy_boxes(fine).len();
    if greedy <= PRIMITIVE_BUDGET {
        // Merged-cuboid compound: exact, cheap rebuild bounded by the box count,
        // working CCD. Cell count does not matter here.
        return Ok(ColliderPlan {
            representation: Representation::MergedCuboids,
            coarsen_k: 1,
            grid: fine.clone(),
            primitives: greedy,
        });
    }

    // Over the merged-cuboid budget: the exact native voxel shape covers the
    // identical fine solid set with one primitive — no coarsening, no inflation,
    // no dropped mass. Its full rebuild cost scales with the cell count, so it
    // is only feasible up to MAX_ACTIVE_COLLIDER_CELLS; past that the exact
    // per-tick-editable workload cannot be supported.
    let dims = fine.dims();
    let cells = u128::from(dims[0]) * u128::from(dims[1]) * u128::from(dims[2]);
    if cells > MAX_ACTIVE_COLLIDER_CELLS {
        return Err(ColliderInfeasible::TooLarge {
            cells,
            greedy_boxes: greedy,
            limit: MAX_ACTIVE_COLLIDER_CELLS,
        });
    }

    Ok(ColliderPlan {
        representation: Representation::NativeVoxels,
        coarsen_k: 1,
        grid: fine.clone(),
        primitives: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
    use spall_physics::{
        BodyKind, BodySpec, PhysicsConfig, PhysicsWorld, analytic_mass_properties, build_collider,
    };
    use spall_voxel::{EditPlan, Volume};
    use std::time::Instant;

    fn vid(n: u64) -> VolumeId {
        VolumeId::new(n).unwrap()
    }

    /// A connected fine-grid lattice: the struts of a 2-cell grid over an
    /// `edge`³ region. Every strut meets its neighbours at the even nodes, so
    /// the whole body is one connected component; it is riddled with 1×1 air
    /// tunnels and is merge-hostile, so greedy decomposition needs roughly
    /// `3·(edge/2)²` boxes.
    fn connected_lattice(edge: u32) -> OccupancyGrid {
        let dims = [edge; 3];
        let n = (edge as usize).pow(3);
        let lin = |x: u32, y: u32, z: u32| (x + edge * (y + edge * z)) as usize;
        let mut solid = vec![false; n];
        let mut material = vec![MaterialId::AIR; n];
        let strut = |a: u32, b: u32| a.is_multiple_of(2) && b.is_multiple_of(2);
        for z in 0..edge {
            for y in 0..edge {
                for x in 0..edge {
                    if strut(x, y) || strut(y, z) || strut(x, z) {
                        solid[lin(x, y, z)] = true;
                        material[lin(x, y, z)] = MaterialId(1);
                    }
                }
            }
        }
        OccupancyGrid::from_solid_mask(GlobalCell::new(0, 0, 0), dims, solid, material).unwrap()
    }

    fn solid_box(edge: i64, material: MaterialId) -> OccupancyGrid {
        let mut v = Volume::new(vid(1), CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            v.id(),
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(edge - 1, edge - 1, edge - 1),
            material,
        ))
        .unwrap();
        OccupancyGrid::from_volume(&v).unwrap().unwrap()
    }

    #[test]
    fn a_compact_body_is_built_at_full_resolution_as_a_compound() {
        let grid = solid_box(10, MaterialId(1));
        let plan = plan_collider(&grid).unwrap();
        assert_eq!(plan.representation, Representation::MergedCuboids);
        assert_eq!(plan.coarsen_k, 1);
        assert!(plan.primitives <= PRIMITIVE_BUDGET);
        assert_eq!(plan.grid.dims(), grid.dims());
        assert_eq!(plan.grid.solid_count(), grid.solid_count());
    }

    #[test]
    fn an_over_budget_connected_body_uses_the_exact_native_voxel_shape() {
        // edge 48: 110 592 cells (< the 131 072 native budget) but ~27 649
        // greedy boxes — well over the 4096 primitive budget.
        let grid = connected_lattice(48);
        assert!(
            greedy_boxes(&grid).len() > PRIMITIVE_BUDGET,
            "fixture must actually exceed the primitive budget"
        );
        assert!(
            (u128::from(grid.dims()[0]).pow(3)) <= MAX_ACTIVE_COLLIDER_CELLS,
            "fixture must stay within the native-collider cell budget"
        );

        let plan = plan_collider(&grid).unwrap();
        assert_eq!(
            plan.representation,
            Representation::NativeVoxels,
            "over-budget active body must fall back to the exact voxel shape, not coarsen"
        );
        assert_eq!(plan.coarsen_k, 1, "never coarsened");
        assert_eq!(plan.primitives, 1);

        // No inflation and no dropped mass: identical grid, cell for cell.
        assert_eq!(plan.grid.dims(), grid.dims());
        assert_eq!(plan.grid.solid_count(), grid.solid_count());
        for z in 0..grid.dims()[2] {
            for y in 0..grid.dims()[1] {
                for x in 0..grid.dims()[0] {
                    assert_eq!(
                        plan.grid.is_solid(x, y, z),
                        grid.is_solid(x, y, z),
                        "cell ({x},{y},{z}) occupancy changed"
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "manual sweep for picking MAX_ACTIVE_COLLIDER_CELLS; run with --release --ignored"]
    fn sweep_native_rebuild_cost() {
        for edge in [24u32, 32, 40, 48, 56, 64, 76, 96] {
            let grid = connected_lattice(edge);
            let boxes = greedy_boxes(&grid).len();
            let cell_m = 0.25_f32;
            let mut world = PhysicsWorld::new(PhysicsConfig::default());
            let id = world.add_body(BodySpec {
                kind: BodyKind::Dynamic { ccd: false },
                representation: Representation::NativeVoxels,
                grid: grid.clone(),
                cell_m,
                density_kg_m3: 1000.0,
                mass_properties: None,
                translation_m: [0.0; 3],
                linvel_m_s: [0.0; 3],
            });
            let mut best = std::time::Duration::MAX;
            for _ in 0..5 {
                let d = world.rebuild_collider(id, &grid, Representation::NativeVoxels);
                best = best.min(d);
            }
            eprintln!(
                "edge {edge:>3}: {:>8} total / {:>7} solid cells, {boxes:>6} greedy boxes, native rebuild min {best:?}",
                (edge as u64).pow(3),
                grid.solid_count(),
            );
        }
    }

    #[test]
    fn the_native_fallback_preserves_solid_mass_with_a_measured_bounded_rebuild() {
        let grid = connected_lattice(48);
        let plan = plan_collider(&grid).unwrap();
        assert_eq!(plan.representation, Representation::NativeVoxels);
        let cell_m = 0.25_f32;

        // Every fine solid cell's mass is kept (unit-density reference); the
        // grid comparison in the previous test already proved no air cell was
        // filled, so mass is neither inflated nor dropped.
        let analytic = analytic_mass_properties(&plan.grid, f64::from(cell_m), |_| 1.0);
        let expected_mass = grid.solid_count() as f64 * f64::from(cell_m).powi(3);
        assert!(
            (analytic.mass_kg - expected_mass).abs() / expected_mass < 1e-9,
            "native fallback keeps exactly the fine grid's solid mass"
        );

        // Real measurement: a fresh build and the full-collider rebuild an
        // active body runs on every accepted edit.
        let t0 = Instant::now();
        let built = build_collider(&plan.grid, cell_m, plan.representation);
        let fresh_build = t0.elapsed();
        assert_eq!(built.primitives, 1, "one native voxel shape");

        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let id = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: plan.representation,
            grid: plan.grid.clone(),
            cell_m,
            density_kg_m3: 1000.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        let mut rebuild = std::time::Duration::MAX;
        for _ in 0..3 {
            rebuild = rebuild.min(world.rebuild_collider(id, &plan.grid, plan.representation));
        }
        eprintln!(
            "native voxel fallback @ {} solid / {} total cells ({} build): fresh build {:?}, rebuild min {:?}",
            grid.solid_count(),
            grid.dims().iter().map(|&d| u64::from(d)).product::<u64>(),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            fresh_build,
            rebuild,
        );
        // The tick-budget guarantee only holds for the optimised build the
        // server actually runs. `sweep_native_rebuild_cost` measured ~4.5 ms
        // release at this size; debug `cargo test` is ~20× slower and only
        // records the figure.
        if cfg!(not(debug_assertions)) {
            assert!(
                rebuild.as_millis() < 8,
                "native rebuild {rebuild:?} exceeds the active-body tick budget"
            );
        }
    }

    #[test]
    fn an_over_budget_body_past_the_cell_bound_is_infeasible_not_approximated() {
        // edge 64: 262 144 cells (> the 131 072 native budget) and ~65 537
        // greedy boxes (> the primitive budget). No exact representation fits a
        // per-tick rebuild, and coarsening is banned — the policy reports it.
        let grid = connected_lattice(64);
        let greedy = greedy_boxes(&grid).len();
        assert!(greedy > PRIMITIVE_BUDGET);

        let err = plan_collider(&grid).unwrap_err();
        assert_eq!(
            err,
            ColliderInfeasible::TooLarge {
                cells: 64 * 64 * 64,
                greedy_boxes: greedy,
                limit: MAX_ACTIVE_COLLIDER_CELLS,
            }
        );
    }

    #[test]
    fn a_large_but_simple_body_stays_a_cheap_exact_compound() {
        // A solid 200³ box is 8 M cells — far past the native-voxel cell bound —
        // but decomposes to a single greedy span, so it is a cheap exact
        // compound, not infeasible. The cell bound gates only the native
        // fallback.
        let plan = plan_collider(&solid_box_grid(200)).unwrap();
        assert_eq!(plan.representation, Representation::MergedCuboids);
        assert_eq!(plan.primitives, 1);
        assert_eq!(plan.grid.solid_count(), 200 * 200 * 200);
    }

    fn solid_box_grid(edge: u32) -> OccupancyGrid {
        let n = (edge as usize).pow(3);
        OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [edge; 3],
            vec![true; n],
            vec![MaterialId(1); n],
        )
        .unwrap()
    }
}

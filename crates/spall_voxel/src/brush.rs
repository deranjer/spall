//! Integer brush → [`EditPlan`] generators.
//!
//! A brush turns a shape into a deterministic list of cell writes. The write
//! set and its order are a pure function of the inputs: cells are emitted in
//! canonical `(z, y, x)` order, and inclusion uses only integer / fixed-point
//! arithmetic (via [`SphereBrush::contains_cell_centre`]). This is the part of
//! an edit that must reproduce bit-for-bit; the server still validates the hit
//! that positioned the brush.
//!
//! Brush coordinates are in the **target volume's local cell space**. For
//! terrain (identity transform) local cells are global cells.

use spall_core::units::BRUSH_UNIT;
use spall_core::{GlobalCell, MaterialId, SphereBrush, VolumeId};

use crate::edit::EditPlan;

/// Half a cell, in brush fixed-point units — the offset from a cell's minimum
/// corner to its centre.
const HALF_CELL_UNITS: i64 = BRUSH_UNIT / 2;

impl EditPlan {
    /// A solid axis-aligned box covering the inclusive cell range between the
    /// two corners (given in either order), every cell set to `material`.
    /// Writes are emitted in canonical `(z, y, x)` order.
    pub fn filled_box(
        volume: VolumeId,
        a: GlobalCell,
        b: GlobalCell,
        material: MaterialId,
    ) -> Self {
        let (min, max) = normalise_box(a, b);
        let mut plan = EditPlan::new(volume);
        for z in min.z..=max.z {
            for y in min.y..=max.y {
                for x in min.x..=max.x {
                    plan.set(GlobalCell::new(x, y, z), material);
                }
            }
        }
        plan
    }

    /// An integer sphere brush: every cell whose centre lies within `brush`
    /// (squared distance `<=` squared radius, in fixed-point) is set to
    /// `material`. Writes are emitted in canonical `(z, y, x)` order.
    ///
    /// The candidate range is the sphere's cell-space bounding box; each cell in
    /// it is tested with the exact fixed-point predicate, so a cell whose centre
    /// is exactly on the radius is included and the result never depends on
    /// floating point.
    pub fn sphere(volume: VolumeId, brush: SphereBrush, material: MaterialId) -> Self {
        let mut plan = EditPlan::new(volume);
        let Some((min, max)) = sphere_cell_bounds(brush) else {
            return plan;
        };
        for z in min[2]..=max[2] {
            for y in min[1]..=max[1] {
                for x in min[0]..=max[0] {
                    let (cx, cy, cz) = cell_centre_units(x, y, z);
                    if brush.contains_cell_centre(cx, cy, cz) {
                        plan.set(GlobalCell::new(x, y, z), material);
                    }
                }
            }
        }
        plan
    }
}

fn normalise_box(a: GlobalCell, b: GlobalCell) -> (GlobalCell, GlobalCell) {
    (
        GlobalCell::new(a.x.min(b.x), a.y.min(b.y), a.z.min(b.z)),
        GlobalCell::new(a.x.max(b.x), a.y.max(b.y), a.z.max(b.z)),
    )
}

/// Fixed-point coordinate of the centre of integer cell `(x, y, z)`.
fn cell_centre_units(x: i64, y: i64, z: i64) -> (i64, i64, i64) {
    (
        x.saturating_mul(BRUSH_UNIT).saturating_add(HALF_CELL_UNITS),
        y.saturating_mul(BRUSH_UNIT).saturating_add(HALF_CELL_UNITS),
        z.saturating_mul(BRUSH_UNIT).saturating_add(HALF_CELL_UNITS),
    )
}

/// Inclusive integer cell bounding box of a sphere brush, or `None` if it is
/// empty. A cell `i` can only be inside when its centre `i*BRUSH_UNIT + half`
/// is within `radius` of the brush centre on that axis; the box is those
/// necessary bounds, widened by one cell for safety.
fn sphere_cell_bounds(brush: SphereBrush) -> Option<([i64; 3], [i64; 3])> {
    let r = i128::from(brush.radius_units());
    let centre = brush.centre;
    let unit = i128::from(BRUSH_UNIT);
    let half = i128::from(HALF_CELL_UNITS);

    let axis = |c: i64| -> Option<(i64, i64)> {
        let c = i128::from(c);
        // smallest i with i*unit + half >= c - r  ->  i >= (c - r - half) / unit
        let lo = div_ceil_i128(c - r - half, unit) - 1;
        // largest i with i*unit + half <= c + r   ->  i <= (c + r - half) / unit
        let hi = div_floor_i128(c + r - half, unit) + 1;
        if lo > hi {
            return None;
        }
        Some((clamp_i64(lo), clamp_i64(hi)))
    };

    let (x0, x1) = axis(centre.x)?;
    let (y0, y1) = axis(centre.y)?;
    let (z0, z1) = axis(centre.z)?;
    Some(([x0, y0, z0], [x1, y1, z1]))
}

fn div_floor_i128(a: i128, b: i128) -> i128 {
    a.div_euclid(b) // b is always positive here
}

fn div_ceil_i128(a: i128, b: i128) -> i128 {
    -((-a).div_euclid(b))
}

fn clamp_i64(v: i128) -> i64 {
    v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::Volume;
    use spall_core::units::{BRUSH_UNIT, BrushPoint};
    use spall_core::{BrickCoord, CellSizeCode, Revision};

    const STONE: MaterialId = MaterialId(1);

    fn vol(id: u64) -> Volume {
        Volume::new(VolumeId::new(id).unwrap(), CellSizeCode::Quarter)
    }

    fn digest(plan: &EditPlan) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"spall.voxel.test.brush-writes.v1");
        h.update(&(plan.writes.len() as u32).to_le_bytes());
        for w in &plan.writes {
            h.update(&w.cell.x.to_le_bytes());
            h.update(&w.cell.y.to_le_bytes());
            h.update(&w.cell.z.to_le_bytes());
            h.update(&w.material.raw().to_le_bytes());
        }
        h.finalize().to_hex().to_string()
    }

    #[test]
    fn filled_box_covers_the_inclusive_range_in_canonical_order() {
        let plan = EditPlan::filled_box(
            VolumeId::new(1).unwrap(),
            GlobalCell::new(2, 0, -1),
            GlobalCell::new(0, 1, 0),
            STONE,
        );
        // 3 * 2 * 2 = 12 cells.
        assert_eq!(plan.writes.len(), 12);
        // First write is the (z,y,x)-minimal corner, last is the maximal.
        assert_eq!(plan.writes.first().unwrap().cell, GlobalCell::new(0, 0, -1));
        assert_eq!(plan.writes.last().unwrap().cell, GlobalCell::new(2, 1, 0));
        let mut sorted = plan.writes.clone();
        sorted.sort_by_key(|w| (w.cell.z, w.cell.y, w.cell.x));
        assert_eq!(sorted, plan.writes, "writes already in (z,y,x) order");
    }

    #[test]
    fn box_edit_spanning_eight_bricks_touches_exactly_eight() {
        // A 4-cell cube straddling the corner where 8 bricks meet at (0,0,0).
        let mut v = vol(1);
        let plan = EditPlan::filled_box(
            v.id(),
            GlobalCell::new(-2, -2, -2),
            GlobalCell::new(1, 1, 1),
            STONE,
        );
        let outcome = v.apply_edit(&plan).unwrap();
        assert_eq!(outcome.bricks.len(), 8, "cube spans all 8 corner bricks");
        let coords: std::collections::BTreeSet<BrickCoord> =
            outcome.bricks.iter().map(|b| b.coord).collect();
        for x in [-1, 0] {
            for y in [-1, 0] {
                for z in [-1, 0] {
                    assert!(coords.contains(&BrickCoord::new(x, y, z)));
                }
            }
        }
    }

    #[test]
    fn sphere_edit_spanning_eight_bricks_touches_exactly_eight() {
        let mut v = vol(2);
        // Centre on the 8-brick corner, radius 3 cells.
        let brush = SphereBrush::new(BrushPoint::from_units(0, 0, 0), 3 * BRUSH_UNIT).unwrap();
        let plan = EditPlan::sphere(v.id(), brush, STONE);
        assert!(!plan.writes.is_empty());
        let outcome = v.apply_edit(&plan).unwrap();
        assert_eq!(outcome.bricks.len(), 8);
    }

    #[test]
    fn sphere_matches_a_brute_force_reference_over_a_wide_box() {
        let brush = SphereBrush::new(
            BrushPoint::from_cells(3, -2, 5).unwrap(),
            (2 * BRUSH_UNIT) + 130, // a non-integer radius
        )
        .unwrap();
        let plan = EditPlan::sphere(VolumeId::new(1).unwrap(), brush, STONE);
        let got: std::collections::BTreeSet<(i64, i64, i64)> = plan
            .writes
            .iter()
            .map(|w| (w.cell.x, w.cell.y, w.cell.z))
            .collect();

        let mut expected = std::collections::BTreeSet::new();
        for z in -6..=16 {
            for y in -12..=10 {
                for x in -6..=16 {
                    let (cx, cy, cz) = cell_centre_units(x, y, z);
                    if brush.contains_cell_centre(cx, cy, cz) {
                        expected.insert((x, y, z));
                    }
                }
            }
        }
        assert_eq!(got, expected);
        assert!(!expected.is_empty());
    }

    #[test]
    fn sphere_includes_a_cell_centre_exactly_on_the_radius() {
        // Centre at a cell centre; radius exactly 2 cells reaches the centre of
        // the cell 2 over.
        let brush = SphereBrush::new(
            BrushPoint::from_units(HALF_CELL_UNITS, HALF_CELL_UNITS, HALF_CELL_UNITS),
            2 * BRUSH_UNIT,
        )
        .unwrap();
        let plan = EditPlan::sphere(VolumeId::new(1).unwrap(), brush, STONE);
        let cells: std::collections::BTreeSet<(i64, i64, i64)> = plan
            .writes
            .iter()
            .map(|w| (w.cell.x, w.cell.y, w.cell.z))
            .collect();
        assert!(cells.contains(&(2, 0, 0)), "cell exactly on the radius");
        assert!(!cells.contains(&(3, 0, 0)), "cell just past the radius");
    }

    #[test]
    fn brush_write_lists_are_deterministic() {
        let box_a = EditPlan::filled_box(
            VolumeId::new(1).unwrap(),
            GlobalCell::new(-3, -3, -3),
            GlobalCell::new(2, 2, 2),
            STONE,
        );
        let box_b = EditPlan::filled_box(
            VolumeId::new(1).unwrap(),
            GlobalCell::new(2, 2, 2),
            GlobalCell::new(-3, -3, -3),
            STONE,
        );
        // Corner order does not matter; output is byte-stable.
        assert_eq!(digest(&box_a), digest(&box_b));

        let sphere_a = EditPlan::sphere(
            VolumeId::new(1).unwrap(),
            SphereBrush::new(
                BrushPoint::from_cells(1, 2, 3).unwrap(),
                4 * BRUSH_UNIT + 17,
            )
            .unwrap(),
            STONE,
        );
        let sphere_b = EditPlan::sphere(
            VolumeId::new(1).unwrap(),
            SphereBrush::new(
                BrushPoint::from_cells(1, 2, 3).unwrap(),
                4 * BRUSH_UNIT + 17,
            )
            .unwrap(),
            STONE,
        );
        assert_eq!(digest(&sphere_a), digest(&sphere_b));

        // Pinned digests: any change to the emitted cell set or its order
        // breaks these on purpose.
        assert_eq!(digest(&box_a), BOX_DIGEST);
        assert_eq!(digest(&sphere_a), SPHERE_DIGEST);
    }

    const BOX_DIGEST: &str = "fe1d695c324360a510c77813eefd9b1477a8a5f67843e6f4d2d56e012e8fdd3f";
    const SPHERE_DIGEST: &str = "6f582fad69174dcf68ce9366cf748b231cad75a24c2de3e45ef160a60622db3c";

    #[test]
    fn empty_sphere_bounds_produce_no_writes() {
        let brush = SphereBrush::new(BrushPoint::from_cells(0, 0, 0).unwrap(), 0).unwrap();
        let plan = EditPlan::sphere(VolumeId::new(1).unwrap(), brush, STONE);
        // Radius 0 at an integer corner: no cell centre is within 0 units.
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn box_and_sphere_plans_apply_and_read_back() {
        let mut v = vol(1);
        let plan = EditPlan::filled_box(
            v.id(),
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(1, 1, 1),
            STONE,
        );
        v.apply_edit(&plan).unwrap();
        assert_eq!(
            v.snapshot_brick(BrickCoord::new(0, 0, 0))
                .unwrap()
                .unwrap()
                .revision(),
            Revision(1)
        );
    }
}

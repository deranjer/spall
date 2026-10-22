use super::covering_spheres;
use spall_core::{BRUSH_UNIT, GlobalCell};

#[test]
fn empty_cells_has_no_spheres() {
    assert!(covering_spheres(&[], 5).is_empty());
}

#[test]
fn a_flat_wide_notch_is_covered_without_one_giant_sphere() {
    // A dam-gate-shaped notch: wide and tall, but only 8 cells thick —
    // nothing like a sphere. One bounding sphere over this would need
    // a radius of roughly the box's own half-diagonal (~14 cells for a
    // 24x14x8 box); no individual covering sphere here should come
    // anywhere near that.
    let mut cells = Vec::new();
    for x in 0..24 {
        for y in 0..14 {
            for z in 0..8 {
                cells.push(GlobalCell::new(x, y, z));
            }
        }
    }
    let spheres = covering_spheres(&cells, 5);
    assert!(
        spheres.len() > 1,
        "a shape this size needs more than one sphere"
    );
    for brush in &spheres {
        assert!(
            brush.radius_units() <= 7 * BRUSH_UNIT,
            "a single covering sphere ballooned out to the whole notch: {} units",
            brush.radius_units()
        );
    }
    // Every authored cell must still land inside at least one sphere.
    let half = BRUSH_UNIT / 2;
    for cell in &cells {
        let covered = spheres.iter().any(|brush| {
            brush.contains_cell_centre(
                cell.x * BRUSH_UNIT + half,
                cell.y * BRUSH_UNIT + half,
                cell.z * BRUSH_UNIT + half,
            )
        });
        assert!(covered, "{cell:?} is not covered by any sphere");
    }
}

use super::bounding_sphere_brush;
use spall_core::{BRUSH_UNIT, GlobalCell};

#[test]
fn empty_cells_has_no_brush() {
    assert!(bounding_sphere_brush(&[]).is_none());
}

#[test]
fn covers_every_marked_cell_with_only_a_little_slack() {
    // A compact, roughly spherical blob (as a scene author would place a
    // gate marker) around (10, 5, 2), radius 3 cells.
    let mut cells = Vec::new();
    for x in -3..=3 {
        for y in -3..=3 {
            for z in -3..=3 {
                if x * x + y * y + z * z <= 9 {
                    cells.push(GlobalCell::new(10 + x, 5 + y, 2 + z));
                }
            }
        }
    }
    let brush = bounding_sphere_brush(&cells).expect("non-empty cells");
    for cell in &cells {
        let half = BRUSH_UNIT / 2;
        assert!(
            brush.contains_cell_centre(
                cell.x * BRUSH_UNIT + half,
                cell.y * BRUSH_UNIT + half,
                cell.z * BRUSH_UNIT + half,
            ),
            "{cell:?} must be inside its own covering brush"
        );
    }
    // A blob of authored radius 3 should not need to balloon out to the
    // bounding box's own corner-to-corner reach (radius 3 * sqrt(3) ~= 5.2
    // cells): the fix in `bounding_sphere_brush` keeps it near 3 + 1 slack.
    assert!(
        brush.radius_units() <= 5 * BRUSH_UNIT,
        "brush grew past the authored blob: {} units",
        brush.radius_units()
    );
}

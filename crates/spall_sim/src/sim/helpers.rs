//! Pure geometry/contact helpers used by dam-gate and contact-damage code.

use super::*;

/// Covers `cells` with a set of modest spheres instead of one big bounding
/// sphere: partitions them into clusters no more than `max_radius_cells`
/// from a shared seed cell (first-remaining-cell order — simple, and correct
/// regardless of `cells`' own order), then covers each cluster with its own
/// tight [`bounding_sphere_brush`]. For a compact blob this converges to the
/// same single sphere `bounding_sphere_brush` alone would give; for a flat,
/// elongated shape (a dam gate's straight notch, not remotely spherical) it
/// keeps every individual cut close to cluster-sized instead of one sphere
/// stretched to the whole shape's own diagonal. Empty for an empty slice.
pub(super) fn covering_spheres(cells: &[GlobalCell], max_radius_cells: i64) -> Vec<SphereBrush> {
    let mut remaining: Vec<GlobalCell> = cells.to_vec();
    let mut spheres = Vec::new();
    let r2 = max_radius_cells * max_radius_cells;
    while let Some(seed) = remaining.first().copied() {
        let mut cluster = Vec::new();
        remaining.retain(|&c| {
            let d2 = (c.x - seed.x).pow(2) + (c.y - seed.y).pow(2) + (c.z - seed.z).pow(2);
            if d2 <= r2 {
                cluster.push(c);
                false
            } else {
                true
            }
        });
        if let Some(brush) = bounding_sphere_brush(&cluster) {
            spheres.push(brush);
        }
    }
    spheres
}

/// The smallest sphere, centred on `cells`' bounding box, that covers every
/// cell in `cells` (plus one cell of slack so a `Place` on close reliably
/// refills every voxel a matching `Cut` opened). Radius is the true farthest
/// marked cell from that centre, not the bounding box's own half-diagonal —
/// for a compact cluster (what [`covering_spheres`] always passes), that
/// keeps the brush close to the cluster's actual size instead of its
/// bounding box's corner-to-corner distance. `None` for an empty slice.
pub(super) fn bounding_sphere_brush(cells: &[GlobalCell]) -> Option<SphereBrush> {
    let first = *cells.first()?;
    let (mut lo, mut hi) = (first, first);
    for &c in &cells[1..] {
        lo = GlobalCell::new(lo.x.min(c.x), lo.y.min(c.y), lo.z.min(c.z));
        hi = GlobalCell::new(hi.x.max(c.x), hi.y.max(c.y), hi.z.max(c.z));
    }
    // Cell-centred bounding box: [lo, hi + 1) in whole cells.
    let centre = BrushPoint::from_units(
        (lo.x + hi.x + 1) * BRUSH_UNIT / 2,
        (lo.y + hi.y + 1) * BRUSH_UNIT / 2,
        (lo.z + hi.z + 1) * BRUSH_UNIT / 2,
    );
    let centre_cells = [
        (lo.x + hi.x + 1) as f64 / 2.0,
        (lo.y + hi.y + 1) as f64 / 2.0,
        (lo.z + hi.z + 1) as f64 / 2.0,
    ];
    let radius_cells = cells
        .iter()
        .map(|c| {
            let d = [
                c.x as f64 + 0.5 - centre_cells[0],
                c.y as f64 + 0.5 - centre_cells[1],
                c.z as f64 + 0.5 - centre_cells[2],
            ];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
        })
        .fold(0.0_f64, f64::max)
        + 1.0;
    let radius_units = (radius_cells * BRUSH_UNIT as f64).ceil() as i64;
    SphereBrush::new(centre, radius_units).ok()
}

/// Converts Rapier's body-0-to-body-1 contact normal to a target-to-striker
/// normal. `target_index` is the target's position in the contact pair.
pub(super) fn target_to_striker_normal(pair_normal: [f64; 3], target_index: usize) -> [f64; 3] {
    if target_index == 0 {
        pair_normal
    } else {
        pair_normal.map(|component| -component)
    }
}

/// Resolves the solid material just inside a contact surface. Solver points can
/// lie on cell boundaries, so step half a cell against the target-to-striker
/// normal and fail closed when the backing data is not resident.
pub(super) fn sample_contact_material(
    world: &SimWorld,
    volume_id: spall_core::VolumeId,
    point_cell: [f64; 3],
    normal: [f64; 3],
) -> Option<spall_core::MaterialId> {
    let volume = world.volume_ref(volume_id)?;
    for offset in [0.51, 1.01, 1.51] {
        let cell = GlobalCell::new(
            (point_cell[0] - normal[0] * offset).floor() as i64,
            (point_cell[1] - normal[1] * offset).floor() as i64,
            (point_cell[2] - normal[2] * offset).floor() as i64,
        );
        if let Ok(Sample::Filled(material)) = volume.sample(cell) {
            return Some(material);
        }
    }
    None
}

/// Nearest-surface gap, metres, between a world-space AABB `(min, max)` and a
/// sphere. Negative when the sphere overlaps the box. Used to decide whether a
/// terrain cut this tick is close enough to a settled body to wake it.
pub(super) fn box_sphere_gap(bbox: ([f64; 3], [f64; 3]), centre_m: [f64; 3], radius_m: f64) -> f64 {
    let (lo, hi) = bbox;
    let mut d2 = 0.0;
    for axis in 0..3 {
        let (a, b) = (lo[axis].min(hi[axis]), lo[axis].max(hi[axis]));
        let outside = (a - centre_m[axis]).max(centre_m[axis] - b).max(0.0);
        d2 += outside * outside;
    }
    d2.sqrt() - radius_m
}

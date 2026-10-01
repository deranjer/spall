//! Presentation-only water look. The server owns the water simulation; this
//! module only describes what the renderer needs to *draw* it: a per-column
//! surface-height field the opaque shader reads for underwater fog, tint and
//! caustics (the surface sheet itself is an ordinary [`crate::GpuVertex`] mesh).

use crate::vertex::GpuVertex;

/// Sentinel stored for a column with no water. Far below any world height, so
/// a plain `y < height` test is false without a separate validity flag.
pub const NO_WATER: f32 = -1.0e9;

/// Heights of the water surface on a regular horizontal grid.
///
/// Cell `(ix, iz)` covers `[origin + i * cell_m, origin + (i + 1) * cell_m)` on
/// each horizontal axis and holds the world height of that column's surface,
/// or [`NO_WATER`].
#[derive(Debug, Clone, PartialEq)]
pub struct WaterField {
    pub origin_xz: [f32; 2],
    pub cell_m: f32,
    pub dim: u32,
    /// `dim * dim` heights, X fastest.
    pub heights: Vec<f32>,
}

impl WaterField {
    /// An all-dry field of `dim * dim` cells.
    pub fn new(origin_xz: [f32; 2], cell_m: f32, dim: u32) -> Self {
        assert!(cell_m > 0.0 && dim > 0, "water field needs a positive size");
        Self {
            origin_xz,
            cell_m,
            dim,
            heights: vec![NO_WATER; dim as usize * dim as usize],
        }
    }

    fn index(&self, ix: i64, iz: i64) -> Option<usize> {
        let dim = i64::from(self.dim);
        ((0..dim).contains(&ix) && (0..dim).contains(&iz)).then(|| (ix + dim * iz) as usize)
    }

    /// Grid cell containing the world position `(x, z)`, if inside the field.
    pub fn cell_of(&self, x: f32, z: f32) -> Option<(i64, i64)> {
        let ix = ((x - self.origin_xz[0]) / self.cell_m).floor() as i64;
        let iz = ((z - self.origin_xz[1]) / self.cell_m).floor() as i64;
        self.index(ix, iz).map(|_| (ix, iz))
    }

    /// Raises cell `(ix, iz)` to `height` (the higher surface wins); cells
    /// outside the field are ignored.
    pub fn raise(&mut self, ix: i64, iz: i64, height: f32) {
        if let Some(i) = self.index(ix, iz) {
            self.heights[i] = self.heights[i].max(height);
        }
    }

    /// Surface height above world position `(x, z)`, if there is water there.
    pub fn surface_at(&self, x: f32, z: f32) -> Option<f32> {
        let (ix, iz) = self.cell_of(x, z)?;
        let h = self.heights[self.index(ix, iz)?];
        (h > NO_WATER * 0.5).then_some(h)
    }

    /// The surface height at `(x, z)` if `point` is below it (in the water).
    pub fn submerged_surface(&self, point: [f32; 3]) -> Option<f32> {
        self.surface_at(point[0], point[2])
            .filter(|&surface| point[1] < surface)
    }
}

impl WaterField {
    /// Marks every field cell whose centre lies under `column` with the
    /// column's top height (the higher surface wins). `origin_xz` and `cell_m`
    /// place the column grid in the world.
    pub fn raise_column(&mut self, column: &WaterColumn, origin_xz: [f32; 2], cell_m: f32) {
        let x0 = origin_xz[0] + column.ix as f32 * cell_m;
        let z0 = origin_xz[1] + column.iz as f32 * cell_m;
        let lo = |w: f32, o: f32| ((w - o) / self.cell_m - 0.5).ceil() as i64;
        let hi = |w: f32, o: f32| ((w - o) / self.cell_m - 0.5).ceil() as i64 - 1;
        let (ix0, ix1) = (
            lo(x0, self.origin_xz[0]),
            hi(x0 + cell_m, self.origin_xz[0]),
        );
        let (iz0, iz1) = (
            lo(z0, self.origin_xz[1]),
            hi(z0 + cell_m, self.origin_xz[1]),
        );
        // Anything beyond the field is skipped, however large the column.
        let dim = i64::from(self.dim);
        for iz in iz0.max(0)..=iz1.min(dim - 1) {
            for ix in ix0.max(0)..=ix1.min(dim - 1) {
                self.raise(ix, iz, column.top);
            }
        }
    }
}

/// One column of water on a regular horizontal grid: grid cell `(ix, iz)` and
/// the world heights of its lowest wet point and its surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaterColumn {
    pub ix: i32,
    pub iz: i32,
    pub bottom: f32,
    pub top: f32,
}

/// Heights within this many metres count as level (a cell merges into a flat
/// quad), well under a pixel at any viewing distance.
const LEVEL_EPSILON_M: f32 = 1.0e-3;

/// Builds the water surface sheet for one grid of columns.
///
/// Unlike drawing each column as a box, the top is a continuous heightfield:
/// every grid corner takes the mean surface of the wet columns around it, so
/// neighbouring columns of different heights meet without a step. Each cell is
/// then flat-shaded per triangle, which keeps the chunky voxel read while the
/// stepped "columns" disappear. Level cells merge into large quads. A vertical
/// wall is added only where a column has no wet neighbour (a waterfall edge or
/// the shore); it runs from the column's bottom to the smoothed edge heights.
///
/// World position of grid line `i` is `origin_xz + i * cell_m`. `columns` must
/// have at most one entry per `(ix, iz)`.
pub fn build_water_surface(
    columns: &[WaterColumn],
    origin_xz: [f32; 2],
    cell_m: f32,
    material: u32,
) -> (Vec<GpuVertex>, Vec<u32>) {
    use std::collections::HashMap;

    let cells: HashMap<(i32, i32), WaterColumn> =
        columns.iter().map(|c| ((c.ix, c.iz), *c)).collect();
    // Mean surface of the wet cells touching grid corner (cx, cz).
    let corner = |cx: i32, cz: i32| -> f32 {
        let (mut sum, mut n) = (0.0, 0.0);
        for (dx, dz) in [(-1, -1), (0, -1), (-1, 0), (0, 0)] {
            if let Some(c) = cells.get(&(cx + dx, cz + dz)) {
                sum += c.top;
                n += 1.0;
            }
        }
        sum / n
    };

    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let world = |cx: i32, y: f32, cz: i32| {
        [
            origin_xz[0] + cx as f32 * cell_m,
            y,
            origin_xz[1] + cz as f32 * cell_m,
        ]
    };
    let mut push_tri = |a: [f32; 3], b: [f32; 3], c: [f32; 3], toward: [f32; 3]| {
        let (ba, ca) = (sub(b, a), sub(c, a));
        let mut n = cross(ba, ca);
        let len = dot(n, n).sqrt();
        if len < 1.0e-9 {
            return;
        }
        n = [n[0] / len, n[1] / len, n[2] / len];
        if dot(n, toward) < 0.0 {
            n = [-n[0], -n[1], -n[2]];
        }
        let base = vertices.len() as u32;
        for p in [a, b, c] {
            vertices.push(GpuVertex {
                position: p,
                normal: n,
                local_uv: [p[0], p[2]],
                ao: 1.0,
                material,
            });
        }
        indices.extend([base, base + 1, base + 2]);
    };

    // Level cells, merged into rectangles; the rest become two triangles each.
    let mut level: Vec<(i32, i32, i32)> = Vec::new(); // (height key, ix, iz)
    let up = [0.0, 1.0, 0.0];
    for c in columns {
        let h00 = corner(c.ix, c.iz);
        let h10 = corner(c.ix + 1, c.iz);
        let h01 = corner(c.ix, c.iz + 1);
        let h11 = corner(c.ix + 1, c.iz + 1);
        let lo = h00.min(h10).min(h01).min(h11);
        let hi = h00.max(h10).max(h01).max(h11);
        if hi - lo <= LEVEL_EPSILON_M {
            level.push(((h00 / LEVEL_EPSILON_M).round() as i32, c.ix, c.iz));
        } else {
            let (p00, p10) = (world(c.ix, h00, c.iz), world(c.ix + 1, h10, c.iz));
            let (p01, p11) = (world(c.ix, h01, c.iz + 1), world(c.ix + 1, h11, c.iz + 1));
            // Split along the flatter diagonal.
            if (h00 - h11).abs() <= (h10 - h01).abs() {
                push_tri(p00, p10, p11, up);
                push_tri(p00, p11, p01, up);
            } else {
                push_tri(p00, p10, p01, up);
                push_tri(p10, p11, p01, up);
            }
        }

        // Walls only where there is no wet neighbour.
        let wall = |a: (i32, f32), b: (i32, f32), fixed: i32, along_x: bool, out: [f32; 3]| {
            let (pa, pb) = if along_x {
                (world(a.0, a.1, fixed), world(b.0, b.1, fixed))
            } else {
                (world(fixed, a.1, a.0), world(fixed, b.1, b.0))
            };
            (pa, pb, out)
        };
        let sides = [
            (
                -1,
                0,
                wall((c.iz, h00), (c.iz + 1, h01), c.ix, false, [-1.0, 0.0, 0.0]),
            ),
            (
                1,
                0,
                wall(
                    (c.iz, h10),
                    (c.iz + 1, h11),
                    c.ix + 1,
                    false,
                    [1.0, 0.0, 0.0],
                ),
            ),
            (
                0,
                -1,
                wall((c.ix, h00), (c.ix + 1, h10), c.iz, true, [0.0, 0.0, -1.0]),
            ),
            (
                0,
                1,
                wall(
                    (c.ix, h01),
                    (c.ix + 1, h11),
                    c.iz + 1,
                    true,
                    [0.0, 0.0, 1.0],
                ),
            ),
        ];
        for (dx, dz, (pa, pb, out)) in sides {
            if cells.contains_key(&(c.ix + dx, c.iz + dz)) || pa[1] - c.bottom < LEVEL_EPSILON_M {
                continue;
            }
            let (qa, qb) = ([pa[0], c.bottom, pa[2]], [pb[0], c.bottom, pb[2]]);
            push_tri(qa, qb, pb, out);
            push_tri(qa, pb, pa, out);
        }
    }

    // Greedy rectangles over level cells: runs along x, then equal runs along z.
    level.sort_unstable_by_key(|&(key, ix, iz)| (key, iz, ix));
    let mut runs: Vec<(i32, i32, i32, i32)> = Vec::new(); // (key, iz, ix0, len)
    for &(key, ix, iz) in &level {
        match runs.last_mut() {
            Some(r) if r.0 == key && r.1 == iz && r.2 + r.3 == ix => r.3 += 1,
            _ => runs.push((key, iz, ix, 1)),
        }
    }
    runs.sort_unstable_by_key(|&(key, iz, ix0, len)| (key, ix0, len, iz));
    let mut rects: Vec<(i32, i32, i32, i32, i32)> = Vec::new(); // (key, ix0, len, iz0, depth)
    for (key, iz, ix0, len) in runs {
        match rects.last_mut() {
            Some(r) if (r.0, r.1, r.2) == (key, ix0, len) && r.3 + r.4 == iz => r.4 += 1,
            _ => rects.push((key, ix0, len, iz, 1)),
        }
    }
    for (key, ix0, len, iz0, depth) in rects {
        let h = key as f32 * LEVEL_EPSILON_M;
        let (a, b) = (world(ix0, h, iz0), world(ix0 + len, h, iz0));
        let (c, d) = (world(ix0 + len, h, iz0 + depth), world(ix0, h, iz0 + depth));
        push_tri(a, b, c, up);
        push_tri(a, c, d, up);
    }
    (vertices, indices)
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_cells_and_cells_outside_the_field_have_no_surface() {
        let mut field = WaterField::new([10.0, 20.0], 0.25, 8);
        field.raise(2, 3, 5.0);
        assert_eq!(
            field.surface_at(10.0 + 2.1 * 0.25, 20.0 + 3.1 * 0.25),
            Some(5.0)
        );
        assert_eq!(field.surface_at(10.0 + 3.1 * 0.25, 20.0 + 3.1 * 0.25), None);
        assert_eq!(field.surface_at(9.9, 20.1), None);
        assert_eq!(field.surface_at(10.0 + 8.5 * 0.25, 20.1), None);
    }

    fn lake(w: i32, d: i32, top: impl Fn(i32, i32) -> f32) -> Vec<WaterColumn> {
        let mut out = Vec::new();
        for iz in 0..d {
            for ix in 0..w {
                out.push(WaterColumn {
                    ix,
                    iz,
                    bottom: 0.0,
                    top: top(ix, iz),
                });
            }
        }
        out
    }

    #[test]
    fn a_flat_lake_is_two_triangles_plus_its_rim() {
        let columns = lake(10, 10, |_, _| 2.0);
        let (vertices, indices) = build_water_surface(&columns, [0.0, 0.0], 0.25, 3);
        let top = vertices.iter().filter(|v| v.normal[1] > 0.5).count();
        assert_eq!(top, 6, "one merged quad for the whole level lake");
        // 4 sides x 10 cells x 2 triangles of rim wall.
        let walls = vertices.iter().filter(|v| v.normal[1].abs() < 0.5).count();
        assert_eq!(walls, 4 * 10 * 6);
        assert_eq!(indices.len(), vertices.len());
        assert!(vertices.iter().all(|v| v.material == 3));
    }

    #[test]
    fn neighbouring_columns_of_different_height_meet_without_a_step() {
        // A staircase: every column a different height.
        let columns = lake(6, 6, |ix, iz| 1.0 + 0.25 * (ix + iz) as f32);
        let (vertices, _) = build_water_surface(&columns, [0.0, 0.0], 0.5, 0);
        let mut at = std::collections::BTreeMap::new();
        for v in vertices.iter().filter(|v| v.normal[1] > 0.5) {
            let key = ((v.position[0] * 1e3) as i64, (v.position[2] * 1e3) as i64);
            let y = *at.entry(key).or_insert(v.position[1]);
            assert!((y - v.position[1]).abs() < 1e-4, "gap at {key:?}");
        }
    }

    #[test]
    fn walls_stand_only_where_a_column_has_no_wet_neighbour() {
        let columns = vec![
            WaterColumn {
                ix: 0,
                iz: 0,
                bottom: 0.0,
                top: 1.0,
            },
            WaterColumn {
                ix: 1,
                iz: 0,
                bottom: 0.0,
                top: 1.0,
            },
        ];
        let (vertices, _) = build_water_surface(&columns, [0.0, 0.0], 1.0, 0);
        // The shared edge (x = 1) must have no wall.
        let shared = vertices
            .iter()
            .filter(|v| v.normal[0].abs() > 0.5 && (v.position[0] - 1.0).abs() < 1e-6)
            .count();
        assert_eq!(shared, 0);
        let rim = vertices.iter().filter(|v| v.normal[1].abs() < 0.5).count();
        assert_eq!(rim, 6 * 6, "six rim walls of two triangles");
    }

    #[test]
    fn a_column_raises_every_field_cell_under_it() {
        let mut field = WaterField::new([0.0, 0.0], 0.25, 16);
        // One 1 m column at world (1..2, 1..2).
        field.raise_column(
            &WaterColumn {
                ix: 1,
                iz: 1,
                bottom: 0.0,
                top: 3.0,
            },
            [0.0, 0.0],
            1.0,
        );
        let wet = field
            .heights
            .iter()
            .filter(|&&h| h > NO_WATER * 0.5)
            .count();
        assert_eq!(wet, 16, "4x4 quarter-metre cells");
        assert_eq!(field.surface_at(1.1, 1.9), Some(3.0));
        assert_eq!(field.surface_at(0.9, 1.5), None);
        assert_eq!(field.surface_at(2.1, 1.5), None);
    }

    #[test]
    fn raise_keeps_the_higher_surface() {
        let mut field = WaterField::new([0.0, 0.0], 1.0, 2);
        field.raise(1, 1, 3.0);
        field.raise(1, 1, 2.0);
        field.raise(5, 5, 9.0); // outside: ignored, no panic
        assert_eq!(field.surface_at(1.5, 1.5), Some(3.0));
    }

    #[test]
    fn a_point_is_submerged_only_below_the_local_surface() {
        let mut field = WaterField::new([0.0, 0.0], 1.0, 2);
        field.raise(0, 0, 4.0);
        assert_eq!(field.submerged_surface([0.5, 3.9, 0.5]), Some(4.0));
        assert_eq!(field.submerged_surface([0.5, 4.1, 0.5]), None);
        assert_eq!(field.submerged_surface([1.5, 0.0, 0.5]), None);
    }
}

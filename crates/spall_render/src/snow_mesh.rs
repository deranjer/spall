//! Meshes the ground-snow layer as opaque voxel geometry resting on terrain.
//!
//! Each exposed +Y terrain face with snow is a 4x4 grid of 6.25 cm columns (the
//! near "fine" level) or one flat 0.25 m column (the far "coarse" level). A
//! column's height is the snow depth quantised to whole 6.25 cm steps, dithered
//! by a deterministic per-column hash so the surface settles in soft steps. The
//! mesher merges equal columns greedily and emits side faces wherever a column
//! stands higher than its neighbour; it never emits a cube per column. Footprints
//! carve a foot-shaped dent into the fine columns and leave a packed floor.
//!
//! The output is ordinary [`GpuVertex`] geometry drawn through the lit terrain
//! pipeline with the snow materials below, so it is shadowed and fogged like
//! terrain.

use crate::vertex::GpuVertex;

/// Snow column height quantum: one 6.25 cm step.
pub const SNOW_STEP_M: f32 = 0.0625;
/// Column heights are stored in quarter steps so a shallow print can sit half a
/// step down.
pub const SNOW_UNIT_M: f32 = SNOW_STEP_M / 4.0;
const UNITS_PER_STEP: u16 = 4;
const CELL_M: f32 = 0.25;
const TILE_CELLS: usize = 16;
const FINE_PER_CELL: usize = 4;
/// Shader material ids at or above this value are procedural snow (no table lookup).
pub const SNOW_MATERIAL_BASE: u32 = 0xFFFF_0000;
pub const SNOW_MATERIAL_PACKED: u32 = SNOW_MATERIAL_BASE + 1;
/// A footprint's raised rim is the width of one fine column.
const RIM_WIDTH_M: f32 = 0.07;

/// `(tile_x, tile_z, top_cell_y)`: one 4 m tile at one terrain layer.
pub type SnowTileKey = (i32, i32, i32);

/// An authoritative footprint, already bound to a valid snow face by the client.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnowFootprintMark {
    /// Foot centre in world metres.
    pub centre_xz: [f32; 2],
    /// Walking heading; zero faces -Z, positive turns toward +X.
    pub heading_rad: f32,
    /// 1 for a fresh print, falling toward 0 as new snow refills it.
    pub strength: f32,
}

/// Validated snow for one tile and layer.
#[derive(Debug, Clone, PartialEq)]
pub struct SnowTile {
    pub key: SnowTileKey,
    /// Row-major `local_z * 16 + local_x`; zero means no snow face.
    pub depth_q16: [u16; 256],
    /// Near tiles use 6.25 cm columns and footprints; far tiles one column per cell.
    pub fine: bool,
    pub marks: Vec<SnowFootprintMark>,
}

/// What lies in the neighbouring terrain cell column at a tile's layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnowBeyond {
    /// A solid cell stands beside the support plane: the snow side is hidden.
    Wall,
    /// Open air: the snow edge shows its full depth.
    Open,
    Snow {
        depth_q16: u16,
        fine: bool,
    },
}

pub trait SnowNeighbours {
    /// The cell column at global cell `(x, z)` seen from layer `layer_y`.
    fn beyond(&self, cell_x: i64, cell_z: i64, layer_y: i32) -> SnowBeyond;
}

#[derive(Debug, Default, Clone)]
pub struct SnowMesh {
    pub vertices: Vec<GpuVertex>,
    pub indices: Vec<u32>,
}

impl SnowMesh {
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn unit_hash(a: i64, b: i64, salt: u64) -> f32 {
    let h = mix((a as u64) ^ mix((b as u64) ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15)));
    (h >> 40) as f32 / (1u64 << 24) as f32
}

fn depth_m(depth_q16: u16) -> f32 {
    f32::from(depth_q16) / 65_536.0
}

fn first_step_units(steps: f32) -> u16 {
    2 + (2.0 * steps.clamp(0.0, 1.0)).round() as u16
}

/// Height in quarter-step units of the fine column at global fine index `(fx, fz)`.
///
/// `steps + u` with `u` uniform in `[0, 1)` has expectation `steps`, so the mean
/// surface height matches the authoritative depth, and a depth below one step
/// shows as a sparse dusting that fills in as snow arrives. `u` is a 6x6-column
/// block hash plus smaller 3x3 and per-column terms, so equal columns form
/// terraced lumps instead of salt-and-pepper noise.
pub fn column_units(depth_q16: u16, fx: i64, fz: i64) -> u16 {
    if depth_q16 == 0 {
        return 0;
    }
    let steps = depth_m(depth_q16) / SNOW_STEP_M;
    let broad = unit_hash(fx.div_euclid(6), fz.div_euclid(6), 1);
    let lump = unit_hash(fx.div_euclid(3), fz.div_euclid(3), 4);
    let own = unit_hash(fx, fz, 2);
    let u = (broad + 0.22 * lump + 0.1 * own).fract();
    let n = (steps + u).floor() as u16;
    match n {
        0 => 0,
        1 => first_step_units(steps),
        n => n * UNITS_PER_STEP,
    }
}

/// Height of a far (one column per cell) column at global cell `(cx, cz)`.
pub fn cell_units(depth_q16: u16, cx: i64, cz: i64) -> u16 {
    if depth_q16 == 0 {
        return 0;
    }
    let steps = depth_m(depth_q16) / SNOW_STEP_M;
    if steps < 1.0 {
        return if unit_hash(cx, cz, 3) < steps {
            first_step_units(steps)
        } else {
            0
        };
    }
    (depth_m(depth_q16) / SNOW_UNIT_M).round() as u16
}

struct Grid {
    n: usize,
    size: f32,
    x0: f32,
    z0: f32,
    y0: f32,
    h: Vec<u16>,
    packed: Vec<bool>,
}

impl Grid {
    fn at(&self, ix: usize, iz: usize) -> usize {
        iz * self.n + ix
    }
}

/// Foot sole: a tapered capsule from heel to ball, about 38 x 16 cm (a little over
/// life size, so it still reads on a 6.25 cm grid). Returns the
/// signed distance outside the sole edge (negative inside) in metres.
fn sole_distance(side: f32, forward: f32) -> f32 {
    const HEEL: f32 = -0.125;
    const TOE: f32 = 0.11;
    const HEEL_R: f32 = 0.058;
    const TOE_R: f32 = 0.08;
    let t = ((forward - HEEL) / (TOE - HEEL)).clamp(0.0, 1.0);
    let radius = HEEL_R + (TOE_R - HEEL_R) * t;
    let along = HEEL + (TOE - HEEL) * t;
    (side * side + (forward - along) * (forward - along)).sqrt() - radius
}

fn carve(grid: &mut Grid, marks: &[SnowFootprintMark]) {
    let reach = 0.2 + RIM_WIDTH_M;
    for mark in marks {
        let (sin, cos) = mark.heading_rad.sin_cos();
        let (forward, side) = ([sin, -cos], [cos, sin]);
        let lo = |origin: f32, centre: f32| {
            (((centre - reach - origin) / grid.size).floor().max(0.0)) as usize
        };
        let hi = |origin: f32, centre: f32| {
            (((centre + reach - origin) / grid.size).ceil().max(0.0) as usize).min(grid.n)
        };
        let strength = mark.strength.clamp(0.0, 1.0);
        for iz in lo(grid.z0, mark.centre_xz[1])..hi(grid.z0, mark.centre_xz[1]) {
            for ix in lo(grid.x0, mark.centre_xz[0])..hi(grid.x0, mark.centre_xz[0]) {
                let dx = grid.x0 + (ix as f32 + 0.5) * grid.size - mark.centre_xz[0];
                let dz = grid.z0 + (iz as f32 + 0.5) * grid.size - mark.centre_xz[1];
                let distance = sole_distance(
                    dx * side[0] + dz * side[1],
                    dx * forward[0] + dz * forward[1],
                );
                let index = grid.at(ix, iz);
                let height = grid.h[index];
                if height == 0 || distance > RIM_WIDTH_M {
                    continue;
                }
                if distance <= 0.0 {
                    // Packed floor: one step (half a step in thin snow).
                    let floor = if height >= 2 * UNITS_PER_STEP {
                        UNITS_PER_STEP
                    } else {
                        (height / 2).max(1)
                    };
                    let removed = (f32::from(height - floor.min(height)) * strength).round() as u16;
                    grid.h[index] = (height - removed).max(1);
                    grid.packed[index] |= strength > 0.25;
                } else if height >= 2 * UNITS_PER_STEP && strength > 0.25 {
                    // Displaced snow heaps up in a thin rim around the sole.
                    grid.h[index] = (height + UNITS_PER_STEP).min(8 * UNITS_PER_STEP + 4);
                }
            }
        }
    }
}

fn quad(
    mesh: &mut SnowMesh,
    corners: [[f32; 3]; 4],
    normal: [f32; 3],
    uv: [[f32; 2]; 4],
    material: u32,
) {
    let base = mesh.vertices.len() as u32;
    for (position, local_uv) in corners.into_iter().zip(uv) {
        mesh.vertices.push(GpuVertex {
            position,
            normal,
            local_uv,
            ao: 1.0,
            material,
        });
    }
    let e1 = sub(corners[1], corners[0]);
    let e2 = sub(corners[2], corners[0]);
    let facing = [
        e1[1] * e2[2] - e1[2] * e2[1],
        e1[2] * e2[0] - e1[0] * e2[2],
        e1[0] * e2[1] - e1[1] * e2[0],
    ];
    let dot = facing[0] * normal[0] + facing[1] * normal[1] + facing[2] * normal[2];
    let order: [u32; 6] = if dot >= 0.0 {
        [0, 1, 2, 0, 2, 3]
    } else {
        [0, 2, 1, 0, 3, 2]
    };
    mesh.indices.extend(order.map(|i| base + i));
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn emit_tops(grid: &Grid, grid_uv: f32, mesh: &mut SnowMesh) {
    let n = grid.n;
    let mut done = vec![false; n * n];
    for iz in 0..n {
        for ix in 0..n {
            let index = grid.at(ix, iz);
            if done[index] || grid.h[index] == 0 {
                continue;
            }
            let (height, packed) = (grid.h[index], grid.packed[index]);
            let same = |i: usize| !done[i] && grid.h[i] == height && grid.packed[i] == packed;
            let mut w = 1;
            while ix + w < n && same(grid.at(ix + w, iz)) {
                w += 1;
            }
            let mut d = 1;
            'rows: while iz + d < n {
                for k in 0..w {
                    if !same(grid.at(ix + k, iz + d)) {
                        break 'rows;
                    }
                }
                d += 1;
            }
            for dz in 0..d {
                for dx in 0..w {
                    done[grid.at(ix + dx, iz + dz)] = true;
                }
            }
            let y = grid.y0 + f32::from(height) * SNOW_UNIT_M;
            let (xa, xb) = (
                grid.x0 + ix as f32 * grid.size,
                grid.x0 + (ix + w) as f32 * grid.size,
            );
            let (za, zb) = (
                grid.z0 + iz as f32 * grid.size,
                grid.z0 + (iz + d) as f32 * grid.size,
            );
            quad(
                mesh,
                [[xa, y, za], [xa, y, zb], [xb, y, zb], [xb, y, za]],
                [0.0, 1.0, 0.0],
                [
                    [xa * grid_uv, za * grid_uv],
                    [xa * grid_uv, zb * grid_uv],
                    [xb * grid_uv, zb * grid_uv],
                    [xb * grid_uv, za * grid_uv],
                ],
                if packed {
                    SNOW_MATERIAL_PACKED
                } else {
                    SNOW_MATERIAL_BASE
                },
            );
        }
    }
}

/// Meshes one tile layer. `neighbours` answers what lies beside cells without
/// snow in this tile or beyond its border.
pub fn mesh_tile(tile: &SnowTile, neighbours: &dyn SnowNeighbours, mesh: &mut SnowMesh) {
    let (tile_x, tile_z, layer_y) = tile.key;
    let per_cell = if tile.fine { FINE_PER_CELL } else { 1 };
    let n = TILE_CELLS * per_cell;
    let cell0 = [
        i64::from(tile_x) * TILE_CELLS as i64,
        i64::from(tile_z) * TILE_CELLS as i64,
    ];
    let mut grid = Grid {
        n,
        size: CELL_M / per_cell as f32,
        x0: cell0[0] as f32 * CELL_M,
        z0: cell0[1] as f32 * CELL_M,
        y0: (layer_y as f32 + 1.0) * CELL_M,
        h: vec![0; n * n],
        packed: vec![false; n * n],
    };
    if tile.depth_q16.iter().all(|d| *d == 0) {
        return;
    }
    let column = |depth: u16, fine: bool, ix: i64, iz: i64| {
        if fine {
            column_units(depth, ix, iz)
        } else {
            cell_units(depth, ix, iz)
        }
    };
    for iz in 0..n {
        for ix in 0..n {
            let depth = tile.depth_q16[(iz / per_cell) * TILE_CELLS + ix / per_cell];
            let g = (
                cell0[0] * per_cell as i64 + ix as i64,
                cell0[1] * per_cell as i64 + iz as i64,
            );
            let index = grid.at(ix, iz);
            grid.h[index] = column(depth, tile.fine, g.0, g.1);
        }
    }
    if tile.fine {
        carve(&mut grid, &tile.marks);
    }
    emit_tops(&grid, if tile.fine { 1.0 / SNOW_STEP_M } else { 0.0 }, mesh);

    // Side faces toward every lower neighbour, merged along the edge.
    let mut beyond_cache: Vec<Option<SnowBeyond>> = vec![None; TILE_CELLS * TILE_CELLS * 4];
    let neighbour_height = |ix: i64,
                            iz: i64,
                            dir: usize,
                            cache: &mut Vec<Option<SnowBeyond>>|
     -> Option<(u16, bool)> {
        let (dx, dz) = [(1, 0), (-1, 0), (0, 1), (0, -1)][dir];
        let (nx, nz) = (ix + dx, iz + dz);
        let inside = nx >= 0 && nz >= 0 && nx < n as i64 && nz < n as i64;
        if inside {
            let cell = (nz as usize / per_cell) * TILE_CELLS + nx as usize / per_cell;
            if tile.depth_q16[cell] > 0 {
                let at = grid.at(nx as usize, nz as usize);
                return Some((grid.h[at], grid.packed[at]));
            }
        }
        let cell_index = ((iz as usize / per_cell) * TILE_CELLS + ix as usize / per_cell) * 4 + dir;
        let result = *cache[cell_index].get_or_insert_with(|| {
            let cx = (cell0[0] * per_cell as i64 + nx).div_euclid(per_cell as i64);
            let cz = (cell0[1] * per_cell as i64 + nz).div_euclid(per_cell as i64);
            neighbours.beyond(cx, cz, layer_y)
        });
        match result {
            SnowBeyond::Wall => None,
            SnowBeyond::Open => Some((0, false)),
            SnowBeyond::Snow { depth_q16, fine } => {
                let per = per_cell as i64;
                let (gx, gz) = (cell0[0] * per + nx, cell0[1] * per + nz);
                let (cx, cz) = (gx.div_euclid(per), gz.div_euclid(per));
                Some((
                    if fine {
                        // A far tile beside a near one compares against the cell's middle.
                        let (fx, fz) = if tile.fine {
                            (gx, gz)
                        } else {
                            (cx * 4 + 2, cz * 4 + 2)
                        };
                        column_units(depth_q16, fx, fz)
                    } else {
                        cell_units(depth_q16, cx, cz)
                    },
                    false,
                ))
            }
        }
    };
    for dir in 0..4 {
        let along_x = dir >= 2; // faces toward +/-z run along x
        for line in 0..n {
            let mut run: Option<(usize, usize, u16, u16, bool)> = None; // start, end, h, lower, lower packed
            for pos in 0..=n {
                let item = (pos < n)
                    .then(|| {
                        let (ix, iz) = if along_x { (pos, line) } else { (line, pos) };
                        let height = grid.h[grid.at(ix, iz)];
                        if height == 0 {
                            return None;
                        }
                        let (lower, packed) =
                            neighbour_height(ix as i64, iz as i64, dir, &mut beyond_cache)?;
                        (height > lower).then_some((height, lower, packed))
                    })
                    .flatten();
                match (&mut run, item) {
                    (Some((_, end, h, l, p)), Some((ih, il, ip)))
                        if *h == ih && *l == il && *p == ip =>
                    {
                        *end = pos + 1;
                    }
                    (current, next) => {
                        if let Some((start, end, h, l, p)) = current.take() {
                            emit_side(&grid, dir, line, (start, end), (h, l), p, mesh);
                        }
                        *current = next.map(|(h, l, p)| (pos, pos + 1, h, l, p));
                    }
                }
            }
        }
    }
}

/// One side quad along an edge: `span` is the run of columns, `heights` the high and
/// low column heights, and `packed` colours it as a dent wall.
fn emit_side(
    grid: &Grid,
    dir: usize,
    line: usize,
    span: (usize, usize),
    heights: (u16, u16),
    packed: bool,
    mesh: &mut SnowMesh,
) {
    let ((start, end), (height, lower)) = (span, heights);
    let ya = grid.y0 + f32::from(lower) * SNOW_UNIT_M;
    let yb = grid.y0 + f32::from(height) * SNOW_UNIT_M;
    let (s, e) = (start as f32 * grid.size, end as f32 * grid.size);
    let (corners, normal) = match dir {
        0 | 1 => {
            let x = grid.x0 + if dir == 0 { line + 1 } else { line } as f32 * grid.size;
            let (za, zb) = (grid.z0 + s, grid.z0 + e);
            (
                [[x, ya, za], [x, yb, za], [x, yb, zb], [x, ya, zb]],
                [if dir == 0 { 1.0 } else { -1.0 }, 0.0, 0.0],
            )
        }
        _ => {
            let z = grid.z0 + if dir == 2 { line + 1 } else { line } as f32 * grid.size;
            let (xa, xb) = (grid.x0 + s, grid.x0 + e);
            (
                [[xa, ya, z], [xa, yb, z], [xb, yb, z], [xb, ya, z]],
                [0.0, 0.0, if dir == 2 { 1.0 } else { -1.0 }],
            )
        }
    };
    let uv_scale = if grid.size < CELL_M {
        1.0 / SNOW_STEP_M
    } else {
        0.0
    };
    let uv = corners.map(|c| [(c[0] + c[2]) * uv_scale, c[1] * uv_scale]);
    let material = if packed {
        SNOW_MATERIAL_PACKED
    } else {
        SNOW_MATERIAL_BASE
    };
    quad(mesh, corners, normal, uv, material);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Open;
    impl SnowNeighbours for Open {
        fn beyond(&self, _: i64, _: i64, _: i32) -> SnowBeyond {
            SnowBeyond::Open
        }
    }

    fn tile(depth_m: f32, fine: bool, marks: Vec<SnowFootprintMark>) -> SnowTile {
        SnowTile {
            key: (0, 0, 0),
            depth_q16: [(depth_m * 65_536.0) as u16; 256],
            fine,
            marks,
        }
    }

    #[test]
    fn column_height_is_deterministic_quantised_and_tracks_depth() {
        for fx in -40..40 {
            assert_eq!(column_units(20_000, fx, 7), column_units(20_000, fx, 7));
        }
        // 0.3 m is 4.8 steps: every column is 4 or 5 whole steps.
        let mut sum = 0.0;
        let mut count = 0.0;
        for fx in 0..64 {
            for fz in 0..64 {
                let units = column_units((0.3 * 65_536.0) as u16, fx, fz);
                assert!(units == 16 || units == 20, "{units}");
                sum += f64::from(units) * f64::from(SNOW_UNIT_M);
                count += 1.0;
            }
        }
        assert!((sum / count - 0.3).abs() < 0.012, "mean {}", sum / count);
        assert_eq!(column_units(0, 3, 4), 0);
    }

    #[test]
    fn dusting_covers_fewer_columns_and_fills_in_with_depth() {
        let covered = |depth_m: f32| {
            (0..64)
                .flat_map(|x| (0..64).map(move |z| (x, z)))
                .filter(|(x, z)| column_units((depth_m * 65_536.0) as u16, *x, *z) > 0)
                .count()
        };
        let thin = covered(0.01);
        let more = covered(0.04);
        assert!(thin > 0 && thin < more && more < 64 * 64);
        assert_eq!(covered(0.0625), 64 * 64);
    }

    #[test]
    fn depth_above_one_step_stacks_to_more_than_one_step_and_reaches_the_cap() {
        let tall = (0..64)
            .map(|x| column_units(32_768, x, 0))
            .max()
            .expect("columns");
        assert!(
            tall >= 8 * UNITS_PER_STEP - 4,
            "half-metre snow is eight steps"
        );
        assert!(column_units(6_000, 1, 1) <= 2 * UNITS_PER_STEP);
    }

    #[test]
    fn footprint_dent_is_foot_shaped_oriented_by_heading_and_leaves_a_packed_floor() {
        let carved = |heading: f32| {
            let mut mesh = SnowMesh::default();
            let mark = SnowFootprintMark {
                centre_xz: [2.03, 2.03],
                heading_rad: heading,
                strength: 1.0,
            };
            let flat = tile(0.4, true, vec![mark]);
            mesh_tile(&flat, &Open, &mut mesh);
            let mut grid = Grid {
                n: 64,
                size: SNOW_STEP_M,
                x0: 0.0,
                z0: 0.0,
                y0: 0.25,
                h: vec![24; 64 * 64],
                packed: vec![false; 64 * 64],
            };
            carve(&mut grid, &flat.marks);
            grid
        };
        let facing_minus_z = carved(0.0);
        let dented: Vec<(usize, usize)> = (0..64)
            .flat_map(|z| (0..64).map(move |x| (x, z)))
            .filter(|(x, z)| facing_minus_z.packed[z * 64 + x])
            .collect();
        let (min_x, max_x) = dented
            .iter()
            .fold((64, 0), |(lo, hi), (x, _)| (lo.min(*x), hi.max(*x)));
        let (min_z, max_z) = dented
            .iter()
            .fold((64, 0), |(lo, hi), (_, z)| (lo.min(*z), hi.max(*z)));
        let (width, length) = (max_x - min_x + 1, max_z - min_z + 1);
        assert!(
            (1..=3).contains(&width) && (4..=6).contains(&length),
            "{width}x{length}"
        );
        // Deep snow is left as exactly one packed step on the floor.
        assert!(
            dented
                .iter()
                .all(|(x, z)| facing_minus_z.h[z * 64 + x] == UNITS_PER_STEP)
        );
        // Heading a quarter turn toward +X rotates the sole onto the other axis.
        let facing_x = carved(std::f32::consts::FRAC_PI_2);
        let rotated: Vec<(usize, usize)> = (0..64)
            .flat_map(|z| (0..64).map(move |x| (x, z)))
            .filter(|(x, z)| facing_x.packed[z * 64 + x])
            .collect();
        let span = |points: &[(usize, usize)], axis: usize| {
            let values: Vec<usize> = points
                .iter()
                .map(|p| if axis == 0 { p.0 } else { p.1 })
                .collect();
            values.iter().max().unwrap() - values.iter().min().unwrap() + 1
        };
        assert!(span(&rotated, 0) > span(&rotated, 1));
        // A raised rim stands one step above the surrounding snow.
        assert!(facing_minus_z.h.contains(&(24 + UNITS_PER_STEP)));
    }

    #[test]
    fn fine_tile_meshes_greedily_with_sides_and_far_tile_is_flat() {
        let mut fine = SnowMesh::default();
        mesh_tile(&tile(0.3, true, Vec::new()), &Open, &mut fine);
        let columns = 64 * 64;
        let top_quads = fine.vertices.iter().filter(|v| v.normal[1] == 1.0).count() / 4;
        assert!(
            top_quads > 0 && top_quads * 2 < columns,
            "tops merge: {top_quads}"
        );
        assert!(
            fine.vertices
                .iter()
                .any(|v| v.normal[0] != 0.0 || v.normal[2] != 0.0),
            "sides"
        );
        assert!(
            fine.indices
                .iter()
                .all(|i| (*i as usize) < fine.vertices.len())
        );
        let mut far = SnowMesh::default();
        mesh_tile(&tile(0.3, false, Vec::new()), &Open, &mut far);
        // One top quad plus four border sides for a uniform tile.
        assert_eq!(far.triangles(), 10);
    }

    #[test]
    fn walls_hide_sides() {
        struct Walls;
        impl SnowNeighbours for Walls {
            fn beyond(&self, _: i64, _: i64, _: i32) -> SnowBeyond {
                SnowBeyond::Wall
            }
        }
        let mut mesh = SnowMesh::default();
        mesh_tile(&tile(0.3, false, Vec::new()), &Walls, &mut mesh);
        assert_eq!(mesh.triangles(), 2);
    }
}

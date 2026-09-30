//! Underground voids.
//!
//! Two kinds of cave, both evaluated as a scalar field that is positive inside
//! a void and sampled on a 4-cell lattice per brick (then trilinearly
//! interpolated). Sampling corners first lets a brick with no positive corner
//! skip the per-cell pass entirely, and an all-solid brick then stays a cheap
//! uniform brick.
//!
//! * **Natural caves** — "cheese" chambers (thresholded 3D noise) and
//!   "spaghetti" tunnels (two noise fields near zero at once). Gated: only in
//!   a band of the rock column, never near the bedrock floor, and kept far
//!   below the swamp basin so a lake bed is never breached.
//! * **Mouths** — a handful of explicit sloped tunnels that start above the
//!   surface and descend into a chamber, so the showcase always has walkable
//!   cave entrances.

use crate::columns::ColumnMap;
use crate::noise::{fbm2, fbm3, hash01};
use crate::spec::SEA_LEVEL;

/// Lattice spacing of the sampled field, in cells.
pub const STEP: i64 = 4;
/// Corners per brick edge (`32 / STEP + 1`).
pub const CORNERS: usize = 9;
/// Lowest cell a natural cave or mouth may reach.
pub const CAVE_FLOOR: i64 = 40;
/// Cells of solid kept between a natural cave and the surface.
const ROOF: i64 = 6;
/// Extra roof under the swamp basin.
const BASIN_ROOF: i64 = 24;

const S_CHEESE: u64 = 0x71;
const S_TUBE_A: u64 = 0x72;
const S_TUBE_B: u64 = 0x73;
const S_REGION: u64 = 0x74;
const S_MOUTH: u64 = 0x75;

const CHEESE_THRESHOLD: f64 = 0.46;
const TUBE_WIDTH: f64 = 0.11;
const MOUTH_COUNT: usize = 8;
/// Unit-ish horizontal directions a mouth may descend along.
const D: f64 = std::f64::consts::FRAC_1_SQRT_2;
const COMPASS: [[f64; 2]; 8] = [
    [1.0, 0.0],
    [D, D],
    [0.0, 1.0],
    [-D, D],
    [-1.0, 0.0],
    [-D, -D],
    [0.0, -1.0],
    [D, -D],
];
/// Tunnel radius in cells.
pub const MOUTH_RADIUS: f64 = 7.0;
const CHAMBER_RADIUS: f64 = 16.0;

#[derive(Debug, Clone, Copy)]
struct Mouth {
    start: [f64; 3],
    end: [f64; 3],
}

pub struct CaveField {
    seed: u64,
    mouths: Vec<Mouth>,
}

/// Field values at the `9^3` lattice corners of one brick.
pub struct CornerGrid {
    natural: Vec<f32>,
    mouth: Vec<f32>,
    any_natural: bool,
    any_mouth: bool,
}

impl CornerGrid {
    /// Whether any cell of the brick could be carved.
    pub fn may_carve(&self) -> bool {
        self.any_natural || self.any_mouth
    }

    /// Trilinear `(natural, mouth)` at a brick-local cell centre.
    pub fn at(&self, lx: i64, ly: i64, lz: i64) -> (f64, f64) {
        let f = |local: i64| {
            let t = (local as f64 + 0.5) / STEP as f64;
            let i = (t.floor() as usize).min(CORNERS - 2);
            (i, t - i as f64)
        };
        let ((ix, fx), (iy, fy), (iz, fz)) = (f(lx), f(ly), f(lz));
        let interp = |v: &[f32]| {
            let c = |dx: usize, dy: usize, dz: usize| {
                f64::from(v[(ix + dx) + CORNERS * ((iy + dy) + CORNERS * (iz + dz))])
            };
            let l = |a: f64, b: f64, t: f64| a + (b - a) * t;
            let x00 = l(c(0, 0, 0), c(1, 0, 0), fx);
            let x10 = l(c(0, 1, 0), c(1, 1, 0), fx);
            let x01 = l(c(0, 0, 1), c(1, 0, 1), fx);
            let x11 = l(c(0, 1, 1), c(1, 1, 1), fx);
            l(l(x00, x10, fy), l(x01, x11, fy), fz)
        };
        (interp(&self.natural), interp(&self.mouth))
    }
}

fn dist_to_segment(p: [f64; 3], a: [f64; 3], b: [f64; 3]) -> f64 {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let ap = [p[0] - a[0], p[1] - a[1], p[2] - a[2]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
    let t = ((ap[0] * ab[0] + ap[1] * ab[1] + ap[2] * ab[2]) / len2).clamp(0.0, 1.0);
    let d = [ap[0] - ab[0] * t, ap[1] - ab[1] * t, ap[2] - ab[2] * t];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

impl CaveField {
    pub fn new(seed: u64, columns: &ColumnMap) -> Self {
        let size = f64::from(columns.size());
        let length = (size * 0.15).min(160.0);
        // Steep enough that the end chamber always sits well under the surface,
        // even in small arenas where the tunnel is short.
        let drop = (length * 0.45).max(CHAMBER_RADIUS + 10.0);
        let mut mouths = Vec::new();
        // Candidate sites on a hashed walk; keep those on dry land away from
        // the swamp basin and with room to descend and stay inside the arena.
        let mut attempt = 0u64;
        while mouths.len() < MOUTH_COUNT && attempt < 400 {
            let r = |k: u64| hash01(seed ^ S_MOUTH, attempt * 4 + k);
            let (u, v) = (0.12 + 0.76 * r(0), 0.06 + 0.56 * r(1));
            let (x, z) = (u * size, v * size);
            let dir = COMPASS[(r(2) * 8.0) as usize % 8];
            attempt += 1;
            let (ex, ez) = (x + dir[0] * length, z + dir[1] * length);
            let margin = CHAMBER_RADIUS + 6.0;
            if ex < margin || ez < margin || ex > size - margin || ez > size - margin {
                continue;
            }
            let (ix, iz) = (x as i64, z as i64);
            let (jx, jz) = (ex as i64, ez as i64);
            if columns.lowland(ix, iz) > 0 || columns.lowland(jx, jz) > 0 {
                continue;
            }
            let surface = f64::from(columns.height(ix, iz));
            let end_y = surface - drop;
            if end_y - CHAMBER_RADIUS < CAVE_FLOOR as f64 + 4.0 || surface < f64::from(SEA_LEVEL) {
                continue;
            }
            mouths.push(Mouth {
                start: [x, surface + 4.0, z],
                end: [ex, end_y, ez],
            });
        }
        Self { seed, mouths }
    }

    /// Whether the region noise allows natural caves in this column.
    fn region(&self, x: f64, z: f64) -> f64 {
        fbm2(self.seed ^ S_REGION, x / 300.0, z / 300.0, 2)
    }

    fn natural(&self, x: f64, y: f64, z: f64) -> f64 {
        let s = self.seed;
        let cheese = fbm3(s ^ S_CHEESE, x / 64.0, y / 48.0, z / 64.0, 2) - CHEESE_THRESHOLD;
        let a = fbm3(s ^ S_TUBE_A, x / 56.0, y / 40.0, z / 56.0, 2);
        let b = fbm3(s ^ S_TUBE_B, x / 56.0, y / 40.0, z / 56.0, 2);
        let tube = TUBE_WIDTH - a.abs().max(b.abs());
        let carve = cheese.max(tube);
        // Caves only in some regions, so most of the rock stays solid.
        carve - 2.0 * (-0.05 - self.region(x, z)).max(0.0)
    }

    fn mouth(&self, p: [f64; 3]) -> f64 {
        let mut best = f64::MIN;
        for m in &self.mouths {
            best = best.max(MOUTH_RADIUS - dist_to_segment(p, m.start, m.end));
            let d = [p[0] - m.end[0], p[1] - m.end[1], p[2] - m.end[2]];
            best = best.max(CHAMBER_RADIUS - (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt());
        }
        best
    }

    /// Samples the lattice for the brick whose minimum cell is `origin`.
    pub fn corners(&self, origin: [i64; 3]) -> CornerGrid {
        let n = CORNERS * CORNERS * CORNERS;
        let (mut natural, mut mouth) = (vec![0f32; n], vec![0f32; n]);
        let (mut any_natural, mut any_mouth) = (false, false);
        for k in 0..CORNERS {
            for j in 0..CORNERS {
                for i in 0..CORNERS {
                    let p = [
                        (origin[0] + STEP * i as i64) as f64,
                        (origin[1] + STEP * j as i64) as f64,
                        (origin[2] + STEP * k as i64) as f64,
                    ];
                    let idx = i + CORNERS * (j + CORNERS * k);
                    // Below the floor no natural cave is ever allowed.
                    let nat = if p[1] >= CAVE_FLOOR as f64 - STEP as f64 {
                        self.natural(p[0], p[1], p[2])
                    } else {
                        -1.0
                    };
                    let mth = if self.mouths.is_empty() {
                        -1.0
                    } else {
                        self.mouth(p)
                    };
                    natural[idx] = nat as f32;
                    mouth[idx] = mth as f32;
                    any_natural |= nat > 0.0;
                    any_mouth |= mth > 0.0;
                }
            }
        }
        CornerGrid {
            natural,
            mouth,
            any_natural,
            any_mouth,
        }
    }

    /// Whether a natural cave may open a solid cell at height `y` in a column
    /// whose surface is `h` and whose basin weight is `lowland`.
    pub fn natural_allowed(y: i64, h: i64, lowland: u8) -> bool {
        let roof = if lowland > 0 { BASIN_ROOF } else { ROOF };
        y >= CAVE_FLOOR && y <= h - roof
    }

    /// Whether a mouth may open a solid cell at height `y`.
    pub fn mouth_allowed(y: i64) -> bool {
        y >= CAVE_FLOOR
    }

    /// Horizontal distance from `(x, z)` to the nearest mouth tunnel, in cells.
    /// Within about [`MOUTH_RADIUS`] of a tunnel the surface may be opened.
    pub fn mouth_distance_xz(&self, x: f64, z: f64) -> f64 {
        let mut best = f64::MAX;
        for m in &self.mouths {
            let a = [m.start[0], 0.0, m.start[2]];
            let b = [m.end[0], 0.0, m.end[2]];
            best = best.min(dist_to_segment([x, 0.0, z], a, b));
        }
        best
    }

    pub fn mouth_count(&self) -> usize {
        self.mouths.len()
    }

    /// Surface positions `(x, z)` of every mouth, in cells.
    pub fn mouth_sites(&self) -> Vec<(i64, i64)> {
        self.mouths
            .iter()
            .map(|m| (m.start[0] as i64, m.start[2] as i64))
            .collect()
    }
}

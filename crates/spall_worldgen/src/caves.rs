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
//!   surface and descend into a chamber, so a world always has walkable cave
//!   entrances. The showcase and gallery keep the original round tube and
//!   sphere. In a natural world each mouth has its own shape: a small round
//!   tube, a wide low gallery, a tall arch, a round bore or a great cavern
//!   mouth, widening at the entrance, with walls that waver, bending once or
//!   twice on its way down, ending in one or two chambers of different sizes.
//!   None opens in or beside a river or lake.

use crate::columns::ColumnMap;
use crate::noise::{fbm2, fbm3, hash01};
use crate::spec::{BORDER_CELLS, SEA_LEVEL};

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
/// Tunnel radius in cells (the showcase's; a natural mouth varies around it).
pub const MOUTH_RADIUS: f64 = 7.0;
const CHAMBER_RADIUS: f64 = 16.0;
/// Cells a natural mouth stays from water at its opening and along its way.
const WATER_CLEAR_OPENING: u8 = 28;
const WATER_CLEAR_PATH: u8 = 14;
/// Most mouths of a natural world, and the arena edge a mouth is allotted.
const NATURAL_MOUTHS_MAX: usize = 24;
const NATURAL_MOUTH_CELLS: u32 = 256;

/// One stretch of a tunnel: its two ends and the half-width and half-height of
/// its cross-section at each.
#[derive(Debug, Clone, Copy)]
struct Reach {
    a: [f64; 3],
    b: [f64; 3],
    radii_a: [f64; 2],
    radii_b: [f64; 2],
}

/// An ellipsoidal chamber: centre, horizontal and vertical radii.
#[derive(Debug, Clone, Copy)]
struct Chamber {
    centre: [f64; 3],
    radii: [f64; 2],
}

#[derive(Debug, Clone)]
struct Mouth {
    /// Where it opens above the surface (the site) and where it ends.
    start: [f64; 3],
    end: [f64; 3],
    /// A natural mouth's tunnel and chambers; empty for the original shape, a
    /// round tube of [`MOUTH_RADIUS`] and a sphere of [`CHAMBER_RADIUS`].
    reaches: Vec<Reach>,
    chambers: Vec<Chamber>,
    /// Added to the entrance radius at the opening, fading out along the first
    /// quarter of the tunnel.
    flare: f64,
    /// Horizontal radius at the opening, for keeping spawns clear.
    opening_radius: f64,
    /// Box (min, max) outside which the mouth carves nothing.
    bounds: ([f64; 3], [f64; 3]),
}

impl Mouth {
    fn legacy(start: [f64; 3], end: [f64; 3]) -> Self {
        Self {
            start,
            end,
            reaches: Vec::new(),
            chambers: Vec::new(),
            flare: 0.0,
            opening_radius: MOUTH_RADIUS,
            bounds: ([f64::MIN; 3], [f64::MAX; 3]),
        }
    }

    fn is_legacy(&self) -> bool {
        self.reaches.is_empty()
    }
}

fn smooth01(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
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

/// The mouths of a natural world: a few more than a showcase's, each its own
/// size and shape (see the module docs), none in or near water.
fn natural_mouths(seed: u64, columns: &ColumnMap) -> Vec<Mouth> {
    let size = f64::from(columns.size());
    let want = ((columns.size() / NATURAL_MOUTH_CELLS) as usize).clamp(8, NATURAL_MOUTHS_MAX);
    let mut out: Vec<Mouth> = Vec::new();
    let mut attempt = 0u64;
    while out.len() < want && attempt < 4000 {
        let this = attempt;
        attempt += 1;
        let r = |k: u64| hash01(seed ^ S_MOUTH, this * 64 + k);
        let (x, z) = ((0.08 + 0.84 * r(0)) * size, (0.08 + 0.84 * r(1)) * size);
        let (ix, iz) = (x as i64, z as i64);
        if columns.lowland(ix, iz) > 0
            || columns.is_water(ix, iz)
            || columns.river_dist(ix, iz) < WATER_CLEAR_OPENING
        {
            continue;
        }
        let surface = f64::from(columns.height(ix, iz));
        if surface < f64::from(SEA_LEVEL) + 4.0 {
            continue;
        }
        // Cross-section: half-width, half-height.
        let pick = r(2);
        let (rh, rv) = if pick < 0.25 {
            // A small round tube.
            let a = 3.8 + 2.4 * r(3);
            (a, a * (0.9 + 0.2 * r(4)))
        } else if pick < 0.50 {
            // A wide, low gallery.
            (9.0 + 5.5 * r(3), 3.6 + 2.0 * r(4))
        } else if pick < 0.70 {
            // A tall arch.
            (5.5 + 3.5 * r(3), 8.0 + 4.0 * r(4))
        } else if pick < 0.90 {
            // A round bore.
            let a = 6.0 + 5.0 * r(3);
            (a, a * (0.85 + 0.3 * r(4)))
        } else {
            // A great cavern mouth.
            (12.0 + 5.0 * r(3), 8.0 + 5.0 * r(4))
        };
        let flare = 0.15 + 0.85 * r(5);
        let length = (size * 0.15).min(160.0) * (0.45 + 0.55 * r(6));
        let chamber_rh = 9.0 + 14.0 * r(7);
        let chamber_rv = chamber_rh * (0.45 + 0.5 * r(8));
        let max_drop = surface - (CAVE_FLOOR as f64 + 4.0 + chamber_rv);
        let min_drop = chamber_rv + rv * (1.0 + flare) + 6.0;
        if max_drop < min_drop {
            continue;
        }
        let drop = (length * (0.25 + 0.45 * r(9))).clamp(min_drop, max_drop);
        // The way down: one to three stretches, shallow first, bending.
        let segments = 1 + (r(10) * 3.0) as usize % 3;
        let weights: &[f64] = match segments {
            1 => &[1.0],
            2 => &[0.35, 0.65],
            _ => &[0.2, 0.35, 0.45],
        };
        let mut yaw = r(11) * std::f64::consts::TAU;
        let mut point = [x, surface + 0.4 * rv, z];
        let mut reaches = Vec::new();
        for (k, w) in weights.iter().enumerate() {
            if k > 0 {
                yaw += (r(12 + k as u64) - 0.5) * 1.8;
            }
            let run = length / segments as f64;
            let next = [
                point[0] + yaw.cos() * run,
                point[1] - drop * w,
                point[2] + yaw.sin() * run,
            ];
            let taper = |t: f64| 1.0 - 0.2 * t;
            let (t0, t1) = (k as f64 / segments as f64, (k + 1) as f64 / segments as f64);
            reaches.push(Reach {
                a: point,
                b: next,
                radii_a: [rh * taper(t0), rv * taper(t0)],
                radii_b: [rh * taper(t1), rv * taper(t1)],
            });
            point = next;
        }
        let end = point;
        let mut chambers = vec![Chamber {
            centre: end,
            radii: [chamber_rh, chamber_rv],
        }];
        if r(20) < 0.4 {
            // A second, smaller chamber off to one side.
            let side = yaw + std::f64::consts::FRAC_PI_2 * if r(21) < 0.5 { 1.0 } else { -1.0 };
            let off = chamber_rh * (0.9 + 0.5 * r(22));
            chambers.push(Chamber {
                centre: [
                    end[0] + side.cos() * off,
                    end[1] - 0.15 * chamber_rv,
                    end[2] + side.sin() * off,
                ],
                radii: [chamber_rh * 0.65, chamber_rv * 0.7],
            });
        }
        // Inside the arena with room for the widest part, on dry land all the
        // way, and clear of water along the path.
        let margin = chamber_rh * 1.5 + BORDER_CELLS as f64 + 6.0;
        let inside = |p: [f64; 3]| {
            p[0] >= margin && p[0] <= size - margin && p[2] >= margin && p[2] <= size - margin
        };
        let mut samples: Vec<[f64; 3]> = Vec::new();
        for reach in &reaches {
            let steps = (run_len(reach) / 8.0).ceil().max(1.0) as usize;
            for i in 0..=steps {
                let t = i as f64 / steps as f64;
                samples.push([
                    reach.a[0] + (reach.b[0] - reach.a[0]) * t,
                    reach.a[1],
                    reach.a[2] + (reach.b[2] - reach.a[2]) * t,
                ]);
            }
        }
        samples.extend(chambers.iter().map(|c| c.centre));
        let dry = samples.iter().all(|p| {
            let (cx, cz) = (p[0] as i64, p[2] as i64);
            inside(*p)
                && columns.lowland(cx, cz) == 0
                && !columns.is_water(cx, cz)
                && columns.river_dist(cx, cz) >= WATER_CLEAR_PATH
        });
        if !dry {
            continue;
        }
        // Apart from the mouths already placed.
        if out
            .iter()
            .any(|m| (m.start[0] - x).hypot(m.start[2] - z) < size * 0.05)
        {
            continue;
        }
        let pad_h = (rh * (1.0 + flare)).max(chamber_rh) * 1.4 + 4.0;
        let pad_v = (rv * (1.0 + flare)).max(chamber_rv) * 1.4 + 4.0;
        let mut lo = [f64::MAX; 3];
        let mut hi = [f64::MIN; 3];
        for p in samples
            .iter()
            .chain(reaches.iter().map(|r| &r.a))
            .chain(std::iter::once(&end))
        {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        for c in &chambers {
            for k in 0..3 {
                lo[k] = lo[k].min(c.centre[k]);
                hi[k] = hi[k].max(c.centre[k]);
            }
        }
        lo = [lo[0] - pad_h, lo[1] - pad_v, lo[2] - pad_h];
        hi = [hi[0] + pad_h, hi[1] + pad_v, hi[2] + pad_h];
        out.push(Mouth {
            start: reaches[0].a,
            end,
            reaches,
            chambers,
            flare,
            opening_radius: rh * (1.0 + flare),
            bounds: (lo, hi),
        });
    }
    out
}

fn run_len(r: &Reach) -> f64 {
    ((r.b[0] - r.a[0]).powi(2) + (r.b[2] - r.a[2]).powi(2)).sqrt()
}

impl CaveField {
    pub fn new(seed: u64, columns: &ColumnMap) -> Self {
        let size = f64::from(columns.size());
        let length = (size * 0.15).min(160.0);
        // Steep enough that the end chamber always sits well under the surface,
        // even in small arenas where the tunnel is short.
        let drop = (length * 0.45).max(CHAMBER_RADIUS + 10.0);
        let mut mouths = Vec::new();
        if columns.is_gallery() {
            // Flat pads for looking at plants: no cave mouths on them.
            return Self { seed, mouths };
        }
        if columns.is_natural() {
            return Self {
                seed,
                mouths: natural_mouths(seed, columns),
            };
        }
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
            let margin = CHAMBER_RADIUS + 6.0 + BORDER_CELLS as f64;
            let out = |a: f64| a < margin || a > size - margin;
            if out(ex) || out(ez) || out(x) || out(z) {
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
            mouths.push(Mouth::legacy([x, surface + 4.0, z], [ex, end_y, ez]));
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
            if m.is_legacy() {
                best = best.max(MOUTH_RADIUS - dist_to_segment(p, m.start, m.end));
                let d = [p[0] - m.end[0], p[1] - m.end[1], p[2] - m.end[2]];
                best = best.max(CHAMBER_RADIUS - (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt());
                continue;
            }
            let (lo, hi) = m.bounds;
            if (0..3).any(|k| p[k] < lo[k] || p[k] > hi[k]) {
                continue;
            }
            best = best.max(self.shaped(m, p));
        }
        best
    }

    /// The field of a natural mouth: positive inside its tunnel and chambers.
    /// Each is measured by how far into its (elliptical) cross-section the point
    /// lies, scaled by the smaller radius so that values stay comparable, and
    /// the radii waver with a slow 3D noise so the walls are not machined.
    fn shaped(&self, m: &Mouth, p: [f64; 3]) -> f64 {
        let waver = 1.0
            + 0.22
                * fbm3(
                    self.seed ^ S_MOUTH ^ 0x5eed,
                    p[0] / 11.0,
                    p[1] / 9.0,
                    p[2] / 11.0,
                    2,
                );
        let total: f64 = m
            .reaches
            .iter()
            .map(|r| {
                ((r.b[0] - r.a[0]).powi(2) + (r.b[1] - r.a[1]).powi(2) + (r.b[2] - r.a[2]).powi(2))
                    .sqrt()
            })
            .sum();
        let mut best = f64::MIN;
        let mut along = 0.0;
        for r in &m.reaches {
            let ab = [r.b[0] - r.a[0], r.b[1] - r.a[1], r.b[2] - r.a[2]];
            let len2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
            let ap = [p[0] - r.a[0], p[1] - r.a[1], p[2] - r.a[2]];
            let t = ((ap[0] * ab[0] + ap[1] * ab[1] + ap[2] * ab[2]) / len2).clamp(0.0, 1.0);
            let d = [ap[0] - ab[0] * t, ap[1] - ab[1] * t, ap[2] - ab[2] * t];
            let reach_len = len2.sqrt();
            let s_total = if total > 0.0 {
                (along + t * reach_len) / total
            } else {
                0.0
            };
            let widen = 1.0 + m.flare * (1.0 - smooth01(s_total / 0.25));
            let rh = (r.radii_a[0] + (r.radii_b[0] - r.radii_a[0]) * t) * widen * waver;
            let rv = (r.radii_a[1] + (r.radii_b[1] - r.radii_a[1]) * t) * widen * waver;
            let e = ((d[0] * d[0] + d[2] * d[2]) / (rh * rh) + d[1] * d[1] / (rv * rv)).sqrt();
            best = best.max((1.0 - e) * rh.min(rv));
            along += reach_len;
        }
        for c in &m.chambers {
            let d = [p[0] - c.centre[0], p[1] - c.centre[1], p[2] - c.centre[2]];
            let (rh, rv) = (c.radii[0] * waver, c.radii[1] * waver);
            let e = ((d[0] * d[0] + d[2] * d[2]) / (rh * rh) + d[1] * d[1] / (rv * rv)).sqrt();
            best = best.max((1.0 - e) * rh.min(rv));
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
            if m.is_legacy() {
                let a = [m.start[0], 0.0, m.start[2]];
                let b = [m.end[0], 0.0, m.end[2]];
                best = best.min(dist_to_segment([x, 0.0, z], a, b));
                continue;
            }
            // A wider opening counts as nearer, so the callers' clearance
            // (measured from a round tube of MOUTH_RADIUS) holds.
            let extra = (m.opening_radius - MOUTH_RADIUS).max(0.0);
            for r in &m.reaches {
                let a = [r.a[0], 0.0, r.a[2]];
                let b = [r.b[0], 0.0, r.b[2]];
                best = best.min(dist_to_segment([x, 0.0, z], a, b) - extra);
            }
        }
        best
    }

    /// Each mouth's entrance half-width and half-height in cells, after the
    /// widening at the opening.
    pub fn mouth_entrances(&self) -> Vec<[f64; 2]> {
        self.mouths
            .iter()
            .map(|m| match m.reaches.first() {
                Some(r) => [
                    r.radii_a[0] * (1.0 + m.flare),
                    r.radii_a[1] * (1.0 + m.flare),
                ],
                None => [MOUTH_RADIUS, MOUTH_RADIUS],
            })
            .collect()
    }

    /// Each mouth's path from the opening to its last chamber, as `(x, z)` in
    /// cells.
    pub fn mouth_paths(&self) -> Vec<Vec<(f64, f64)>> {
        self.mouths
            .iter()
            .map(|m| {
                if m.is_legacy() {
                    return vec![(m.start[0], m.start[2]), (m.end[0], m.end[2])];
                }
                let mut path: Vec<(f64, f64)> =
                    m.reaches.iter().map(|r| (r.a[0], r.a[2])).collect();
                path.push((m.end[0], m.end[2]));
                path.extend(m.chambers.iter().map(|c| (c.centre[0], c.centre[2])));
                path
            })
            .collect()
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

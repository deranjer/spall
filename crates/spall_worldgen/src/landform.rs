//! Macro terrain on a coarse grid (one sample per metre): seed-placed mountain
//! ranges, a coastline or a landlocked map, hills, and terminal basins.
//!
//! This is the raw land before erosion. Heights are in world cells. Everything
//! is a pure function of the seed, using only the platform-exact noise
//! primitives and `+ - * /`, `floor`, `sqrt`.

use crate::noise::{fbm2, hash01, ridged2, smoothstep};
use crate::spec::SEA_LEVEL;

// Noise streams. Changing one changes the world: bump GEN_VERSION.
const S_COAST: u64 = 0x4c01;
const S_EDGE: u64 = 0x4c02;
const S_SHORE: u64 = 0x4c03;
const S_RANGE: u64 = 0x4c10;
const S_WIGGLE: u64 = 0x4c11;
const S_RIDGE: u64 = 0x4c12;
const S_HILLS: u64 = 0x4c20;
const S_HILLS_FINE: u64 = 0x4c21;
const S_BASIN: u64 = 0x4c30;
const S_JITTER: u64 = 0x4c50;
const S_WARP_U: u64 = 0x4c40;
const S_WARP_V: u64 = 0x4c41;

/// Flat inland land sits this far above the sea, in cells.
const LAND_BASE: f64 = 45.0;
/// How far below the sea the open ocean floor lies at the arena edge.
const OCEAN_FLOOR: f64 = 45.0;
/// Raised rim along edges that are not sea, in cells, so nothing drains out.
const RIM_HEIGHT: f64 = 28.0;
const RIM_WIDTH: f64 = 0.10;
/// Normalized distance from a sea edge where the shore sits.
const SHORE_WIDTH: f64 = 0.16;
/// Regional tilt toward the sea (or a terminal basin) across the whole map, in
/// cells: broad bowls cannot form when everything leans downhill.
const TILT: f64 = 60.0;
/// Terrain never rises above this (the arena wall is higher).
pub const MAX_TERRAIN: f64 = 440.0;

/// North (`z = 0`), south, west (`x = 0`), east.
pub const EDGES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Basin {
    /// Grid position of the terminal sink.
    pub x: usize,
    pub z: usize,
    /// Surface of the terminal lake, in cells.
    pub level: f64,
}

pub struct Landform {
    pub n: usize,
    /// Heights in cells.
    pub height: Vec<f64>,
    /// Rock uplift per erosion iteration, in cells.
    pub uplift: Vec<f64>,
    /// Which arena edges open onto the sea (N, S, W, E).
    pub sea_edges: [bool; EDGES],
    /// Terminal basins; empty on maps that reach the sea.
    pub basins: Vec<Basin>,
}

impl Landform {
    pub fn is_coastal(&self) -> bool {
        self.sea_edges.iter().any(|&e| e)
    }
}

fn norm(x: f64, z: f64) -> (f64, f64) {
    let len = (x * x + z * z).sqrt().max(1e-9);
    (x / len, z / len)
}

/// Distance from `(px, pz)` to the segment `a..b`.
fn segment_distance(px: f64, pz: f64, a: (f64, f64), b: (f64, f64)) -> f64 {
    let (vx, vz) = (b.0 - a.0, b.1 - a.1);
    let len2 = (vx * vx + vz * vz).max(1e-12);
    let t = (((px - a.0) * vx + (pz - a.1) * vz) / len2).clamp(0.0, 1.0);
    let (cx, cz) = (a.0 + vx * t - px, a.1 + vz * t - pz);
    (cx * cx + cz * cz).sqrt()
}

struct Range {
    spine: Vec<(f64, f64)>,
    width: f64,
    peak: f64,
}

fn place_ranges(seed: u64) -> Vec<Range> {
    let roll = hash01(seed, S_RANGE);
    let count = 1 + usize::from(roll > 0.4) + usize::from(roll > 0.8);
    (0..count)
        .map(|r| {
            let k = (r as u64 + 1) * 16;
            let start = (
                0.2 + 0.6 * hash01(seed ^ S_RANGE, k),
                0.2 + 0.6 * hash01(seed ^ S_RANGE, k + 1),
            );
            let dir = norm(
                hash01(seed ^ S_RANGE, k + 2) - 0.5,
                hash01(seed ^ S_RANGE, k + 3) - 0.5,
            );
            let length = 0.45 + 0.3 * hash01(seed ^ S_RANGE, k + 4);
            let perp = (-dir.1, dir.0);
            const POINTS: usize = 6;
            let spine = (0..POINTS)
                .map(|p| {
                    let t = p as f64 / (POINTS - 1) as f64 - 0.5;
                    let wiggle = fbm2(seed ^ S_WIGGLE, t * 2.5 + 40.0 * r as f64, 0.5, 2) * 0.09;
                    (
                        (start.0 + dir.0 * length * t + perp.0 * wiggle).clamp(0.08, 0.92),
                        (start.1 + dir.1 * length * t + perp.1 * wiggle).clamp(0.08, 0.92),
                    )
                })
                .collect();
            Range {
                spine,
                width: 0.07 + 0.05 * hash01(seed ^ S_RANGE, k + 5),
                peak: 150.0 + 80.0 * hash01(seed ^ S_RANGE, k + 6),
            }
        })
        .collect()
}

/// Which edges are sea. About a third of seeds are landlocked.
fn roll_coast(seed: u64) -> [bool; EDGES] {
    let roll = hash01(seed, S_COAST);
    let first = (hash01(seed, S_EDGE) * EDGES as f64) as usize % EDGES;
    let mut edges = [false; EDGES];
    if roll < 0.35 {
        return edges;
    }
    edges[first] = true;
    if roll >= 0.8 {
        // A corner: one adjacent edge as well, never the opposite one.
        let side = usize::from(hash01(seed, S_EDGE + 1) > 0.5);
        let adjacent = if first < 2 { 2 + side } else { side };
        edges[adjacent] = true;
    }
    edges
}

pub fn generate(seed: u64, n: usize) -> Landform {
    let sea = f64::from(SEA_LEVEL);
    let sea_edges = roll_coast(seed);
    let ranges = place_ranges(seed);

    // Terminal basins on landlocked maps.
    let mut basins = Vec::new();
    if !sea_edges.iter().any(|&e| e) {
        let count = 1 + usize::from(hash01(seed, S_BASIN) > 0.5);
        for b in 0..count {
            let k = (b as u64 + 1) * 8;
            let (u, v) = (
                0.3 + 0.4 * hash01(seed ^ S_BASIN, k),
                0.3 + 0.4 * hash01(seed ^ S_BASIN, k + 1),
            );
            basins.push(Basin {
                x: ((u * n as f64) as usize).clamp(1, n - 2),
                z: ((v * n as f64) as usize).clamp(1, n - 2),
                level: sea + LAND_BASE - 20.0,
            });
        }
    }

    let mut height = vec![0.0; n * n];
    let mut uplift = vec![0.0; n * n];
    let inv = 1.0 / n as f64;
    for j in 0..n {
        for i in 0..n {
            let (u0, v0) = ((i as f64 + 0.5) * inv, (j as f64 + 0.5) * inv);
            // Warp the layout so ranges and shores are not ruler-straight.
            let u = u0 + fbm2(seed ^ S_WARP_U, u0 * 3.0, v0 * 3.0, 3) * 0.05;
            let v = v0 + fbm2(seed ^ S_WARP_V, u0 * 3.0, v0 * 3.0, 3) * 0.05;

            let edge_distance = [v0, 1.0 - v0, u0, 1.0 - u0];
            let shore_noise = fbm2(seed ^ S_SHORE, u0 * 4.0, v0 * 4.0, 3) * 0.06;
            // Continentalness: 1 well inland, 0 at open sea.
            let mut land = 1.0f64;
            let mut rim = 0.0f64;
            for (e, &d) in edge_distance.iter().enumerate() {
                if sea_edges[e] {
                    land = land.min(smoothstep(0.0, SHORE_WIDTH, d + shore_noise));
                } else {
                    rim = rim.max(1.0 - smoothstep(0.0, RIM_WIDTH, d));
                }
            }
            // Distance (normalized) to whatever water the land drains toward.
            let mut to_water = f64::MAX;
            for (e, &d) in edge_distance.iter().enumerate() {
                if sea_edges[e] {
                    to_water = to_water.min(d);
                }
            }
            for basin in &basins {
                let (bx, bz) = (basin.x as f64 * inv, basin.z as f64 * inv);
                to_water = to_water.min(((u0 - bx) * (u0 - bx) + (v0 - bz) * (v0 - bz)).sqrt());
            }
            let base =
                sea - OCEAN_FLOOR + land * (OCEAN_FLOOR + LAND_BASE) + TILT * to_water.min(1.0);

            let hills = fbm2(seed ^ S_HILLS, u * 6.0, v * 6.0, 4) * 12.0
                + fbm2(seed ^ S_HILLS_FINE, u * 22.0, v * 22.0, 3) * 5.0;

            let mut mountains = 0.0f64;
            let mut mask = 0.0f64;
            for range in &ranges {
                let d = range
                    .spine
                    .windows(2)
                    .map(|w| segment_distance(u, v, w[0], w[1]))
                    .fold(f64::MAX, f64::min);
                let profile = 1.0 - smoothstep(0.0, range.width, d);
                let ridge = ridged2(seed ^ S_RIDGE, u * 18.0, v * 18.0, 4);
                mountains = mountains.max(range.peak * profile * profile * (0.5 + 0.5 * ridge));
                mask = mask.max(profile);
            }

            let mut dip = 0.0f64;
            for basin in &basins {
                let (bx, bz) = (basin.x as f64 * inv, basin.z as f64 * inv);
                let d = ((u0 - bx) * (u0 - bx) + (v0 - bz) * (v0 - bz)).sqrt();
                dip = dip.max((1.0 - smoothstep(0.0, 0.11, d)) * 35.0);
            }

            // Inland features fade out toward the sea.
            let inland = smoothstep(0.35, 0.8, land);
            // Sub-metre texture: flats drain along wandering lines, not ruler-straight ones.
            let jitter = fbm2(seed ^ S_JITTER, i as f64 * 0.17, j as f64 * 0.17, 2) * 0.6;
            let mut h = base + inland * (hills + mountains) - dip + rim * RIM_HEIGHT + jitter;
            h = h.clamp(2.0, MAX_TERRAIN);
            let idx = i + n * j;
            height[idx] = h;
            uplift[idx] = inland * (0.05 + 1.2 * mask);
        }
    }
    // The terminal sink is a fixed low point: sit it at the bottom of its dip.
    for basin in &basins {
        height[basin.x + n * basin.z] = sea + LAND_BASE - 35.0;
    }
    Landform {
        n,
        height,
        uplift,
        sea_edges,
        basins,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landform_is_repeatable_and_seed_sensitive() {
        let a = generate(1, 64);
        let b = generate(1, 64);
        assert_eq!(a.height, b.height);
        assert_eq!(a.sea_edges, b.sea_edges);
        assert_ne!(a.height, generate(2, 64).height);
    }

    #[test]
    fn heights_stay_inside_the_world() {
        for seed in 1..=8 {
            let l = generate(seed, 64);
            assert!(
                l.height.iter().all(|h| (2.0..=MAX_TERRAIN).contains(h)),
                "seed {seed}"
            );
        }
    }

    #[test]
    fn seeds_roll_both_coastal_and_landlocked_maps() {
        let landlocked = (1..=60).filter(|&s| roll_coast(s) == [false; 4]).count();
        assert!(
            landlocked > 8 && landlocked < 40,
            "landlocked {landlocked}/60"
        );
        // A corner coast is two adjacent edges, never opposite ones.
        for s in 1..=200 {
            let e = roll_coast(s);
            assert!((!e[0] || !e[1]) && (!e[2] || !e[3]), "seed {s}: {e:?}");
        }
        assert!((1..=200).any(|s| roll_coast(s).iter().filter(|&&e| e).count() == 2));
    }

    #[test]
    fn landlocked_maps_get_basins_and_coasts_get_ocean() {
        for seed in 1..=30 {
            let l = generate(seed, 64);
            if l.is_coastal() {
                assert!(l.basins.is_empty());
                assert!(l.height.iter().any(|&h| h < f64::from(SEA_LEVEL)));
            } else {
                assert!(!l.basins.is_empty());
                for b in &l.basins {
                    assert!(l.height[b.x + 64 * b.z] >= f64::from(SEA_LEVEL));
                }
            }
        }
    }
}

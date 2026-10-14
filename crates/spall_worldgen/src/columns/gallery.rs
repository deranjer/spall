//! The gallery preset: one pad for every biome, side by side, each with its own
//! ground, wetness and (for the wet ones) water, so the whole of the flora can
//! be looked at in one walk. Not a world: a showcase.
//!
//! Thirteen pads in a grid of four by four (the last row holds only the sea). Each pad is a flat or gently shaped
//! patch of one biome; the gaps between pads take the biome of the nearest pad
//! and step between their heights. Everything is a pure function of the size
//! and seed.

use super::natural::biome_climate;
use super::{Biome, ColumnMap};
use crate::noise::{fbm2, smoothstep};
use crate::spec::{BORDER_CELLS, GenError, SEA_LEVEL, WorldGenSpec};

const S_LAND: u64 = 0x6a01;
const S_ROUGH: u64 = 0x6a02;
const S_POOLS: u64 = 0x6a03;
const S_DUNES: u64 = 0x6a04;

const COLS: i64 = 4;
/// Cells between neighbouring pads.
const GAP: i64 = 24;
/// Ground height of a dry pad.
const LAND: i32 = SEA_LEVEL + 24;

/// Which biome sits where: rows from north to south, columns west to east.
const LAYOUT: [[Option<Biome>; 4]; 4] = [
    [
        Some(Biome::Meadow),
        Some(Biome::TemperateForest),
        Some(Biome::BorealForest),
        Some(Biome::PineForest),
    ],
    [
        Some(Biome::Steppe),
        Some(Biome::Alpine),
        Some(Biome::Scree),
        Some(Biome::Snow),
    ],
    [
        Some(Biome::Desert),
        Some(Biome::Swamp),
        Some(Biome::Riparian),
        Some(Biome::Beach),
    ],
    [None, None, None, Some(Biome::Ocean)],
];

/// One pad: its biome and its cells, `[x0, x1) x [z0, z1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GalleryPatch {
    pub biome: Biome,
    pub x0: i64,
    pub z0: i64,
    pub x1: i64,
    pub z1: i64,
}

impl GalleryPatch {
    pub fn centre(&self) -> (i64, i64) {
        ((self.x0 + self.x1) / 2, (self.z0 + self.z1) / 2)
    }
}

/// The pads of a gallery of `size` cells, in layout order (rows north to south).
pub fn patches(size: u32) -> Vec<GalleryPatch> {
    let margin = BORDER_CELLS + 8;
    let pitch = (i64::from(size) - 2 * margin) / COLS;
    let mut out = Vec::new();
    for (row, biomes) in LAYOUT.iter().enumerate() {
        for (col, biome) in biomes.iter().enumerate() {
            let Some(biome) = *biome else {
                continue;
            };
            let (x0, z0) = (
                margin + col as i64 * pitch + GAP / 2,
                margin + row as i64 * pitch + GAP / 2,
            );
            out.push(GalleryPatch {
                biome,
                x0,
                z0,
                x1: x0 + pitch - GAP,
                z1: z0 + pitch - GAP,
            });
        }
    }
    debug_assert_eq!(out.len(), 13);
    out
}

/// What a column of a pad looks like: ground height, water surface (0 dry),
/// distance to water in cells, and whether it is swamp lowland.
struct Shape {
    height: f64,
    water: i32,
    river: f64,
    lowland: u8,
}

fn bump(r: f64) -> f64 {
    1.0 - smoothstep(0.0, 1.0, r)
}

/// The shape of column `(x, z)` on a pad; `u` and `v` are its place on the
/// pad, `0..=1` from the west and north edges.
fn shape(biome: Biome, seed: u64, x: f64, z: f64, u: f64, v: f64, width: f64) -> Shape {
    let land = f64::from(LAND);
    let sea = f64::from(SEA_LEVEL);
    let base = land + 0.9 * fbm2(seed ^ S_LAND, x / 14.0, z / 14.0, 2);
    let r = (((u - 0.5) * (u - 0.5) + (v - 0.5) * (v - 0.5)).sqrt() * 2.0).min(1.2);
    let dry = |height: f64| Shape {
        height,
        water: 0,
        river: 255.0,
        lowland: 0,
    };
    match biome {
        Biome::Alpine => dry(base + 10.0 * bump(r)),
        Biome::Scree => {
            dry(base + 14.0 * bump(r) + 2.5 * fbm2(seed ^ S_ROUGH, x / 9.0, z / 9.0, 2))
        }
        Biome::Snow => dry(base + 30.0 * bump(r)),
        Biome::Swamp => {
            // Flat wet ground at 136 with shallow pools that never get deep.
            let level = sea + 8.0;
            let pools = smoothstep(0.2, 0.45, fbm2(seed ^ S_POOLS, x / 22.0, z / 22.0, 2));
            let floor = level - 5.0 * pools;
            Shape {
                height: floor,
                water: if pools > 0.4 { (level - 1.0) as i32 } else { 0 },
                river: (1.0 - pools) * 14.0,
                lowland: 255,
            }
        }
        Biome::Riparian => {
            // A pond 10 m deep in the middle and a shallow stream across.
            let level = land - 6.0;
            let pond_r = 0.18 * width;
            let (dx, dz) = ((u - 0.5) * width, (v - 0.55) * width);
            let d = (dx * dx + dz * dz).sqrt();
            let pond = if d < pond_r {
                level - 44.0 * (1.0 - (d / pond_r) * (d / pond_r))
            } else {
                level
            };
            let half = 6.0;
            let channel = if dz.abs() < half {
                level - 13.0 * (1.0 - (dz / half) * (dz / half))
            } else {
                level
            };
            let floor = pond.min(channel);
            let water_level = level - 3.0;
            let to_pond = (d - pond_r).max(0.0);
            let to_channel = (dz.abs() - half).max(0.0);
            Shape {
                height: floor,
                water: if floor < water_level {
                    water_level as i32
                } else {
                    0
                },
                river: to_pond.min(to_channel),
                lowland: 0,
            }
        }
        Biome::Beach => {
            // Dunes sloping down to the sea along the east edge.
            let slope = (land - (sea + 1.0)) * smoothstep(0.0, 1.0, u);
            dry(base - slope + 1.2 * fbm2(seed ^ S_DUNES, x / 7.0, z / 7.0, 2))
        }
        Biome::Ocean => {
            // The sea floor falls away from the beach to 16 m deep, and rises
            // back to dry land at the north and south edges.
            let floor = sea + 1.0 - 64.0 * smoothstep(0.0, 0.7, u);
            let edge = smoothstep(0.78, 1.0, ((v - 0.5) * 2.0).abs());
            let height = floor + (land - floor) * edge;
            Shape {
                height,
                water: if height < sea { SEA_LEVEL } else { 0 },
                river: 255.0,
                lowland: 0,
            }
        }
        Biome::Meadow
        | Biome::TemperateForest
        | Biome::BorealForest
        | Biome::PineForest
        | Biome::Steppe
        | Biome::Desert => dry(base),
    }
}

pub(super) fn compute(spec: &WorldGenSpec) -> Result<ColumnMap, GenError> {
    let size = spec.size_cells;
    let n = size as usize;
    let seed = spec.seed;
    let pads = patches(size);
    let mut heights = vec![0u16; n * n];
    let mut biomes = vec![0u8; n * n];
    let mut lowland = vec![0u8; n * n];
    let mut moisture = vec![0u8; n * n];
    let mut temperature = vec![0u8; n * n];
    let mut river_dist = vec![255u8; n * n];
    let mut water_surface = vec![0u16; n * n];
    for z in 0..n {
        for x in 0..n {
            let (xi, zi) = (x as i64, z as i64);
            // The nearest pad (the one the column is in, or the closest).
            let (mut best, mut best_d) = (0, i64::MAX);
            for (i, p) in pads.iter().enumerate() {
                let dx = (p.x0 - xi).max(0).max(xi - (p.x1 - 1));
                let dz = (p.z0 - zi).max(0).max(zi - (p.z1 - 1));
                let d = dx * dx + dz * dz;
                if d < best_d {
                    (best, best_d) = (i, d);
                }
            }
            let p = &pads[best];
            let (w, h) = ((p.x1 - p.x0) as f64, (p.z1 - p.z0) as f64);
            let u = ((xi - p.x0) as f64 / w).clamp(0.0, 1.0);
            let v = ((zi - p.z0) as f64 / h).clamp(0.0, 1.0);
            let s = shape(p.biome, seed, xi as f64, zi as f64, u, v, w);
            let i = x + n * z;
            heights[i] = s.height.round().clamp(1.0, 65535.0) as u16;
            biomes[i] = p.biome as u8;
            lowland[i] = s.lowland;
            water_surface[i] = s.water.max(0) as u16;
            river_dist[i] = s.river.round().clamp(0.0, 255.0) as u8;
            // Each pad's wetness runs from drier in the west to wetter in the
            // east, so cover can be seen thinning and thickening in one pad.
            let (t, m) = biome_climate(p.biome);
            let m = (m + (u - 0.5) * 0.24).clamp(0.0, 1.0);
            moisture[i] = (m * 255.0).round() as u8;
            temperature[i] = ((t + 30.0) * 3.1875).round().clamp(0.0, 255.0) as u8;
        }
    }
    Ok(ColumnMap {
        size,
        seed,
        heights,
        biomes,
        lowland,
        moisture,
        temperature,
        river_dist,
        water_surface,
        natural: true,
        springs: Vec::new(),
        sinks: Vec::new(),
        repaired: 0,
        gallery: true,
    })
}

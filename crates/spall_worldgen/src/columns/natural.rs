//! The natural preset's column map: the coarse landscape (landform, erosion,
//! hydrology) carved with river channels, sampled to cell resolution, with
//! climate and a Whittaker-style biome table on top.

use super::{Biome, ColumnMap, Stack};
use crate::climate::{self, ClimateInput};
use crate::hydrology::{FP, NONE, OUTLET_SEA};
use crate::landform::MAX_TERRAIN;
use crate::landscape::{Landscape, SAMPLE_CELLS};
use crate::noise::{fbm2, smoothstep, value2};
use crate::spec::{GenError, SEA_LEVEL, WorldGenSpec, WorldgenPalette};

// Noise streams. Changing one changes the world: bump the preset's version.
const S_DETAIL: u64 = 0x6e01;
const S_DITHER_T: u64 = 0x6e02;
const S_DITHER_M: u64 = 0x6e03;
const S_PATCH: u64 = 0x6e04;

const CELLS: f64 = SAMPLE_CELLS as f64;

/// Chamfer (3-4) distance in samples from every `true` sample.
fn distance_samples(mask: &[bool], n: usize) -> Vec<f64> {
    const FAR: i32 = 1 << 28;
    let mut d: Vec<i32> = mask.iter().map(|&m| if m { 0 } else { FAR }).collect();
    let at = |d: &[i32], x: i64, z: i64| {
        if x < 0 || z < 0 || x >= n as i64 || z >= n as i64 {
            FAR
        } else {
            d[x as usize + n * z as usize]
        }
    };
    for z in 0..n as i64 {
        for x in 0..n as i64 {
            let best = at(&d, x, z)
                .min(at(&d, x - 1, z) + 3)
                .min(at(&d, x, z - 1) + 3)
                .min(at(&d, x - 1, z - 1) + 4)
                .min(at(&d, x + 1, z - 1) + 4);
            d[x as usize + n * z as usize] = best;
        }
    }
    for z in (0..n as i64).rev() {
        for x in (0..n as i64).rev() {
            let best = at(&d, x, z)
                .min(at(&d, x + 1, z) + 3)
                .min(at(&d, x, z + 1) + 3)
                .min(at(&d, x + 1, z + 1) + 4)
                .min(at(&d, x - 1, z + 1) + 4);
            d[x as usize + n * z as usize] = best;
        }
    }
    d.into_iter().map(|v| f64::from(v) / 3.0).collect()
}

/// Coarse fields the cell pass samples.
struct Coarse {
    n: usize,
    ground: Vec<f64>,
    /// Water surface height in cells, 0 where dry.
    surface: Vec<f64>,
    temperature: Vec<f64>,
    moisture: Vec<f64>,
    fresh_distance: Vec<f64>,
}

impl Coarse {
    fn corners(&self, x: i64, z: i64) -> [(usize, f64); 4] {
        let g = |c: i64| (c as f64 + 0.5) / CELLS - 0.5;
        let max = (self.n - 1) as f64;
        let (gx, gz) = (g(x).clamp(0.0, max), g(z).clamp(0.0, max));
        let (x0, z0) = (gx.floor(), gz.floor());
        let (fx, fz) = (gx - x0, gz - z0);
        let (x0, z0) = (x0 as usize, z0 as usize);
        let (x1, z1) = ((x0 + 1).min(self.n - 1), (z0 + 1).min(self.n - 1));
        let at = |x: usize, z: usize| x + self.n * z;
        [
            (at(x0, z0), (1.0 - fx) * (1.0 - fz)),
            (at(x1, z0), fx * (1.0 - fz)),
            (at(x0, z1), (1.0 - fx) * fz),
            (at(x1, z1), fx * fz),
        ]
    }

    fn bilinear(&self, field: &[f64], x: i64, z: i64) -> f64 {
        self.corners(x, z).iter().map(|&(i, w)| field[i] * w).sum()
    }

    /// Water surface from the wet corners only, so a shore does not drag the
    /// surface toward zero.
    fn water(&self, x: i64, z: i64) -> f64 {
        let (mut sum, mut weight) = (0.0, 0.0);
        for (i, w) in self.corners(x, z) {
            if self.surface[i] > 0.0 {
                sum += self.surface[i] * w;
                weight += w;
            }
        }
        if weight > 0.0 { sum / weight } else { 0.0 }
    }
}

fn build_coarse(scape: &Landscape) -> Coarse {
    let n = scape.n;
    let h = &scape.hydrology;
    let mut ground: Vec<f64> = h
        .height
        .iter()
        .map(|&v| f64::from(v) / f64::from(FP))
        .collect();
    let mut surface = vec![0.0f64; n * n];
    let sea_level = f64::from(SEA_LEVEL);

    for (i, s) in surface.iter_mut().enumerate() {
        if h.outlet[i] == OUTLET_SEA {
            *s = sea_level;
        } else if h.lake_of[i] != NONE {
            *s = f64::from(h.lakes[h.lake_of[i] as usize].level) / f64::from(FP);
        }
    }

    // River channels: a parabolic cross-section cut into the ground, filled
    // with water below the banks.
    for i in 0..n * n {
        if !h.river[i] {
            continue;
        }
        let area = f64::from(h.area[i]).sqrt();
        let depth = 2.0 + area / 60.0;
        let half_width = 1.0 + area / 45.0;
        let bed = f64::from(h.bed[i]) / f64::from(FP);
        let floor = bed - depth;
        let water = bed - 0.3 * depth;
        let (cx, cz) = ((i % n) as i64, (i / n) as i64);
        let reach = half_width.ceil() as i64;
        for dz in -reach..=reach {
            for dx in -reach..=reach {
                let (x, z) = (cx + dx, cz + dz);
                if x < 0 || z < 0 || x >= n as i64 || z >= n as i64 {
                    continue;
                }
                let dist = ((dx * dx + dz * dz) as f64).sqrt();
                if dist > half_width {
                    continue;
                }
                let j = x as usize + n * z as usize;
                if h.outlet[j] == OUTLET_SEA {
                    continue;
                }
                let t = dist / half_width;
                let profile = floor + depth * t * t;
                if profile < ground[j] {
                    ground[j] = profile;
                }
                if profile < water && surface[j] < water {
                    surface[j] = water;
                }
            }
        }
    }

    let sea: Vec<bool> = h.outlet.iter().map(|&o| o == OUTLET_SEA).collect();
    // Fresh water: every sample under a river, lake or channel, not the sea.
    let fresh: Vec<bool> = (0..n * n).map(|i| surface[i] > 0.0 && !sea[i]).collect();
    let fresh_distance = distance_samples(&fresh, n);
    let climate = climate::compute(
        scape.seed,
        &ClimateInput {
            n,
            ground: &ground,
            sea: &sea,
            fresh: &fresh,
            fresh_distance: &fresh_distance,
        },
    );
    Coarse {
        n,
        ground,
        surface,
        temperature: climate.temperature,
        moisture: climate.moisture,
        fresh_distance,
    }
}

/// Biome of a column from climate and terrain (Whittaker-style): temperature
/// `t` in degrees, wetness `m` in `0..=1`.
fn classify(t: f64, m: f64, h: i32, gradient: f64, submerged: bool, river_dist: f64) -> Biome {
    if submerged {
        return if river_dist <= 2.0 {
            Biome::Riparian
        } else {
            Biome::Ocean
        };
    }
    let above = h - SEA_LEVEL;
    if above <= 6 && gradient < 0.5 && river_dist > 10.0 {
        return Biome::Beach;
    }
    if t < -2.0 {
        return Biome::Snow;
    }
    if gradient >= 1.2 || (gradient >= 0.8 && t < 6.0) {
        return Biome::Scree;
    }
    if river_dist <= 7.0 && gradient < 0.4 {
        return Biome::Riparian;
    }
    if river_dist <= 30.0 && m > 0.58 && gradient < 0.15 && t > 5.0 {
        return Biome::Swamp;
    }
    if t < 4.0 {
        return if m >= 0.35 {
            Biome::Alpine
        } else {
            Biome::Scree
        };
    }
    if t < 9.0 {
        return if m < 0.3 {
            Biome::Steppe
        } else {
            Biome::BorealForest
        };
    }
    if m < 0.16 {
        Biome::Desert
    } else if m < 0.32 {
        Biome::Steppe
    } else if m < 0.5 {
        Biome::Meadow
    } else {
        Biome::TemperateForest
    }
}

pub(super) fn compute(spec: &WorldGenSpec) -> Result<ColumnMap, GenError> {
    let size = spec.size_cells;
    let n = size as usize;
    let scape = Landscape::generate(spec.seed, size)?;
    let coarse = build_coarse(&scape);
    let seed = spec.seed;

    let mut heights = vec![0u16; n * n];
    let mut water_surface = vec![0u16; n * n];
    let mut moisture = vec![0u8; n * n];
    let mut temperature = vec![0u8; n * n];
    let mut river_dist = vec![0u8; n * n];
    let border = crate::spec::BORDER_CELLS;
    let edge = i64::from(size) - border;

    let threads = std::thread::available_parallelism()
        .map_or(1, |p| p.get())
        .min(16);
    let rows_per = n.div_ceil(threads);
    std::thread::scope(|scope| {
        let parts = heights
            .chunks_mut(rows_per * n)
            .zip(water_surface.chunks_mut(rows_per * n))
            .zip(moisture.chunks_mut(rows_per * n))
            .zip(temperature.chunks_mut(rows_per * n))
            .zip(river_dist.chunks_mut(rows_per * n))
            .enumerate();
        for (i, ((((h, w), m), t), r)) in parts {
            let coarse = &coarse;
            scope.spawn(move || {
                let rows = h
                    .chunks_mut(n)
                    .zip(w.chunks_mut(n))
                    .zip(m.chunks_mut(n))
                    .zip(t.chunks_mut(n))
                    .zip(r.chunks_mut(n));
                for (row, ((((h, w), m), t), r)) in rows.enumerate() {
                    let z = (i * rows_per + row) as i64;
                    for x in 0..n {
                        let xi = x as i64;
                        let dist_cells = coarse.bilinear(&coarse.fresh_distance, xi, z) * CELLS;
                        // Surface texture, calmer beside water.
                        let amp = 1.3 * smoothstep(0.0, 16.0, dist_cells);
                        let detail =
                            fbm2(seed ^ S_DETAIL, xi as f64 / 11.0, z as f64 / 11.0, 2) * amp;
                        let ground = (coarse.bilinear(&coarse.ground, xi, z) + detail)
                            .clamp(1.0, MAX_TERRAIN);
                        let height = ground.round() as i64;
                        h[x] = height as u16;
                        let surface = coarse.water(xi, z).floor() as i64;
                        let inside = xi >= border && z >= border && xi < edge && z < edge;
                        w[x] = if inside && surface > height {
                            surface as u16
                        } else {
                            0
                        };
                        m[x] = (coarse.bilinear(&coarse.moisture, xi, z) * 255.0)
                            .round()
                            .clamp(0.0, 255.0) as u8;
                        t[x] = ((coarse.bilinear(&coarse.temperature, xi, z) + 30.0) * 3.1875)
                            .round()
                            .clamp(0.0, 255.0) as u8;
                        r[x] = dist_cells.round().clamp(0.0, 255.0) as u8;
                    }
                }
            });
        }
    });

    let mut map = ColumnMap {
        size,
        seed,
        heights,
        biomes: vec![0u8; n * n],
        lowland: vec![0u8; n * n],
        moisture,
        temperature,
        river_dist,
        water_surface,
        natural: true,
    };

    // Biomes need neighbouring heights, so they come second.
    let mut biomes = vec![0u8; n * n];
    let mut lowland = vec![0u8; n * n];
    std::thread::scope(|scope| {
        let map = &map;
        let parts = biomes
            .chunks_mut(rows_per * n)
            .zip(lowland.chunks_mut(rows_per * n))
            .enumerate();
        for (i, (b, l)) in parts {
            scope.spawn(move || {
                for (row, (b, l)) in b.chunks_mut(n).zip(l.chunks_mut(n)).enumerate() {
                    let z = (i * rows_per + row) as i64;
                    for x in 0..n {
                        let xi = x as i64;
                        // Dither the thresholds so biome borders are ragged, not ruled.
                        let t = map.temperature_c(xi, z)
                            + 1.6 * value2(seed ^ S_DITHER_T, xi as f64 / 8.0, z as f64 / 8.0);
                        let m = f64::from(map.moisture(xi, z)) / 255.0
                            + 0.07
                                * value2(
                                    seed ^ S_DITHER_M,
                                    xi as f64 / 10.0 + 100.0,
                                    z as f64 / 10.0,
                                );
                        let biome = classify(
                            t,
                            m,
                            map.height(xi, z),
                            map.gradient(xi, z),
                            map.is_water(xi, z),
                            f64::from(map.river_dist(xi, z)),
                        );
                        b[x] = biome as u8;
                        l[x] = if biome == Biome::Swamp { 255 } else { 0 };
                    }
                }
            });
        }
    });
    map.biomes = biomes;
    map.lowland = lowland;
    Ok(map)
}

impl ColumnMap {
    pub(super) fn natural_stack(&self, p: &WorldgenPalette, x: i64, z: i64) -> Stack {
        let st = |top, top_cells, sub, sub_cells| Stack {
            top,
            top_cells,
            sub,
            sub_cells,
        };
        let h = self.height(x, z);
        let biome = self.biome(x, z);
        let gradient = self.gradient(x, z);
        let rd = self.river_dist(x, z);
        let patch = fbm2(self.seed ^ S_PATCH, x as f64 / 17.0, z as f64 / 17.0, 2);
        let rock = || st(p.stone, 1, p.stone, 0);

        let depth = self.water_surface(x, z) - h;
        if depth > 0 {
            // Under water: sea floor, shallow river and shore gravel, deep lake clay.
            return if biome == Biome::Ocean {
                st(p.sand, 6, p.sandstone, 20)
            } else if depth <= 4 {
                st(p.gravel, 3, p.dirt, 6)
            } else {
                st(p.clay, 4, p.gravel, 6)
            };
        }
        match biome {
            Biome::Snow => st(p.snow, 4, p.stone, 0),
            Biome::Scree => {
                if patch > 0.2 {
                    st(p.slate, 6, p.stone, 0)
                } else if gradient >= 1.1 {
                    rock()
                } else {
                    st(p.gravel, 4, p.stone, 0)
                }
            }
            _ if gradient >= 1.1 => rock(),
            Biome::Beach => st(p.sand, 8, p.sandstone, 20),
            Biome::Desert => st(p.sand, 10, p.sandstone, 30),
            Biome::Ocean => st(p.sand, 6, p.sandstone, 20),
            Biome::Riparian => {
                if rd <= 3 && patch > 0.25 {
                    st(p.gravel, 3, p.dirt, 4)
                } else if rd <= 3 {
                    st(p.mud, 3, p.dirt, 6)
                } else {
                    st(p.grass, 2, p.dirt, 10)
                }
            }
            Biome::Swamp => {
                if rd <= 6 {
                    st(p.mud, 6, p.clay, 4)
                } else {
                    st(p.moss, 3, p.mud, 6)
                }
            }
            Biome::Alpine => st(p.grass, 2, p.dirt, 8),
            Biome::BorealForest => {
                if patch > 0.1 {
                    st(p.moss, 3, p.dirt, 8)
                } else {
                    st(p.grass, 2, p.dirt, 10)
                }
            }
            Biome::Steppe => {
                if patch > 0.0 {
                    st(p.grass, 1, p.dirt, 10)
                } else {
                    st(p.dirt, 2, p.dirt, 9)
                }
            }
            Biome::Meadow | Biome::TemperateForest => st(p.grass, 2, p.dirt, 10),
        }
    }
}

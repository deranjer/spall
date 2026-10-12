//! The natural preset's column map: the coarse landscape (landform, erosion,
//! hydrology) carved with river channels, sampled to cell resolution, with
//! climate and a Whittaker-style biome table on top.

use super::{Biome, ColumnMap, Stack};
use crate::climate::{self, ClimateInput};
use crate::hydrology::{FP, NONE, OUTLET_SEA};
use crate::landform::MAX_TERRAIN;
use crate::landscape::{Landscape, SAMPLE_CELLS};
use crate::noise::{fbm2, smoothstep, value2};
use crate::rivers;
use crate::spec::{GenError, SEA_LEVEL, WorldGenSpec, WorldgenPalette};

// Noise streams. Changing one changes the world: bump the preset's version.
const S_DETAIL: u64 = 0x6e01;
const S_DITHER_T: u64 = 0x6e02;
const S_DITHER_M: u64 = 0x6e03;
const S_PATCH: u64 = 0x6e04;
const S_PATCH_B: u64 = 0x6e05;
const S_PATCH_C: u64 = 0x6e06;

const CELLS: f64 = SAMPLE_CELLS as f64;

/// Whether river channels are planned full of water. Water standing in a
/// channel on a slope is not at rest: if it is woken it runs downhill within
/// seconds and pools in the valley floors (ENG-188: with the springs off, about
/// 21k columns went wet off the plan and 1,000 m3 moved on a 1024-cell world).
/// With the water asleep, which is how it starts, it is a still river; live
/// rivers are opt-in (`--live-rivers`) until springs have a discharge limit.
/// `false` carves the channels empty and leaves the springs to fill them.
const FILL_RIVER_CHANNELS: bool = true;
const CELL_STEP: i64 = SAMPLE_CELLS;

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

    let mut channel = vec![false; n * n];
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
        // The depth this river runs at while carrying what its catchment sheds,
        // on the slope of its bed (a drop over `SAMPLE_CELLS` cells, or a
        // diagonal of them).
        let r = h.receiver[i] as usize;
        let step = if i % n != r % n && i / n != r / n {
            std::f64::consts::SQRT_2
        } else {
            1.0
        };
        let slope = (bed - f64::from(h.bed[r]) / f64::from(FP)).max(0.0) / (step * CELLS);
        let flowing = rivers::uniform_flow_depth(
            rivers::discharge(u64::from(h.area[i])),
            slope,
            depth,
            half_width * CELLS,
        );
        let water = floor + flowing;
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
                if profile < water {
                    channel[j] = true;
                    if FILL_RIVER_CHANNELS && surface[j] < water {
                        surface[j] = water;
                    }
                }
            }
        }
    }

    let sea: Vec<bool> = h.outlet.iter().map(|&o| o == OUTLET_SEA).collect();
    // Fresh water: every sample under a river, lake or channel, not the sea.
    let fresh: Vec<bool> = (0..n * n)
        .map(|i| (surface[i] > 0.0 && !sea[i]) || channel[i])
        .collect();
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

/// The smallest share of a world's land a biome may hold before the generator
/// paints it in: every world shows every biome.
const MIN_BIOME_SHARE: f64 = 0.01;

/// The climate a painted biome carries: temperature in degrees and wetness
/// `0..=1`, so cover and plants read the same values the biome implies.
fn biome_climate(biome: Biome) -> (f64, f64) {
    match biome {
        Biome::Snow => (-6.0, 0.40),
        Biome::Scree => (3.0, 0.20),
        Biome::Alpine => (2.0, 0.50),
        Biome::BorealForest => (6.5, 0.50),
        Biome::Steppe => (14.0, 0.24),
        Biome::Meadow => (13.0, 0.41),
        Biome::TemperateForest => (13.0, 0.65),
        Biome::Desert => (24.0, 0.06),
        Biome::Swamp => (12.0, 0.75),
        Biome::Riparian => (12.0, 0.60),
        Biome::Beach => (16.0, 0.30),
        Biome::Ocean => (14.0, 0.50),
    }
}

/// How well a land column suits `biome` to be painted there (lower is better);
/// `None` where it cannot be.
fn repair_score(map: &ColumnMap, biome: Biome, x: i64, z: i64) -> Option<f64> {
    if map.is_water(x, z) || map.in_border(x, z) {
        return None;
    }
    let above = f64::from(map.height(x, z) - SEA_LEVEL);
    let gradient = map.gradient(x, z);
    let t = map.temperature_c(x, z);
    let m = f64::from(map.moisture(x, z)) / 255.0;
    let river = f64::from(map.river_dist(x, z));
    Some(match biome {
        Biome::Snow => -above,
        Biome::Scree => -gradient,
        Biome::Alpine => t + (1.0 - m) * 20.0,
        Biome::BorealForest => (t - 6.5).abs() + (m - 0.5).abs() * 20.0,
        Biome::Steppe => (m - 0.25).abs() * 50.0 + (t - 14.0).abs(),
        Biome::Meadow => (m - 0.41).abs() * 50.0 + (t - 13.0).abs(),
        Biome::TemperateForest => (m - 0.65).abs() * 50.0 + (t - 13.0).abs(),
        Biome::Desert => m * 100.0 - t,
        Biome::Swamp => gradient * 50.0 + above * 0.1 + river * 0.2,
        Biome::Riparian => (river - 3.0).abs() * 2.0 + gradient * 10.0,
        Biome::Beach => {
            if above > 12.0 {
                return None;
            }
            above + gradient * 5.0
        }
        // Water cannot be painted onto land.
        Biome::Ocean => return None,
    })
}

/// Paints in every biome the climate left out (or left under
/// [`MIN_BIOME_SHARE`] of the land): a ragged patch where the terrain and
/// climate suit it best, carrying that biome's moisture and temperature.
/// Deterministic, and a no-op for worlds that already have everything. Returns
/// the biomes painted, as bits.
fn ensure_every_biome(map: &mut ColumnMap) -> u16 {
    let size = i64::from(map.size);
    let land = |map: &ColumnMap, x: i64, z: i64| !map.is_water(x, z) && !map.in_border(x, z);
    let count = |map: &ColumnMap| {
        let mut per = [0usize; 12];
        let mut land_columns = 0usize;
        for z in 0..size {
            for x in 0..size {
                let b = map.biome(x, z) as usize;
                per[b] += 1;
                land_columns += usize::from(land(map, x, z));
            }
        }
        (per, land_columns)
    };
    let mut repaired = 0u16;
    // The rarest to place first, so a later patch does not erase an earlier one.
    for biome in [
        Biome::Snow,
        Biome::Scree,
        Biome::Alpine,
        Biome::Swamp,
        Biome::Desert,
        Biome::BorealForest,
        Biome::Steppe,
        Biome::Meadow,
        Biome::TemperateForest,
        Biome::Riparian,
        Biome::Beach,
    ] {
        let (per, land_columns) = count(map);
        let wanted = ((MIN_BIOME_SHARE * land_columns as f64) as usize).max(64);
        if per[biome as usize] >= wanted {
            continue;
        }
        // The best spot on a coarse grid, first in scan order on ties.
        let mut best: Option<(f64, i64, i64)> = None;
        for z in (0..size).step_by(8) {
            for x in (0..size).step_by(8) {
                if let Some(score) = repair_score(map, biome, x, z)
                    && best.is_none_or(|b| score < b.0)
                {
                    best = Some((score, x, z));
                }
            }
        }
        let Some((_, cx, cz)) = best else {
            continue;
        };
        let (t, m) = biome_climate(biome);
        let temperature = ((t + 30.0) * 3.1875).round().clamp(0.0, 255.0) as u8;
        let moisture = (m * 255.0).round().clamp(0.0, 255.0) as u8;
        // Sized to hold the wanted share; water and the border eat into the
        // disc, so it grows until the biome has what it needs.
        let mut radius = ((wanted as f64 / std::f64::consts::PI).sqrt() * 1.25).max(10.0);
        for _ in 0..5 {
            let reach = radius.ceil() as i64 + 6;
            for z in cz - reach..=cz + reach {
                for x in cx - reach..=cx + reach {
                    if x < 0 || z < 0 || x >= size || z >= size || !land(map, x, z) {
                        continue;
                    }
                    let d = (((x - cx).pow(2) + (z - cz).pow(2)) as f64).sqrt();
                    let ragged =
                        d + 5.0 * value2(map.seed ^ S_PATCH, x as f64 / 6.0, z as f64 / 6.0);
                    if ragged > radius {
                        continue;
                    }
                    let i = map.idx(x, z);
                    map.biomes[i] = biome as u8;
                    map.moisture[i] = moisture;
                    map.temperature[i] = temperature;
                    map.lowland[i] = if biome == Biome::Swamp { 255 } else { 0 };
                }
            }
            if count(map).0[biome as usize] >= wanted {
                break;
            }
            radius *= 1.3;
        }
        repaired |= 1 << biome as u16;
    }
    repaired
}

/// Biome of a column from climate and terrain (Whittaker-style): temperature
/// `t` in degrees, wetness `m` in `0..=1`.
fn classify(
    t: f64,
    m: f64,
    h: i32,
    gradient: f64,
    submerged: bool,
    sea: bool,
    river_dist: f64,
) -> Biome {
    if submerged {
        // Lakes and rivers are riparian water however wide; only the sea is ocean.
        return if river_dist <= 2.0 || !sea {
            Biome::Riparian
        } else {
            Biome::Ocean
        };
    }
    let above = h - SEA_LEVEL;
    if above <= 6 && gradient < 0.5 && river_dist > 10.0 {
        return Biome::Beach;
    }
    if t < 1.0 {
        return Biome::Snow;
    }
    if gradient >= 1.2 || (gradient >= 0.8 && t < 6.0) {
        return Biome::Scree;
    }
    if river_dist <= 7.0 && gradient < 0.4 {
        return Biome::Riparian;
    }
    if river_dist <= 40.0 && m > 0.5 && gradient < 0.25 && t > 5.0 {
        return Biome::Swamp;
    }
    if t < 4.0 {
        return if m >= 0.22 {
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
    // Warmer land dries out at a higher wetness: hot and dry is desert, cool and
    // dry is steppe.
    let dry = 0.16 + 0.012 * (t - 9.0).max(0.0);
    if m < dry {
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
        springs: Vec::new(),
        sinks: Vec::new(),
        repaired: 0,
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
                            map.water_surface(xi, z) <= SEA_LEVEL + 1,
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
    map.repaired = ensure_every_biome(&mut map);

    // Exchange cells: a spring refills the water cell at a river head; a
    // terminal basin drains at the surface of its lake.
    let cell_above_water = |map: &ColumnMap, x: i64, z: i64| -> Option<[i64; 3]> {
        if map.in_border(x, z) {
            return None;
        }
        let ground = i64::from(map.height(x, z));
        let top = i64::from(map.water_surface(x, z));
        Some([x, top.max(ground + 1), z])
    };
    let hydro = &scape.hydrology;
    let center = |i: usize| {
        (
            (i % hydro.n) as i64 * CELL_STEP + 2,
            (i / hydro.n) as i64 * CELL_STEP + 2,
        )
    };
    map.springs = hydro
        .heads
        .iter()
        .filter_map(|&(i, samples)| {
            let (x, z) = center(i as usize);
            let flow = rivers::discharge(samples) as f32;
            cell_above_water(&map, x, z).map(|cell| (cell, flow))
        })
        .collect();
    let total_flow: f64 = map.springs.iter().map(|(_, f)| f64::from(*f)).sum();
    map.sinks = water_sinks(&map, &scape, total_flow);
    Ok(map)
}

/// One drain cell empties at most one cell^3 of the water standing in it per
/// exchange, so a patch of them just above the water surface holds the level:
/// whatever the springs add above it leaves. The patch is sized for the springs' inflow
/// with headroom; a drain whose water is below its cell takes nothing.
const SINK_HEADROOM: f64 = 1.5;
const MIN_SINK_CELLS: usize = 16;

/// Where water leaves the world: the terminal lake of a landlocked map, or the
/// open sea of a coastal one (so river mouths never raise the sea).
fn water_sinks(map: &ColumnMap, scape: &Landscape, total_flow: f64) -> Vec<[i64; 3]> {
    let wanted = ((SINK_HEADROOM * total_flow / rivers::SINK_CELL_CAPACITY_CELLS3_S).ceil()
        as usize)
        .max(MIN_SINK_CELLS);
    let size = i64::from(map.size());
    let border = crate::spec::BORDER_CELLS;
    // The cell just above the surface: empty at the planned level, so the drain
    // only takes what rises above it and the water stays at its planned height.
    let top_cell = |x: i64, z: i64| -> Option<[i64; 3]> {
        // Deep enough that the column is wet water, not a film.
        (map.is_water(x, z) && map.water_surface(x, z) - map.height(x, z) >= 2)
            .then(|| [x, i64::from(map.water_surface(x, z)) + 1, z])
    };
    let mut sinks = Vec::new();
    if scape.landform.is_coastal() {
        let edges = scape
            .landform
            .sea_edges
            .iter()
            .filter(|&&e| e)
            .count()
            .max(1);
        let per_edge = wanted.div_ceil(edges);
        let inset = border + 4;
        for edge in (0..scape.landform.sea_edges.len()).filter(|&e| scape.landform.sea_edges[e]) {
            // Walk the edge just inside the wall, keeping evenly spaced wet columns.
            let mut line: Vec<[i64; 3]> = Vec::new();
            for t in inset..size - inset {
                let (x, z) = match edge {
                    0 => (t, inset),
                    1 => (t, size - 1 - inset),
                    2 => (inset, t),
                    _ => (size - 1 - inset, t),
                };
                line.extend(top_cell(x, z));
            }
            let step = (line.len() / per_edge.max(1)).max(1);
            sinks.extend(line.into_iter().step_by(step).take(per_edge));
        }
    } else {
        let hydro = &scape.hydrology;
        for basin in &scape.landform.basins {
            let (cx, cz) = (
                (basin.x as i64) * CELL_STEP + 2,
                (basin.z as i64) * CELL_STEP + 2,
            );
            let _ = hydro;
            // The wet columns nearest the basin's centre, nearest first.
            let reach = 48;
            let mut near: Vec<(i64, [i64; 3])> = Vec::new();
            for dz in -reach..=reach {
                for dx in -reach..=reach {
                    if let Some(cell) = top_cell(cx + dx, cz + dz) {
                        near.push((dx * dx + dz * dz, cell));
                    }
                }
            }
            near.sort_by_key(|&(d, cell)| (d, cell[2], cell[0]));
            let share = wanted.div_ceil(scape.landform.basins.len());
            sinks.extend(near.into_iter().take(share).map(|(_, cell)| cell));
        }
    }
    sinks
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
        let wet = f64::from(self.moisture(x, z)) / 255.0;
        // Three independent patch noises: where one kind of ground gives way to
        // another inside a biome. Each biome shows at least three surfaces.
        let (fx, fz) = (x as f64, z as f64);
        let a = fbm2(self.seed ^ S_PATCH, fx / 17.0, fz / 17.0, 2);
        let b = fbm2(self.seed ^ S_PATCH_B, fx / 11.0, fz / 11.0, 2);
        let c = fbm2(self.seed ^ S_PATCH_C, fx / 23.0, fz / 23.0, 2);
        let rock = || st(p.stone, 1, p.stone, 0);

        let depth = self.water_surface(x, z) - h;
        if depth > 0 {
            // Under water: the sea floor grades from sand through gravel to clay
            // with depth; fresh water has gravel shallows, mud pockets, clay deeps.
            return if biome == Biome::Ocean {
                if depth <= 3 {
                    st(p.sand, 6, p.sandstone, 20)
                } else if a > 0.25 {
                    st(p.gravel, 3, p.sand, 8)
                } else if depth > 14 {
                    st(p.clay, 4, p.gravel, 6)
                } else {
                    st(p.sand, 6, p.sandstone, 20)
                }
            } else if depth <= 4 {
                if b > 0.3 {
                    st(p.mud, 2, p.dirt, 6)
                } else {
                    st(p.gravel, 3, p.dirt, 6)
                }
            } else if a > 0.3 {
                st(p.mud, 3, p.clay, 6)
            } else {
                st(p.clay, 4, p.gravel, 6)
            };
        }
        match biome {
            Biome::Snow => {
                if a > 0.2 {
                    st(p.slate, 6, p.stone, 0)
                } else if b > 0.35 {
                    rock()
                } else {
                    st(p.snow, 4, p.stone, 0)
                }
            }
            Biome::Scree => {
                if a > 0.2 {
                    st(p.slate, 6, p.stone, 0)
                } else if gradient >= 1.1 || b > 0.3 {
                    rock()
                } else {
                    st(p.gravel, 4, p.stone, 0)
                }
            }
            _ if gradient >= 1.1 => rock(),
            Biome::Beach => {
                if a > 0.3 {
                    st(p.gravel, 3, p.sand, 6)
                } else if b < -0.3 {
                    st(p.sandstone, 3, p.sandstone, 20)
                } else {
                    st(p.sand, 8, p.sandstone, 20)
                }
            }
            Biome::Desert => {
                if a > 0.28 {
                    st(p.sandstone, 3, p.sandstone, 30)
                } else if b < -0.32 {
                    st(p.gravel, 3, p.sand, 10)
                } else if c > 0.35 {
                    st(p.clay, 3, p.sand, 10)
                } else {
                    st(p.sand, 10, p.sandstone, 30)
                }
            }
            Biome::Ocean => st(p.sand, 6, p.sandstone, 20),
            Biome::Riparian => {
                if rd <= 3 {
                    if a > 0.25 {
                        st(p.gravel, 3, p.dirt, 4)
                    } else if b < -0.3 {
                        st(p.clay, 3, p.dirt, 6)
                    } else {
                        st(p.mud, 3, p.dirt, 6)
                    }
                } else if wet > 0.6 && b > 0.2 {
                    st(p.moss, 2, p.dirt, 10)
                } else {
                    st(p.grass, 2, p.dirt, 10)
                }
            }
            Biome::Swamp => {
                if rd <= 6 {
                    if a > 0.12 {
                        st(p.clay, 4, p.mud, 4)
                    } else {
                        st(p.mud, 6, p.clay, 4)
                    }
                } else if b > 0.12 {
                    st(p.grass, 1, p.mud, 6)
                } else if c < -0.2 {
                    st(p.mud, 4, p.clay, 4)
                } else {
                    st(p.moss, 3, p.mud, 6)
                }
            }
            Biome::Alpine => {
                if a > 0.25 {
                    st(p.moss, 2, p.dirt, 8)
                } else if b > 0.3 {
                    st(p.gravel, 3, p.dirt, 8)
                } else if b < -0.35 {
                    st(p.dirt, 2, p.dirt, 8)
                } else {
                    st(p.grass, 2, p.dirt, 8)
                }
            }
            Biome::BorealForest => {
                if a > 0.1 {
                    st(p.moss, 3, p.dirt, 8)
                } else if b < -0.3 {
                    st(p.dirt, 2, p.dirt, 8)
                } else if wet > 0.7 && c > 0.2 {
                    st(p.mud, 2, p.dirt, 8)
                } else {
                    st(p.grass, 2, p.dirt, 10)
                }
            }
            Biome::Steppe => {
                if b > 0.32 {
                    st(p.gravel, 2, p.dirt, 9)
                } else if c < -0.35 {
                    st(p.clay, 2, p.dirt, 9)
                } else if a > 0.0 {
                    st(p.grass, 1, p.dirt, 10)
                } else {
                    st(p.dirt, 2, p.dirt, 9)
                }
            }
            Biome::Meadow => {
                if b < -0.33 {
                    st(p.dirt, 2, p.dirt, 10)
                } else if wet > 0.42 && a > 0.15 {
                    st(p.moss, 2, p.dirt, 10)
                } else if c > 0.38 {
                    st(p.gravel, 2, p.dirt, 10)
                } else {
                    st(p.grass, 2, p.dirt, 10)
                }
            }
            Biome::TemperateForest => {
                if a > 0.1 {
                    st(p.moss, 2, p.dirt, 10)
                } else if b < -0.25 {
                    st(p.dirt, 2, p.dirt, 10)
                } else if wet > 0.75 && c > 0.2 {
                    st(p.mud, 2, p.dirt, 10)
                } else {
                    st(p.grass, 2, p.dirt, 10)
                }
            }
        }
    }
}

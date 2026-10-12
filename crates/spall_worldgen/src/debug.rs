//! Previews and statistics from the 2D column map (no voxels needed), used by
//! the editor minimap and by coverage tests.

use crate::columns::{Biome, ColumnMap};
use crate::spec::SEA_LEVEL;

/// Preview colour of a biome (linear-ish sRGB bytes), independent of the
/// game's material palette.
pub fn biome_color(biome: Biome) -> [u8; 3] {
    match biome {
        Biome::Meadow => [96, 158, 70],
        Biome::Alpine => [140, 140, 148],
        Biome::Swamp => [84, 100, 58],
        Biome::Desert => [214, 190, 120],
        Biome::Ocean => [30, 70, 150],
        Biome::Beach => [226, 210, 150],
        Biome::Riparian => [70, 150, 96],
        Biome::TemperateForest => [52, 120, 56],
        Biome::BorealForest => [58, 96, 78],
        Biome::Scree => [128, 120, 112],
        Biome::Snow => [240, 244, 250],
        Biome::Steppe => [170, 164, 96],
        Biome::PineForest => [34, 76, 58],
    }
}

/// `size x size` RGBA8, row-major with `z` as the row: biome colour shaded by
/// height, snow-white peaks, water in blue that darkens with depth.
pub fn top_down_rgba(columns: &ColumnMap) -> Vec<u8> {
    let size = i64::from(columns.size());
    let mut out = Vec::with_capacity((size * size) as usize * 4);
    for z in 0..size {
        for x in 0..size {
            let h = columns.height(x, z);
            let rgb = if columns.is_water(x, z) {
                let depth = f64::from(columns.water_surface(x, z) - h).min(30.0) / 30.0;
                [
                    (60.0 - 40.0 * depth) as u8,
                    (130.0 - 60.0 * depth) as u8,
                    (190.0 - 40.0 * depth) as u8,
                ]
            } else {
                let base = biome_color(columns.biome(x, z));
                let shade = 0.65 + 0.5 * (f64::from(h - SEA_LEVEL) / 220.0).min(1.0);
                let base = if !columns.is_natural() && h > crate::columns::SNOW_LINE {
                    [240, 244, 250]
                } else {
                    base
                };
                [
                    (f64::from(base[0]) * shade).min(255.0) as u8,
                    (f64::from(base[1]) * shade).min(255.0) as u8,
                    (f64::from(base[2]) * shade).min(255.0) as u8,
                ]
            };
            out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnStats {
    /// Share of columns per biome, indexed by stored biome number (sums to 1).
    pub biome_share: [f64; 13],
    /// Share of columns covered by water.
    pub water_share: f64,
    pub min_height: i32,
    pub max_height: i32,
}

pub fn column_stats(columns: &ColumnMap) -> ColumnStats {
    let size = i64::from(columns.size());
    let total = (size * size) as f64;
    let mut counts = [0u64; 13];
    let mut water = 0u64;
    for z in 0..size {
        for x in 0..size {
            counts[columns.biome(x, z) as usize] += 1;
            water += u64::from(columns.is_water(x, z));
        }
    }
    let (min_height, max_height) = columns.min_max_height();
    ColumnStats {
        biome_share: counts.map(|c| c as f64 / total),
        water_share: water as f64 / total,
        min_height,
        max_height,
    }
}

/// What a top-down preview of the column map shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreviewLayer {
    /// Biome colour shaded by height (what [`top_down_rgba`] draws).
    Biome,
    /// Ground height: green lowland through brown to white peaks; water blue.
    Height,
    /// Wetness, 0 (dry) to 100%.
    Moisture,
    /// Air temperature at the ground.
    Temperature,
    /// Where the water is and where it comes from: the sea, rivers and lakes,
    /// springs (the river heads) and sinks (where water leaves the world).
    Water,
    /// An estimate of how thickly plants grow, from biome and wetness.
    Vegetation,
}

impl PreviewLayer {
    pub const ALL: [PreviewLayer; 6] = [
        PreviewLayer::Biome,
        PreviewLayer::Height,
        PreviewLayer::Moisture,
        PreviewLayer::Temperature,
        PreviewLayer::Water,
        PreviewLayer::Vegetation,
    ];

    pub fn name(self) -> &'static str {
        match self {
            PreviewLayer::Biome => "Biomes",
            PreviewLayer::Height => "Height",
            PreviewLayer::Moisture => "Moisture",
            PreviewLayer::Temperature => "Temperature",
            PreviewLayer::Water => "Rivers, lakes and springs",
            PreviewLayer::Vegetation => "Vegetation density",
        }
    }
}

/// The range a continuous layer covers on a map, in the layer's own unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerRange {
    pub min: f64,
    pub max: f64,
    pub unit: &'static str,
}

/// How thickly plants grow at a column, `0..=1`: zero in water, scaled by how
/// fertile the biome is and by wetness. An estimate for the editor, from the
/// same biome and moisture maps the plants are placed from; it is not a count of
/// the plants the game places.
pub fn vegetation_density(columns: &ColumnMap, x: i64, z: i64) -> f64 {
    if columns.is_water(x, z) {
        return 0.0;
    }
    let fertile = match columns.biome(x, z) {
        Biome::PineForest => 1.0,
        Biome::TemperateForest => 0.95,
        Biome::Swamp => 0.85,
        Biome::BorealForest => 0.8,
        Biome::Riparian => 0.8,
        Biome::Meadow => 0.55,
        Biome::Steppe => 0.4,
        Biome::Alpine => 0.35,
        Biome::Beach => 0.12,
        Biome::Scree => 0.1,
        Biome::Desert => 0.08,
        Biome::Snow | Biome::Ocean => 0.0,
    };
    fertile * (0.35 + 0.65 * f64::from(columns.moisture(x, z)) / 255.0)
}

fn lerp_rgb(a: [u8; 3], b: [u8; 3], t: f64) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    std::array::from_fn(|i| {
        (f64::from(a[i]) + (f64::from(b[i]) - f64::from(a[i])) * t).round() as u8
    })
}

/// A colour ramp through `stops` (position `0..=1`, colour), linear between.
fn ramp(stops: &[(f64, [u8; 3])], t: f64) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    for pair in stops.windows(2) {
        if t <= pair[1].0 {
            let span = (pair[1].0 - pair[0].0).max(1e-9);
            return lerp_rgb(pair[0].1, pair[1].1, (t - pair[0].0) / span);
        }
    }
    stops.last().map_or([0; 3], |s| s.1)
}

const WATER_SHALLOW: [u8; 3] = [90, 160, 215];
const WATER_DEEP: [u8; 3] = [20, 60, 140];
const FRESH_WATER: [u8; 3] = [60, 200, 220];
const HEIGHT_RAMP: [(f64, [u8; 3]); 4] = [
    (0.0, [70, 150, 80]),
    (0.4, [150, 170, 90]),
    (0.7, [140, 105, 70]),
    (1.0, [245, 245, 250]),
];
const MOISTURE_RAMP: [(f64, [u8; 3]); 3] = [
    (0.0, [214, 190, 130]),
    (0.5, [110, 170, 110]),
    (1.0, [30, 100, 180]),
];
const TEMPERATURE_RAMP: [(f64, [u8; 3]); 4] = [
    (0.0, [60, 90, 200]),
    (0.35, [200, 225, 245]),
    (0.65, [240, 215, 90]),
    (1.0, [200, 50, 40]),
];
const VEGETATION_RAMP: [(f64, [u8; 3]); 3] = [
    (0.0, [60, 52, 40]),
    (0.5, [110, 160, 60]),
    (1.0, [14, 90, 36]),
];

/// Springs (river heads) and sinks (where water leaves) on the water layer.
pub const SPRING_COLOR: [u8; 3] = [255, 230, 60];
pub const SINK_COLOR: [u8; 3] = [230, 60, 200];

/// The temperature range a preview ramp is stretched over, degrees Celsius.
const TEMPERATURE_SPAN: (f64, f64) = (-20.0, 35.0);

/// The range of a continuous layer on this map (`None` for the discrete ones).
pub fn layer_range(columns: &ColumnMap, layer: PreviewLayer) -> Option<LayerRange> {
    let size = i64::from(columns.size());
    let mut lo = f64::MAX;
    let mut hi = f64::MIN;
    let mut note = |v: f64| {
        lo = lo.min(v);
        hi = hi.max(v);
    };
    match layer {
        PreviewLayer::Biome | PreviewLayer::Water => return None,
        PreviewLayer::Height => {
            let (a, b) = columns.min_max_height();
            return Some(LayerRange {
                min: f64::from(a) * 0.25,
                max: f64::from(b) * 0.25,
                unit: "m above the arena floor",
            });
        }
        PreviewLayer::Moisture => {
            for z in (0..size).step_by(4) {
                for x in (0..size).step_by(4) {
                    note(f64::from(columns.moisture(x, z)) / 255.0 * 100.0);
                }
            }
        }
        PreviewLayer::Temperature => {
            for z in (0..size).step_by(4) {
                for x in (0..size).step_by(4) {
                    note(columns.temperature_c(x, z));
                }
            }
        }
        PreviewLayer::Vegetation => {
            for z in (0..size).step_by(4) {
                for x in (0..size).step_by(4) {
                    note(vegetation_density(columns, x, z) * 100.0);
                }
            }
        }
    }
    let unit = match layer {
        PreviewLayer::Temperature => "degrees C",
        _ => "%",
    };
    Some(LayerRange {
        min: lo,
        max: hi,
        unit,
    })
}

/// `size x size` RGBA8, row-major with `z` as the row, for one preview layer.
pub fn layer_rgba(columns: &ColumnMap, layer: PreviewLayer) -> Vec<u8> {
    if layer == PreviewLayer::Biome {
        return top_down_rgba(columns);
    }
    let size = i64::from(columns.size());
    let (low, high) = columns.min_max_height();
    let height_span = f64::from((high - low).max(1));
    let mut out = Vec::with_capacity((size * size) as usize * 4);
    for z in 0..size {
        for x in 0..size {
            let h = columns.height(x, z);
            let wet = columns.is_water(x, z);
            let sea = wet && columns.water_surface(x, z) <= SEA_LEVEL + 1;
            let depth = f64::from(columns.water_surface(x, z) - h).max(0.0);
            let water_colour = |fresh_tint: bool| {
                let base = lerp_rgb(WATER_SHALLOW, WATER_DEEP, depth / 30.0);
                if fresh_tint {
                    lerp_rgb(base, FRESH_WATER, 0.5)
                } else {
                    base
                }
            };
            let rgb = match layer {
                PreviewLayer::Biome => unreachable!("drawn above"),
                PreviewLayer::Height => {
                    if wet {
                        water_colour(!sea)
                    } else {
                        ramp(&HEIGHT_RAMP, f64::from(h - low) / height_span)
                    }
                }
                PreviewLayer::Moisture => {
                    if wet {
                        water_colour(!sea)
                    } else {
                        ramp(&MOISTURE_RAMP, f64::from(columns.moisture(x, z)) / 255.0)
                    }
                }
                PreviewLayer::Temperature => {
                    let t = (columns.temperature_c(x, z) - TEMPERATURE_SPAN.0)
                        / (TEMPERATURE_SPAN.1 - TEMPERATURE_SPAN.0);
                    let c = ramp(&TEMPERATURE_RAMP, t);
                    if wet {
                        lerp_rgb(c, WATER_SHALLOW, 0.5)
                    } else {
                        c
                    }
                }
                PreviewLayer::Water => {
                    if wet {
                        if sea {
                            water_colour(false)
                        } else {
                            lerp_rgb(FRESH_WATER, WATER_DEEP, depth / 40.0)
                        }
                    } else {
                        // Land dimmed to a backdrop so the water reads.
                        let shade = (0.28 + 0.22 * f64::from(h - low) / height_span) * 2.0;
                        [
                            (60.0 * shade) as u8,
                            (70.0 * shade) as u8,
                            (60.0 * shade) as u8,
                        ]
                    }
                }
                PreviewLayer::Vegetation => {
                    if wet {
                        lerp_rgb(WATER_DEEP, [0, 0, 0], 0.5)
                    } else {
                        ramp(&VEGETATION_RAMP, vegetation_density(columns, x, z))
                    }
                }
            };
            out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
    if layer == PreviewLayer::Water {
        let mut mark = |cell: [i64; 3], half: i64, rgb: [u8; 3]| {
            for dz in -half..=half {
                for dx in -half..=half {
                    let (x, z) = (cell[0] + dx, cell[2] + dz);
                    if (0..size).contains(&x) && (0..size).contains(&z) {
                        let i = ((z * size + x) * 4) as usize;
                        out[i..i + 3].copy_from_slice(&rgb);
                    }
                }
            }
        };
        for (cell, _) in columns.springs() {
            mark(*cell, 3, SPRING_COLOR);
        }
        for cell in columns.sinks() {
            mark(*cell, 2, SINK_COLOR);
        }
    }
    out
}

/// Colour swatches and labels for a layer's legend (empty for the biome layer,
/// whose legend is the biome list).
pub fn layer_swatches(layer: PreviewLayer) -> Vec<([u8; 3], &'static str)> {
    match layer {
        PreviewLayer::Biome => Vec::new(),
        PreviewLayer::Height => vec![
            (ramp(&HEIGHT_RAMP, 0.0), "low"),
            (ramp(&HEIGHT_RAMP, 0.7), "high"),
            (ramp(&HEIGHT_RAMP, 1.0), "peaks"),
            (WATER_SHALLOW, "water"),
        ],
        PreviewLayer::Moisture => vec![
            (ramp(&MOISTURE_RAMP, 0.0), "dry"),
            (ramp(&MOISTURE_RAMP, 0.5), "moist"),
            (ramp(&MOISTURE_RAMP, 1.0), "wet"),
        ],
        PreviewLayer::Temperature => vec![
            (ramp(&TEMPERATURE_RAMP, 0.0), "cold"),
            (ramp(&TEMPERATURE_RAMP, 0.35), "cool"),
            (ramp(&TEMPERATURE_RAMP, 0.65), "warm"),
            (ramp(&TEMPERATURE_RAMP, 1.0), "hot"),
        ],
        PreviewLayer::Water => vec![
            (WATER_SHALLOW, "sea"),
            (FRESH_WATER, "river or lake"),
            (SPRING_COLOR, "spring"),
            (SINK_COLOR, "sink"),
        ],
        PreviewLayer::Vegetation => vec![
            (ramp(&VEGETATION_RAMP, 0.0), "bare"),
            (ramp(&VEGETATION_RAMP, 0.5), "grass"),
            (ramp(&VEGETATION_RAMP, 1.0), "forest"),
        ],
    }
}

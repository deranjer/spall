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
                let depth = f64::from(SEA_LEVEL - h).min(30.0) / 30.0;
                [
                    (60.0 - 40.0 * depth) as u8,
                    (130.0 - 60.0 * depth) as u8,
                    (190.0 - 40.0 * depth) as u8,
                ]
            } else {
                let base = biome_color(columns.biome(x, z));
                let shade = 0.65 + 0.5 * (f64::from(h - SEA_LEVEL) / 220.0).min(1.0);
                let base = if h > crate::columns::SNOW_LINE {
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
    /// Share of columns per biome, in [`Biome::ALL`] order (sums to 1).
    pub biome_share: [f64; 4],
    /// Share of columns covered by water.
    pub water_share: f64,
    pub min_height: i32,
    pub max_height: i32,
}

pub fn column_stats(columns: &ColumnMap) -> ColumnStats {
    let size = i64::from(columns.size());
    let total = (size * size) as f64;
    let mut counts = [0u64; 4];
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

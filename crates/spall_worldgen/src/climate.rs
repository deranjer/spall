//! Climate on the coarse grid: temperature from a latitude axis and the lapse
//! rate, moisture from prevailing wind with orographic rain and rain shadow.
//!
//! Wind blows along one of eight grid directions chosen by the seed. Each
//! upwind edge cell starts a line of air that picks up water over the sea and
//! over lakes and rivers, and rains out where the ground rises. What is left
//! behind a range is dry. Only `+ - * / floor sqrt` touch `f64`.

use crate::noise::{fbm2, hash01};
use crate::spec::SEA_LEVEL;

const S_WIND: u64 = 0x5c01;
const S_AXIS: u64 = 0x5c02;
const S_TEMP_NOISE: u64 = 0x5c03;
const S_MOIST_NOISE: u64 = 0x5c04;

/// Degrees Celsius lost per cell of height. Game mountains are low, so the
/// lapse is steeper than the real 6.5 per km: a 60 m peak is about 15 degrees
/// colder than the coast.
const LAPSE_PER_CELL: f64 = 0.065;
/// Sea-level temperature at the middle of the map and the swing toward each pole.
const SEA_TEMP: f64 = 17.0;
const LATITUDE_SWING: f64 = 11.0;

const WINDS: [(i64, i64); 8] = [
    (1, 0),
    (-1, 0),
    (0, 1),
    (0, -1),
    (1, 1),
    (1, -1),
    (-1, 1),
    (-1, -1),
];

pub struct Climate {
    /// Degrees Celsius at the ground.
    pub temperature: Vec<f64>,
    /// Wetness `0..=1`: rain plus nearness to fresh water.
    pub moisture: Vec<f64>,
    /// The prevailing wind direction (towards where it blows).
    pub wind: (i64, i64),
}

/// Inputs per grid sample.
pub struct ClimateInput<'a> {
    pub n: usize,
    /// Ground height in cells.
    pub ground: &'a [f64],
    /// Open sea (outlet) samples.
    pub sea: &'a [bool],
    /// Samples holding fresh standing or running water.
    pub fresh: &'a [bool],
    /// Distance in samples to the nearest fresh water.
    pub fresh_distance: &'a [f64],
}

fn smooth01(edge0: f64, edge1: f64, x: f64) -> f64 {
    crate::noise::smoothstep(edge0, edge1, x)
}

pub fn compute(seed: u64, input: &ClimateInput) -> Climate {
    let n = input.n;
    let sea_level = f64::from(SEA_LEVEL);
    let wind = WINDS[(hash01(seed, S_WIND) * 8.0) as usize % 8];

    // Latitude runs along a seeded axis; the map's middle is temperate.
    let (ax, az) = (hash01(seed, S_AXIS) - 0.5, hash01(seed, S_AXIS + 1) - 0.5);
    let len = (ax * ax + az * az).sqrt().max(1e-9);
    let (ax, az) = (ax / len, az / len);

    let inv = 1.0 / n as f64;
    let mut temperature = vec![0.0; n * n];
    for j in 0..n {
        for i in 0..n {
            let (u, v) = ((i as f64 + 0.5) * inv, (j as f64 + 0.5) * inv);
            let lat = ((u - 0.5) * ax + (v - 0.5) * az) * 2.0;
            let idx = i + n * j;
            let noise = fbm2(seed ^ S_TEMP_NOISE, u * 5.0, v * 5.0, 2) * 2.0;
            temperature[idx] = SEA_TEMP + LATITUDE_SWING * lat + noise
                - LAPSE_PER_CELL * (input.ground[idx].max(sea_level) - sea_level);
        }
    }

    // Air moves along the wind: one line per upwind edge sample.
    let mut humidity = vec![0.0; n * n];
    let mut rain = vec![0.0; n * n];
    let (dx, dz) = wind;
    let in_grid = |x: i64, z: i64| x >= 0 && z >= 0 && x < n as i64 && z < n as i64;
    for sz in 0..n as i64 {
        for sx in 0..n as i64 {
            if in_grid(sx - dx, sz - dz) {
                continue; // not an upwind edge sample
            }
            let (mut x, mut z) = (sx, sz);
            let mut q = 0.55;
            let mut prev = input.ground[sx as usize + n * sz as usize];
            while in_grid(x, z) {
                let i = x as usize + n * z as usize;
                let h = input.ground[i];
                if input.sea[i] {
                    q += (1.0 - q) * 0.4;
                } else {
                    let rise = (h - prev).max(0.0);
                    let oro = (rise * 0.04).min(0.5);
                    let removed = q * (0.004 + oro * 0.5);
                    q -= removed;
                    rain[i] = removed;
                    if input.fresh[i] {
                        q += (0.9 - q) * 0.02;
                    }
                    q += (0.55 - q) * 0.004;
                }
                humidity[i] = q;
                prev = h;
                x += dx;
                z += dz;
            }
        }
    }

    let mut moisture = vec![0.0; n * n];
    for j in 0..n {
        for i in 0..n {
            let (u, v) = ((i as f64 + 0.5) * inv, (j as f64 + 0.5) * inv);
            let idx = i + n * j;
            let near_water = 1.0 - smooth01(0.0, 14.0, input.fresh_distance[idx]);
            let noise = fbm2(seed ^ S_MOIST_NOISE, u * 8.0, v * 8.0, 3) * 0.08;
            moisture[idx] =
                (0.12 + 0.7 * humidity[idx] + 6.0 * rain[idx] + 0.3 * near_water + noise)
                    .clamp(0.0, 1.0);
        }
    }
    Climate {
        temperature,
        moisture,
        wind,
    }
}

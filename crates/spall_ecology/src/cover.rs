//! Stateless ground cover: the carpet of grass, sedge, clover and tussock.
//!
//! Cover has no server state, no save and no per-plant wire record. Whether a
//! cell carries a tuft, and what kind, is a pure function of the world seed,
//! the cell, the soil under it, and the climate there ([`ground_cover`]); the
//! same inputs give the same tuft on every machine, every time, so nothing
//! about it is ever stored. The climate comes from a coarse raster
//! ([`CoverField`], one sample per 4 m) that the world carries instead of the
//! thousands of per-plant records the carpet used to need.
//!
//! Digging the soil away removes its cover and filling the cell above it does
//! too, because both change what the renderer finds at the surface. Nothing
//! here grows, seeds or dies; the living organisms (trees, ferns, reeds,
//! flowers, scrub) stay in [`crate::living`].
use serde::{Deserialize, Serialize};
use spall_worldgen::{Biome, ColumnMap};

use crate::living::{Season, SoftPart, hash};

/// Cells (0.25 m) per sample of the climate raster: 4 m.
pub const FIELD_CELLS: i64 = 16;
/// The largest raster edge accepted from the wire (4096 cells / 16, with room).
pub const MAX_FIELD_WIDTH: u32 = 512;
/// Up to here from the eye every cell may carry a full tuft (metres).
pub const NEAR_M: f32 = 14.0;
/// Up to here a quarter of the cells carry a coarse, wider tuft (metres).
pub const MID_M: f32 = 34.0;
/// Soft parts one cover rebuild may produce: its own budget, apart from trees.
pub const INSTANCE_BUDGET: usize = 48_000;
/// Ground forms that cover owns: grass, sedge, clover rosette, tussock cushion.
pub const CARPET_FORMS: [u8; 4] = [6, 9, 11, 12];
/// A species' tuft density is this many in 256 cells.
pub const MAX_COVER_SPECIES: usize = 32;
/// Most destroyed-cover records a world keeps at once.
pub const MAX_CLEARED: usize = 65_536;
/// Cover destroyed this long ago (seconds of ecological time) is a small
/// sprout, then medium, then full grown (and forgotten).
pub const SMALL_AFTER_S: u32 = 20;
pub const MEDIUM_AFTER_S: u32 = 90;
pub const FULL_AFTER_S: u32 = 180;

/// A column whose cover was destroyed, and when (whole seconds of the world's
/// ecological clock). The one thing about cover the server remembers: cover
/// itself is stateless, but where it was torn up and how long ago is not
/// something a formula can know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cleared {
    pub x: u16,
    pub z: u16,
    pub at_s: u32,
}

/// How grown cover is `age_s` seconds after it was destroyed: `None` while
/// nothing has sprouted yet, then small, medium and, once [`FULL_AFTER_S`] has
/// passed, full size (`1.0`: the record can be dropped).
pub fn regrowth(age_s: u32) -> Option<f32> {
    match age_s {
        a if a < SMALL_AFTER_S => None,
        a if a < MEDIUM_AFTER_S => Some(0.3),
        a if a < FULL_AFTER_S => Some(0.65),
        _ => Some(1.0),
    }
}

/// The destroyed-cover records laid out for lookups.
#[derive(Debug, Clone, Default)]
pub struct CoverMemory {
    cleared: std::collections::HashMap<(i64, i64), u32>,
    now_s: u32,
}

impl CoverMemory {
    pub fn new(cleared: &[Cleared], now_s: u32) -> Self {
        Self {
            cleared: cleared
                .iter()
                .map(|c| ((i64::from(c.x), i64::from(c.z)), c.at_s))
                .collect(),
            now_s,
        }
    }

    /// Whether the server has recorded this column as destroyed.
    pub fn is_recorded(&self, x: i64, z: i64) -> bool {
        self.cleared.contains_key(&(x, z))
    }

    /// The size cover has in this column: `Some(1.0)` if untouched or fully
    /// regrown, `Some(0.3)` / `Some(0.65)` while regrowing, `None` while
    /// destroyed.
    pub fn growth_at(&self, x: i64, z: i64) -> Option<f32> {
        match self.cleared.get(&(x, z)) {
            None => Some(1.0),
            Some(at) => regrowth(self.now_s.saturating_sub(*at)),
        }
    }
}

pub fn is_carpet_form(form: u8) -> bool {
    CARPET_FORMS.contains(&form)
}

/// One kind of ground cover. The sandbox authors these; ids are stable
/// save/wire values and keep the numbers the species had as organisms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverSpecies {
    pub id: u16,
    /// Bit `1 << biome` for each biome the cover grows in.
    pub biomes: u16,
    pub soils: [u16; 4],
    pub moisture: [u8; 2],
    /// Air temperature range this cover lives in, degrees C.
    pub temperature: [i8; 2],
    /// Ground form code (see `living::Species`): 6 grass, 9 sedge, 11 rosette,
    /// 12 cushion.
    pub form: u8,
    /// Mature height in quarter-cells.
    pub height: u16,
    /// Colour index per season (spring, summer, autumn, winter).
    pub colours: [u8; 4],
    pub evergreen: bool,
    /// Tufts per 256 cells where the species is the one chosen.
    pub density: u8,
    /// Water depth in cells the species grows in: `[0, 0]` is dry land only,
    /// anything else lives under water (seagrass, anemones, pondweed).
    pub depth: [u8; 2],
}

impl CoverSpecies {
    fn accepts(&self, biome: Biome, moisture: u8, temperature: i8, soil: u16, depth: u8) -> bool {
        self.biomes & (1 << biome as u16) != 0
            && (self.depth[0]..=self.depth[1]).contains(&depth)
            && self.soils.contains(&soil)
            && (self.moisture[0]..=self.moisture[1]).contains(&moisture)
            && (self.temperature[0]..=self.temperature[1]).contains(&temperature)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id == 0
            || self.biomes == 0
            || self.biomes > crate::living::ALL_BIOMES
            || !is_carpet_form(self.form)
            || self.height == 0
            || self.height > 64
            || self.density == 0
            || self.moisture[0] > self.moisture[1]
            || self.temperature[0] > self.temperature[1]
            || self.depth[0] > self.depth[1]
            || self
                .colours
                .iter()
                .any(|c| usize::from(*c) >= crate::living::PALETTE.len())
        {
            return Err("invalid cover species".into());
        }
        Ok(())
    }
}

/// One tuft of cover on a cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cover {
    pub species: u16,
    pub form: u8,
    pub colour: u8,
    /// Standing height, metres.
    pub height_m: f32,
    pub evergreen: bool,
    /// Per-tuft key: blades' yaw, lean and jitter derive from it.
    pub key: u64,
}

/// The cover on `cell` (the open cell just above the soil), if any. `depth`
/// is the water standing over the cell in cells, 0 on dry ground.
///
/// Pure: the answer depends on nothing but the arguments. `surface_material`
/// is the soil under the cell, `moisture` is 0..=255, `temperature_c` is the
/// air temperature there, and `biome` the cell's biome. `set` is the world's
/// cover catalogue; where several species fit, a hash of the cell picks one,
/// and a second hash decides whether the cell carries a tuft at all.
#[allow(clippy::too_many_arguments)]
pub fn ground_cover(
    set: &[CoverSpecies],
    seed: u64,
    cell: [i64; 3],
    surface_material: u16,
    moisture: u8,
    temperature_c: i8,
    biome: Biome,
    season: Season,
    depth: u8,
) -> Option<Cover> {
    let key =
        hash(seed ^ (cell[0] as u64).rotate_left(21) ^ (cell[2] as u64).rotate_left(43) ^ 0xC0FE);
    let mut fits = [0usize; MAX_COVER_SPECIES];
    let mut n = 0;
    for (i, s) in set.iter().enumerate().take(MAX_COVER_SPECIES) {
        if s.accepts(biome, moisture, temperature_c, surface_material, depth) {
            fits[n] = i;
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    let s = &set[fits[(key >> 8) as usize % n]];
    // Wetter ground carries more cover and taller cover: a species' density
    // is for average wetness, dry ground gets under half of it and the wettest
    // gets more. (Wetter ground also admits more species: see their ranges.)
    let wet = f32::from(moisture) / 255.0;
    let gate = (f32::from(s.density) * (0.35 + 1.0 * wet)).min(255.0);
    if ((key >> 24) & 255) as f32 >= gate {
        return None;
    }
    let size = (0.7 + 0.6 * ((key >> 40) & 255) as f32 / 255.0) * (0.85 + 0.3 * wet);
    let winter_shrink = s.form == 11 && season == Season::Winter && !s.evergreen;
    Some(Cover {
        species: s.id,
        form: s.form,
        colour: s.colours[season as usize],
        height_m: f32::from(s.height) * 0.25 * size * if winter_shrink { 0.25 } else { 1.0 },
        evergreen: s.evergreen,
        key,
    })
}

/// The world's climate at 4 m resolution, as carried on the wire: biome,
/// moisture and temperature planes, run-length coded. Moisture is kept in
/// steps of 8 and temperature in steps of 2 degrees, which is finer than any
/// cover species cares about and makes the planes long runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverField {
    pub width: u32,
    pub rle: Vec<u8>,
}

/// The raster expanded for lookups.
#[derive(Debug, Clone)]
pub struct CoverGrid {
    width: usize,
    biome: Vec<u8>,
    moisture: Vec<u8>,
    temperature: Vec<u8>,
    water: Vec<u8>,
}

fn rle_push(out: &mut Vec<u8>, plane: &[u8]) {
    let mut i = 0;
    while i < plane.len() {
        let v = plane[i];
        let mut run = 1;
        while i + run < plane.len() && plane[i + run] == v && run < 255 {
            run += 1;
        }
        out.push(run as u8);
        out.push(v);
        i += run;
    }
}

impl CoverField {
    pub fn is_empty(&self) -> bool {
        self.width == 0
    }

    /// Samples a world's climate. One sample per [`FIELD_CELLS`] cells, taken
    /// at the middle of the 4 m square.
    pub fn from_columns(columns: &ColumnMap) -> Self {
        let size = i64::from(columns.size());
        let width = ((size + FIELD_CELLS - 1) / FIELD_CELLS) as usize;
        let (mut biome, mut moisture, mut temperature, mut water) = (
            Vec::with_capacity(width * width),
            Vec::with_capacity(width * width),
            Vec::with_capacity(width * width),
            Vec::with_capacity(width * width),
        );
        for gz in 0..width as i64 {
            for gx in 0..width as i64 {
                let x = (gx * FIELD_CELLS + FIELD_CELLS / 2).min(size - 1);
                let z = (gz * FIELD_CELLS + FIELD_CELLS / 2).min(size - 1);
                // The highest water level among nine samples, in steps of two
                // cells (0: none), so a river narrower than a square still shows.
                let mut level = 0;
                for (dx, dz) in [
                    (0, 0),
                    (-5, -5),
                    (0, -5),
                    (5, -5),
                    (-5, 0),
                    (5, 0),
                    (-5, 5),
                    (0, 5),
                    (5, 5),
                ] {
                    let (sx, sz) = ((x + dx).clamp(0, size - 1), (z + dz).clamp(0, size - 1));
                    if columns.is_water(sx, sz) {
                        level = level.max(i64::from(columns.water_surface(sx, sz)) / 2 + 1);
                    }
                }
                water.push(level.clamp(0, 255) as u8);
                biome.push(columns.biome(x, z) as u8);
                moisture.push(columns.moisture(x, z) >> 3);
                temperature.push(
                    ((columns.temperature_c(x, z).round().clamp(-62.0, 62.0) as i32 + 64) / 2)
                        as u8,
                );
            }
        }
        let mut rle = Vec::new();
        rle_push(&mut rle, &biome);
        rle_push(&mut rle, &moisture);
        rle_push(&mut rle, &temperature);
        rle_push(&mut rle, &water);
        Self {
            width: width as u32,
            rle,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.width > MAX_FIELD_WIDTH
            || self.rle.len() > 4 * 2 * (MAX_FIELD_WIDTH as usize).pow(2)
        {
            return Err("cover field limits".into());
        }
        Ok(())
    }

    pub fn expand(&self) -> Result<CoverGrid, String> {
        self.validate()?;
        let width = self.width as usize;
        let cells = width * width;
        let mut planes: [Vec<u8>; 4] = std::array::from_fn(|_| Vec::with_capacity(cells));
        let mut pairs = self.rle.chunks_exact(2);
        for plane in &mut planes {
            while plane.len() < cells {
                let Some(pair) = pairs.next() else {
                    return Err("cover field truncated".into());
                };
                if pair[0] == 0 || plane.len() + usize::from(pair[0]) > cells {
                    return Err("cover field run".into());
                }
                plane.extend(std::iter::repeat_n(pair[1], usize::from(pair[0])));
            }
        }
        if pairs.next().is_some() || !self.rle.len().is_multiple_of(2) {
            return Err("cover field trailing bytes".into());
        }
        let [biome, moisture, temperature, water] = planes;
        if biome.iter().any(|b| usize::from(*b) >= Biome::EVERY.len()) {
            return Err("cover field biome".into());
        }
        Ok(CoverGrid {
            width,
            biome,
            moisture,
            temperature,
            water,
        })
    }
}

impl CoverGrid {
    pub fn width(&self) -> usize {
        self.width
    }

    /// The water level near cell `(x, z)` as the `y` of its top water cell, or
    /// 0 where the raster shows no water.
    pub fn water_at(&self, x: i64, z: i64) -> i64 {
        let last = self.width as i64 - 1;
        let i = (z.div_euclid(FIELD_CELLS).clamp(0, last) as usize) * self.width
            + x.div_euclid(FIELD_CELLS).clamp(0, last) as usize;
        match self.water[i] {
            0 => 0,
            v => (i64::from(v) - 1) * 2,
        }
    }

    /// The climate at cell `(x, z)`: biome, moisture 0..=255, temperature C.
    pub fn at(&self, x: i64, z: i64) -> (Biome, u8, i8) {
        let last = self.width as i64 - 1;
        let i = (z.div_euclid(FIELD_CELLS).clamp(0, last) as usize) * self.width
            + x.div_euclid(FIELD_CELLS).clamp(0, last) as usize;
        (
            Biome::EVERY[usize::from(self.biome[i])],
            (self.moisture[i] << 3) | 4,
            (i32::from(self.temperature[i]) * 2 - 64) as i8,
        )
    }
}

/// How finely a tuft is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// Seven blades (thirteen for a cushion), or the full rosette.
    Full,
    /// Three wider blades: one stands in for a few tufts.
    Coarse,
}

/// The soft parts of one ground plant: blades, a rosette of leaves, or a
/// cushion. `base` is the middle of the cell at the top of the soil. Shared
/// by the living ground plants and by stateless cover, so both draw alike.
#[allow(clippy::too_many_arguments)]
pub fn ground_parts(
    form: u8,
    height: f32,
    colour: u8,
    flower: u8,
    season: Season,
    key: u64,
    base: [f32; 3],
    detail: Detail,
    out: &mut Vec<SoftPart>,
) {
    if matches!(form, 8 | 11 | 13) {
        let count = if form == 13 {
            9
        } else if detail == Detail::Coarse {
            4
        } else {
            6
        };
        for leaf in 0..count {
            let angle = leaf as f32 * std::f32::consts::TAU / count as f32;
            let reach = height
                * if form == 8 {
                    0.65
                } else if form == 13 {
                    0.40
                } else {
                    0.75
                };
            let level = if form == 13 {
                height * (0.3 + (leaf % 3) as f32 * 0.23)
            } else {
                height * 0.3
            };
            let stem_colour = if form == 13 { 7 } else { colour };
            out.push(SoftPart {
                center: [base[0], base[1] + level / 2., base[2]],
                size: [0.02, level, 0.02],
                colour: stem_colour,
                yaw: angle,
                lean: 0.,
            });
            out.push(SoftPart {
                center: [
                    base[0] + angle.cos() * reach / 2.,
                    base[1] + level,
                    base[2] + angle.sin() * reach / 2.,
                ],
                size: [reach, 0.02, 0.02],
                colour: stem_colour,
                yaw: -angle,
                lean: 0.,
            });
            for segment in 1..=3 {
                let distance = reach * segment as f32 / 3.;
                let y = base[1]
                    + level
                    + if form == 8 {
                        height * 0.12 * (3 - segment) as f32
                    } else {
                        0.
                    };
                let size = if form == 8 {
                    [0.13 * (4 - segment) as f32, 0.025, 0.10]
                } else {
                    [0.10, 0.04, 0.12]
                };
                out.push(SoftPart {
                    center: [
                        base[0] + angle.cos() * distance,
                        y,
                        base[2] + angle.sin() * distance,
                    ],
                    size,
                    colour,
                    yaw: angle,
                    lean: 0.,
                });
            }
        }
        return;
    }
    if matches!(form, 14..=17) {
        aquatic_parts(form, height, colour, flower, key, base, detail, out);
        return;
    }
    let blades = match (detail, form) {
        (Detail::Coarse, 12) => 5,
        (Detail::Coarse, _) => 3,
        (Detail::Full, 12) => 13,
        (Detail::Full, _) => 7_u64,
    };
    let wide = if detail == Detail::Coarse {
        0.045
    } else {
        0.025
    };
    for blade in 0..blades {
        let k = hash(key ^ blade);
        let yaw = (k % 628) as f32 / 100.;
        let dx = ((k % 101) as f32 / 100. - 0.5) * 0.20;
        let dz = ((k.rotate_left(23) % 101) as f32 / 100. - 0.5) * 0.20;
        let h = (height * (0.65 + (k % 35) as f32 / 100.)).max(0.025);
        let lean = if form == 7 {
            0.04
        } else {
            0.12 + (k % 25) as f32 / 100.
        };
        out.push(SoftPart {
            center: [
                base[0] + dx - h * lean.sin() * yaw.cos() / 2.,
                base[1] + (h * lean.cos() + wide * lean.sin()) / 2.,
                base[2] + dz + h * lean.sin() * yaw.sin() / 2.,
            ],
            size: [wide, h, wide],
            colour,
            yaw,
            lean,
        });
        if form == 7 || (form == 10 && matches!(season, Season::Spring | Season::Summer)) {
            out.push(SoftPart {
                center: [
                    base[0] + dx - h * lean.sin() * yaw.cos(),
                    base[1] + h * lean.cos(),
                    base[2] + dz + h * lean.sin() * yaw.sin(),
                ],
                size: if form == 7 {
                    [0.055, 0.18, 0.055]
                } else {
                    [0.08, 0.045, 0.08]
                },
                colour: if form == 7 { 7 } else { flower },
                yaw,
                lean: 0.,
            });
        }
    }
}

/// A tilted thin box standing on `base`, offset by `(dx, dz)`, leaning `lean`
/// radians towards its yaw: one blade, frond or coral branch.
#[allow(clippy::too_many_arguments)]
fn lean_part(
    base: [f32; 3],
    offset: [f32; 2],
    h: f32,
    lean: f32,
    yaw: f32,
    size_xz: [f32; 2],
    colour: u8,
    out: &mut Vec<SoftPart>,
) {
    out.push(SoftPart {
        center: [
            base[0] + offset[0] - h * lean.sin() * yaw.cos() / 2.,
            base[1] + (h * lean.cos() + size_xz[0] * lean.sin()) / 2.,
            base[2] + offset[1] + h * lean.sin() * yaw.sin() / 2.,
        ],
        size: [size_xz[0], h, size_xz[1]],
        colour,
        yaw,
        lean,
    });
}

/// Underwater growth. Forms: 14 kelp (tall straight fronds with float pods),
/// 15 branching coral, 16 sea fan (spokes in one plane), 17 barrel sponge.
/// `colour` is the body, `flower` the tips, pods or rim.
#[allow(clippy::too_many_arguments)]
fn aquatic_parts(
    form: u8,
    height: f32,
    colour: u8,
    tip: u8,
    key: u64,
    base: [f32; 3],
    detail: Detail,
    out: &mut Vec<SoftPart>,
) {
    let full = detail == Detail::Full;
    let frac = |k: u64, n: u64| (k % n) as f32 / n as f32;
    match form {
        14 => {
            for i in 0..if full { 5_u64 } else { 2 } {
                let k = hash(key ^ i);
                let off = [
                    (frac(k, 101) - 0.5) * 0.24,
                    (frac(k >> 9, 101) - 0.5) * 0.24,
                ];
                let h = height * (0.75 + 0.5 * frac(k >> 17, 50));
                let lean = 0.03 + 0.12 * frac(k >> 25, 30);
                let yaw = frac(k >> 33, 628) * std::f32::consts::TAU;
                lean_part(base, off, h, lean, yaw, [0.05, 0.03], colour, out);
                // A float pod near the top of the frond.
                out.push(SoftPart {
                    center: [
                        base[0] + off[0] - h * 0.85 * lean.sin() * yaw.cos(),
                        base[1] + h * 0.85 * lean.cos(),
                        base[2] + off[1] + h * 0.85 * lean.sin() * yaw.sin(),
                    ],
                    size: [0.07, 0.07, 0.07],
                    colour: tip,
                    yaw,
                    lean: 0.,
                });
            }
        }
        15 => {
            lean_part(
                base,
                [0., 0.],
                height * 0.4,
                0.,
                0.,
                [0.08, 0.08],
                colour,
                out,
            );
            let count = if full { 7_u64 } else { 4 };
            for i in 0..count {
                let k = hash(key ^ (i + 40));
                let yaw = i as f32 * std::f32::consts::TAU / count as f32;
                let lean = 0.45 + 0.35 * frac(k, 40);
                let h = height * (0.55 + 0.4 * frac(k >> 11, 40));
                let off = [0., 0.];
                let b = [base[0], base[1] + height * 0.25, base[2]];
                lean_part(b, off, h, lean, yaw, [0.055, 0.055], colour, out);
                out.push(SoftPart {
                    center: [
                        b[0] - h * lean.sin() * yaw.cos(),
                        b[1] + h * lean.cos(),
                        b[2] + h * lean.sin() * yaw.sin(),
                    ],
                    size: [0.08, 0.05, 0.08],
                    colour: tip,
                    yaw,
                    lean: 0.,
                });
            }
        }
        16 => {
            let yaw = frac(key, 628) * std::f32::consts::TAU;
            let spokes: i32 = if full { 9 } else { 5 };
            for i in 0..spokes {
                let lean = (i - spokes / 2) as f32 * (1.7 / spokes as f32);
                let h = height * (0.9 - 0.12 * (lean.abs() / 0.85));
                lean_part(base, [0., 0.], h, lean, yaw, [0.035, 0.012], colour, out);
            }
            // The rim joining the spokes' tips.
            out.push(SoftPart {
                center: [base[0], base[1] + height * 0.85, base[2]],
                size: [height * 1.1, 0.02, 0.02],
                colour: tip,
                yaw: yaw + std::f32::consts::FRAC_PI_2,
                lean: 0.,
            });
        }
        _ => {
            let s = height / 0.3;
            for (w, h, y) in [(0.16, 0.12, 0.06), (0.21, 0.15, 0.19), (0.18, 0.1, 0.31)] {
                out.push(SoftPart {
                    center: [base[0], base[1] + y * s, base[2]],
                    size: [w * s.sqrt(), h * s, w * s.sqrt()],
                    colour,
                    yaw: frac(key, 628) * std::f32::consts::TAU,
                    lean: 0.,
                });
            }
            out.push(SoftPart {
                center: [base[0], base[1] + 0.37 * s, base[2]],
                size: [0.14 * s.sqrt(), 0.03, 0.14 * s.sqrt()],
                colour: tip,
                yaw: 0.,
                lean: 0.,
            });
        }
    }
}

/// What the renderer finds at a column: the first open cell above the soil
/// and the soil's material. `None` where there is no open ground (water,
/// cliffs, unloaded terrain).
pub type SurfaceLookup<'a> = dyn FnMut(i64, i64) -> Option<(i64, u16)> + 'a;

/// All the cover within `radius_m` of `eye`, nearest first, as soft parts.
///
/// Within [`NEAR_M`] every cell is considered and a tuft is drawn in full;
/// out to [`MID_M`] a quarter of the cells are considered and each draws a
/// coarse, taller-scaled tuft, so the carpet thins with distance instead of
/// stopping at an edge. The result never exceeds `budget` parts: when the
/// budget runs out the farthest cover is the part that goes missing.
#[allow(clippy::too_many_arguments)]
pub fn cover_parts(
    grid: &CoverGrid,
    set: &[CoverSpecies],
    seed: u64,
    season: Season,
    eye: [f32; 3],
    radius_m: f32,
    surface: &mut SurfaceLookup<'_>,
    memory: &CoverMemory,
    budget: usize,
) -> Vec<SoftPart> {
    let radius = radius_m.min(MID_M);
    let (ex, ez) = ((eye[0] * 4.0).floor() as i64, (eye[2] * 4.0).floor() as i64);
    let reach = (radius * 4.0).ceil() as i64;
    let mut out = Vec::new();
    // Two passes keep the near carpet whole when the budget is tight.
    for pass in [Detail::Full, Detail::Coarse] {
        for z in ez - reach..=ez + reach {
            for x in ex - reach..=ex + reach {
                if out.len() >= budget {
                    return out;
                }
                let d2 = ((x - ex).pow(2) + (z - ez).pow(2)) as f32 * 0.0625;
                let near = d2 <= NEAR_M * NEAR_M;
                if d2 > radius * radius || (pass == Detail::Full) != near {
                    continue;
                }
                let thin =
                    hash(seed ^ (x as u64).rotate_left(7) ^ (z as u64).rotate_left(33) ^ 0xFA7);
                if !near && thin & 3 != 0 {
                    continue;
                }
                // Destroyed cover stays away until it sprouts, then grows.
                let Some(growth) = memory.growth_at(x, z) else {
                    continue;
                };
                let Some((y, soil)) = surface(x, z) else {
                    continue;
                };
                // Sample the raster at a jittered spot so its 4 m squares
                // blur into organic edges rather than showing as a grid.
                let jx = (thin >> 8) as i64 % 9 - 4;
                let jz = (thin >> 20) as i64 % 9 - 4;
                let (biome, moisture, temperature) = grid.at(x + jx, z + jz);
                // Water standing over this cell, from the raster's level.
                let level = grid.water_at(x + jx, z + jz);
                let depth = if level >= y { level - y + 1 } else { 0 }.min(255) as u8;
                let Some(c) = ground_cover(
                    set,
                    seed,
                    [x, y, z],
                    soil,
                    moisture,
                    temperature,
                    biome,
                    season,
                    depth,
                ) else {
                    continue;
                };
                let base = [
                    (x as f32 + 0.5) * 0.25,
                    y as f32 * 0.25,
                    (z as f32 + 0.5) * 0.25,
                ];
                let (height, detail) = if near {
                    (c.height_m * growth, Detail::Full)
                } else {
                    (c.height_m * 1.15 * growth, Detail::Coarse)
                };
                ground_parts(
                    c.form, height, c.colour, 0, season, c.key, base, detail, &mut out,
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> Vec<CoverSpecies> {
        vec![
            CoverSpecies {
                id: 11,
                biomes: 1,
                soils: [1, 2, 0, 0],
                moisture: [50, 235],
                temperature: [-5, 40],
                form: 6,
                height: 2,
                colours: [3, 1, 7, 8],
                evergreen: true,
                density: 128,
                depth: [0, 0],
            },
            CoverSpecies {
                id: 19,
                biomes: 8,
                soils: [3, 0, 0, 0],
                moisture: [0, 65],
                temperature: [0, 60],
                form: 6,
                height: 3,
                colours: [15, 15, 7, 8],
                evergreen: true,
                density: 255,
                depth: [0, 0],
            },
        ]
    }

    #[test]
    fn cover_is_a_pure_function_of_its_inputs() {
        let s = set();
        let at = |x, z| {
            ground_cover(
                &s,
                7,
                [x, 10, z],
                1,
                120,
                15,
                Biome::Meadow,
                Season::Summer,
                0,
            )
        };
        let first: Vec<_> = (0..200).map(|i| at(i, i * 3)).collect();
        let again: Vec<_> = (0..200).map(|i| at(i, i * 3)).collect();
        assert_eq!(first, again);
        // Roughly the species' density, and not a constant pattern.
        let tufts = first.iter().flatten().count();
        assert!((60..140).contains(&tufts), "{tufts} tufts of 200 cells");
        let other_seed = (0..200)
            .map(|i| {
                ground_cover(
                    &s,
                    8,
                    [i, 10, i * 3],
                    1,
                    120,
                    15,
                    Biome::Meadow,
                    Season::Summer,
                    0,
                )
            })
            .collect::<Vec<_>>();
        assert_ne!(first, other_seed, "the seed changes the carpet");
    }

    #[test]
    fn cover_respects_soil_biome_moisture_and_temperature() {
        let s = set();
        let all = |soil, moisture, temp, biome| {
            (0..400)
                .filter_map(|i| {
                    ground_cover(
                        &s,
                        1,
                        [i, 4, i * 7],
                        soil,
                        moisture,
                        temp,
                        biome,
                        Season::Summer,
                        0,
                    )
                })
                .count()
        };
        assert!(all(1, 120, 15, Biome::Meadow) > 0);
        assert_eq!(all(9, 120, 15, Biome::Meadow), 0, "wrong soil");
        assert_eq!(all(1, 10, 15, Biome::Meadow), 0, "too dry");
        assert_eq!(all(1, 120, -30, Biome::Meadow), 0, "too cold");
        assert_eq!(all(1, 120, 15, Biome::Ocean), 0, "no cover at sea");
        assert_eq!(all(1, 120, 15, Biome::Snow), 0, "no cover on snow");
        // The desert bunchgrass takes the desert class on sand and nothing else.
        assert!(all(3, 30, 25, Biome::Desert) > 150);
    }

    #[test]
    fn wetter_ground_carries_more_cover_and_more_kinds() {
        // Two species that need different wetness: a dry turf and a wet sedge.
        let mut s = set();
        s[0].moisture = [0, 140];
        s.push(CoverSpecies {
            id: 14,
            biomes: 1,
            soils: [1, 0, 0, 0],
            moisture: [120, 255],
            temperature: [-5, 40],
            form: 9,
            height: 4,
            colours: [3, 1, 7, 8],
            evergreen: true,
            density: 128,
            depth: [0, 0],
        });
        let at = |moisture: u8| {
            let mut tufts = 0;
            let mut kinds = std::collections::BTreeSet::new();
            for i in 0..4000 {
                if let Some(c) = ground_cover(
                    &s,
                    5,
                    [i, 4, i * 3],
                    1,
                    moisture,
                    15,
                    Biome::Meadow,
                    Season::Summer,
                    0,
                ) {
                    tufts += 1;
                    kinds.insert(c.species);
                }
            }
            (tufts, kinds.len())
        };
        let (dry, dry_kinds) = at(40);
        let (mid, mid_kinds) = at(130);
        let (wet, wet_kinds) = at(240);
        assert!(dry < mid && mid < wet, "{dry} < {mid} < {wet}");
        assert_eq!(
            (dry_kinds, mid_kinds, wet_kinds),
            (1, 2, 1),
            "kinds follow each range"
        );
    }

    #[test]
    fn cover_height_and_colour_follow_the_season() {
        let s = set();
        let at = |season| {
            (0..400)
                .find_map(|i| {
                    ground_cover(&s, 1, [i, 4, 2 * i], 1, 120, 15, Biome::Meadow, season, 0)
                })
                .unwrap()
        };
        assert_eq!(at(Season::Summer).colour, 1);
        assert_eq!(at(Season::Autumn).colour, 7);
        assert!(at(Season::Summer).height_m > 0.1 && at(Season::Summer).height_m < 0.9);
    }

    fn grid(width: u32, biome: u8, moisture_q: u8, temp_q: u8) -> CoverField {
        let cells = (width * width) as usize;
        let mut rle = Vec::new();
        rle_push(&mut rle, &vec![biome; cells]);
        rle_push(&mut rle, &vec![moisture_q; cells]);
        rle_push(&mut rle, &vec![temp_q; cells]);
        rle_push(&mut rle, &vec![0; cells]);
        CoverField { width, rle }
    }

    #[test]
    fn the_field_round_trips_and_rejects_corruption() {
        let f = grid(20, Biome::Desert as u8, 5, 40);
        let g = f.expand().unwrap();
        assert_eq!(g.width(), 20);
        let (b, m, t) = g.at(100, 100);
        assert_eq!(b, Biome::Desert);
        assert_eq!(m, (5 << 3) | 4);
        assert_eq!(t, 16);
        // Outside the raster clamps to the nearest sample.
        assert_eq!(g.at(-50, 9_999).0, Biome::Desert);
        let mut bad = f.clone();
        bad.rle.pop();
        assert!(bad.expand().is_err());
        let mut bad = f.clone();
        bad.rle[0] = 0;
        assert!(bad.expand().is_err());
        let mut bad = f.clone();
        bad.rle[1] = 200;
        assert!(bad.expand().is_err(), "biome number out of range");
        let mut bad = f;
        bad.rle.extend([1, 1]);
        assert!(bad.expand().is_err(), "trailing bytes");
        assert!(
            CoverField {
                width: MAX_FIELD_WIDTH + 1,
                rle: vec![]
            }
            .expand()
            .is_err()
        );
    }

    #[test]
    fn destroyed_cover_sprouts_then_grows_through_small_medium_and_full() {
        assert_eq!(regrowth(0), None);
        assert_eq!(regrowth(SMALL_AFTER_S - 1), None);
        assert_eq!(regrowth(SMALL_AFTER_S), Some(0.3));
        assert_eq!(regrowth(MEDIUM_AFTER_S - 1), Some(0.3));
        assert_eq!(regrowth(MEDIUM_AFTER_S), Some(0.65));
        assert_eq!(regrowth(FULL_AFTER_S - 1), Some(0.65));
        assert_eq!(regrowth(FULL_AFTER_S), Some(1.0));
        let cleared = [Cleared {
            x: 5,
            z: 6,
            at_s: 100,
        }];
        let at = |now| CoverMemory::new(&cleared, now);
        assert_eq!(at(110).growth_at(5, 6), None);
        assert_eq!(at(125).growth_at(5, 6), Some(0.3));
        assert_eq!(at(200).growth_at(5, 6), Some(0.65));
        assert_eq!(at(300).growth_at(5, 6), Some(1.0));
        assert_eq!(
            at(110).growth_at(5, 7),
            Some(1.0),
            "other columns are untouched"
        );
        assert!(at(110).is_recorded(5, 6) && !at(110).is_recorded(6, 5));
    }

    #[test]
    fn a_cleared_column_draws_nothing_then_small_then_medium_then_full_tufts() {
        let s = set();
        let g = grid(64, Biome::Meadow as u8, 15, 38).expand().unwrap();
        let eye = [128.0, 10.0, 128.0];
        let mut flat = |_x: i64, _z: i64| Some((8_i64, 1_u16));
        // Every column within 6 m of the eye is destroyed at t = 1000 s.
        let (ex, ez) = (512, 512);
        let cleared: Vec<Cleared> = (ex - 24..=ex + 24)
            .flat_map(|x| {
                (ez - 24..=ez + 24).map(move |z| Cleared {
                    x: x as u16,
                    z: z as u16,
                    at_s: 1000,
                })
            })
            .collect();
        let inside = |p: &SoftPart| {
            let (x, z) = (
                (p.center[0] * 4.0).floor() as i64,
                (p.center[2] * 4.0).floor() as i64,
            );
            // A margin: blades lean out of the neighbouring columns a little.
            (ex - 20..=ex + 20).contains(&x) && (ez - 20..=ez + 20).contains(&z)
        };
        let top = |parts: &[SoftPart]| {
            parts
                .iter()
                .filter(|p| inside(p))
                .map(|p| p.size[1])
                .fold(0.0f32, f32::max)
        };
        let mut run = |now| {
            cover_parts(
                &g,
                &s,
                3,
                Season::Summer,
                eye,
                40.0,
                &mut flat,
                &CoverMemory::new(&cleared, now),
                INSTANCE_BUDGET,
            )
        };
        let fresh = run(1005);
        assert_eq!(
            fresh.iter().filter(|p| inside(p)).count(),
            0,
            "bare while destroyed"
        );
        assert!(
            fresh.iter().any(|p| !inside(p)),
            "the rest of the carpet is untouched"
        );
        let (small, medium, full) = (
            run(1000 + SMALL_AFTER_S),
            run(1000 + MEDIUM_AFTER_S),
            run(1000 + FULL_AFTER_S),
        );
        assert!(small.iter().any(inside) && top(&small) > 0.0);
        assert!(
            top(&small) < top(&medium) && top(&medium) < top(&full),
            "{} {} {}",
            top(&small),
            top(&medium),
            top(&full)
        );
    }

    #[test]
    fn land_cover_stays_dry_and_underwater_cover_needs_its_depth() {
        let mut s = set();
        s.push(CoverSpecies {
            id: 80,
            biomes: 1 << Biome::Ocean as u16,
            soils: [1, 0, 0, 0],
            moisture: [0, 255],
            temperature: [-20, 62],
            form: 6,
            height: 2,
            colours: [20, 20, 20, 20],
            evergreen: true,
            density: 255,
            depth: [2, 30],
        });
        let at = |biome, depth| {
            (0..300)
                .filter_map(|i| {
                    ground_cover(
                        &s,
                        4,
                        [i, 4, i * 5],
                        1,
                        120,
                        15,
                        biome,
                        Season::Summer,
                        depth,
                    )
                })
                .map(|c| c.species)
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert!(
            at(Biome::Meadow, 0).contains(&11),
            "land cover on dry ground"
        );
        assert!(
            at(Biome::Meadow, 1).is_empty(),
            "never in water, even a film"
        );
        assert!(at(Biome::Ocean, 0).is_empty(), "no seagrass on land");
        assert!(at(Biome::Ocean, 1).is_empty(), "too shallow for seagrass");
        assert_eq!(at(Biome::Ocean, 8), [80].into(), "seagrass in the sea");
        assert!(at(Biome::Ocean, 31).is_empty(), "too deep");
    }

    #[test]
    fn cover_under_a_water_level_in_the_raster_is_drawn_only_where_it_is_deep_enough() {
        let mut s = set();
        s.push(CoverSpecies {
            id: 80,
            biomes: 1 << Biome::Ocean as u16,
            soils: [1, 0, 0, 0],
            moisture: [0, 255],
            temperature: [-20, 62],
            form: 6,
            height: 2,
            colours: [20, 20, 20, 20],
            evergreen: true,
            density: 255,
            depth: [2, 60],
        });
        // An ocean raster whose water level is the top cell at y = 20.
        let (width, cells) = (64u32, 64usize * 64);
        let mut rle = Vec::new();
        rle_push(&mut rle, &vec![Biome::Ocean as u8; cells]);
        rle_push(&mut rle, &vec![15; cells]);
        rle_push(&mut rle, &vec![38; cells]);
        rle_push(&mut rle, &vec![11; cells]);
        let g = CoverField { width, rle }.expand().unwrap();
        assert_eq!(g.water_at(100, 100), 20);
        let eye = [128.0, 8.0, 128.0];
        let run = |floor: i64| {
            let mut surface = |_x: i64, _z: i64| Some((floor, 1_u16));
            cover_parts(
                &g,
                &s,
                3,
                Season::Summer,
                eye,
                30.0,
                &mut surface,
                &CoverMemory::default(),
                5_000,
            )
        };
        assert!(!run(10).is_empty(), "10 cells down is deep enough");
        assert!(run(20).is_empty(), "a single cell of water is too shallow");
        assert!(
            run(25).is_empty(),
            "above the water level it is dry, and land cover needs the land biome"
        );
    }

    #[test]
    fn every_underwater_form_draws_and_the_coarse_form_draws_less() {
        for form in 14..=17 {
            let draw = |detail| {
                let mut out = Vec::new();
                ground_parts(
                    form,
                    0.4,
                    20,
                    18,
                    Season::Summer,
                    77,
                    [1.0, 2.0, 3.0],
                    detail,
                    &mut out,
                );
                out
            };
            let (full, coarse) = (draw(Detail::Full), draw(Detail::Coarse));
            assert!(!full.is_empty(), "form {form}");
            assert!(coarse.len() <= full.len(), "form {form}");
            for p in &full {
                assert!(p.center.iter().all(|v| v.is_finite()) && p.size.iter().all(|v| *v > 0.0));
                // Grows up from the seabed it stands on, not down through it.
                assert!(p.center[1] >= 2.0 - 0.1, "form {form} dips below the floor");
            }
            assert_eq!(draw(Detail::Full).len(), full.len(), "deterministic");
        }
    }

    #[test]
    fn parts_thin_with_distance_and_stay_inside_the_budget() {
        let s = set();
        let g = grid(64, Biome::Meadow as u8, 15, 38).expand().unwrap();
        let mut flat = |_x: i64, _z: i64| Some((8_i64, 1_u16));
        let eye = [128.0, 10.0, 128.0];
        let parts = cover_parts(
            &g,
            &s,
            3,
            Season::Summer,
            eye,
            60.0,
            &mut flat,
            &CoverMemory::default(),
            INSTANCE_BUDGET,
        );
        assert!(!parts.is_empty());
        assert!(parts.len() <= INSTANCE_BUDGET + 40, "{}", parts.len());
        // Same call, same parts: nothing is remembered between rebuilds.
        let again = cover_parts(
            &g,
            &s,
            3,
            Season::Summer,
            eye,
            60.0,
            &mut flat,
            &CoverMemory::default(),
            INSTANCE_BUDGET,
        );
        assert_eq!(parts.len(), again.len());
        let dist =
            |p: &SoftPart| ((p.center[0] - eye[0]).powi(2) + (p.center[2] - eye[2]).powi(2)).sqrt();
        let near = parts.iter().filter(|p| dist(p) < NEAR_M).count();
        let mid = parts
            .iter()
            .filter(|p| (NEAR_M..=MID_M + 1.0).contains(&dist(p)))
            .count();
        assert!(near > 0 && mid > 0);
        assert!(
            parts.iter().all(|p| dist(p) <= MID_M + 1.0),
            "nothing beyond the mid ring"
        );
        // The ring beyond the near one is far bigger in area; per unit area it
        // carries far fewer parts.
        let near_area = std::f32::consts::PI * NEAR_M * NEAR_M;
        let mid_area = std::f32::consts::PI * (MID_M * MID_M - NEAR_M * NEAR_M);
        assert!(mid as f32 / mid_area < 0.5 * near as f32 / near_area);
        // A tight budget keeps the near carpet whole and drops the far cover.
        let few = cover_parts(
            &g,
            &s,
            3,
            Season::Summer,
            eye,
            60.0,
            &mut flat,
            &CoverMemory::default(),
            2_000,
        );
        assert!(few.len() <= 2_040);
        assert!(few.iter().all(|p| dist(p) < NEAR_M + 1.0));
        // No ground, no cover.
        let mut none = |_x: i64, _z: i64| None;
        assert!(
            cover_parts(
                &g,
                &s,
                3,
                Season::Summer,
                eye,
                60.0,
                &mut none,
                &CoverMemory::default(),
                1000
            )
            .is_empty()
        );
    }
}

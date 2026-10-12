//! The 2D half of generation: per-column height, biome and surface stack.
//!
//! Cheap enough to compute for the full arena in tests and for editor
//! previews, and it fixes everything the 3D fill needs to know. Coordinates
//! are cells; `u = x / size`, `v = z / size` are the preset's normalized
//! layout coordinates, so one layout scales to any arena size.

mod gallery;
mod natural;

pub use gallery::{GalleryPatch, patches as gallery_patches};

use crate::noise::{fbm2, ridged2, smoothstep};
use crate::spec::{BORDER_CELLS, GenError, Preset, SEA_LEVEL, WorldGenSpec, WorldgenPalette};
use spall_core::MaterialId;

/// Above this many cells over the sea, mountain surfaces are snow.
pub const SNOW_LINE: i32 = SEA_LEVEL + 105;

// Layout of the showcase, in normalized arena coordinates. Mountains fill the
// north, the desert the south-east, and a circular swamp basin holding the lake
// and river the south-west; hills cover the rest. The basin is the only place
// terrain dips below sea level, which keeps every water cell inside one
// bounded domain.
const BASIN_CENTER: (f64, f64) = (0.22, 0.80);
const BASIN_RADIUS: f64 = 0.15;
const LAKE_RADIUS: f64 = 0.055;
const LAKE_DEPTH: f64 = 28.0;
const RIVER_END: (f64, f64) = (0.13, 0.88);
const RIVER_HALF_WIDTH: f64 = 11.0;
const RIVER_DEPTH: f64 = 12.0;
const RIVER_SEGMENTS: usize = 24;

// Noise stream ids. Changing one changes the world: bump GEN_VERSION.
const S_WARP_U: u64 = 0x11;
const S_WARP_V: u64 = 0x12;
const S_HILLS: u64 = 0x21;
const S_HILL_DETAIL: u64 = 0x22;
const S_RIDGE: u64 = 0x31;
const S_MOUNT_DETAIL: u64 = 0x32;
const S_DUNES: u64 = 0x41;
const S_DUNE_DETAIL: u64 = 0x42;
const S_SWAMP: u64 = 0x51;
const S_POOLS: u64 = 0x52;
const S_RIVER: u64 = 0x53;
const S_SLATE: u64 = 0x61;
const S_RIM: u64 = 0x54;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Biome {
    // The four biomes of the showcase. Their numbers are stored in saves and
    // in vegetation masks: never renumber them.
    /// Showcase hills; grassland in natural worlds.
    Meadow = 0,
    /// Showcase mountains; alpine meadow in natural worlds.
    Alpine = 1,
    /// Showcase basin; wetland in natural worlds.
    Swamp = 2,
    Desert = 3,
    // Natural worlds only.
    Ocean = 4,
    Beach = 5,
    /// Banks of rivers and lakes.
    Riparian = 6,
    TemperateForest = 7,
    BorealForest = 8,
    /// Bare broken rock and gravel on steep slopes and above the tree line.
    Scree = 9,
    Snow = 10,
    Steppe = 11,
}

impl Biome {
    /// The biomes the showcase uses.
    pub const ALL: [Biome; 4] = [Biome::Meadow, Biome::Alpine, Biome::Swamp, Biome::Desert];
    /// Every biome, indexed by its stored number.
    pub const EVERY: [Biome; 12] = [
        Biome::Meadow,
        Biome::Alpine,
        Biome::Swamp,
        Biome::Desert,
        Biome::Ocean,
        Biome::Beach,
        Biome::Riparian,
        Biome::TemperateForest,
        Biome::BorealForest,
        Biome::Scree,
        Biome::Snow,
        Biome::Steppe,
    ];

    fn from_u8(v: u8) -> Self {
        Self::EVERY[usize::from(v)]
    }

    /// The showcase biome whose vegetation fits this one, until vegetation
    /// understands every biome.
    pub fn vegetation_class(self) -> Biome {
        match self {
            Biome::Meadow | Biome::TemperateForest | Biome::Steppe => Biome::Meadow,
            Biome::Alpine | Biome::BorealForest | Biome::Scree | Biome::Snow => Biome::Alpine,
            Biome::Swamp | Biome::Riparian => Biome::Swamp,
            Biome::Desert | Biome::Beach | Biome::Ocean => Biome::Desert,
        }
    }
}

/// Materials of the top of a column: `top` for `top_cells` cells below the
/// surface, then `sub` for `sub_cells`, then the rock bands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stack {
    pub top: MaterialId,
    pub top_cells: u8,
    pub sub: MaterialId,
    pub sub_cells: u8,
}

impl Stack {
    pub fn depth(&self) -> i32 {
        i32::from(self.top_cells) + i32::from(self.sub_cells)
    }
}

/// Height, biome and lowland weight for every column of the arena.
pub struct ColumnMap {
    size: u32,
    seed: u64,
    heights: Vec<u16>,
    biomes: Vec<u8>,
    lowland: Vec<u8>,
    /// Wetness `0..=255`. Showcase: a constant per biome.
    moisture: Vec<u8>,
    /// Degrees Celsius as `(t + 30) * 3.1875`. Showcase: a constant.
    temperature: Vec<u8>,
    /// Cells to the nearest river or lake, clamped to 255. Showcase: 255.
    river_dist: Vec<u8>,
    /// `y` of the top water cell per column, 0 for dry. Empty for the
    /// showcase, whose water is everything below [`SEA_LEVEL`].
    water_surface: Vec<u16>,
    natural: bool,
    /// Water exchange cells the generator places: river heads and the sinks
    /// of landlocked maps. Empty for the showcase.
    springs: Vec<([i64; 3], f32)>,
    sinks: Vec<[i64; 3]>,
    /// Biomes (bit `1 << biome`) that the climate did not produce in enough
    /// quantity and the generator painted in. Natural worlds only.
    repaired: u16,
    /// The gallery preset: one pad per biome (see [`gallery_patches`]).
    gallery: bool,
}

struct Layout {
    size: f64,
    seed: u64,
    river: [(f64, f64); RIVER_SEGMENTS + 1],
}

impl Layout {
    fn new(seed: u64, size: u32) -> Self {
        let size = f64::from(size);
        let start = (BASIN_CENTER.0 * size, BASIN_CENTER.1 * size);
        let end = (RIVER_END.0 * size, RIVER_END.1 * size);
        let (dx, dz) = (end.0 - start.0, end.1 - start.1);
        let len = (dx * dx + dz * dz).sqrt();
        let (px, pz) = (-dz / len, dx / len);
        let mut river = [(0.0, 0.0); RIVER_SEGMENTS + 1];
        for (i, p) in river.iter_mut().enumerate() {
            let t = i as f64 / RIVER_SEGMENTS as f64;
            // Meander sideways, pinned at both ends.
            let wiggle = fbm2(seed ^ S_RIVER, t * 3.0, 0.5, 2) * 0.12 * len * (t * (1.0 - t) * 4.0);
            *p = (
                start.0 + dx * t + px * wiggle,
                start.1 + dz * t + pz * wiggle,
            );
        }
        Self { size, seed, river }
    }

    fn river_distance(&self, x: f64, z: f64) -> f64 {
        let mut best = f64::MAX;
        for w in self.river.windows(2) {
            let ((ax, az), (bx, bz)) = (w[0], w[1]);
            let (vx, vz) = (bx - ax, bz - az);
            let t = (((x - ax) * vx + (z - az) * vz) / (vx * vx + vz * vz)).clamp(0.0, 1.0);
            let (cx, cz) = (ax + vx * t - x, az + vz * t - z);
            best = best.min((cx * cx + cz * cz).sqrt());
        }
        best
    }

    /// `(height in cells, lowland weight 0..=1, desert weight, mountain weight)`.
    fn sample(&self, x: f64, z: f64) -> (f64, f64, f64, f64) {
        let s = self.seed;
        let sea = f64::from(SEA_LEVEL);
        let (u, v) = (x / self.size, z / self.size);
        // Warp the layout so borders are organic, not straight.
        let wu = u + 0.09 * fbm2(s ^ S_WARP_U, x / 260.0, z / 260.0, 3);
        let wv = v + 0.09 * fbm2(s ^ S_WARP_V, x / 260.0, z / 260.0, 3);

        let mut mountain = smoothstep(0.34, 0.20, wv);
        let mut desert = smoothstep(0.55, 0.68, wu) * smoothstep(0.38, 0.52, wv);
        let taken = mountain + desert;
        if taken > 1.0 {
            mountain /= taken;
            desert /= taken;
        }
        let hills_w = 1.0 - mountain - desert;

        let hills = sea
            + 30.0
            + 22.0 * fbm2(s ^ S_HILLS, x / 150.0, z / 150.0, 4)
            + 4.0 * fbm2(s ^ S_HILL_DETAIL, x / 40.0, z / 40.0, 2);
        let hills = hills.max(sea + 4.0);
        let ridge = ridged2(s ^ S_RIDGE, x / 260.0, z / 260.0, 4);
        let peaks =
            (sea + 40.0 + 170.0 * ridge + 10.0 * fbm2(s ^ S_MOUNT_DETAIL, x / 30.0, z / 30.0, 2))
                .max(sea + 12.0);
        // Dunes: stretched along x.
        let dunes = (sea
            + 16.0
            + 12.0 * fbm2(s ^ S_DUNES, x / 100.0, z / 38.0, 3)
            + 3.0 * fbm2(s ^ S_DUNE_DETAIL, x / 20.0, z / 12.0, 2))
        .max(sea + 6.0);
        let blended = hills * hills_w + peaks * mountain + dunes * desert;

        // Swamp basin.
        let (bx, bz) = (BASIN_CENTER.0 * self.size, BASIN_CENTER.1 * self.size);
        let r = BASIN_RADIUS * self.size;
        // The rim is perturbed but never pushed past 1.0 from outside: the
        // basin is still bounded by a circle of radius `BASIN_RADIUS`.
        let rim = 1.0 + 0.18 * fbm2(s ^ S_RIM, x / 55.0, z / 55.0, 2);
        let q = ((x - bx) * (x - bx) + (z - bz) * (z - bz)).sqrt() / r / rim.min(1.0);
        let lowland = smoothstep(1.0, 0.78, q);
        if lowland <= 0.0 {
            return (blended, 0.0, desert, mountain);
        }
        let flats = sea + 3.0 + 3.0 * fbm2(s ^ S_SWAMP, x / 30.0, z / 30.0, 2);
        let pools = smoothstep(0.1, 0.4, fbm2(s ^ S_POOLS, x / 45.0, z / 45.0, 2));
        let lake_r = LAKE_RADIUS * self.size;
        let lake_d = ((x - bx) * (x - bx) + (z - bz) * (z - bz)).sqrt();
        let lake = smoothstep(lake_r, lake_r * 0.3, lake_d);
        let river = smoothstep(
            RIVER_HALF_WIDTH,
            RIVER_HALF_WIDTH * 0.4,
            self.river_distance(x, z),
        );
        let basin = flats - 7.0 * pools - LAKE_DEPTH * lake - RIVER_DEPTH * river;
        (
            blended + (basin - blended) * lowland,
            lowland,
            desert,
            mountain,
        )
    }
}

impl ColumnMap {
    pub fn compute(spec: &WorldGenSpec) -> Result<Self, GenError> {
        spec.validate()?;
        match spec.preset {
            Preset::Showcase => Self::compute_showcase(spec),
            Preset::Natural => natural::compute(spec),
            Preset::Gallery => gallery::compute(spec),
        }
    }

    fn compute_showcase(spec: &WorldGenSpec) -> Result<Self, GenError> {
        let size = spec.size_cells;
        let layout = Layout::new(spec.seed, size);
        let n = size as usize;
        let mut heights = vec![0u16; n * n];
        let mut biomes = vec![0u8; n * n];
        let mut lowland = vec![0u8; n * n];
        let threads = std::thread::available_parallelism()
            .map_or(1, |p| p.get())
            .min(16);
        let rows_per = n.div_ceil(threads);
        std::thread::scope(|scope| {
            let parts = heights
                .chunks_mut(rows_per * n)
                .zip(biomes.chunks_mut(rows_per * n))
                .zip(lowland.chunks_mut(rows_per * n))
                .enumerate();
            for (i, ((h, b), l)) in parts {
                let layout = &layout;
                scope.spawn(move || {
                    for (row, ((h, b), l)) in h
                        .chunks_mut(n)
                        .zip(b.chunks_mut(n))
                        .zip(l.chunks_mut(n))
                        .enumerate()
                    {
                        let z = i * rows_per + row;
                        for x in 0..n {
                            let (height, low, desert, mountain) =
                                layout.sample(x as f64 + 0.5, z as f64 + 0.5);
                            h[x] = height.round().clamp(1.0, 65535.0) as u16;
                            l[x] = (low * 255.0).round() as u8;
                            b[x] = if low > 0.3 {
                                Biome::Swamp
                            } else if desert > 0.5 {
                                Biome::Desert
                            } else if mountain > 0.5 {
                                Biome::Alpine
                            } else {
                                Biome::Meadow
                            } as u8;
                        }
                    }
                });
            }
        });
        let moisture = biomes
            .iter()
            .map(|&b| match Biome::from_u8(b) {
                Biome::Meadow => 120,
                Biome::Alpine => 85,
                Biome::Swamp => 210,
                _ => 25,
            })
            .collect();
        Ok(Self {
            size,
            seed: spec.seed,
            heights,
            biomes,
            lowland,
            moisture,
            temperature: vec![128; n * n],
            river_dist: vec![255; n * n],
            water_surface: Vec::new(),
            natural: false,
            springs: Vec::new(),
            sinks: Vec::new(),
            repaired: 0,
            gallery: false,
        })
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    #[inline]
    fn idx(&self, x: i64, z: i64) -> usize {
        let max = i64::from(self.size) - 1;
        (z.clamp(0, max) * i64::from(self.size) + x.clamp(0, max)) as usize
    }

    /// Terrain surface: the highest solid cell `y` of column `(x, z)`.
    /// Out-of-arena coordinates clamp to the nearest edge column.
    pub fn height(&self, x: i64, z: i64) -> i32 {
        i32::from(self.heights[self.idx(x, z)])
    }

    pub fn biome(&self, x: i64, z: i64) -> Biome {
        Biome::from_u8(self.biomes[self.idx(x, z)])
    }

    /// Basin weight in `0..=255`.
    pub fn lowland(&self, x: i64, z: i64) -> u8 {
        self.lowland[self.idx(x, z)]
    }

    /// Whether `(x, z)` is inside the arena's bedrock border wall, where the
    /// generated terrain is replaced by solid wall.
    pub fn in_border(&self, x: i64, z: i64) -> bool {
        let edge = i64::from(self.size) - BORDER_CELLS;
        x < BORDER_CELLS || z < BORDER_CELLS || x >= edge || z >= edge
    }

    /// `y` of the top water cell of the column, or 0 if it is dry. Water never
    /// enters the border wall.
    pub fn water_surface(&self, x: i64, z: i64) -> i32 {
        if self.in_border(x, z) {
            return 0;
        }
        if self.water_surface.is_empty() {
            return if self.height(x, z) < SEA_LEVEL {
                SEA_LEVEL
            } else {
                0
            };
        }
        i32::from(self.water_surface[self.idx(x, z)])
    }

    /// How deep the water stands over the ground in cells, 0 on dry ground.
    pub fn water_depth(&self, x: i64, z: i64) -> i32 {
        (self.water_surface(x, z) - self.height(x, z)).max(0)
    }

    /// Whether water covers the column: its water surface is above the ground.
    pub fn is_water(&self, x: i64, z: i64) -> bool {
        self.water_surface(x, z) > self.height(x, z)
    }

    /// Wetness `0..=255`.
    pub fn moisture(&self, x: i64, z: i64) -> u8 {
        self.moisture[self.idx(x, z)]
    }

    /// Ground temperature in degrees Celsius.
    pub fn temperature_c(&self, x: i64, z: i64) -> f64 {
        f64::from(self.temperature[self.idx(x, z)]) / 3.1875 - 30.0
    }

    /// Whether this is the gallery preset's map.
    pub fn is_gallery(&self) -> bool {
        self.gallery
    }

    /// The biomes the generator had to paint in because the climate left them
    /// out or nearly so (bit `1 << biome`); zero for most seeds.
    pub fn repaired_biomes(&self) -> u16 {
        self.repaired
    }

    /// Cells to the nearest river or lake, clamped to 255.
    pub fn river_dist(&self, x: i64, z: i64) -> u8 {
        self.river_dist[self.idx(x, z)]
    }

    /// River heads: the cell each spring adds water to, and its discharge in
    /// cells^3/s (the catchment it carries times the runoff).
    pub fn springs(&self) -> &[([i64; 3], f32)] {
        &self.springs
    }

    /// Where landlocked water ends: cells to empty every step.
    pub fn sinks(&self) -> &[[i64; 3]] {
        &self.sinks
    }

    /// Whether this map was made by the natural pipeline.
    pub fn is_natural(&self) -> bool {
        self.natural
    }

    /// Steepness in cells of height per cell, over a four-cell baseline.
    pub fn gradient(&self, x: i64, z: i64) -> f64 {
        let dx = self.height(x + 2, z) - self.height(x - 2, z);
        let dz = self.height(x, z + 2) - self.height(x, z - 2);
        f64::from(dx.abs().max(dz.abs())) / 4.0
    }

    pub fn min_max_height(&self) -> (i32, i32) {
        let lo = self.heights.iter().copied().min().unwrap_or(0);
        let hi = self.heights.iter().copied().max().unwrap_or(0);
        (i32::from(lo), i32::from(hi))
    }

    /// Largest height difference to a 4-neighbour.
    fn slope(&self, x: i64, z: i64) -> i32 {
        let h = self.height(x, z);
        [(1, 0), (-1, 0), (0, 1), (0, -1)]
            .iter()
            .map(|(dx, dz)| (self.height(x + dx, z + dz) - h).abs())
            .max()
            .unwrap_or(0)
    }

    /// The material stack of a column.
    pub fn stack(&self, p: &WorldgenPalette, x: i64, z: i64) -> Stack {
        let h = self.height(x, z);
        let slope = self.slope(x, z);
        let st = |top, top_cells, sub, sub_cells| Stack {
            top,
            top_cells,
            sub,
            sub_cells,
        };
        let rock = || st(p.stone, 1, p.stone, 0);
        if self.natural {
            return self.natural_stack(p, x, z);
        }
        match self.biome(x, z) {
            Biome::Swamp => {
                if h < SEA_LEVEL - 6 {
                    st(p.gravel, 2, p.clay, 8)
                } else if h <= SEA_LEVEL + 1 {
                    st(p.mud, 8, p.clay, 4)
                } else {
                    st(p.moss, 3, p.mud, 6)
                }
            }
            Biome::Desert => st(p.sand, 10, p.sandstone, 30),
            Biome::Alpine => {
                if h > SNOW_LINE {
                    st(p.snow, 4, p.stone, 0)
                } else if slope >= 2 || h > SEA_LEVEL + 70 {
                    let patch = fbm2(self.seed ^ S_SLATE, x as f64 / 22.0, z as f64 / 22.0, 2);
                    if patch > 0.25 {
                        st(p.slate, 6, p.stone, 0)
                    } else {
                        rock()
                    }
                } else {
                    st(p.grass, 2, p.dirt, 8)
                }
            }
            Biome::Meadow => {
                if slope >= 2 {
                    rock()
                } else {
                    st(p.grass, 2, p.dirt, 10)
                }
            }
            _ => unreachable!("showcase maps only hold showcase biomes"),
        }
    }
}

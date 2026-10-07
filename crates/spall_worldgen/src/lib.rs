//! `spall_worldgen` — deterministic procedural terrain for Spall worlds.
//!
//! A world is a pure function of a [`WorldGenSpec`] (preset, seed, size,
//! material palette) and [`GEN_VERSION`]. Generation has two halves:
//! [`columns`] (2D: heights, biomes, surface layers — cheap, previewable) and
//! [`generate`] (3D: rock bands, caves, brick fill). Output is a
//! `spall_voxel::Volume` with every brick of the arena resident (air included,
//! so nothing reads as `Unknown`), plus water as data for the fluid solver —
//! water is never a voxel material.
//!
//! No GPU, window, network or game dependency: only `spall_core` and
//! `spall_voxel`. The game supplies its own material ids through
//! [`WorldgenPalette`].

pub mod caves;
pub mod columns;
pub mod debug;
pub mod generate;
pub mod noise;
pub mod spec;

pub use columns::{Biome, ColumnMap, SNOW_LINE};
pub use generate::{
    GeneratedWorld, GenerationTimings, SPAWN_HEADROOM_CELLS, WATER_DOMAIN_BUDGET, WaterPlan,
    generate, generate_with_timings,
};
pub use spec::{
    BORDER_CELLS, GEN_VERSION, GenError, HEIGHT_CELLS, Preset, SEA_LEVEL, SHOWCASE_SIZE_CELLS,
    WALL_TOP, WorldGenSpec, WorldgenPalette,
};

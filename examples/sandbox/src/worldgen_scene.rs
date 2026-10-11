//! Plays a procedurally generated world (ENG-114).
//!
//! [`spall_worldgen`] produces the terrain volume, the initial water and the
//! player spawns; this module is the game side of that contract: it supplies
//! the sandbox material palette ([`crate::game::terrain_palette`]), turns the
//! water plan into a server fluid region, and packages everything as a
//! [`spall_server::CustomWorld`]. The engine crate never names game materials,
//! and this module never re-implements generation.

use std::sync::Arc;

use spall_core::GlobalCell;
use spall_server::CustomWorld;
use spall_sim::WorldSetup;
use spall_sim::column_water::ColumnWaterSetup;
use spall_structure::AnchorPlane;
use spall_worldgen::{
    GenError, GeneratedWorld, Preset, SHOWCASE_SIZE_CELLS, WaterPlan, WorldGenSpec,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorldgenSceneError {
    #[error("unknown --worldgen preset `{0}` (expected: showcase)")]
    UnknownPreset(String),
    #[error("world generation failed: {0}")]
    Generate(#[from] GenError),
    #[error("vegetation: {0}")]
    Vegetation(String),
}

/// The default arena edge in cells (256 m).
pub const DEFAULT_SIZE_CELLS: u32 = SHOWCASE_SIZE_CELLS;

/// A generated world, ready to package for the server.
/// Where the player stands beside a lake.
struct LakeSite {
    spawn: [f64; 3],
}

pub struct GeneratedScene {
    world: Arc<GeneratedWorld>,
    /// Stacked-column water (ENG-167).
    column_water: Option<ColumnWaterSetup>,
    /// Replaces the generated spawns (see [`Self::with_lake_spawn`]).
    spawn_override: Option<[f64; 3]>,
    vegetation: spall_ecology::living::LivingState,
}

/// Generates the named preset. Deterministic in `(preset, seed, size_cells)`.
pub fn generate(
    preset: &str,
    seed: u64,
    size_cells: u32,
) -> Result<GeneratedScene, WorldgenSceneError> {
    generate_with_season(
        preset,
        seed,
        size_cells,
        spall_ecology::living::Season::Summer,
    )
}

pub fn generate_with_season(
    preset: &str,
    seed: u64,
    size_cells: u32,
    season: spall_ecology::living::Season,
) -> Result<GeneratedScene, WorldgenSceneError> {
    let preset = Preset::from_name(preset)
        .ok_or_else(|| WorldgenSceneError::UnknownPreset(preset.into()))?;
    let spec = WorldGenSpec::new(preset, seed, size_cells, crate::game::terrain_palette());
    let started = std::time::Instant::now();
    let (mut world, timings) = spall_worldgen::generate_with_timings(&spec)?;
    for (stage, elapsed) in [
        ("worldgen.columns", timings.columns),
        ("worldgen.fill", timings.fill),
        ("worldgen.packing", timings.packing),
        ("worldgen.compact", timings.compact),
        ("worldgen.insert", timings.insert),
        ("worldgen.source_release", timings.source_release),
        ("worldgen.water_and_spawns", timings.water_and_spawns),
    ] {
        tracing::info!(
            stage,
            elapsed_us = elapsed.as_micros() as u64,
            "startup profile"
        );
    }
    tracing::info!(
        stage = "terrain_generation",
        elapsed_us = started.elapsed().as_micros() as u64,
        size_cells,
        "startup profile"
    );
    let spawns = world
        .spawns
        .iter()
        .map(|p| p.map(|v| (v / 0.25).floor() as i64))
        .collect();
    let started = std::time::Instant::now();
    let vegetation = spall_ecology::living::LivingState::generate(
        seed,
        &world.columns,
        &mut world.terrain,
        crate::vegetation::catalogue(),
        spawns,
        season,
    )
    .map_err(WorldgenSceneError::Vegetation)?;
    tracing::info!(
        stage = "vegetation_generation",
        elapsed_us = started.elapsed().as_micros() as u64,
        "startup profile"
    );
    let started = std::time::Instant::now();
    let column_water = column_water_setup(&world.water, world.region);
    tracing::info!(
        stage = "water_setup",
        elapsed_us = started.elapsed().as_micros() as u64,
        "startup profile"
    );
    Ok(GeneratedScene {
        world: Arc::new(world),
        column_water,
        spawn_override: None,
        vegetation,
    })
}

/// The stacked-column water for a water plan: the plan's full cells seeded at
/// rest over the world's whole height range. There is no domain box and no
/// coarsening, so a one-voxel wall always blocks water.
pub fn column_water_setup(
    plan: &WaterPlan,
    region: (GlobalCell, GlobalCell),
) -> Option<ColumnWaterSetup> {
    plan.bounds?;
    let y_lo = region.0.y.div_euclid(32) * 32;
    let y_hi = (region.1.y + 1 + 31).div_euclid(32) * 32;
    Some(ColumnWaterSetup::new(
        y_lo as i32,
        y_hi as i32,
        plan.cells.clone(),
    ))
}

impl GeneratedScene {
    pub fn vegetation(&self) -> &spall_ecology::living::LivingState {
        &self.vegetation
    }

    pub fn world(&self) -> &GeneratedWorld {
        &self.world
    }

    pub fn column_water_setup(&self) -> Option<&ColumnWaterSetup> {
        self.column_water.as_ref()
    }

    /// Player start positions in metres.
    pub fn player_spawns(&self) -> &[[f64; 3]] {
        match &self.spawn_override {
            Some(spawn) => std::slice::from_ref(spawn),
            None => &self.world.spawns,
        }
    }

    /// Spawns the player on solid ground beside the shore of the deepest lake
    /// instead of at a generated meadow spawn. Unchanged if the world has no
    /// lake or no suitable ground is found (see [`Self::lake_spawn`]).
    #[must_use]
    pub fn with_lake_spawn(mut self) -> Self {
        self.spawn_override = self.lake_spawn();
        self
    }

    /// A feet position (metres) a few cells inland from the shore of the
    /// deepest lake: standing room on solid ground with headroom.
    pub fn lake_spawn(&self) -> Option<[f64; 3]> {
        self.lake_site().map(|site| site.spawn)
    }

    /// Where the player stands beside the deepest lake.
    fn lake_site(&self) -> Option<LakeSite> {
        use std::collections::{HashMap, HashSet};
        let plan = &self.world.water;
        let mut depth: HashMap<(i64, i64), i64> = HashMap::new();
        for c in &plan.cells {
            *depth.entry((c.x, c.z)).or_default() += 1;
        }
        // The deepest column (ties by position, so the result is stable).
        let deep = depth
            .iter()
            .max_by_key(|((x, z), d)| (**d, std::cmp::Reverse((*x, *z))))
            .map(|(&c, _)| c)?;
        let wet: HashSet<(i64, i64)> = depth.keys().copied().collect();
        const DIRS: [(i64, i64); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
        // Shore columns of that lake: wet with a dry neighbour, nearest first.
        let wet_ref = &wet;
        let mut shores: Vec<((i64, i64), (i64, i64))> = wet
            .iter()
            .flat_map(|&(x, z)| {
                DIRS.iter()
                    .filter(move |(dx, dz)| !wet_ref.contains(&(x + dx, z + dz)))
                    .map(move |&dir| ((x, z), dir))
            })
            .collect();
        let dist = |(x, z): (i64, i64)| (x - deep.0).pow(2) + (z - deep.1).pow(2);
        shores.sort_by_key(|&(c, dir)| (dist(c), c, dir));
        let (lo, hi) = self.world.region;
        let terrain = &self.world.terrain;
        for (shore, (dx, dz)) in shores.into_iter().take(400) {
            // A few columns inland, on ground that is not water.
            for inland in [4i64, 6, 8] {
                let (x, z) = (shore.0 + dx * inland, shore.1 + dz * inland);
                if wet.contains(&(x, z)) {
                    continue;
                }
                // The first solid cell scanning down from the top of the world.
                let ground = (lo.y..=hi.y).rev().find(|&y| {
                    matches!(
                        terrain.sample(GlobalCell::new(x, y, z)),
                        Ok(spall_voxel::Sample::Filled(_))
                    )
                })?;
                let clear = (1..=9).all(|up| {
                    matches!(
                        terrain.sample(GlobalCell::new(x, ground + up, z)),
                        Ok(spall_voxel::Sample::Empty { .. })
                    )
                });
                if clear {
                    return Some(LakeSite {
                        spawn: [
                            (x as f64 + 0.5) * 0.25,
                            (ground + 1) as f64 * 0.25,
                            (z as f64 + 0.5) * 0.25,
                        ],
                    });
                }
            }
        }
        None
    }

    /// A raft and a handful of test objects floating on the deepest lake, in
    /// open water a few metres out from where [`Self::lake_spawn`] stands:
    /// a 6 m x 4 m wood raft, three light crates that float flat, a long log,
    /// and a stone block that sinks. Empty if the lake has no clear stretch of
    /// deep water.
    pub fn lake_floaters(&self) -> Vec<spall_server::InitialBody> {
        use std::collections::HashMap;
        let Some(site) = self.lake_site() else {
            return Vec::new();
        };
        let plan = &self.world.water;
        let mut depth: HashMap<(i64, i64), i64> = HashMap::new();
        for c in &plan.cells {
            *depth.entry((c.x, c.z)).or_default() += 1;
        }
        let surface_m = (plan.surface_y + 1) as f64 * 0.25;
        // Open water at least 1 m deep over the whole stretch the objects use
        // (12 m by 10 m around the centre), the nearest such spot to the spawn.
        let clear_at = |cx: i64, cz: i64| {
            (-24..=24).all(|dx| {
                (-20..=20).all(|dz| depth.get(&(cx + dx, cz + dz)).is_some_and(|d| *d >= 4))
            })
        };
        let spawn = ((site.spawn[0] / 0.25) as i64, (site.spawn[2] / 0.25) as i64);
        let mut candidates: Vec<(i64, i64)> = depth.keys().copied().collect();
        candidates.sort_by_key(|&(x, z)| ((x - spawn.0).pow(2) + (z - spawn.1).pow(2), x, z));
        let Some(centre) = candidates.into_iter().find(|&(x, z)| clear_at(x, z)) else {
            return Vec::new();
        };
        let (cx, cz) = (centre.0 as f64 * 0.25, centre.1 as f64 * 0.25);
        let wood = crate::game::materials::WOOD;
        let stone = crate::game::materials::STONE;
        // `dims` in cells (x, y, z); `offset` is the body's centre relative to
        // the lake point, metres. Bodies start above the surface and drop.
        let body = |dims: [i64; 3], material, density: f32, offset: [f64; 2], drop_m: f64| {
            let half = dims.map(|d| d as f64 * 0.125);
            spall_server::InitialBody {
                dims,
                material,
                density_kg_m3: density,
                position_m: [
                    cx + offset[0] - half[0],
                    surface_m + drop_m,
                    cz + offset[1] - half[2],
                ],
            }
        };
        vec![
            // The raft: 6 m x 0.5 m x 4 m of wood, 70% of its thickness under.
            body([24, 2, 16], wood, 700.0, [0.0, 0.0], 0.4),
            // Three 1 m crates light enough to float flat, 25% under.
            body([4, 4, 4], wood, 250.0, [-5.0, -2.5], 0.8),
            body([4, 4, 4], wood, 250.0, [-5.0, 0.0], 0.8),
            body([4, 4, 4], wood, 250.0, [-5.0, 2.5], 0.8),
            // A 6 m log, 0.75 m across, 60% under: it rolls.
            body([24, 3, 3], wood, 600.0, [0.0, 4.5], 0.8),
            // Stone does not float.
            body([4, 4, 4], stone, 2600.0, [5.0, 0.0], 1.2),
        ]
    }

    /// Builds the terrain, materials and physics config. The terrain volume is
    /// cloned from the generated one, which shares its dense brick payloads.
    pub fn world_setup(&self) -> WorldSetup {
        // A committed cut rebuilds the terrain collider, and CCD's stale proxy
        // can panic mid-sweep: the same choice every other server scene makes.
        let physics = spall_physics::PhysicsConfig {
            disable_ccd: true,
            ..Default::default()
        };
        WorldSetup {
            terrain: self.world.terrain.clone(),
            terrain_collider_region: self.world.region,
            materials: crate::game::manifest(),
            anchor: AnchorPlane::at(self.world.anchor_y),
            physics,
        }
    }

    /// Packages the world for [`spall_server::ServeConfig::custom_world`].
    pub fn into_custom_world(self) -> CustomWorld {
        let spawns = self.player_spawns().to_vec();
        let column_water = self.column_water.clone();
        let vegetation = self.vegetation.clone();
        let scene = Arc::new(self);
        let mut custom =
            CustomWorld::new(spawns, move || scene.world_setup()).with_vegetation(vegetation);
        if let Some(setup) = column_water {
            custom = custom.with_column_water(setup);
        }
        custom
    }
}

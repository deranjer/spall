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
use spall_fluid::DomainSpec;
use spall_server::CustomWorld;
use spall_sim::column_water::ColumnWaterSetup;
use spall_sim::{WaterSetup, WorldSetup};
use spall_structure::AnchorPlane;
use spall_worldgen::{
    GenError, GeneratedWorld, Preset, SHOWCASE_SIZE_CELLS, WaterPlan, WorldGenSpec,
};
use thiserror::Error;

use crate::editor_scene::{WATER_MARGIN_ABOVE, WATER_MARGIN_BELOW, WATER_MARGIN_XZ, WATER_STEP_S};

/// Fluid cells a generated world's water domain may start with. The solver
/// costs roughly 0.5-2 microseconds per fluid cell per step and the water worker
/// has 50 ms per 1/20 s step, so a domain much over this runs slower than real
/// time. Measured on the seed-1 showcase (release): 512-cell arena, 87,000 cells
/// (0.5 m) = median 102 ms per step flowing, 27,500 cells (0.75 m) = 15 ms;
/// 1024-cell arena, 347,000 cells = 700 ms (water at 0.06x real time), 107,000
/// (0.75 m) = 64 ms, 46,000 (1 m) = 16 ms. See `docs/reports/ENG-121.md`.
/// Deliberate performance default (2026-10-03): keep this adaptive coarse MAC
/// path with original velocity sampling; fine-phase/conservative momentum is
/// experimental and must not replace it based on small-fixture results.
pub const WATER_FLUID_CELL_BUDGET: usize = 50_000;

/// Domain growth may take a region to this multiple of the budget.
const WATER_GROWTH_BUDGET_FACTOR: usize = 2;

/// The domain box for water bounds `lo`..=`hi` at coarsening `c`: the bounds plus
/// the sandbox margins, snapped outward to whole fluid cells.
fn water_domain_box(lo: GlobalCell, hi: GlobalCell, c: i64) -> ([i64; 3], [i64; 3]) {
    let snap_down = |v: i64| v.div_euclid(c) * c;
    let snap_up = |v: i64| (v + c).div_euclid(c) * c; // exclusive end
    (
        [
            snap_down(lo.x - WATER_MARGIN_XZ),
            snap_down(lo.y - WATER_MARGIN_BELOW),
            snap_down(lo.z - WATER_MARGIN_XZ),
        ],
        [
            snap_up(hi.x + WATER_MARGIN_XZ),
            snap_up(hi.y + WATER_MARGIN_ABOVE),
            snap_up(hi.z + WATER_MARGIN_XZ),
        ],
    )
}

/// The finest coarsening (2..=8 voxels per fluid cell) whose domain fits
/// [`WATER_FLUID_CELL_BUDGET`]; 8 if none does. Coarser cells lose resolution:
/// a coarse cell is solid when at least half its voxels are, so walls thinner
/// than half a cell (a one-voxel wall at 0.75 m or 1 m) no longer block water,
/// and trenches a couple of cells wide fill unevenly. Hand-authored scenes keep
/// `editor_scene::WATER_COARSEN`.
pub fn pick_water_coarsening(lo: GlobalCell, hi: GlobalCell) -> u32 {
    for c in 2..=8u32 {
        let (min, max) = water_domain_box(lo, hi, i64::from(c));
        let voxels: u128 = (0..3).map(|a| (max[a] - min[a]) as u128).product();
        if voxels / u128::from(c).pow(3) <= WATER_FLUID_CELL_BUDGET as u128 {
            return c;
        }
    }
    8
}

#[derive(Debug, Error)]
pub enum WorldgenSceneError {
    #[error("unknown --worldgen preset `{0}` (expected: showcase)")]
    UnknownPreset(String),
    #[error("world generation failed: {0}")]
    Generate(#[from] GenError),
    #[error("vegetation: {0}")]
    Vegetation(String),
    #[error("water domain: {0}")]
    WaterDomain(String),
}

/// The default arena edge in cells (256 m).
pub const DEFAULT_SIZE_CELLS: u32 = SHOWCASE_SIZE_CELLS;

/// A generated world, ready to package for the server.
pub struct GeneratedScene {
    world: Arc<GeneratedWorld>,
    water: Option<WaterSetup>,
    /// Stacked-column water (ENG-167): the production model. The dense
    /// `water` setup is retained for the legacy tests until W5.
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
    let water = water_setup(&world.water)?;
    let column_water = column_water_setup(&world.water, world.region);
    tracing::info!(
        stage = "water_setup",
        elapsed_us = started.elapsed().as_micros() as u64,
        "startup profile"
    );
    Ok(GeneratedScene {
        world: Arc::new(world),
        water,
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

/// The server fluid region for a water plan: its bounding box plus margins,
/// snapped outward to whole fluid cells. `None` for a dry world.
pub fn water_setup(plan: &WaterPlan) -> Result<Option<WaterSetup>, WorldgenSceneError> {
    let Some((lo, hi)) = plan.bounds else {
        return Ok(None);
    };
    let coarsen = pick_water_coarsening(lo, hi);
    let (min, max) = water_domain_box(lo, hi, i64::from(coarsen));
    let dimensions = [0, 1, 2].map(|a| (max[a] - min[a]) as u32);
    let domain = DomainSpec::new(
        GlobalCell::new(min[0], min[1], min[2]),
        dimensions,
        spall_protocol::water::MAX_WATER_VOXEL_CELLS,
    )
    .map_err(|e| WorldgenSceneError::WaterDomain(e.to_string()))?;
    Ok(Some(
        WaterSetup::new(domain, plan.cells.iter().map(|cell| (*cell, 1.0)).collect())
            .with_coarsening(coarsen)
            .with_growth(spall_sim::WaterGrowth {
                max_voxel_cells: WATER_GROWTH_BUDGET_FACTOR
                    * WATER_FLUID_CELL_BUDGET
                    * (coarsen as usize).pow(3),
                ..spall_sim::WaterGrowth::default()
            })
            .on_worker(WATER_STEP_S),
    ))
}

impl GeneratedScene {
    pub fn vegetation(&self) -> &spall_ecology::living::LivingState {
        &self.vegetation
    }

    pub fn world(&self) -> &GeneratedWorld {
        &self.world
    }

    pub fn water_setup(&self) -> Option<&WaterSetup> {
        self.water.as_ref()
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
                    return Some([
                        (x as f64 + 0.5) * 0.25,
                        (ground + 1) as f64 * 0.25,
                        (z as f64 + 0.5) * 0.25,
                    ]);
                }
            }
        }
        None
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

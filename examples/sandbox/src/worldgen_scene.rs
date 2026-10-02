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
use spall_sim::{WaterSetup, WorldSetup};
use spall_structure::AnchorPlane;
use spall_worldgen::{
    GenError, GeneratedWorld, Preset, SHOWCASE_SIZE_CELLS, WaterPlan, WorldGenSpec,
};
use thiserror::Error;

use crate::editor_scene::{
    MAX_SCENE_WATER_CELLS, WATER_MARGIN_ABOVE, WATER_MARGIN_BELOW, WATER_MARGIN_XZ, WATER_STEP_S,
};

/// Fluid cells a generated world's water domain may start with. The solver
/// costs roughly 0.5-2 microseconds per fluid cell per step and the water worker
/// has 50 ms per 1/20 s step, so a domain much over this runs slower than real
/// time. Measured on the seed-1 showcase (release): 512-cell arena, 87,000 cells
/// (0.5 m) = median 102 ms per step flowing, 27,500 cells (0.75 m) = 15 ms;
/// 1024-cell arena, 347,000 cells = 700 ms (water at 0.06x real time), 107,000
/// (0.75 m) = 64 ms, 46,000 (1 m) = 16 ms. See `docs/reports/ENG-121.md`.
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
    let mut world = spall_worldgen::generate(&spec)?;
    let spawns = world
        .spawns
        .iter()
        .map(|p| p.map(|v| (v / 0.25).floor() as i64))
        .collect();
    let vegetation = spall_ecology::living::LivingState::generate(
        seed,
        &world.columns,
        &mut world.terrain,
        crate::vegetation::catalogue(),
        spawns,
        season,
    )
    .map_err(WorldgenSceneError::Vegetation)?;
    let water = water_setup(&world.water)?;
    Ok(GeneratedScene {
        world: Arc::new(world),
        water,
        vegetation,
    })
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
        MAX_SCENE_WATER_CELLS,
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

    /// Player start positions in metres.
    pub fn player_spawns(&self) -> &[[f64; 3]] {
        &self.world.spawns
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
        let spawns = self.world.spawns.clone();
        let water = self.water.clone();
        let vegetation = self.vegetation.clone();
        let scene = Arc::new(self);
        CustomWorld::new_with_water(spawns, water, move || scene.world_setup())
            .with_vegetation(vegetation)
    }
}

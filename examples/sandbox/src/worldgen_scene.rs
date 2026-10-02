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
    MAX_SCENE_WATER_CELLS, WATER_COARSEN, WATER_MARGIN_ABOVE, WATER_MARGIN_BELOW, WATER_MARGIN_XZ,
    WATER_STEP_S,
};

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
    let c = i64::from(WATER_COARSEN);
    let snap_down = |v: i64| v.div_euclid(c) * c;
    let snap_up = |v: i64| (v + c).div_euclid(c) * c; // exclusive end
    let min = [
        snap_down(lo.x - WATER_MARGIN_XZ),
        snap_down(lo.y - WATER_MARGIN_BELOW),
        snap_down(lo.z - WATER_MARGIN_XZ),
    ];
    let max = [
        snap_up(hi.x + WATER_MARGIN_XZ),
        snap_up(hi.y + WATER_MARGIN_ABOVE),
        snap_up(hi.z + WATER_MARGIN_XZ),
    ];
    let dimensions = [0, 1, 2].map(|a| (max[a] - min[a]) as u32);
    let domain = DomainSpec::new(
        GlobalCell::new(min[0], min[1], min[2]),
        dimensions,
        MAX_SCENE_WATER_CELLS,
    )
    .map_err(|e| WorldgenSceneError::WaterDomain(e.to_string()))?;
    Ok(Some(
        WaterSetup::new(domain, plan.cells.iter().map(|cell| (*cell, 1.0)).collect())
            .with_coarsening(WATER_COARSEN)
            .with_growth(spall_sim::WaterGrowth::default())
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

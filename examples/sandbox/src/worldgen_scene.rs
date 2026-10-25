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
use spall_sim::column_water::{ColumnWaterSetup, Discharge};
use spall_structure::AnchorPlane;
use spall_worldgen::{
    GenError, GeneratedWorld, Preset, SHOWCASE_SIZE_CELLS, WaterPlan, WorldGenSpec,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorldgenSceneError {
    #[error("unknown --worldgen preset `{0}` (expected: showcase, natural)")]
    UnknownPreset(String),
    #[error("world generation failed: {0}")]
    Generate(#[from] GenError),
    #[error("vegetation: {0}")]
    Vegetation(String),
}

/// A pipe quieter than this (cells^3/s, 8 mL/s at 0.5) lets its region sleep.
/// At 0.1 a 4096 world's active regions climbed from 100 to about 450 in 15
/// minutes with nobody walking (most of them carrying 0.1-1: slow currents),
/// each step cost 14-40 ms and the water fell behind real time; at 0.5 they
/// stay at 50-80 and a step costs about 1 ms (measured, seed 4).
pub const WORLDGEN_SLEEP_FLOW: f32 = 0.5;

/// The default arena edge in cells (1 km). Needs regional streaming and several
/// GiB of server memory; CI and quick iteration use [`CI_SIZE_CELLS`].
pub const DEFAULT_SIZE_CELLS: u32 = 4096;
/// The arena edge CPU CI fixtures and fast iteration use (256 m).
pub const CI_SIZE_CELLS: u32 = SHOWCASE_SIZE_CELLS;

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
    let mut spec = WorldGenSpec::new(preset, seed, size_cells, crate::game::terrain_palette());
    // `SPALL_NO_WATER=1`: the same terrain with its lakes, rivers and sea dry.
    if std::env::var_os("SPALL_NO_WATER").is_some() {
        spec = spec.dry();
    }
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
    .and_then(|state| state.with_cover(crate::vegetation::cover_catalogue(), &world.columns))
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

/// The stacked-column water for a water plan: each wet column seeded at rest
/// to its own surface, over the world's whole height range, plus the plan's
/// springs (refilled every step) and sinks (emptied every step). There is no
/// domain box and no coarsening, so a one-voxel wall always blocks water.
pub fn column_water_setup(
    plan: &WaterPlan,
    region: (GlobalCell, GlobalCell),
) -> Option<ColumnWaterSetup> {
    plan.bounds?;
    let y_lo = region.0.y.div_euclid(32) * 32;
    let y_hi = (region.1.y + 1 + 31).div_euclid(32) * 32;
    let mut setup = ColumnWaterSetup::new(y_lo as i32, y_hi as i32, Vec::new());
    setup.initial_levels = plan
        .columns
        .iter()
        .map(|c| {
            (
                i64::from(c.x),
                i64::from(c.z),
                i64::from(c.bed),
                i64::from(c.top),
            )
        })
        .collect();
    // Springs are fixed discharges (cells^3/s from the catchment each carries),
    // not refill-to-full cells: a refilling spring takes whatever the channel
    // lets through, about 1.3 m3/s each measured, and floods a basin.
    setup.discharges = plan
        .springs
        .iter()
        .map(|s| Discharge {
            cell: s.cell,
            flow_cells3_s: f64::from(s.flow_cells3_s),
        })
        .collect();
    setup.sinks = plan.sinks.clone();
    // Rivers creep for a long time (about 0.1 m/s on a 1% slope), and at the
    // solver's default sensitivity every region they touch stays awake. A pipe
    // moving under 0.1 cells^3/s (6 mL/s) does not need stepping: with this,
    // measured on a 1024-cell world, the active regions fall from 272 to 143
    // in five minutes and keep falling.
    setup.config.sleep_flow = WORLDGEN_SLEEP_FLOW;
    // Experiments: `SPALL_WATER_SLEEP_FLOW` (cells^3/s) and
    // `SPALL_WATER_STEADY_STEPS` override the sleep threshold and turn on the
    // steady-flow sleep.
    if let Some(v) = std::env::var("SPALL_WATER_SLEEP_FLOW")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
    {
        setup.config.sleep_flow = v;
    }
    if let Some(v) = std::env::var("SPALL_WATER_STEADY_STEPS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        setup.config.steady_steps = v;
    }
    // Deep still sea stays a record until something disturbs it.
    setup.config.lazy_ocean = true;
    // Water far from every player steps every 2nd, 4th, 8th time beyond 160,
    // 320 and 640 m, so a far lake drains late rather than costing every step,
    // and a region catches up on what it missed when a player comes back.
    // `SPALL_WATER_THROTTLE=off` steps all water every time.
    if !std::env::var("SPALL_WATER_THROTTLE").is_ok_and(|v| v == "off") {
        setup.config.throttle_from_cells = [640.0, 1280.0, 2560.0];
        setup.config.catch_up_substeps = 24;
    }
    if water_coarse() {
        // `SPALL_WATER_COARSE=1`: 1 m water columns, with the sea
        // held at its level and evaporation balancing the springs. A coarse
        // pipe is four terrain pipes wide, so the same speed carries four times
        // the flow; distances are in coarse columns.
        let n = spall_fluid::columns::COARSE_CELLS as f32;
        setup.config.column_cells = n as u32;
        setup.config.sleep_flow *= n;
        setup.config.throttle_from_cells = setup.config.throttle_from_cells.map(|c| c / n);
        setup.config.lazy_ocean = false;
        setup.sea_level_y = Some((plan.surface_y + 1) as f64);
        setup.evaporation_cells_per_s = 1.45e-4;
        setup.evaporation_above_y = (plan.surface_y + 2) as f64;
    }
    Some(setup)
}

/// Whether `SPALL_WATER_COARSE=1` asks for 1 m water columns (off by default
/// while it is compared in play).
pub fn water_coarse() -> bool {
    std::env::var("SPALL_WATER_COARSE").is_ok_and(|v| v == "1")
}

/// Water seconds a settle may run before giving up on its regions going still.
const SETTLE_MAX_S: f64 = 4000.0;

impl GeneratedScene {
    /// Starts the water settled instead of at the planned levels, which are not
    /// at rest (rivers are seeded above their running depth and run off for
    /// about half an hour of water time). The first time, the water runs with
    /// no player (nothing throttled) until its active and wet regions stop
    /// changing, about 10 minutes for a 1 km world at 1 m columns, and the result
    /// is cached in `cache_dir` under `key` plus the solver settings; later
    /// starts load it. Any failure to read or write the cache is reported and
    /// falls back to settling or to the planned levels.
    pub fn settle_water(mut self, cache_dir: &std::path::Path, key: &str) -> Self {
        use std::hash::{Hash, Hasher};
        let Some(setup) = self.column_water.clone() else {
            return self;
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        format!(
            "{:?} {:?} {:?} {}",
            setup.config,
            setup.sea_level_y,
            setup.evaporation_cells_per_s,
            spall_protocol::WIRE_SCHEMA_VERSION
        )
        .hash(&mut hasher);
        self.world.version.hash(&mut hasher);
        let path = cache_dir.join(format!("water-{key}-{:016x}.bin", hasher.finish()));
        let state = match std::fs::read(&path) {
            Ok(bytes) => match spall_store::dto::decode::<spall_protocol::ColumnWaterState>(&bytes)
            {
                Ok(state) => {
                    eprintln!(
                        "sandbox-server: settled water loaded from {}",
                        path.display()
                    );
                    Some(state)
                }
                Err(error) => {
                    eprintln!(
                        "sandbox-server: settled water cache {} unreadable ({error}); settling again",
                        path.display()
                    );
                    None
                }
            },
            Err(_) => None,
        };
        let state = state.or_else(|| {
            let started = std::time::Instant::now();
            let mut water = match spall_sim::column_water::AuthoritativeColumnWater::new(
                &self.world.terrain,
                setup.clone(),
            ) {
                Ok(water) => water,
                Err(error) => {
                    eprintln!("sandbox-server: cannot settle the water: {error}");
                    return None;
                }
            };
            water.set_focus(&[]);
            let dt = 1.0 / 60.0;
            let (mut previous, mut calm, mut t) = ((0usize, 0usize), 0u32, 0.0);
            while t < SETTLE_MAX_S {
                let mut metrics = None;
                for _ in 0..6000 {
                    match water.tick(&self.world.terrain, &[], &[], dt) {
                        Ok(m) => metrics = Some(m),
                        Err(error) => {
                            eprintln!("sandbox-server: settling stopped: {error}");
                            return None;
                        }
                    }
                }
                t += 100.0;
                let m = metrics?;
                let now = (m.active_regions, m.allocated_regions);
                // Still: no region newly wet, and the active ones within 3% (or
                // three regions, for small worlds) of the last check.
                let still = now.1 == previous.1
                    && now.0.abs_diff(previous.0) as f64 <= (0.03 * previous.0 as f64).max(3.0);
                calm = if still { calm + 1 } else { 0 };
                eprintln!(
                    "sandbox-server: settling water: {t:.0} s of water time in {:.0} s, {} active regions",
                    started.elapsed().as_secs_f64(),
                    now.0
                );
                previous = now;
                if calm >= 2 {
                    break;
                }
            }
            let state = water.canonical_state();
            let written = std::fs::create_dir_all(cache_dir)
                .map_err(|e| e.to_string())
                .and_then(|()| spall_store::dto::encode(&state).map_err(|e| e.to_string()))
                .and_then(|bytes| {
                    let tmp = path.with_extension("tmp");
                    std::fs::write(&tmp, bytes)
                        .and_then(|()| std::fs::rename(&tmp, &path))
                        .map_err(|e| e.to_string())
                });
            match written {
                Ok(()) => eprintln!("sandbox-server: settled water cached at {}", path.display()),
                Err(error) => eprintln!("sandbox-server: settled water not cached: {error}"),
            }
            Some(state)
        });
        if let (Some(state), Some(water)) = (state, self.column_water.as_mut()) {
            water.initial_state = Some(state);
        }
        self
    }

    /// Stretches every season to `season_ms`, so the start season never ends.
    #[must_use]
    pub fn with_held_season(mut self, season_ms: u64) -> Self {
        self.vegetation.season_ms = season_ms;
        self
    }

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
        for c in &plan.columns {
            depth.insert((i64::from(c.x), i64::from(c.z)), i64::from(c.top - c.bed));
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
        for c in &plan.columns {
            depth.insert((i64::from(c.x), i64::from(c.z)), i64::from(c.top - c.bed));
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
    pub fn into_custom_world(mut self) -> CustomWorld {
        let spawns = self.player_spawns().to_vec();
        // The scene's own copy of the planned water would outlive the world setup
        // that needs it; hand it over instead of cloning.
        let column_water = self.column_water.take();
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

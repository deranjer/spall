//! Snow as real terrain near players.
//!
//! The thin ground coat (`snow.rs`) covers the whole world cheaply. Near each player this module
//! also stacks whole-cell snow layers on sky-exposed ground, so snow gets deeper the longer it
//! falls and footprints are real dents. Every change is one authored *layer edit* (a tile's worth
//! of cells in one transaction, see [`crate::stage::LayerWrite`]) through the normal edit
//! pipeline, so revisions, journal and replication are the ordinary ones.
//!
//! The accumulated height is not saved: snow already placed is ordinary terrain, and after a
//! restart new snowfall simply adds on top (up to [`MAX_LAYERS`] in a column).

use super::*;
use crate::stage::{LayerWrite, MAX_LAYER_WRITES};
use spall_core::{BRUSH_UNIT, BrushPoint, MaterialId, SphereBrush};
use std::collections::{HashMap, HashSet, VecDeque};

/// Most snow cells stacked on one column.
pub const MAX_LAYERS: u32 = 6;
/// Layers one tile edit adds or removes per column (`MAX_LAYER_WRITES` is 4 x 256 cells).
const LAYERS_PER_EDIT: u32 = (MAX_LAYER_WRITES / 256) as u32;
/// Tiles are 16 x 16 cells (4 m), like the thin snow's.
const TILE_CELLS: i64 = 16;
/// Layers are kept for tiles this many tiles from a player.
const RADIUS_TILES: i32 = 7;
/// Tile edits in flight at once. Every edit commits on the tick thread.
const MAX_IN_FLIGHT: usize = 2;
/// Free air above a column that counts as open sky, in cells.
const SKY_CELLS: i64 = 32;
/// Snow layers gained per second per unit of precipitation: heavy snow (0.85) adds one 0.25 m
/// cell about every 80 s.
const LAYERS_PER_S_PER_PRECIP: f64 = 0.0147;
/// Layers lost per second per degree C above freezing.
const LAYERS_PER_S_PER_DEGREE: f64 = 0.00083;
/// A column this close to a player (metres, horizontally) is left alone, so snow never lands
/// inside a capsule.
const PLAYER_CLEARANCE_M: f64 = 0.7;
/// Ticks between rebuilds of the visiting order.
const REORDER_TICKS: u64 = 30;
const ACTOR_ID: u64 = (1 << 40) + 3;

/// What the snow layer remembers; see the module docs for what is deliberately not saved.
#[derive(Default)]
pub(crate) struct SnowLayer {
    material: Option<MaterialId>,
    /// Accumulated snowfall, in layers.
    height: f64,
    /// Layers this session has added to each tile (removed again on melt).
    tiles: HashMap<(i32, i32), u32>,
    order: VecDeque<(i32, i32)>,
    ordered_at: u64,
    /// Request ids of edits not yet committed or rejected.
    pending: HashSet<u64>,
    next_seq: u64,
    /// Where each player's previous foot landed, for the dent it leaves.
    last_foot: HashMap<spall_core::EntityId, GlobalCell>,
}

impl SnowLayer {
    pub(crate) fn owns(&self, request: u64) -> bool {
        self.pending.contains(&request)
    }

    /// Whole layers fallen so far.
    pub(crate) fn target(&self) -> u32 {
        self.height.floor().clamp(0.0, f64::from(MAX_LAYERS)) as u32
    }

    /// Folds one tick of weather into the accumulated height: snowfall at or below freezing adds,
    /// warmth melts, anything else holds.
    fn weather_tick(&mut self, weather: &spall_protocol::WeatherState) {
        let snowing = matches!(
            weather.kind,
            spall_protocol::WeatherKind::Snow | spall_protocol::WeatherKind::Blizzard
        ) && weather.temperature_centi_c <= 0;
        let per_tick = 1.0 / 60.0;
        if snowing {
            let precipitation = f64::from(weather.precipitation_q16) / 65_535.0;
            self.height += precipitation * LAYERS_PER_S_PER_PRECIP * per_tick;
        } else if weather.temperature_centi_c > 0 {
            let degrees = f64::from(weather.temperature_centi_c) / 100.0;
            self.height -= degrees * LAYERS_PER_S_PER_DEGREE * per_tick;
        }
        self.height = self.height.clamp(0.0, f64::from(MAX_LAYERS) + 1.0);
    }
}

/// One column's snow: the top solid cell, and how many snow cells are stacked on the ground.
struct Column {
    top_y: i64,
    stack: u32,
}

/// The top solid cell of the column at (`x`, `z`) near `hint`, if it has open sky above it.
fn exposed_column(
    terrain: &spall_voxel::Volume,
    snow: MaterialId,
    x: i64,
    z: i64,
    hint: i64,
) -> Option<Column> {
    use spall_voxel::Sample;
    let mut y = hint;
    for _ in 0..64 {
        match terrain.sample(GlobalCell::new(x, y, z)).ok()? {
            Sample::Unknown(_) => return None,
            Sample::Filled(_) => y += 1,
            Sample::Empty { .. } => break,
        }
    }
    let mut descended = 0;
    let top_y = loop {
        if descended > 160 {
            return None;
        }
        match terrain.sample(GlobalCell::new(x, y, z)).ok()? {
            Sample::Unknown(_) => return None,
            Sample::Filled(_) => break y,
            Sample::Empty { .. } => {
                y -= 1;
                descended += 1;
            }
        }
    };
    for dy in 1..=SKY_CELLS {
        match terrain.sample(GlobalCell::new(x, top_y + dy, z)).ok()? {
            Sample::Empty { .. } => {}
            Sample::Filled(_) | Sample::Unknown(_) => return None,
        }
    }
    let mut stack = 0;
    while stack < MAX_LAYERS + LAYERS_PER_EDIT
        && matches!(
            terrain.sample(GlobalCell::new(x, top_y - i64::from(stack), z)),
            Ok(Sample::Filled(m)) if m == snow
        )
    {
        stack += 1;
    }
    Some(Column { top_y, stack })
}

impl Simulation {
    /// Installs the material snow layers are made of; `None` (the default) leaves terrain alone.
    pub fn set_snow_layer_material(&mut self, material: Option<MaterialId>) {
        self.snow_layer.material = material;
    }

    /// Accumulated snowfall in whole layers (for tests and tools).
    pub fn snow_layer_target(&self) -> u32 {
        self.snow_layer.target()
    }

    /// One tick: accumulate, settle finished edits, and start tile edits near players.
    pub(super) fn advance_snow_layer(&mut self, report: &TickReport) {
        let Some(snow) = self.snow_layer.material else {
            return;
        };
        let layer = &mut self.snow_layer;
        layer.weather_tick(&self.weather);
        for (request, _) in &report.committed {
            layer.pending.remove(&request.0);
        }
        for (request, _) in &report.rejected {
            layer.pending.remove(&request.0);
        }
        for request in &report.discarded_stale {
            layer.pending.remove(&request.0);
        }
        if self.world.terrain().cell_size() != spall_core::CellSizeCode::Quarter {
            return;
        }
        let players: Vec<[f64; 3]> = self
            .world
            .players()
            .map(|player| player.state.position_m)
            .collect();
        if players.is_empty() {
            return;
        }
        let tick = self.tick.get();
        if self.snow_layer.order.is_empty()
            || tick.saturating_sub(self.snow_layer.ordered_at) >= REORDER_TICKS
        {
            self.snow_layer.order = tiles_near(&players);
            self.snow_layer.ordered_at = tick;
        }
        let target = self.snow_layer.target();
        let warming = self.weather.temperature_centi_c > 0;
        let mut started = 0;
        let mut inspected = 0;
        while let Some(&tile) = self.snow_layer.order.front() {
            if self.snow_layer.pending.len() >= MAX_IN_FLIGHT || started >= 1 || inspected >= 64 {
                break;
            }
            inspected += 1;
            let done =
                *self
                    .snow_layer
                    .tiles
                    .entry(tile)
                    .or_insert(if warming { MAX_LAYERS } else { 0 });
            if done == target {
                self.snow_layer.order.pop_front();
                continue;
            }
            let adding = done < target;
            let layers = if adding {
                (target - done).min(LAYERS_PER_EDIT)
            } else {
                (done - target).min(LAYERS_PER_EDIT)
            };
            let writes = self.snow_tile_writes(tile, snow, layers, adding, &players);
            self.snow_layer.order.pop_front();
            let done_after = if adding { done + layers } else { done - layers };
            self.snow_layer.tiles.insert(tile, done_after);
            if done_after != target {
                // More layers to go: look at this tile again after the others.
                self.snow_layer.order.push_back(tile);
            }
            if !writes.is_empty() && self.submit_snow_layer(writes) {
                started += 1;
            }
        }
    }

    /// The cell writes that add or remove `layers` snow cells on every exposed column of `tile`.
    fn snow_tile_writes(
        &self,
        tile: (i32, i32),
        snow: MaterialId,
        layers: u32,
        adding: bool,
        players: &[[f64; 3]],
    ) -> Vec<LayerWrite> {
        let terrain = &self.world.terrain().volume;
        let water = self.column_water.as_ref().map(|w| w.frame());
        let sampler = water.as_ref().map(|frame| frame.sampler());
        let x0 = i64::from(tile.0) * TILE_CELLS;
        let z0 = i64::from(tile.1) * TILE_CELLS;
        let mut writes = Vec::new();
        let mut hint = (players[0][1] / 0.25).floor() as i64 + 8;
        for dz in 0..TILE_CELLS {
            for step in 0..TILE_CELLS {
                // Snake order: the next column's surface is near the last one's.
                let dx = if dz % 2 == 0 {
                    step
                } else {
                    TILE_CELLS - 1 - step
                };
                let (x, z) = (x0 + dx, z0 + dz);
                let (cx, cz) = ((x as f64 + 0.5) * 0.25, (z as f64 + 0.5) * 0.25);
                let Some(column) = exposed_column(terrain, snow, x, z, hint) else {
                    continue;
                };
                hint = column.top_y + 1;
                let top_m = (column.top_y as f64 + 1.0) * 0.25;
                if players.iter().any(|p| {
                    (p[0] - cx).hypot(p[2] - cz) < PLAYER_CLEARANCE_M
                        && p[1] > top_m - 3.0
                        && p[1] < top_m + 3.0
                }) {
                    continue;
                }
                if adding {
                    if sampler
                        .as_ref()
                        .and_then(|s| s.sample_at_m(cx, top_m + 0.01, cz))
                        .is_some_and(|surface| surface.surface_y > top_m + 0.02)
                    {
                        continue;
                    }
                    for k in 1..=layers.min(MAX_LAYERS.saturating_sub(column.stack)) {
                        writes.push(LayerWrite {
                            cell: GlobalCell::new(x, column.top_y + i64::from(k), z),
                            material: snow,
                            expect: MaterialId::AIR,
                        });
                    }
                } else {
                    for k in 0..layers.min(column.stack) {
                        writes.push(LayerWrite {
                            cell: GlobalCell::new(x, column.top_y - i64::from(k), z),
                            material: MaterialId::AIR,
                            expect: snow,
                        });
                    }
                }
            }
        }
        writes
    }

    /// Queues one layer edit around `tile`.
    fn submit_snow_layer(&mut self, writes: Vec<LayerWrite>) -> bool {
        let cell = writes[0].cell;
        let centre = |v: i64| v * BRUSH_UNIT + BRUSH_UNIT / 2;
        self.submit_snow_edit([centre(cell.x), centre(cell.y), centre(cell.z)], writes)
    }

    fn submit_snow_edit(&mut self, centre: [i64; 3], writes: Vec<LayerWrite>) -> bool {
        let Some(snow) = self.snow_layer.material else {
            return false;
        };
        let Ok(actor) = spall_core::EntityId::new(ACTOR_ID) else {
            return false;
        };
        let Ok(brush) =
            SphereBrush::new(BrushPoint::from_units(centre[0], centre[1], centre[2]), 0)
        else {
            return false;
        };
        let request_id = RequestId(SERVER_REQUEST_ID_BAND | (1 << 58) | self.snow_layer.next_seq);
        self.snow_layer.next_seq += 1;
        let intent = EditIntent {
            request_id,
            actor,
            target: EditTarget::Terrain,
            kind: EditKind::Place(snow),
            brush,
            explosion: None,
        };
        let layer: std::sync::Arc<[LayerWrite]> = writes.into();
        match self
            .pipeline
            .submit_with_layer(intent, Some(layer), &self.world)
        {
            Ok(_) => {
                self.snow_layer.pending.insert(request_id.0);
                true
            }
            Err(_) => false,
        }
    }

    /// A footfall dents the snow the previous foot stood in: it removes that cell when at least
    /// one snow cell stays beneath it.
    pub(super) fn dent_snow(&mut self, candidates: &[crate::footprints::FootfallCandidate]) {
        let Some(snow) = self.snow_layer.material else {
            return;
        };
        for candidate in candidates {
            let cell = GlobalCell::new(
                (candidate.position_m[0] / 0.25).floor() as i64,
                (candidate.position_m[1] / 0.25).floor() as i64 - 1,
                (candidate.position_m[2] / 0.25).floor() as i64,
            );
            let Some(previous) = self.snow_layer.last_foot.insert(candidate.player, cell) else {
                continue;
            };
            if previous == cell || self.snow_layer.pending.len() >= MAX_IN_FLIGHT {
                continue;
            }
            let terrain = &self.world.terrain().volume;
            let is_snow = |cell: GlobalCell| matches!(terrain.sample(cell), Ok(spall_voxel::Sample::Filled(m)) if m == snow);
            let above = GlobalCell::new(previous.x, previous.y + 1, previous.z);
            let below = GlobalCell::new(previous.x, previous.y - 1, previous.z);
            let open_above = matches!(terrain.sample(above), Ok(spall_voxel::Sample::Empty { .. }));
            if is_snow(previous) && is_snow(below) && open_above {
                let write = LayerWrite {
                    cell: previous,
                    material: MaterialId::AIR,
                    expect: snow,
                };
                let unit = BRUSH_UNIT;
                let centre = [
                    previous.x * unit + unit / 2,
                    previous.y * unit + unit / 2,
                    previous.z * unit + unit / 2,
                ];
                self.submit_snow_edit(centre, vec![write]);
            }
        }
    }
}

/// Tiles within [`RADIUS_TILES`] of any player, nearest first.
fn tiles_near(players: &[[f64; 3]]) -> VecDeque<(i32, i32)> {
    let mut tiles: HashMap<(i32, i32), f64> = HashMap::new();
    for player in players {
        let (px, pz) = (
            (player[0] / 4.0).floor() as i32,
            (player[2] / 4.0).floor() as i32,
        );
        for dz in -RADIUS_TILES..=RADIUS_TILES {
            for dx in -RADIUS_TILES..=RADIUS_TILES {
                let distance = f64::from(dx * dx + dz * dz);
                let tile = (px + dx, pz + dz);
                let entry = tiles.entry(tile).or_insert(distance);
                *entry = entry.min(distance);
            }
        }
    }
    let mut ordered: Vec<_> = tiles.into_iter().collect();
    ordered.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    ordered.into_iter().map(|(tile, _)| tile).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_protocol::{WeatherKind, WeatherState};

    fn weather(kind: WeatherKind, precipitation: u16, temp_c: i16) -> WeatherState {
        let mut state = WeatherState::initial(
            1,
            1,
            spall_protocol::WeatherClock {
                time_ms: 0,
                source: spall_protocol::WeatherClockSource::ServerFallback,
                start_season: 3,
                season_ms: 900_000,
            },
        );
        state.kind = kind;
        state.precipitation_q16 = precipitation;
        state.temperature_centi_c = temp_c * 100;
        state
    }

    #[test]
    fn height_grows_in_snowfall_holds_in_cold_calm_and_melts_when_warm() {
        let mut layer = SnowLayer::default();
        let heavy = weather(WeatherKind::Snow, 55_705, -8);
        for _ in 0..60 * 85 {
            layer.weather_tick(&heavy);
        }
        assert_eq!(layer.target(), 1, "about one cell in 85 s of heavy snow");
        for _ in 0..60 * 600 {
            layer.weather_tick(&heavy);
        }
        assert_eq!(layer.target(), MAX_LAYERS, "the stack is capped");
        let calm = weather(WeatherKind::Clear, 0, -8);
        layer.weather_tick(&calm);
        assert_eq!(layer.target(), MAX_LAYERS);
        let warm = weather(WeatherKind::Clear, 0, 20);
        for _ in 0..60 * 60 * 20 {
            layer.weather_tick(&warm);
        }
        assert_eq!(layer.target(), 0);
    }

    use crate::fixtures::{DIRT, flat_terrain_setup};
    use spall_voxel::{EditPlan, Sample};

    /// A 24 x 24 cell slab with 40 cells of resident air above it, snow made of [`DIRT`].
    fn snowy_sim() -> Simulation {
        let mut setup = flat_terrain_setup();
        setup
            .terrain
            .insert_brick(
                spall_core::BrickCoord::new(0, 1, 0),
                spall_voxel::Brick::empty(),
            )
            .unwrap();
        setup.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(23, 45, 23));
        let mut config = SimulationConfig::new(setup);
        config.snow_layer_material = Some(DIRT);
        let mut sim = Simulation::new(config).unwrap();
        sim.set_season(3).unwrap();
        sim.add_player(spall_core::player_entity_for(1), [1.5, 0.5, 1.5]);
        sim
    }

    fn cell(sim: &Simulation, x: i64, y: i64, z: i64) -> Sample {
        sim.world
            .terrain()
            .volume
            .sample(GlobalCell::new(x, y, z))
            .unwrap()
    }

    #[test]
    fn layers_stack_on_open_ground_away_from_the_player_and_replay_exactly() {
        let mut sim = snowy_sim();
        let mut replica = sim.world.terrain().volume.clone();
        sim.snow_layer.height = 2.5;
        let mut committed = Vec::new();
        for _ in 0..400 {
            let report = sim.tick().unwrap();
            committed.extend(report.committed.into_iter().map(|(_, c)| c.topology));
        }
        assert!(sim.snow_layer.pending.is_empty());
        // Ground tops at y = 1: two snow cells above an open column far from the player.
        for y in [2, 3] {
            assert!(
                matches!(cell(&sim, 20, y, 20), Sample::Filled(m) if m == DIRT),
                "y {y}"
            );
        }
        assert!(matches!(cell(&sim, 20, 4, 20), Sample::Empty { .. }));
        // The column under the player is left alone.
        assert!(matches!(cell(&sim, 6, 2, 6), Sample::Empty { .. }));
        assert!(matches!(cell(&sim, 12, 2, 12), Sample::Filled(m) if m == DIRT));
        assert!(!committed.is_empty());
        // A replica applying only the replicated runs ends with identical cells and revisions.
        for tx in &committed {
            let mut plan = EditPlan::new(replica.id());
            for op in &tx.ops {
                let spall_protocol::TopologyOp::CellRun {
                    start,
                    len,
                    material,
                    ..
                } = op
                else {
                    panic!("a layer commit replicates as cell runs only: {op:?}");
                };
                for i in 0..i64::from(*len) {
                    plan.set(GlobalCell::new(start.x + i, start.y, start.z), *material);
                }
            }
            replica.apply_edit(&plan).unwrap();
        }
        let server = &sim.world.terrain().volume;
        for z in 0..24 {
            for x in 0..24 {
                for y in 0..8 {
                    let c = GlobalCell::new(x, y, z);
                    assert_eq!(
                        server.sample(c).unwrap(),
                        replica.sample(c).unwrap(),
                        "{c:?}"
                    );
                }
            }
        }
        for brick in server.resident_brick_coords() {
            assert_eq!(
                server.brick_revision(brick).unwrap(),
                replica.brick_revision(brick).unwrap()
            );
        }
        sim.wait_for_retired();
    }

    #[test]
    fn a_footfall_dents_the_previous_foot_cell_but_leaves_a_snow_cell_beneath() {
        let mut sim = snowy_sim();
        sim.snow_layer.height = 2.5;
        for _ in 0..400 {
            sim.tick().unwrap();
        }
        let player = spall_core::player_entity_for(1);
        let foot = |x: f64| crate::footprints::FootfallCandidate {
            player,
            position_m: [x, 1.0, 3.125],
            heading_q15: 0,
            foot: 0,
            tick: 1,
        };
        // Standing on the top snow cell (y = 3) at x cell 12, then stepping to x cell 14.
        sim.dent_snow(&[foot(3.125)]);
        sim.dent_snow(&[foot(3.625)]);
        for _ in 0..10 {
            sim.tick().unwrap();
        }
        assert!(
            matches!(cell(&sim, 12, 3, 12), Sample::Empty { .. }),
            "dented"
        );
        assert!(
            matches!(cell(&sim, 12, 2, 12), Sample::Filled(m) if m == DIRT),
            "one cell stays"
        );
        assert!(
            matches!(cell(&sim, 14, 3, 12), Sample::Filled(m) if m == DIRT),
            "the new foot cell is untouched"
        );
        sim.wait_for_retired();
    }

    #[test]
    fn warm_weather_melts_the_stack_back_to_the_ground() {
        let mut sim = snowy_sim();
        sim.snow_layer.height = 2.5;
        for _ in 0..400 {
            sim.tick().unwrap();
        }
        assert!(matches!(cell(&sim, 20, 3, 20), Sample::Filled(m) if m == DIRT));
        sim.set_season(1).unwrap();
        sim.snow_layer.height = 0.0;
        for _ in 0..800 {
            sim.tick().unwrap();
        }
        assert!(matches!(cell(&sim, 20, 2, 20), Sample::Empty { .. }));
        assert!(
            matches!(cell(&sim, 20, 1, 20), Sample::Filled(_)),
            "the ground stays"
        );
        sim.wait_for_retired();
    }

    #[test]
    fn tiles_near_players_are_bounded_and_nearest_first() {
        let tiles = tiles_near(&[[2.0, 5.0, 2.0]]);
        assert_eq!(
            tiles.len(),
            ((2 * RADIUS_TILES + 1) * (2 * RADIUS_TILES + 1)) as usize
        );
        assert_eq!(tiles[0], (0, 0));
    }
}

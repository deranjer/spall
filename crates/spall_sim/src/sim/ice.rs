//! Winter ice (ENG-268): freezing settled shallow water, melting it after
//! winter, and cracking it under players. Planning and the crack roll live in
//! [`crate::ice`]; this drives them from the tick.
//!
//! Every change is a server-authored [`BoxReplace`] edit through the normal
//! pipeline, so clients see it as an ordinary terrain transaction: a warning
//! crack turns ice cells into the cracked material, a break turns ice and
//! cracked cells under the player into air.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use super::*;
use crate::column_water::RegionFrame;
use crate::ice::{CrackRoll, IceSettings};
use crate::intent::BoxReplace;

/// Journal provenance for server-authored ice edits (beside the dam gate,
/// vegetation and creature edit actors).
const ICE_ACTOR_ID: u64 = (1 << 40) + 4;
/// Water regions further than this from every player (m) are visited in turn
/// with the rest of the world; nearer ones every other tick.
const NEAR_PLAYER_M: f64 = 64.0;
/// Ticks between rebuilding the list of water regions near players.
const NEAR_REFRESH_TICKS: u64 = 60;
/// Ticks between freeze or melt edits: each is a full terrain commit (a few
/// ms on a 1 km world). A winter world starts frozen (`ice::freeze_at_rest`),
/// so play only adds ice where water settles later, a little at a time.
const ICE_EDIT_SPACING_TICKS: u64 = 120;
/// Ice cells looked for above a melting column's water.
const MELT_SEARCH_CELLS: i64 = 3;

/// Runtime state of winter ice, present once a game installs [`IceSettings`].
pub(crate) struct IceRuntime {
    settings: IceSettings,
    /// Global round-robin position over the water's regions.
    cursor: Option<spall_fluid::columns::RegionKey>,
    near: Vec<spall_fluid::columns::RegionKey>,
    near_next: usize,
    near_refreshed: Option<u64>,
    /// Each visited region's frame and the tick it was first seen unchanged.
    seen: HashMap<spall_fluid::columns::RegionKey, (Arc<RegionFrame>, u64)>,
    /// A settled region whose plan found the solver busy, tried again next tick.
    retry: Option<spall_fluid::columns::RegionKey>,
    /// Later support layers of the region being frozen.
    layers: VecDeque<Vec<BoxReplace>>,
    in_flight: Vec<RequestId>,
    /// The first tick the next freeze or melt edit may go in.
    next_edit: u64,
    /// The crack or break edit still queued for each player.
    player_edits: HashMap<EntityId, RequestId>,
}

impl IceRuntime {
    pub(super) fn settings(&self) -> &IceSettings {
        &self.settings
    }

    fn new(settings: IceSettings) -> Self {
        Self {
            settings,
            cursor: None,
            near: Vec::new(),
            near_next: 0,
            near_refreshed: None,
            seen: HashMap::new(),
            retry: None,
            layers: VecDeque::new(),
            in_flight: Vec::new(),
            next_edit: 0,
            player_edits: HashMap::new(),
        }
    }
}

impl Simulation {
    /// Turns winter ice on with the game's materials and thresholds. Without
    /// column water it has nothing to freeze.
    pub fn set_ice(&mut self, settings: IceSettings) {
        self.ice = Some(IceRuntime::new(settings));
    }

    /// Current season from the weather clock (0 spring .. 3 winter).
    fn season(&self) -> u64 {
        let w = &self.weather;
        (u64::from(w.start_season) + w.clock_ms / w.season_ms.max(1)) % 4
    }

    fn ice_request(&mut self) -> RequestId {
        let request = RequestId(SERVER_REQUEST_ID_BAND | self.next_damage_seq);
        self.next_damage_seq += 1;
        request
    }

    fn submit_ice(&mut self, replace: BoxReplace) -> Option<RequestId> {
        let request_id = self.ice_request();
        let intent = EditIntent {
            request_id,
            actor: EntityId::new(ICE_ACTOR_ID).expect("non-zero reserved actor id"),
            target: EditTarget::Terrain,
            kind: EditKind::Replace(replace),
            brush: replace.brush(),
            explosion: None,
        };
        self.submit(intent).ok().map(|_| request_id)
    }

    fn queued(&self, request: RequestId) -> bool {
        self.pipeline
            .action_status(request)
            .is_some_and(|s| s.outcome == spall_protocol::ActionOutcome::Queued)
    }

    /// One tick of ice: crack rolls for players on ice, then at most one region
    /// frozen or melted.
    pub(super) fn advance_ice(&mut self) {
        let Some(mut ice) = self.ice.take() else {
            return;
        };
        let span = crate::prof::Span::start("tick.ice");
        self.advance_ice_cracks(&mut ice);
        self.advance_ice_regions(&mut ice);
        drop(span);
        self.ice = Some(ice);
    }

    fn advance_ice_cracks(&mut self, ice: &mut IceRuntime) {
        let s = ice.settings;
        let cell_m = self.world.terrain().cell_size().metres();
        let players: Vec<_> = self
            .world
            .players()
            .map(|p| (p.entity, p.state, p.last_input))
            .collect();
        ice.player_edits
            .retain(|entity, _| players.iter().any(|(e, _, _)| e == entity));
        for (entity, state, input) in players {
            if let Some(&request) = ice.player_edits.get(&entity) {
                if self.queued(request) {
                    continue;
                }
                ice.player_edits.remove(&entity);
            }
            if !state.grounded {
                continue;
            }
            let terrain = &self.world.terrain().volume;
            let [x, y, z] = state.position_m;
            let feet = GlobalCell::new(
                (x / cell_m).floor() as i64,
                ((y - 0.05) / cell_m).floor() as i64,
                (z / cell_m).floor() as i64,
            );
            let material_at = |c: GlobalCell| match terrain.sample(c) {
                Ok(Sample::Filled(m)) => Some(m),
                _ => None,
            };
            let is_ice = |m: Option<MaterialId>| m == Some(s.ice) || m == Some(s.cracked);
            let top = material_at(feet);
            if !is_ice(top) {
                continue;
            }
            let mut ice_cells = 0u32;
            let mut below = feet;
            while is_ice(material_at(below)) && ice_cells < 8 {
                ice_cells += 1;
                below = GlobalCell::new(below.x, below.y - 1, below.z);
            }
            // Ice resting on the ground (shore crust) cannot drop anyone in.
            if !matches!(terrain.sample(below), Ok(Sample::Empty { .. })) {
                continue;
            }
            let cracked = top == Some(s.cracked);
            let moving = input.movement[0] != 0.0 || input.movement[2] != 0.0;
            let roll = CrackRoll {
                seed: self.weather_seed,
                tick: self.tick.get(),
                player: entity.get(),
                cell: feet,
                ice_cells,
                cracked,
                sprinting: moving && state.can_sprint(&input),
            };
            if !s.crack_roll(roll, f64::from(TICK_DT_S)) {
                continue;
            }
            let replace = if cracked {
                let r = s.break_radius_cells;
                let h = i64::from(ice_cells.max(2));
                BoxReplace::new(
                    GlobalCell::new(feet.x - r, feet.y - h + 1, feet.z - r),
                    [(2 * r + 1) as u8, h as u8, (2 * r + 1) as u8],
                    [s.ice, s.cracked],
                    MaterialId::AIR,
                )
            } else {
                let r = s.crack_radius_cells;
                BoxReplace::new(
                    GlobalCell::new(feet.x - r, feet.y, feet.z - r),
                    [(2 * r + 1) as u8, 1, (2 * r + 1) as u8],
                    [s.ice, s.ice],
                    s.cracked,
                )
            };
            if let Some(request) = self.submit_ice(replace) {
                ice.player_edits.insert(entity, request);
            }
        }
    }

    fn advance_ice_regions(&mut self, ice: &mut IceRuntime) {
        if self.column_water.is_none() {
            return;
        }
        ice.in_flight.retain(|r| {
            self.pipeline
                .action_status(*r)
                .is_some_and(|s| s.outcome == spall_protocol::ActionOutcome::Queued)
        });
        let may_edit =
            |ice: &IceRuntime, tick: u64| ice.in_flight.is_empty() && tick >= ice.next_edit;
        if may_edit(ice, self.tick.get())
            && let Some(layer) = ice.layers.pop_front()
        {
            self.submit_ice_layer(ice, layer);
            return;
        }
        let Some(water) = &self.column_water else {
            return;
        };
        let frame = water.frame();
        let winter = self.season() == 3;
        if !winter {
            ice.seen.clear();
        }
        let tick = self.tick.get();
        if ice
            .near_refreshed
            .is_none_or(|t| tick.saturating_sub(t) >= NEAR_REFRESH_TICKS)
        {
            ice.near_refreshed = Some(tick);
            let region_m = spall_fluid::columns::REGION_EDGE as f64
                * self.world.terrain().cell_size().metres();
            let players: Vec<[f64; 3]> = self.world.players().map(|p| p.state.position_m).collect();
            ice.near = frame
                .regions
                .keys()
                .copied()
                .filter(|k| {
                    let centre = [
                        (f64::from(k.0) + 0.5) * region_m,
                        (f64::from(k.1) + 0.5) * region_m,
                    ];
                    players.iter().any(|p| {
                        (p[0] - centre[0]).hypot(p[2] - centre[1]) < NEAR_PLAYER_M + region_m
                    })
                })
                .collect();
            ice.near_next = 0;
        }
        let key = if let Some(key) = ice.retry.take().filter(|_| winter) {
            key
        } else if tick.is_multiple_of(2) && !ice.near.is_empty() {
            let key = ice.near[ice.near_next % ice.near.len()];
            ice.near_next += 1;
            key
        } else {
            use std::ops::Bound::{Excluded, Unbounded};
            let next = match ice.cursor {
                Some(c) => frame.regions.range((Excluded(c), Unbounded)).next(),
                None => None,
            }
            .or_else(|| frame.regions.iter().next());
            let Some((&key, _)) = next else {
                return;
            };
            ice.cursor = Some(key);
            key
        };
        let Some(region) = frame.regions.get(&key).cloned() else {
            return;
        };
        if winter {
            let settle_ticks = (ice.settings.settle_s / f64::from(TICK_DT_S)) as u64;
            let since = match ice.seen.get(&key) {
                Some((seen, since)) if Arc::ptr_eq(seen, &region) => *since,
                _ => {
                    ice.seen.insert(key, (region, tick));
                    return;
                }
            };
            // One region's plan at a time.
            if tick.saturating_sub(since) < settle_ticks || !ice.layers.is_empty() {
                return;
            }
            let Some(layers) = self.plan_region_ice(&ice.settings, &region) else {
                ice.retry = Some(key);
                return;
            };
            ice.layers.extend(layers);
        } else if may_edit(ice, tick)
            && let Some(replace) = self.plan_region_melt(&ice.settings, &region)
            && let Some(request) = self.submit_ice(replace)
        {
            ice.in_flight.push(request);
            ice.next_edit = tick + ICE_EDIT_SPACING_TICKS;
        }
    }

    /// Submits the next box of a support layer, keeping only boxes that would still touch
    /// solid terrain now (an earlier box may have been refused).
    fn submit_ice_layer(&mut self, ice: &mut IceRuntime, layer: Vec<BoxReplace>) {
        let mut rest = Vec::new();
        for replace in layer {
            let terrain = &self.world.terrain().volume;
            let cells = replace.cells(terrain);
            let touches = cells.iter().any(|c| {
                [
                    (1, 0, 0),
                    (-1, 0, 0),
                    (0, 1, 0),
                    (0, -1, 0),
                    (0, 0, 1),
                    (0, 0, -1),
                ]
                .iter()
                .any(|(dx, dy, dz)| {
                    matches!(
                        terrain.sample(GlobalCell::new(c.x + dx, c.y + dy, c.z + dz)),
                        Ok(Sample::Filled(_))
                    )
                })
            });
            if cells.is_empty() || !touches {
                continue;
            }
            // One box per call; the rest of the layer goes on later ticks
            // (also when the queue is full).
            if !ice.in_flight.is_empty() {
                rest.push(replace);
                continue;
            }
            match self.submit_ice(replace) {
                Some(request) => {
                    ice.in_flight.push(request);
                    ice.next_edit = self.tick.get() + ICE_EDIT_SPACING_TICKS;
                }
                None => rest.push(replace),
            }
        }
        if !rest.is_empty() {
            ice.layers.push_front(rest);
        }
    }

    /// The ice for one settled water region, in support layers, or `None`
    /// when the solver is busy. At terrain resolution the published frame
    /// holds each column's exact water (depth to 1/128 cell); with wide columns
    /// the frame is blended, so the solver's own spans are read, without ever
    /// waiting for a step in flight.
    fn plan_region_ice(
        &self,
        settings: &IceSettings,
        region: &RegionFrame,
    ) -> Option<Vec<Vec<BoxReplace>>> {
        let water = self.column_water.as_ref()?;
        let world = if water.setup().config.column_cells > 1 {
            Some(water.try_world()?)
        } else {
            None
        };
        let players: Vec<[f64; 3]> = self.world.players().map(|p| p.state.position_m).collect();
        Some(crate::ice::plan_region(
            &self.world.terrain().volume,
            water.setup(),
            world.as_deref(),
            settings,
            region,
            &players,
        ))
    }

    /// One box clearing every ice and cracked cell just above this region's
    /// water (and the shore crust beside it), or `None` when there is none.
    fn plan_region_melt(&self, settings: &IceSettings, region: &RegionFrame) -> Option<BoxReplace> {
        let terrain = &self.world.terrain().volume;
        let edge = spall_fluid::columns::REGION_EDGE;
        let (x0, z0) = (
            i64::from(region.key.0) * edge,
            i64::from(region.key.1) * edge,
        );
        let mut lo = [i64::MAX; 3];
        let mut hi = [i64::MIN; 3];
        for span in &region.spans {
            let (x, z) = (
                x0 + i64::from(span.column) % edge,
                z0 + i64::from(span.column) / edge,
            );
            let top = i64::from(span.ceiling_y);
            for y in top..top + MELT_SEARCH_CELLS {
                if let Ok(Sample::Filled(m)) = terrain.sample(GlobalCell::new(x, y, z))
                    && (m == settings.ice || m == settings.cracked)
                {
                    for (i, v) in [x, y, z].into_iter().enumerate() {
                        lo[i] = lo[i].min(v);
                        hi[i] = hi[i].max(v);
                    }
                }
            }
        }
        if lo[0] > hi[0] {
            return None;
        }
        // One cell wider for the crust, within the region.
        let lo = [(lo[0] - 1).max(x0), lo[1], (lo[2] - 1).max(z0)];
        let hi = [
            (hi[0] + 1).min(x0 + edge - 1),
            hi[1],
            (hi[2] + 1).min(z0 + edge - 1),
        ];
        let size = [0, 1, 2].map(|i| (hi[i] - lo[i] + 1).clamp(1, 255) as u8);
        Some(BoxReplace::new(
            GlobalCell::new(lo[0], lo[1], lo[2]),
            size,
            [settings.ice, settings.cracked],
            MaterialId::AIR,
        ))
    }
}

//! The client's picture of the water surface (ENG-168).
//!
//! Replicated column water arrives as per-region spans a few times a second.
//! This keeps one persistent height tile per region and feeds the renderer a
//! camera-centred height field, so nothing is rebuilt per frame:
//!
//! * **Eased.** Displayed heights glide toward the server's latest values
//!   instead of jumping at the replication rate, which is what made disturbed
//!   water look like jagged, flickering waves.
//! * **Smoothed.** A small binomial kernel over neighbouring columns (across
//!   region borders) removes the quarter-metre cell noise of the simulation.
//! * **Agitation.** How fast a column's surface is moving, low-passed, drives
//!   ripples and foam in the shader.
//! * **Incremental.** Only tiles that changed are recomposed and only their
//!   rectangle is uploaded.
//!
//! Presentation only; nothing here feeds back into the simulation.

use std::collections::HashMap;
use std::time::Instant;

use spall_protocol::column_water_wire::WireSpan;
use spall_render::water::{NO_WATER, WaterField};

use crate::predict::CELL_M;

const EDGE: i32 = 32;
const TILE_CELLS: usize = (EDGE * EDGE) as usize;
/// Columns per replicated window, and windows per tile.
const WINDOW_COLUMNS: usize = 128;
const WINDOWS: usize = TILE_CELLS / WINDOW_COLUMNS;
/// Wire depth units per cell.
const DEPTH_SCALE: f32 = 128.0;
/// Time constant of the surface following the server's heights.
const EASE_S: f32 = 0.15;
/// Time constant of agitation fading once the surface settles.
const AGITATION_DECAY_S: f32 = 0.7;
/// Surface speed (m/s) that counts as fully agitated, and the speed below
/// which a surface is just settling or draining and shows no agitation at all.
const AGITATION_FULL_M_S: f32 = 0.9;
const AGITATION_FLOOR_M_S: f32 = 0.15;
/// Shortest interval a speed is measured over (the server publishes at 15 Hz).
const MIN_INTERVAL_S: f32 = 0.1;
/// Water appearing in a column deeper than this is a jet or a front, not a
/// shoreline creeping, and starts a little lively.
const LIVELY_DEPTH_M: f32 = 0.3;
/// A tile stops animating once every column is this close (m) to its target.
const SETTLED_M: f32 = 0.0015;
/// Smoothing kernel reach in cells and its binomial weights.
const KERNEL_RADIUS: i32 = 2;
const KERNEL: [f32; 5] = [1.0, 4.0, 6.0, 4.0, 1.0];
/// View radius the field is sized for until told otherwise.
pub const DEFAULT_VIEW_RADIUS_M: f32 = 96.0;
/// Slack beyond the view on each side before the field recentres, in cells
/// (a few regions, so it does not recentre every few steps).
const RECENTRE_SLACK_CELLS: i64 = 128;

/// Cells per side of the camera field for a view radius: the view on both
/// sides of the camera plus the slack, in whole regions.
/// `SPALL_WATER_TRACE=1` logs the client's water work that took over half a
/// millisecond, on the clock the frame trace uses, to find what makes a frame
/// long: `net` is the network thread applying a water update, `sync` the
/// window's per-frame water upload.
pub fn water_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SPALL_WATER_TRACE").is_some())
}

/// Shortest time between two easings of the surface.
const ADVANCE_INTERVAL_S: f64 = 1.0 / 60.0;

pub fn field_dim_for_radius(radius_m: f32) -> u32 {
    let radius_cells = (radius_m / CELL_M).ceil() as i64;
    let dim = 2 * (radius_cells + RECENTRE_SLACK_CELLS);
    (dim as u32).div_ceil(EDGE as u32) * EDGE as u32
}

/// A rectangle of field cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x0: u32,
    pub z0: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone)]
struct Tile {
    /// Server heights (metres) per column, NO_WATER if dry.
    target_top: Vec<f32>,
    /// Quantized water velocity `[x, z]` of the surface span in each column.
    flow: Vec<[i8; 2]>,
    bottom: Vec<f32>,
    /// Displayed heights, easing toward `target_top`.
    top: Vec<f32>,
    agitation: Vec<f32>,
    animating: bool,
    /// When each 128-column window last changed, for measuring how fast it moves.
    last_update_s: [f64; WINDOWS],
}

impl Tile {
    fn empty(now: f64) -> Self {
        Self {
            target_top: vec![NO_WATER; TILE_CELLS],
            flow: vec![[0; 2]; TILE_CELLS],
            bottom: vec![NO_WATER; TILE_CELLS],
            top: vec![NO_WATER; TILE_CELLS],
            agitation: vec![0.0; TILE_CELLS],
            animating: false,
            last_update_s: [now; WINDOWS],
        }
    }
}

/// One region's server heights, bottoms and currents, as prediction reads them.
struct QueryTile {
    top: Vec<f32>,
    bottom: Vec<f32>,
    flow: Vec<[i8; 2]>,
}

/// The replicated water as movement prediction needs it: the server's own
/// heights (not the eased, drawn ones) and currents per column. It lives behind
/// its own lock because the predictor samples it every tick and again for each
/// replayed input on every reconcile, while the render thread holds the
/// [`ColumnSurface`] for the whole of each frame's easing and smoothing; sharing
/// one lock made the prediction tick wait on rendering and fall behind.
#[derive(Default)]
pub struct WaterQuery {
    tiles: HashMap<(i32, i32), QueryTile>,
}

/// A [`WaterQuery`] shared between the network thread (writer) and the
/// predictor (reader).
#[derive(Clone, Default)]
pub struct SharedWaterQuery(std::sync::Arc<std::sync::RwLock<WaterQuery>>);

impl SharedWaterQuery {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, WaterQuery> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, WaterQuery> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }

    /// See [`WaterQuery::sample_at_m`].
    pub fn sample_at_m(&self, x_m: f64, y_m: f64, z_m: f64) -> Option<spall_physics::WaterSample> {
        self.read().sample_at_m(x_m, y_m, z_m)
    }
}

impl WaterQuery {
    /// The water at a point as a character in it feels it: the replicated
    /// surface and current, or `None` where the column is dry or the point lies
    /// below the water's floor. Open sky above the surface is assumed.
    pub fn sample_at_m(&self, x_m: f64, y_m: f64, z_m: f64) -> Option<spall_physics::WaterSample> {
        let cell = f64::from(CELL_M);
        let (cx, cz) = ((x_m / cell).floor() as i64, (z_m / cell).floor() as i64);
        let edge = i64::from(EDGE);
        let tile = self
            .tiles
            .get(&(cx.div_euclid(edge) as i32, cz.div_euclid(edge) as i32))?;
        let i = (cx.rem_euclid(edge) + cz.rem_euclid(edge) * edge) as usize;
        let top = tile.top[i];
        if !wet(top) || y_m < f64::from(tile.bottom[i]) - 0.5 * cell {
            return None;
        }
        Some(spall_physics::WaterSample {
            surface_y: f64::from(top),
            flow_m_s: spall_protocol::column_water_wire::dequantize_flow(tile.flow[i]),
        })
    }
}

fn wet(h: f32) -> bool {
    h > NO_WATER * 0.5
}

/// Persistent replicated-water surface plus the camera field built from it.
pub struct ColumnSurface {
    epoch: Instant,
    /// Cells per side of the camera field, and how many cells of water the
    /// camera must always have around it.
    dim: u32,
    view_cells: i64,
    tiles: HashMap<(i32, i32), Tile>,
    /// World cell at field cell (0, 0), and the CPU copy of the field.
    window_origin: Option<(i64, i64)>,
    field: WaterField,
    dirty: Vec<Rect>,
    needs_full_upload: bool,
    last_advance_s: f64,
    /// A region arrived since the last easing: it is composed at once, not at
    /// the next 60 Hz step.
    fresh: bool,
    /// The server's heights and currents for movement prediction, behind its
    /// own lock so the render thread's long hold of this struct never stalls
    /// the prediction tick (see [`WaterQuery`]).
    query: SharedWaterQuery,
}

impl Default for ColumnSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl ColumnSurface {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            dim: field_dim_for_radius(DEFAULT_VIEW_RADIUS_M),
            view_cells: (DEFAULT_VIEW_RADIUS_M / CELL_M).ceil() as i64,
            tiles: HashMap::new(),
            window_origin: None,
            field: WaterField::new(
                [0.0, 0.0],
                CELL_M,
                field_dim_for_radius(DEFAULT_VIEW_RADIUS_M),
            ),
            dirty: Vec::new(),
            needs_full_upload: false,
            last_advance_s: 0.0,
            fresh: false,
            query: SharedWaterQuery::default(),
        }
    }

    /// The handle movement prediction samples the water through.
    pub fn query(&self) -> SharedWaterQuery {
        SharedWaterQuery::clone(&self.query)
    }

    fn now(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }

    pub fn region_count(&self) -> usize {
        self.tiles.len()
    }

    /// Cells per side of the camera field.
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// Sizes the field for a view radius. The field is rebuilt empty at the
    /// next `recentre`; call this before water arrives (at startup).
    pub fn set_view_radius_m(&mut self, radius_m: f32) {
        self.dim = field_dim_for_radius(radius_m);
        self.view_cells = (radius_m / CELL_M).ceil() as i64;
        self.window_origin = None;
        self.field = WaterField::new([0.0, 0.0], CELL_M, self.dim);
        self.dirty.clear();
        self.needs_full_upload = false;
    }

    pub fn animating_tiles(&self) -> usize {
        self.tiles.values().filter(|t| t.animating).count()
    }

    /// The CPU copy of the camera field.
    pub fn field(&self) -> &WaterField {
        &self.field
    }

    pub fn clear(&mut self) {
        self.tiles.clear();
        self.query.write().tiles.clear();
        self.field = WaterField::new(self.field.origin_xz, CELL_M, self.dim);
        self.dirty.clear();
        self.needs_full_upload = self.window_origin.is_some();
    }

    /// Height (metres) of the replicated water surface over the point, from the
    /// server's heights (not the eased, displayed ones), or `None` where the
    /// column is dry or the point lies below the water's floor. Movement
    /// prediction uses this so the client swims against the surface the server
    /// does. Open sky above the surface is assumed.
    pub fn surface_at_m(&self, x_m: f64, y_m: f64, z_m: f64) -> Option<f64> {
        self.sample_at_m(x_m, y_m, z_m).map(|w| w.surface_y)
    }

    /// [`Self::surface_at_m`] with the replicated current: the water as a
    /// character in it feels it.
    pub fn sample_at_m(&self, x_m: f64, y_m: f64, z_m: f64) -> Option<spall_physics::WaterSample> {
        self.query.read().sample_at_m(x_m, y_m, z_m)
    }

    /// Sets one region from its replicated spans (all of its windows).
    pub fn set_region(&mut self, key: (i32, i32), spans: &[WireSpan]) {
        let now = self.now();
        self.set_region_at(key, spans, now);
    }

    pub fn set_region_at(&mut self, key: (i32, i32), spans: &[WireSpan], now: f64) {
        if spans.is_empty() {
            self.query.write().tiles.remove(&key);
            if self.tiles.remove(&key).is_some() {
                self.recompose_tile_region(key);
            }
            return;
        }
        self.set_columns(key, 0..TILE_CELLS, spans, now);
    }

    /// Sets one replicated window (128 columns) of a region from its spans,
    /// leaving the region's other columns as they are.
    pub fn set_window(&mut self, key: (i32, i32), window: u8, spans: &[WireSpan]) {
        let now = self.now();
        self.set_window_at(key, window, spans, now);
    }

    pub fn set_window_at(&mut self, key: (i32, i32), window: u8, spans: &[WireSpan], now: f64) {
        const WINDOW: usize = 128;
        let lo = usize::from(window) * WINDOW;
        if lo >= TILE_CELLS || (spans.is_empty() && !self.tiles.contains_key(&key)) {
            return;
        }
        self.set_columns(key, lo..lo + WINDOW, spans, now);
    }

    /// Drops a region that holds no water at all.
    pub fn remove_region(&mut self, key: (i32, i32)) {
        self.query.write().tiles.remove(&key);
        if self.tiles.remove(&key).is_some() {
            self.recompose_tile_region(key);
        }
    }

    /// Replaces the columns `range` of a tile from `spans`; columns outside it
    /// keep their current server heights.
    fn set_columns(
        &mut self,
        key: (i32, i32),
        range: std::ops::Range<usize>,
        spans: &[WireSpan],
        now: f64,
    ) {
        let (mut target, mut bottom, mut flow) = match self.tiles.get(&key) {
            Some(t) => (t.target_top.clone(), t.bottom.clone(), t.flow.clone()),
            None => (
                vec![NO_WATER; TILE_CELLS],
                vec![NO_WATER; TILE_CELLS],
                vec![[0; 2]; TILE_CELLS],
            ),
        };
        for i in range.clone() {
            target[i] = NO_WATER;
            bottom[i] = NO_WATER;
            flow[i] = [0; 2];
        }
        for s in spans {
            let i = usize::from(s.column);
            if !range.contains(&i) || s.depth_q == 0 {
                continue;
            }
            let top = (s.floor_y as f32 + f32::from(s.depth_q) / DEPTH_SCALE) * CELL_M;
            // Stacked water: the highest surface is the one that is drawn.
            if top > target[i] {
                target[i] = top;
                bottom[i] = s.floor_y as f32 * CELL_M;
                flow[i] = s.flow;
            }
        }
        let tile = self.tiles.entry(key).or_insert_with(|| Tile::empty(now));
        // Speed is measured over the time since *these columns'* window last
        // changed. Windows of one region arrive in the same instant, so a
        // per-tile clock gave every window after the first a near-zero interval
        // and made slow drains look like surges.
        let windows = range.start / WINDOW_COLUMNS..=(range.end - 1) / WINDOW_COLUMNS;
        let since = windows
            .clone()
            .map(|w| now - tile.last_update_s[w])
            .fold(0.0f64, f64::max);
        let dt = (since as f32).max(MIN_INTERVAL_S);
        for w in windows {
            tile.last_update_s[w] = now;
        }
        for i in range {
            let (old, new) = (tile.target_top[i], target[i]);
            match (wet(old), wet(new)) {
                (true, true) => {
                    // How fast the server surface just moved.
                    let speed = (new - old).abs() / dt;
                    let a = ((speed - AGITATION_FLOOR_M_S)
                        / (AGITATION_FULL_M_S - AGITATION_FLOOR_M_S))
                        .clamp(0.0, 1.0);
                    tile.agitation[i] = tile.agitation[i].max(a);
                }
                (false, true) => {
                    // New water appears at its server height, a little lively
                    // (a spring, or a front arriving).
                    tile.top[i] = new;
                    if new - bottom[i] > LIVELY_DEPTH_M {
                        tile.agitation[i] = tile.agitation[i].max(0.35);
                    }
                }
                (true, false) => {
                    tile.top[i] = NO_WATER;
                    tile.agitation[i] = 0.0;
                }
                (false, false) => {}
            }
        }
        self.query.write().tiles.insert(
            key,
            QueryTile {
                top: target.clone(),
                bottom: bottom.clone(),
                flow: flow.clone(),
            },
        );
        tile.target_top = target;
        tile.bottom = bottom;
        tile.flow = flow;
        // The tile is now animating, so the next `advance` recomposes and
        // reports it; doing it here as well would smooth every tile twice.
        tile.animating = true;
        self.fresh = true;
    }

    /// Eases every animating tile by the time since the last call and returns
    /// the rectangles of the field that changed (also those from `set_region`
    /// and `recentre` since the last call).
    pub fn advance(&mut self) -> Vec<Rect> {
        let now = self.now();
        self.advance_to(now)
    }

    pub fn advance_to(&mut self, now: f64) -> Vec<Rect> {
        // The surface eases at a steady 60 Hz however fast frames come: at 200
        // frames a second the same recomposition, a millisecond of the main
        // thread, would run three times as often to move heights by a third
        // as much, and the eye cannot tell.
        if !self.fresh && now - self.last_advance_s < ADVANCE_INTERVAL_S {
            return std::mem::take(&mut self.dirty);
        }
        let dt = (now - self.last_advance_s).clamp(0.0, 0.25) as f32;
        self.last_advance_s = now;
        self.fresh = false;
        let ease = 1.0 - (-dt / EASE_S).exp();
        let decay = (-dt / AGITATION_DECAY_S).exp();
        let mut keys = Vec::new();
        for (&key, tile) in &mut self.tiles {
            if !tile.animating {
                continue;
            }
            let mut moving = false;
            for i in 0..TILE_CELLS {
                let target = tile.target_top[i];
                if wet(target) && wet(tile.top[i]) {
                    let gap = target - tile.top[i];
                    if gap.abs() > SETTLED_M {
                        tile.top[i] += gap * ease;
                        moving = true;
                    } else {
                        tile.top[i] = target;
                    }
                }
                let a = &mut tile.agitation[i];
                if *a > 0.002 {
                    *a *= decay;
                    moving = true;
                } else {
                    *a = 0.0;
                }
            }
            tile.animating = moving;
            keys.push(key);
        }
        for key in keys {
            self.recompose_tile_region(key);
        }
        std::mem::take(&mut self.dirty)
    }

    /// Whether the field was replaced (recentred or cleared) since the last
    /// call; the renderer then needs the whole field, not rectangles.
    pub fn take_full_upload(&mut self) -> bool {
        std::mem::take(&mut self.needs_full_upload)
    }

    /// Keeps the camera field around `center_m`; returns true if it moved.
    pub fn recentre(&mut self, center_m: [f64; 3]) -> bool {
        let cx = (center_m[0] / f64::from(CELL_M)).floor() as i64;
        let cz = (center_m[2] / f64::from(CELL_M)).floor() as i64;
        let dim = i64::from(self.dim);
        if let Some((ox, oz)) = self.window_origin {
            // Recentre once the camera has less than a full view of field left
            // on any side, so water is always drawn out to the view distance.
            let margin = self.view_cells;
            let near_edge = |c: i64, o: i64| c - o < margin || o + dim - c < margin;
            if !near_edge(cx, ox) && !near_edge(cz, oz) {
                return false;
            }
        }
        let snap = |c: i64| (c - dim / 2).div_euclid(i64::from(EDGE)) * i64::from(EDGE);
        let origin = (snap(cx), snap(cz));
        if self.window_origin == Some(origin) {
            return false;
        }
        self.window_origin = Some(origin);
        self.field = WaterField::new(
            [origin.0 as f32 * CELL_M, origin.1 as f32 * CELL_M],
            CELL_M,
            self.dim,
        );
        self.dirty.clear();
        let keys: Vec<_> = self.tiles.keys().copied().collect();
        for key in keys {
            self.write_tile_raw(key);
        }
        let keys: Vec<_> = self.tiles.keys().copied().collect();
        for key in keys {
            self.smooth_around(key);
        }
        self.needs_full_upload = true;
        true
    }

    /// Field-cell rectangle a tile covers, clipped to the field.
    fn tile_rect(&self, key: (i32, i32), grow: i32) -> Option<Rect> {
        let (ox, oz) = self.window_origin?;
        let dim = i64::from(self.dim);
        let lo = |k: i32, o: i64| i64::from(k) * i64::from(EDGE) - o - i64::from(grow);
        let (x0, z0) = (lo(key.0, ox), lo(key.1, oz));
        let size = i64::from(EDGE) + 2 * i64::from(grow);
        let (cx0, cz0) = (x0.max(0), z0.max(0));
        let (cx1, cz1) = ((x0 + size).min(dim), (z0 + size).min(dim));
        (cx1 > cx0 && cz1 > cz0).then(|| Rect {
            x0: cx0 as u32,
            z0: cz0 as u32,
            w: (cx1 - cx0) as u32,
            h: (cz1 - cz0) as u32,
        })
    }

    /// Writes a tile's raw heights, bottoms and agitation into the field.
    /// A removed tile writes dry columns.
    fn write_tile_raw(&mut self, key: (i32, i32)) {
        let Some(own) = self.tile_rect(key, 0) else {
            return;
        };
        let (ox, oz) = self.window_origin.expect("rect implies a window");
        let dim = self.dim as usize;
        let tile = self.tiles.get(&key);
        for fz in own.z0..own.z0 + own.h {
            for fx in own.x0..own.x0 + own.w {
                let wx = i64::from(fx) + ox - i64::from(key.0) * i64::from(EDGE);
                let wz = i64::from(fz) + oz - i64::from(key.1) * i64::from(EDGE);
                let i = (wx + wz * i64::from(EDGE)) as usize;
                let f = fx as usize + dim * fz as usize;
                match tile {
                    Some(t) if wet(t.top[i]) => {
                        self.field.heights[f] = t.top[i];
                        self.field.bottoms[f] = t.bottom[i];
                        self.field.agitation[f] = t.agitation[i];
                        self.field.flow[f] = t.flow[i];
                    }
                    _ => {
                        self.field.heights[f] = NO_WATER;
                        self.field.bottoms[f] = NO_WATER;
                        self.field.agitation[f] = 0.0;
                        self.field.flow[f] = [0; 2];
                    }
                }
            }
        }
    }

    /// Recomputes the smoothed channel over a tile and the cells within the
    /// kernel's reach of it, and marks that rectangle dirty. The binomial
    /// kernel is separable, so this is a horizontal then a vertical pass over
    /// (height x wet, wet) sums instead of a 5 x 5 window per cell.
    fn smooth_around(&mut self, key: (i32, i32)) {
        let Some(rect) = self.tile_rect(key, KERNEL_RADIUS) else {
            return;
        };
        let dim = i64::from(self.dim);
        let (w, h) = (rect.w as usize, rect.h as usize);
        let r = i64::from(KERNEL_RADIUS);
        // Rows the vertical pass reaches: the rectangle plus the kernel radius.
        let row0 = (i64::from(rect.z0) - r).max(0);
        let row1 = (i64::from(rect.z0) + h as i64 + r).min(dim);
        let rows = (row1 - row0) as usize;
        let mut num = vec![0.0f32; rows * w];
        let mut den = vec![0.0f32; rows * w];
        for (ri, z) in (row0..row1).enumerate() {
            let line = &self.field.heights[(dim * z) as usize..(dim * (z + 1)) as usize];
            for xi in 0..w {
                let x = i64::from(rect.x0) + xi as i64;
                let (mut n, mut d) = (0.0f32, 0.0f32);
                for dx in -r..=r {
                    let px = x + dx;
                    if px < 0 || px >= dim {
                        continue;
                    }
                    let height = line[px as usize];
                    if wet(height) {
                        let k = KERNEL[(dx + r) as usize];
                        n += k * height;
                        d += k;
                    }
                }
                num[ri * w + xi] = n;
                den[ri * w + xi] = d;
            }
        }
        for zi in 0..h {
            let z = i64::from(rect.z0) + zi as i64;
            for xi in 0..w {
                let (mut n, mut d) = (0.0f32, 0.0f32);
                for dz in -r..=r {
                    let pz = z + dz;
                    if pz < row0 || pz >= row1 {
                        continue;
                    }
                    let ri = (pz - row0) as usize;
                    let k = KERNEL[(dz + r) as usize];
                    n += k * num[ri * w + xi];
                    d += k * den[ri * w + xi];
                }
                let f = (i64::from(rect.x0) + xi as i64 + dim * z) as usize;
                self.field.smooth[f] = if d > 0.0 { n / d } else { NO_WATER };
            }
        }
        self.dirty.push(rect);
    }

    fn recompose_tile_region(&mut self, key: (i32, i32)) {
        if self.window_origin.is_none() {
            return;
        }
        self.write_tile_raw(key);
        self.smooth_around(key);
    }

    /// The `(top, bottom, smooth, agitation + flow)` texels of a field rectangle.
    pub fn texels(&self, rect: Rect) -> Vec<[f32; 4]> {
        self.field.texel_rect(rect.x0, rect.z0, rect.w, rect.h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(column: u16, floor_y: i32, depth_q: u16) -> WireSpan {
        WireSpan {
            column,
            floor_y,
            depth_q,
            flow: [0; 2],
        }
    }

    /// A surface centred so tile (0, 0) sits inside the field.
    fn surface() -> ColumnSurface {
        let mut s = ColumnSurface::new();
        s.recentre([4.0, 0.0, 4.0]);
        s
    }

    /// Field cell of world column `(x, z)` for the test window.
    fn cell(s: &ColumnSurface, x: i64, z: i64) -> usize {
        let (ox, oz) = s.window_origin.unwrap();
        (x - ox + (z - oz) * i64::from(s.dim())) as usize
    }

    #[test]
    fn surface_query_returns_the_server_height_over_wet_columns_only() {
        let mut s = surface();
        // Column (3, 2): floor y 10 cells, 2.5 cells deep: top at 12.5 cells.
        s.set_region_at((0, 0), &[span(3 + 32 * 2, 10, 320)], 0.0);
        let cell_m = f64::from(CELL_M);
        let (x, z) = (3.5 * cell_m, 2.5 * cell_m);
        let top = s.surface_at_m(x, 11.0 * cell_m, z).expect("wet column");
        assert!((top - 12.5 * cell_m).abs() < 1e-4, "top {top}");
        assert!(
            s.surface_at_m(x, 5.0 * cell_m, z).is_none(),
            "below the floor"
        );
        assert!(
            s.surface_at_m(4.5 * cell_m, 11.0 * cell_m, z).is_none(),
            "dry neighbour"
        );
        assert!(s.surface_at_m(-3.0, 1.0, -3.0).is_none(), "no tile");
    }

    #[test]
    fn prediction_samples_the_water_while_the_surface_is_held() {
        use std::sync::Mutex;
        let mut s = surface();
        s.set_region_at((0, 0), &[span(3 + 32 * 2, 10, 320)], 0.0);
        let query = s.query();
        // The render thread holds the whole surface for a frame; the
        // predictor's handle must not wait for it.
        let held = Mutex::new(s);
        let _frame = held.lock().unwrap();
        let cell_m = f64::from(CELL_M);
        let sample = query.sample_at_m(3.5 * cell_m, 11.0 * cell_m, 2.5 * cell_m);
        assert!((sample.expect("wet").surface_y - 12.5 * cell_m).abs() < 1e-4);
        assert!(query.sample_at_m(100.0, 11.0, 100.0).is_none());
    }

    /// Prints what one frame's easing costs while a lake drains: the time the
    /// render thread holds the surface. `cargo test -p spall_client --release
    /// --lib easing_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn easing_cost_of_a_draining_lake() {
        let mut s = surface();
        let spans: Vec<WireSpan> = (0..1024).map(|c| span(c, 10, 640)).collect();
        for rx in -3..4 {
            for rz in -3..4 {
                s.set_region_at((rx, rz), &spans, 0.0);
            }
        }
        let shallower: Vec<WireSpan> = (0..1024).map(|c| span(c, 10, 600)).collect();
        for rx in -3..4 {
            for rz in -3..4 {
                s.set_region_at((rx, rz), &shallower, 0.1);
            }
        }
        let t = std::time::Instant::now();
        let rects = s.advance_to(0.2);
        println!(
            "advance over 49 animating regions: {:?}, {} rects",
            t.elapsed(),
            rects.len()
        );
    }

    #[test]
    fn the_sample_carries_the_replicated_current() {
        use spall_protocol::column_water_wire::quantize_flow;
        let mut s = surface();
        let running = WireSpan {
            flow: quantize_flow([1.5, -0.5]),
            ..span(3 + 32 * 2, 10, 320)
        };
        s.set_region_at((0, 0), &[running, span(4 + 32 * 2, 10, 320)], 0.0);
        let cell_m = f64::from(CELL_M);
        let at = |col: f64| {
            s.sample_at_m(col * cell_m, 11.0 * cell_m, 2.5 * cell_m)
                .unwrap()
        };
        assert_eq!(at(3.5).flow_m_s, [1.5, -0.5]);
        assert_eq!(at(4.5).flow_m_s, [0.0, 0.0], "still neighbour");
    }

    #[test]
    fn a_current_reaches_the_field_and_the_gpu_texel_and_stops_where_the_water_does() {
        use spall_protocol::column_water_wire::quantize_flow;
        let mut s = surface();
        let running = WireSpan {
            flow: quantize_flow([1.5, -0.5]),
            ..span(3 + 32 * 2, 10, 320)
        };
        s.set_region_at((0, 0), &[running, span(4 + 32 * 2, 10, 320)], 0.0);
        s.advance_to(0.0);
        let (moving, still) = (cell(&s, 3, 2), cell(&s, 4, 2));
        assert_eq!(
            s.field().flow[moving],
            running.flow,
            "the current reaches the field"
        );
        assert_eq!(s.field().flow[still], [0, 0], "still neighbour");
        // The texel the GPU reads carries it too.
        let dim = s.dim();
        let rect = Rect {
            x0: moving as u32 % dim,
            z0: moving as u32 / dim,
            w: 1,
            h: 1,
        };
        let (_, flow) = spall_render::water::unpack_agitation_flow(s.texels(rect)[0][3]);
        assert_eq!(flow, running.flow);
        // The water drains away: no current is left on a dry column.
        s.set_region_at((0, 0), &[], 1.0);
        s.advance_to(10.0);
        assert_eq!(
            s.field().flow[moving],
            [0, 0],
            "dry columns hold no current"
        );
    }

    #[test]
    fn spans_land_in_the_field_with_top_and_bottom() {
        let mut s = surface();
        // Column (lx 3, lz 2) of region (0,0): floor y 10, 2.5 cells deep.
        s.set_region_at((0, 0), &[span(3 + 32 * 2, 10, 320)], 0.0);
        s.advance_to(0.0);
        let f = s.field();
        let i = cell(&s, 3, 2);
        assert!((f.heights[i] - 12.5 * CELL_M).abs() < 1e-5);
        assert!((f.bottoms[i] - 10.0 * CELL_M).abs() < 1e-6);
        assert!(!wet(f.heights[cell(&s, 4, 2)]), "neighbours stay dry");
    }

    #[test]
    fn the_smoothed_surface_reaches_a_little_past_the_shore_and_no_further() {
        let mut s = surface();
        s.set_region_at((0, 0), &[span(10, 4, 256)], 0.0); // one wet column
        s.advance_to(0.0);
        let f = s.field();
        assert!(wet(f.smooth[cell(&s, 10, 0)]));
        assert!(
            wet(f.smooth[cell(&s, 12, 0)]),
            "two cells away still defined"
        );
        assert!(!wet(f.smooth[cell(&s, 13, 0)]), "three cells away is not");
        assert!(!wet(f.heights[cell(&s, 11, 0)]), "raw heights stay exact");
    }

    #[test]
    fn smoothing_crosses_region_borders_and_flattens_cell_noise() {
        let mut s = surface();
        // A flat pool over the border of regions (0,0) and (1,0), with one
        // column 2 cells higher in the middle of region 1.
        let flat: Vec<WireSpan> = (0..32u16)
            .flat_map(|z| (0..32u16).map(move |x| span(x + 32 * z, 4, 256)))
            .collect();
        let mut bumped = flat.clone();
        bumped[5 + 32 * 5] = span(5 + 32 * 5, 4, 256 + 2 * 128);
        s.set_region_at((0, 0), &flat, 0.0);
        s.set_region_at((1, 0), &bumped, 0.0);
        s.advance_to(0.0);
        let f = s.field();
        let flat_h = 6.0 * CELL_M;
        // Across the border the smoothed height is continuous: no step.
        let a = f.smooth[cell(&s, 31, 8)];
        let b = f.smooth[cell(&s, 32, 8)];
        assert!(
            (a - flat_h).abs() < 1e-5 && (b - flat_h).abs() < 1e-5,
            "{a} {b}"
        );
        // The one-column spike is spread out and lowered, not a needle.
        let spike = f.smooth[cell(&s, 32 + 5, 5)];
        assert!(
            spike < f.heights[cell(&s, 32 + 5, 5)] - 0.05,
            "spike {spike}"
        );
        assert!(spike > flat_h, "but it is still a bump: {spike}");
    }

    #[test]
    fn heights_ease_toward_the_server_instead_of_jumping() {
        let mut s = surface();
        s.set_region_at((0, 0), &[span(0, 4, 256)], 0.0);
        s.advance_to(0.01);
        let start = s.field().heights[cell(&s, 0, 0)];
        s.set_region_at((0, 0), &[span(0, 4, 256 + 256)], 0.05); // +2 cells
        let target = start + 2.0 * CELL_M;
        s.advance_to(0.10);
        let mid = s.field().heights[cell(&s, 0, 0)];
        assert!(mid > start && mid < target - 0.05, "{start} {mid} {target}");
        for k in 0..60 {
            s.advance_to(0.10 + 0.05 * f64::from(k));
        }
        assert!((s.field().heights[cell(&s, 0, 0)] - target).abs() < 0.01);
    }

    #[test]
    fn agitation_rises_with_motion_and_dies_when_still() {
        let mut s = surface();
        s.set_region_at((0, 0), &[span(0, 4, 256)], 0.0);
        let still = s.field().agitation[cell(&s, 0, 0)];
        s.set_region_at((0, 0), &[span(0, 4, 256 + 128)], 0.1); // 0.25 m in 0.1 s
        s.advance_to(0.1);
        let moving = s.field().agitation[cell(&s, 0, 0)];
        assert!(moving > still + 0.3, "{still} {moving}");
        for k in 0..400 {
            s.advance_to(0.1 + 0.05 * f64::from(k));
        }
        assert_eq!(s.field().agitation[cell(&s, 0, 0)], 0.0);
        assert_eq!(s.animating_tiles(), 0, "a settled surface costs nothing");
    }

    /// A region draining slowly, delivered window by window in the same instant
    /// (as the network does), must not look agitated: this is what lit up
    /// strips of the lake one after another.
    #[test]
    fn a_slow_drain_delivered_window_by_window_stays_calm() {
        let mut s = surface();
        let pool =
            |depth_q: u16| -> Vec<WireSpan> { (0..1024u16).map(|c| span(c, 4, depth_q)).collect() };
        s.set_region_at((0, 0), &pool(1280), 0.0);
        let window = |depth_q: u16, w: u16| -> Vec<WireSpan> {
            (w * 128..(w + 1) * 128)
                .map(|c| span(c, 4, depth_q))
                .collect()
        };
        // Every 0.25 s the surface falls 0.02 m (0.08 m/s): well under the floor.
        let mut depth_q = 1280.0f32;
        for step in 1..=20 {
            depth_q -= 0.02 / CELL_M * 128.0;
            let now = f64::from(step) * 0.25;
            for w in 0..8u16 {
                s.set_window_at((0, 0), w as u8, &window(depth_q as u16, w), now);
            }
            s.advance_to(now);
        }
        let worst = s.field().agitation.iter().fold(0.0f32, |m, &a| m.max(a));
        assert!(worst < 0.05, "a slow drain showed agitation {worst}");
    }

    /// A real surge (a metre a second) does register, window by window.
    #[test]
    fn a_fast_surge_is_agitated_in_every_window() {
        let mut s = surface();
        let pool =
            |depth_q: u16| -> Vec<WireSpan> { (0..1024u16).map(|c| span(c, 4, depth_q)).collect() };
        s.set_region_at((0, 0), &pool(256), 0.0);
        for w in 0..8u16 {
            let spans: Vec<WireSpan> = (w * 128..(w + 1) * 128)
                .map(|c| span(c, 4, 256 + 256))
                .collect();
            s.set_window_at((0, 0), w as u8, &spans, 0.5); // +0.5 m in 0.5 s
        }
        s.advance_to(0.5);
        let f = s.field();
        for w in 0..8usize {
            let col = w * 128 + 5;
            let a = f.agitation[cell(&s, (col % 32) as i64, (col / 32) as i64)];
            assert!(a > 0.3, "window {w} agitation {a}");
        }
    }

    #[test]
    fn only_changed_rectangles_are_reported_and_removed_regions_dry_up() {
        let mut s = surface();
        s.set_region_at((0, 0), &[span(0, 4, 256)], 0.0);
        let first = s.advance_to(0.0);
        assert!(!first.is_empty());
        for k in 1..=400 {
            s.advance_to(f64::from(k) * 0.05);
        }
        assert!(
            s.advance_to(30.0).is_empty(),
            "nothing changed, nothing to upload"
        );
        s.set_region_at((0, 0), &[], 31.0);
        let dirty = s.advance_to(31.0);
        assert!(!dirty.is_empty());
        assert!(!wet(s.field().heights[cell(&s, 0, 0)]));
        assert_eq!(s.region_count(), 0);
    }

    #[test]
    fn the_field_recentres_and_keeps_existing_water() {
        let mut s = ColumnSurface::new();
        s.recentre([0.0, 0.0, 0.0]);
        s.set_region_at((0, 0), &[span(0, 4, 256)], 0.0);
        assert!(!s.recentre([1.0, 0.0, 1.0]), "still well inside");
        assert!(s.recentre([300.0, 0.0, 0.0]), "far away recentres");
        assert!(s.take_full_upload());
        assert!(
            s.field().heights.iter().all(|&h| !wet(h)),
            "region 0 left the window"
        );
        assert!(s.recentre([0.0, 0.0, 0.0]));
        assert!(s.field().heights.iter().any(|&h| wet(h)), "and comes back");
    }

    /// Per-frame cost with a whole flooded valley animating: a 16 x 16 block of
    /// regions (65,536 columns, as large as the field) all changing every
    /// replication tick. Release numbers are printed; the bound is loose enough
    /// for a debug build.
    #[test]
    #[ignore = "timing; run with --release --ignored --nocapture"]
    fn a_whole_field_of_moving_water_stays_cheap_per_frame() {
        let mut s = ColumnSurface::new();
        s.recentre([64.0, 0.0, 64.0]);
        let (ox, oz) = s.window_origin.unwrap();
        let (rx0, rz0) = ((ox / 32) as i32, (oz / 32) as i32);
        let spans = |phase: f32| -> Vec<WireSpan> {
            (0..1024u16)
                .map(|c| {
                    let w = ((f32::from(c % 32) * 0.4 + phase).sin()
                        + (f32::from(c / 32) * 0.3).cos())
                        * 0.5;
                    span(c, 4, (256.0 + w * 40.0) as u16)
                })
                .collect()
        };
        let mut t = 0.0;
        let mut worst = 0.0f64;
        let mut total = 0.0f64;
        let frames = 120;
        for f in 0..frames {
            t += 1.0 / 60.0;
            if f % 3 == 0 {
                let started = Instant::now();
                for rz in 0..16 {
                    for rx in 0..16 {
                        s.set_region_at((rx0 + rx, rz0 + rz), &spans(t as f32 * 3.0), t);
                    }
                }
                println!(
                    "replication tick: {:.2} ms",
                    started.elapsed().as_secs_f64() * 1e3
                );
            }
            let started = Instant::now();
            let rects = s.advance_to(t);
            let ms = started.elapsed().as_secs_f64() * 1e3;
            total += ms;
            worst = worst.max(ms);
            let _ = rects;
        }
        println!(
            "advance: mean {:.2} ms, worst {:.2} ms over {frames} frames, {} tiles",
            total / f64::from(frames),
            worst,
            s.region_count()
        );
        assert!(worst < 250.0);
    }
}

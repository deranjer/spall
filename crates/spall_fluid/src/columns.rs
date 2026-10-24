//! Stacked water columns with virtual pipes, stored sparsely by region
//! (ENG-165 solver, ENG-166 regions).
//!
//! One column per terrain cell, each split into vertical open spans holding a
//! water volume. Spans in 4-neighbour columns whose open intervals overlap are
//! joined by a persistent flow `q` (cells^3/s) that accelerates with the head
//! difference. Water volume per span is the single conserved quantity; the
//! depth is `min(volume, capacity)` and any excess stays in the span as
//! "overfill", which raises its head and pushes the water out through its pipes.
//! Water is never deleted. Design: `docs/water-agent-handoff.md`.
//!
//! Columns are grouped into regions of 32 x 32. Only regions that hold water,
//! or touch a region that does, are allocated. A region is *active* or
//! *asleep*; a step costs time only for active regions and the border spans
//! they exchange water with. A region that is not resident (the terrain source
//! returns `None`) is a closed, frozen boundary: never a drain, never empty
//! space. Lengths are in cells (0.25 m), times in seconds. This module is
//! CPU-only with no dependency on bodies, networking or rendering.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use rayon::prelude::*;
use spall_core::GlobalCell;
use spall_voxel::Volume;

use crate::{DomainSpec, SolidBoundary};

mod allocation;
mod coarse;
pub use coarse::{COARSE_CELLS, CoarseVolumeTerrain};
mod water;

/// Edge length of one column cell in metres.
pub const CELL_SIZE_M: f64 = 0.25;
/// Columns per region edge. Matches the brick footprint.
pub const REGION_EDGE: i64 = 32;
const REGION_COLUMNS: usize = (REGION_EDGE * REGION_EDGE) as usize;

pub type RegionKey = (i32, i32);

pub fn region_of(x: i64, z: i64) -> RegionKey {
    (
        x.div_euclid(REGION_EDGE) as i32,
        z.div_euclid(REGION_EDGE) as i32,
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnConfig {
    /// Gravity in cells/s^2.
    pub gravity: f32,
    /// Linear flow damping, 1/s.
    pub damping_per_s: f32,
    /// Courant factor for the substep limit `dt <= cfl / sqrt(g * a_max)`.
    pub cfl: f32,
    /// Wetted depth below which a pipe carries no flow (thin film).
    pub min_film_depth: f32,
    /// Jacobi sweeps of the full-span pressure head per substep.
    pub relax_sweeps: u32,
    /// Upper bound on substeps per `step`; the rest is reported as skipped.
    pub max_substeps: u32,
    /// A region is quiet while every pipe touching it carries less than this.
    pub sleep_flow: f32,
    /// Consecutive quiet steps before a region sleeps.
    pub sleep_steps: u32,
    /// Distances (cells, from the nearest focus point, see
    /// [`ColumnWorld::set_focus`]) beyond which an active region is stepped only
    /// every 2nd, 4th and 8th step. The rest of the steps it rests: its water
    /// exchanges with neighbours through its edge but does not redistribute, so
    /// far water runs slower than real time rather than stopping (and none is
    /// created or lost). 0 in the first entry turns this off.
    pub throttle_from_cells: [f32; 3],
    /// Substeps a region that is near again may spend catching up on the time
    /// it rested, per step (0: no catching up, the water stays behind).
    pub catch_up_substeps: u32,
    /// EXPERIMENTAL, off by default (0). A flowing region freezes as a steady
    /// region when no span's volume moved by more than `steady_volume` over a
    /// window of this many steps. It is then
    /// not stepped, and every pipe holds its flow *averaged over the window*:
    /// that average is balanced by construction, and the river keeps its mean
    /// currents (a river sloshes around them). 0 disables steady sleep.
    pub steady_steps: u32,
    /// Largest change of any span's volume over the window, cells^3.
    pub steady_volume: f64,
    /// A span's volume must drift slower than `steady_drift_abs +
    /// steady_drift_rel * throughput` over the window (cells^3/s, throughput
    /// being half the sum of the mean flows through it). A river keeps its
    /// volumes while a lot of water passes; a pool that is still equalising
    /// changes by about what flows, and must not freeze.
    pub steady_drift_abs: f32,
    pub steady_drift_rel: f32,
    /// The averaged flows must leave no span unbalanced by more than this
    /// (|inflow - outflow|, cells^3/s).
    pub steady_net: f32,
    /// A steady region wakes when a span on its boundary has gained or lost more
    /// than this many cells^3 since it froze (its neighbour is changing), or
    /// when a boundary flow leaves the band it moved in during the window, which
    /// is `steady_wake_abs + steady_wake_rel * |mean| + steady_wake_sigmas` times
    /// the flow's standard deviation over the window (a sudden change).
    pub steady_wake_volume: f64,
    pub steady_wake_abs: f32,
    pub steady_wake_rel: f32,
    pub steady_wake_sigmas: f32,
    /// Keep deep still sea as a record, not as solver regions. A region that
    /// `seed_levels` finds wholly under one flat sea level, with every
    /// neighbour the same, holds a few KiB instead of the ~200 KiB a solver
    /// region costs; it becomes a real region the moment anything touches it
    /// (an edit, a spring or drain, a neighbour that starts moving, a restore).
    pub lazy_ocean: bool,
    /// Terrain cells per column edge (1: a column per terrain cell). With 4,
    /// a column covers 4 x 4 terrain cells (1 m) and its spans keep the
    /// terrain's vertical resolution; heads stay in vertical cells, so a span's
    /// stored `volume` is its depth and a volume moved is divided by the column
    /// area. Callers give wide column x/z (see `CoarseVolumeTerrain` and
    /// `ColumnWorld::world_cell`).
    pub column_cells: u32,
}

impl Default for ColumnConfig {
    fn default() -> Self {
        Self {
            gravity: (9.81 / CELL_SIZE_M) as f32,
            // Tuned on a deep lake drained continuously (tests/column_drain.rs):
            // a safety factor of 0.5 left waves growing without bound there, 0.4
            // is stable, and 0.35 keeps a margin. Damping stays modest so surges
            // keep their momentum.
            damping_per_s: 2.0,
            cfl: 0.35,
            min_film_depth: 1.0 / 64.0,
            relax_sweeps: 4,
            max_substeps: 64,
            sleep_flow: 1.0e-3,
            sleep_steps: 20,
            throttle_from_cells: [0.0; 3],
            catch_up_substeps: 0,
            steady_steps: 0,
            steady_volume: 0.2,
            steady_drift_abs: 2.0e-5,
            steady_drift_rel: 0.05,
            steady_net: 0.05,
            steady_wake_volume: 0.6,
            steady_wake_abs: 0.02,
            steady_wake_rel: 0.1,
            steady_wake_sigmas: 6.0,
            lazy_ocean: false,
            column_cells: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ColumnError {
    #[error("terrain boundary covers a different domain than the column world")]
    DomainMismatch,
    #[error("cell {0:?} is solid, outside the world, or in a region that is not resident")]
    NotOpen(GlobalCell),
    #[error("water amount must be finite and non-negative")]
    InvalidAmount,
}

/// Solid occupancy of one region's columns over the world's full height.
#[derive(Debug, Clone)]
pub struct RegionTerrain {
    /// Solid flags indexed `lx + 32 * (y_rel + height * lz)`.
    pub solid: Vec<bool>,
}

/// Where terrain comes from. All regions share one vertical range; the top of
/// that range is open sky.
pub trait TerrainSource: Send + Sync {
    /// Global y range `[lo, hi)` covered by every region.
    fn y_range(&self) -> (i32, i32);
    /// The region's solid mask, or `None` if its terrain is not resident.
    fn region(&self, key: RegionKey) -> Option<RegionTerrain>;
}

/// A captured dense boundary as a terrain source; columns outside its domain
/// are solid.
#[derive(Debug, Clone)]
pub struct BoundaryTerrain {
    spec: DomainSpec,
    solid: Arc<Vec<bool>>,
}

impl BoundaryTerrain {
    pub fn new(boundary: &SolidBoundary) -> Self {
        Self {
            spec: boundary.spec(),
            solid: Arc::new(boundary.solid.clone()),
        }
    }
}

impl TerrainSource for BoundaryTerrain {
    fn y_range(&self) -> (i32, i32) {
        let o = self.spec.origin().y as i32;
        (o, o + self.spec.dimensions()[1] as i32)
    }

    fn region(&self, key: RegionKey) -> Option<RegionTerrain> {
        let [nx, ny, nz] = self.spec.dimensions().map(|d| d as i64);
        let o = self.spec.origin();
        let (x0, z0) = (
            i64::from(key.0) * REGION_EDGE,
            i64::from(key.1) * REGION_EDGE,
        );
        let mut solid = vec![true; REGION_COLUMNS * ny as usize];
        for lz in 0..REGION_EDGE {
            for lx in 0..REGION_EDGE {
                let (cx, cz) = (x0 + lx - o.x, z0 + lz - o.z);
                if cx < 0 || cz < 0 || cx >= nx || cz >= nz {
                    continue;
                }
                for y in 0..ny {
                    solid[(lx + REGION_EDGE * (y + ny * lz)) as usize] =
                        self.solid[(cx + nx * (y + ny * cz)) as usize];
                }
            }
        }
        Some(RegionTerrain { solid })
    }
}

/// Terrain read from a voxel volume. A brick that is not resident makes its
/// whole region non-resident.
pub struct VolumeTerrain<V> {
    volume: V,
    y_lo: i32,
    y_hi: i32,
}

impl<V: std::ops::Deref<Target = Volume> + Send + Sync> VolumeTerrain<V> {
    /// `y_lo..y_hi` is the global cell range (multiples of the brick edge).
    pub fn new(volume: V, y_lo: i32, y_hi: i32) -> Self {
        assert!(
            i64::from(y_lo) % REGION_EDGE == 0 && i64::from(y_hi) % REGION_EDGE == 0,
            "VolumeTerrain range must be brick aligned"
        );
        Self { volume, y_lo, y_hi }
    }
}

impl<V: std::ops::Deref<Target = Volume> + Send + Sync> TerrainSource for VolumeTerrain<V> {
    fn y_range(&self) -> (i32, i32) {
        (self.y_lo, self.y_hi)
    }

    fn region(&self, key: RegionKey) -> Option<RegionTerrain> {
        let height = (self.y_hi - self.y_lo) as usize;
        let edge = REGION_EDGE as usize;
        let mut solid = vec![false; REGION_COLUMNS * height];
        let (by_lo, by_hi) = (
            i64::from(self.y_lo) / REGION_EDGE,
            i64::from(self.y_hi) / REGION_EDGE,
        );
        for by in by_lo..by_hi {
            let coord = spall_core::BrickCoord::new(i64::from(key.0), by, i64::from(key.1));
            let snapshot = self.volume.snapshot_brick(coord).ok()??;
            let uniform = (!snapshot.is_dense()).then(|| {
                let zero = spall_core::LocalCell::new(0, 0, 0).expect("in-brick");
                snapshot.get(zero) != spall_core::MaterialId::AIR
            });
            let y_base = (by * REGION_EDGE) as usize - self.y_lo as usize;
            for lz in 0..edge {
                for ly in 0..edge {
                    let row = edge * (y_base + ly + height * lz);
                    if let Some(is_solid) = uniform {
                        solid[row..row + edge].fill(is_solid);
                        continue;
                    }
                    for lx in 0..edge {
                        let local = spall_core::LocalCell::new(lx as u8, ly as u8, lz as u8)
                            .expect("in-brick");
                        solid[row + lx] = snapshot.get(local) != spall_core::MaterialId::AIR;
                    }
                }
            }
        }
        Some(RegionTerrain { solid })
    }
}

/// An open vertical run in one column; `y0..y1` are global cell rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    y0: i32,
    y1: i32,
    /// Reaches the world top, which counts as open sky: never pressurised.
    sky: bool,
}

impl Span {
    fn capacity(self) -> f64 {
        f64::from(self.y1 - self.y0)
    }
}

#[derive(Debug, Clone, Copy)]
struct Pipe {
    a: u32,
    b: u32,
    lo: i32,
    hi: i32,
    /// Region slots of the two ends.
    ra: u32,
    rb: u32,
    /// 0 when `b` is the +x neighbour of `a`, 1 for +z.
    axis: u8,
}

#[derive(Debug, Clone)]
struct Region {
    key: RegionKey,
    span_start: u32,
    span_end: u32,
    /// Pipes with an end in this region.
    pipes: Vec<u32>,
    active: bool,
    quiet_steps: u32,
    /// Neighbours have been allocated (or found non-resident).
    ring_done: bool,
    frozen: bool,
    /// Resting this step: far from every focus point, so stepped only every
    /// `period` steps (see [`ColumnConfig::throttle_from_cells`]).
    resting: bool,
    period: u8,
    /// Time this region rested while it was active, seconds: how far behind
    /// real time its water is.
    owed_s: f32,
    /// Flowing but balanced: not stepped, flows and volumes held (see
    /// [`ColumnConfig::steady_steps`]).
    steady: bool,
    steady_steps: u32,
    /// Simulated seconds in the current window.
    steady_time: f32,
    /// What the live boundary pipes moved into (+) or out of (-) this region
    /// while it was frozen. Its boundary spans are fixed reservoirs, so the
    /// exchange is held here instead of changing their volume; it is applied
    /// when the region wakes and counts in [`ColumnWorld::total_volume`].
    steady_ledger: f64,
    /// Span volumes when the current steady window began, or when the region
    /// froze (empty: no window yet).
    steady_snap: Vec<f64>,
    /// Sum over the window of each of `pipes`' flows, and of their squares.
    steady_qsum: Vec<f32>,
    steady_qsq: Vec<f32>,
    /// Boundary pipes with their mean flow and the band it moved in over the
    /// window, sorted by pipe.
    steady_ref: Vec<(u32, f32, f32)>,
}

/// A sleeping region wakes when water in one of its border spans stands this
/// far (cells) above or below the span next to it inside the region. A live
/// pipe from an active neighbour moves water into (or out of) that border span,
/// but the interior is not stepped: before this check a river reaching a
/// sleeping region over many pipes, each under the sleep flow, filled its
/// first column and never woke it, and the region upstream filled behind it
/// as a flat pool with a straight wall.
const BORDER_WAKE_HEAD: f32 = 0.5;

/// A region of still sea kept as a record (see [`ColumnConfig::lazy_ocean`]).
struct Dormant {
    /// Per column: the floor y and the height of the span the sea fills.
    cols: Vec<(i32, i32)>,
    /// The sea fills each span up to and including this cell row.
    top: i64,
    /// The water held, cells^3.
    volume: f64,
}

impl Dormant {
    fn column_volume(&self, c: usize) -> f64 {
        let (y0, height) = self.cols[c];
        (((self.top + 1) as f64) - f64::from(y0)).clamp(0.0, f64::from(height))
    }
}

/// Read-only description of one span for tests and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpanView {
    /// Dense solver-span index, or `usize::MAX` for a dormant-ocean record
    /// returned through `visit_spans_in_column`.
    pub index: usize,
    /// Global y of the floor (first open cell) and one past the last open cell.
    pub floor_y: i64,
    pub ceiling_y: i64,
    pub sky: bool,
    /// Water volume in cells^3, including any overfill.
    pub volume: f64,
    /// Water depth, capped at the span height.
    pub depth: f64,
    /// Volume beyond the span height (pressure ledger).
    pub overfill: f64,
    /// Free-surface height as a global y; for a pressurised span, its head.
    pub head_y: f64,
}

/// One active region's state, see [`ColumnWorld::active_regions_diag`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveRegionDiag {
    pub key: RegionKey,
    pub max_flow: f32,
    pub quiet_steps: u32,
    pub resting: bool,
    pub steady: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ColumnStepMetrics {
    pub substeps: u32,
    /// Regions, spans and pipes that did work this step.
    pub active_regions: usize,
    /// Active regions that rested this step (far from every focus point).
    pub resting_regions: usize,
    /// Regions that spent substeps catching up on rested time this step.
    pub caught_up_regions: usize,
    pub active_spans: usize,
    pub active_pipes: usize,
    pub regions_slept: usize,
    pub regions_woken: usize,
    /// Regions that froze as steady this step, and how many are steady now.
    pub regions_steadied: usize,
    pub steady_regions: usize,
    /// Largest |q| / wetted area over the active pipes at the end, m/s.
    pub peak_speed_m_s: f32,
    pub max_abs_flow: f32,
    /// Fixed time not simulated because `max_substeps` was reached, seconds.
    pub skipped_s: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RemapReport {
    pub columns_changed: usize,
    /// Volume that no longer fit where it was and moved to another span.
    pub relocated_volume: f64,
    /// Volume held in the world ledger because no span could take it.
    pub unplaced_volume: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryReport {
    pub regions: usize,
    pub columns: usize,
    pub spans: usize,
    pub pipes: usize,
    pub bytes: usize,
}

pub struct ColumnWorld {
    terrain: Arc<dyn TerrainSource>,
    /// Dense worlds keep their captured mask to diff against the next one.
    dense: Option<(DomainSpec, Arc<Vec<bool>>)>,
    config: ColumnConfig,
    regions: Vec<Region>,
    slot_of: HashMap<RegionKey, u32>,
    /// Neighbour regions seen non-resident, retried on `notify_resident`.
    waiting: BTreeSet<RegionKey>,
    col_start: Vec<u32>,
    spans: Vec<Span>,
    pipes: Vec<Pipe>,
    adj_start: Vec<u32>,
    /// `pipe << 1 | is_a`.
    adj: Vec<u32>,
    volume: Vec<f64>,
    head: Vec<f32>,
    eff: Vec<f32>,
    scale: Vec<f64>,
    q: Vec<f32>,
    unplaced: f64,
    act_spans: Vec<u32>,
    act_pipes: Vec<u32>,
    /// Whether each pipe is in `act_pipes`. A span at the edge of a steady region
    /// also touches that region's frozen pipes, whose flow must not move water
    /// because their other end is not stepped.
    live: Vec<bool>,
    /// Whether each span belongs to a steady region, and the ranges flagged so
    /// they can be cleared without touching the whole vector.
    steady_span: Vec<bool>,
    steady_flagged: Vec<(u32, u32)>,
    /// Spans of steady regions that live pipes touch, with their region.
    steady_boundary: Vec<(u32, u32)>,
    /// Near-full active spans for the current step.
    nodes: Vec<u32>,
    /// Regions whose water changed since `take_dirty_regions`.
    dirty: BTreeSet<RegionKey>,
    /// Regions touched by this step's active lists (rebuilt with them).
    touched: Vec<u32>,
    /// Largest wetted pipe area seen, for the substep limit.
    a_hint: f32,
    /// Points (cell x, z) the water is kept up to date around, and the number
    /// of steps taken: see [`ColumnConfig::throttle_from_cells`].
    focus: Vec<[f64; 2]>,
    step_counter: u64,
    /// While catching a region up, the only one stepped.
    only: Option<u32>,
    lists_dirty: bool,
    /// Deep still sea held as records, by region; and the water in them.
    dormant: HashMap<RegionKey, Dormant>,
    dormant_volume: f64,
}

fn span_key(x: i64, z: i64, y0: i32) -> u64 {
    ((x as u64 & 0xF_FFFF) << 44) | ((z as u64 & 0xF_FFFF) << 24) | (y0 as u64 & 0xFF_FFFF)
}

fn column_xz(key: RegionKey, local: usize) -> (i64, i64) {
    (
        i64::from(key.0) * REGION_EDGE + (local % REGION_EDGE as usize) as i64,
        i64::from(key.1) * REGION_EDGE + (local / REGION_EDGE as usize) as i64,
    )
}

/// How many steps a region at `key` takes one in, from its distance to the
/// nearest focus point (see [`ColumnConfig::throttle_from_cells`]): 1, 2, 4
/// or 8, and 1 with no focus or the bands off.
fn pace_at(focus: &[[f64; 2]], bands: [f32; 3], key: RegionKey) -> u64 {
    if bands[0] <= 0.0 || focus.is_empty() {
        return 1;
    }
    let centre = [
        f64::from(key.0) * REGION_EDGE as f64 + 0.5 * REGION_EDGE as f64,
        f64::from(key.1) * REGION_EDGE as f64 + 0.5 * REGION_EDGE as f64,
    ];
    let distance = focus
        .iter()
        .map(|p| (p[0] - centre[0]).hypot(p[1] - centre[1]))
        .fold(f64::INFINITY, f64::min);
    if distance >= f64::from(bands[2]) {
        8
    } else if distance >= f64::from(bands[1]) {
        4
    } else if distance >= f64::from(bands[0]) {
        2
    } else {
        1
    }
}

/// The open vertical runs `(floor y, one past the top)` of each of a region's
/// columns (`lx + 32 * lz`), bottom to top.
pub fn open_runs(terrain: &RegionTerrain, y_lo: i32, y_hi: i32) -> Vec<Vec<(i32, i32)>> {
    let (mut spans, mut col_start) = (Vec::new(), Vec::new());
    extract_columns(terrain, y_lo, y_hi, &mut spans, &mut col_start);
    col_start.push(spans.len() as u32);
    col_start
        .windows(2)
        .map(|w| {
            spans[w[0] as usize..w[1] as usize]
                .iter()
                .map(|s| (s.y0, s.y1))
                .collect()
        })
        .collect()
}

fn extract_columns(
    terrain: &RegionTerrain,
    y_lo: i32,
    y_hi: i32,
    spans: &mut Vec<Span>,
    col_start: &mut Vec<u32>,
) {
    let h = (y_hi - y_lo) as usize;
    let edge = REGION_EDGE as usize;
    for lz in 0..edge {
        for lx in 0..edge {
            col_start.push(spans.len() as u32);
            let at = |y: usize| terrain.solid[lx + edge * (y + h * lz)];
            let mut y = 0;
            while y < h {
                if at(y) {
                    y += 1;
                    continue;
                }
                let start = y;
                while y < h && !at(y) {
                    y += 1;
                }
                spans.push(Span {
                    y0: y_lo + start as i32,
                    y1: y_lo + y as i32,
                    sky: y == h,
                });
            }
        }
    }
}

impl ColumnWorld {
    /// A dense world over a captured boundary: every region overlapping the
    /// domain is allocated and awake. Columns outside the domain are solid.
    pub fn new(boundary: &SolidBoundary, config: ColumnConfig) -> Self {
        let terrain = BoundaryTerrain::new(boundary);
        let dense = Some((boundary.spec(), terrain.solid.clone()));
        let mut world = Self::with_terrain(Arc::new(terrain), config);
        world.dense = dense;
        let spec = boundary.spec();
        let o = spec.origin();
        let d = spec.dimensions();
        let (lo, hi) = (
            region_of(o.x, o.z),
            region_of(o.x + i64::from(d[0]) - 1, o.z + i64::from(d[2]) - 1),
        );
        let keys: Vec<_> = (lo.1..=hi.1)
            .flat_map(|rz| (lo.0..=hi.0).map(move |rx| (rx, rz)))
            .collect();
        world.allocate(&keys, &BTreeSet::new());
        for r in &mut world.regions {
            r.active = true;
            r.ring_done = true;
        }
        world.lists_dirty = true;
        world
    }

    /// An empty sparse world; regions are allocated as water appears.
    pub fn with_terrain(terrain: Arc<dyn TerrainSource>, config: ColumnConfig) -> Self {
        Self {
            terrain,
            dense: None,
            config,
            regions: Vec::new(),
            slot_of: HashMap::new(),
            waiting: BTreeSet::new(),
            col_start: vec![0],
            spans: Vec::new(),
            pipes: Vec::new(),
            adj_start: vec![0],
            adj: Vec::new(),
            volume: Vec::new(),
            head: Vec::new(),
            eff: Vec::new(),
            scale: Vec::new(),
            q: Vec::new(),
            unplaced: 0.0,
            act_spans: Vec::new(),
            act_pipes: Vec::new(),
            live: Vec::new(),
            steady_span: Vec::new(),
            steady_flagged: Vec::new(),
            steady_boundary: Vec::new(),
            nodes: Vec::new(),
            dirty: BTreeSet::new(),
            touched: Vec::new(),
            a_hint: 0.25,
            focus: Vec::new(),
            step_counter: 0,
            only: None,
            lists_dirty: false,
            dormant: HashMap::new(),
            dormant_volume: 0.0,
        }
    }

    pub fn config(&self) -> ColumnConfig {
        self.config
    }

    pub fn span_count(&self) -> usize {
        self.spans.len()
    }

    /// Whether the stored pipes (as a set) and per-region pipe lists equal a
    /// from-scratch build over the current spans. For tests of incremental
    /// pipe maintenance.
    #[doc(hidden)]
    pub fn pipes_match_full_rebuild(&self) -> bool {
        let fresh: Vec<Pipe> = (0..self.regions.len())
            .flat_map(|slot| self.pipes_from(slot, |_| true))
            .collect();
        let key = |p: &Pipe| (p.a, p.b, p.lo, p.hi, p.ra, p.rb, p.axis);
        let (mut fresh_keys, mut have_keys): (Vec<_>, Vec<_>) = (
            fresh.iter().map(key).collect(),
            self.pipes.iter().map(key).collect(),
        );
        fresh_keys.sort_unstable();
        have_keys.sort_unstable();
        if fresh_keys != have_keys {
            return false;
        }
        let mut lists = vec![Vec::new(); self.regions.len()];
        for (i, p) in self.pipes.iter().enumerate() {
            lists[p.ra as usize].push(i as u32);
            if p.rb != p.ra {
                lists[p.rb as usize].push(i as u32);
            }
        }
        self.q.len() == self.pipes.len()
            && self.regions.iter().zip(&lists).all(|(r, l)| &r.pipes == l)
    }

    pub fn pipe_count(&self) -> usize {
        self.pipes.len()
    }

    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Sets the points (metres, x and z) around which the water is kept up to
    /// date: active regions farther than [`ColumnConfig::throttle_from_cells`]
    /// step less often. An empty list (no players) throttles nothing.
    pub fn set_focus(&mut self, points_m: &[[f64; 2]]) {
        self.focus = points_m
            .iter()
            .map(|p| [p[0] / self.column_m(), p[1] / self.column_m()])
            .collect();
    }

    /// Width of one column in metres: a terrain cell, or more with
    /// [`ColumnConfig::column_cells`]. Heights stay in terrain cells.
    pub fn column_m(&self) -> f64 {
        CELL_SIZE_M * f64::from(self.config.column_cells)
    }

    /// Seconds a region's water is behind real time because it rested.
    pub fn region_owed_s(&self, key: RegionKey) -> f32 {
        self.slot_of
            .get(&key)
            .map_or(0.0, |&s| self.regions[s as usize].owed_s)
    }

    /// What every active region is doing, for finding out why one does not
    /// sleep: the largest flow through any of its pipes (cells^3/s), its quiet
    /// steps so far, and whether it rests or is steady.
    pub fn active_regions_diag(&self) -> Vec<ActiveRegionDiag> {
        self.regions
            .iter()
            .filter(|r| r.active && !r.frozen)
            .map(|r| ActiveRegionDiag {
                key: r.key,
                max_flow: r
                    .pipes
                    .iter()
                    .map(|&pi| self.q[pi as usize].abs())
                    .fold(0.0, f32::max),
                quiet_steps: r.quiet_steps,
                resting: r.resting,
                steady: r.steady,
            })
            .collect()
    }

    /// Active regions resting at the moment.
    pub fn resting_region_count(&self) -> usize {
        self.regions
            .iter()
            .filter(|r| r.active && !r.frozen && r.resting)
            .count()
    }

    /// How many steps the region at `key` takes one in now, from its distance
    /// to the nearest focus point: 1, 2, 4 or 8 (see
    /// [`ColumnConfig::throttle_from_cells`]).
    pub fn region_pace(&self, key: RegionKey) -> u64 {
        pace_at(&self.focus, self.config.throttle_from_cells, key)
    }

    /// Whether the region holding column `(x, z)` rested in the last step (far
    /// from every focus point, see [`ColumnConfig::throttle_from_cells`]).
    pub fn is_column_resting(&self, x: i64, z: i64) -> bool {
        self.slot_of.get(&region_of(x, z)).is_some_and(|&s| {
            let r = &self.regions[s as usize];
            r.active && !r.frozen && r.resting
        })
    }

    /// Whether span `s`, at the edge of the sleeping region `region`, stands
    /// over [`BORDER_WAKE_HEAD`] above or below a span inside that region it
    /// is piped to (see the constant).
    fn border_out_of_level(&self, s: usize, region: u32) -> bool {
        let hs = self.effective_head(s);
        let (a, b) = (self.adj_start[s] as usize, self.adj_start[s + 1] as usize);
        self.adj[a..b].iter().any(|&entry| {
            let p = &self.pipes[(entry >> 1) as usize];
            if p.ra != region || p.rb != region {
                return false;
            }
            let t = if entry & 1 == 1 { p.b } else { p.a } as usize;
            let ht = self.effective_head(t);
            let lo = p.lo as f32;
            Self::wetted_area(p, hs, ht) >= self.config.min_film_depth
                && (hs.max(lo) - ht.max(lo)).abs() > BORDER_WAKE_HEAD
        })
    }

    /// Whether a region takes part in the step being run.
    fn stepping(&self, slot: usize) -> bool {
        let r = &self.regions[slot];
        r.active && !r.frozen && !r.resting && self.only.is_none_or(|o| o as usize == slot)
    }

    pub fn active_region_count(&self) -> usize {
        self.regions
            .iter()
            .filter(|r| r.active && !r.frozen)
            .count()
    }

    pub fn is_region_active(&self, key: RegionKey) -> bool {
        self.slot_of
            .get(&key)
            .is_some_and(|&s| self.regions[s as usize].active)
    }

    pub fn is_region_allocated(&self, key: RegionKey) -> bool {
        self.slot_of.contains_key(&key)
    }

    /// Approximate heap use of the stored state.
    pub fn memory(&self) -> MemoryReport {
        use std::mem::size_of;
        let bytes = self.col_start.len() * 4
            + self.spans.len() * size_of::<Span>()
            + self.pipes.len() * size_of::<Pipe>()
            + (self.adj_start.len() + self.adj.len()) * 4
            + self.volume.len() * 8
            + self.scale.len() * 8
            + (self.head.len() + self.eff.len() + self.q.len()) * 4
            + self
                .regions
                .iter()
                .map(|r| r.pipes.len() * 4)
                .sum::<usize>()
            + (self.act_spans.len() + self.act_pipes.len()) * 4
            + self.live.len()
            + self
                .dormant
                .values()
                .map(|d| d.cols.len() * 8 + std::mem::size_of::<Dormant>())
                .sum::<usize>();
        MemoryReport {
            regions: self.regions.len(),
            columns: self.regions.len() * REGION_COLUMNS,
            spans: self.spans.len(),
            pipes: self.pipes.len(),
            bytes,
        }
    }

    fn column_range(&self, x: i64, z: i64) -> Option<std::ops::Range<usize>> {
        let slot = *self.slot_of.get(&region_of(x, z))? as usize;
        let local = (x.rem_euclid(REGION_EDGE) + REGION_EDGE * z.rem_euclid(REGION_EDGE)) as usize;
        let c = slot * REGION_COLUMNS + local;
        Some(self.col_start[c] as usize..self.col_start[c + 1] as usize)
    }

    /// 0 for a span below its top cell, rising to 1 as it fills; always 0 for
    /// sky spans. Pressure head is blended in over this ramp so the head is
    /// continuous in volume (a hard full/not-full switch chatters).
    fn pressure_ramp(&self, i: usize) -> f32 {
        let s = self.spans[i];
        if s.sky {
            return 0.0;
        }
        let band = s.capacity().min(1.0);
        ((self.volume[i] - (s.capacity() - band)) / band).clamp(0.0, 1.0) as f32
    }

    /// Head of one span: its free surface plus, near full, the relaxed pressure
    /// head above its ceiling. Volume beyond capacity adds one cell of head per
    /// cell^3 (compliance of one cell^2, like a free surface), so water cannot
    /// hide in a pressurised span.
    fn effective_head(&self, i: usize) -> f32 {
        let s = self.spans[i];
        let free = s.y0 as f32 + self.volume[i] as f32;
        let ramp = self.pressure_ramp(i);
        if ramp > 0.0 {
            free + ramp * (self.head[i] - s.y1 as f32).max(0.0)
        } else {
            free
        }
    }

    fn view(&self, i: usize) -> SpanView {
        let s = self.spans[i];
        SpanView {
            index: i,
            floor_y: i64::from(s.y0),
            ceiling_y: i64::from(s.y1),
            sky: s.sky,
            volume: self.volume[i],
            depth: self.volume[i].min(s.capacity()),
            overfill: (self.volume[i] - s.capacity()).max(0.0),
            head_y: f64::from(self.effective_head(i)),
        }
    }

    /// Height (metres) of the water surface in the span that holds the point
    /// `(x_m, y_m, z_m)`, or `None` if the point is in solid terrain or air
    /// outside any span, or that span holds no water. A span that is full has
    /// its ceiling as its surface. Used to float bodies.
    pub fn surface_at_m(&self, x_m: f64, y_m: f64, z_m: f64) -> Option<f64> {
        let i = self.wet_span_at_m(x_m, y_m, z_m)?;
        let depth = self.volume[i].min(self.spans[i].capacity());
        Some((f64::from(self.spans[i].y0) + depth) * CELL_SIZE_M)
    }

    /// Index of the span holding the point if it carries water (deeper than a
    /// film).
    fn wet_span_at_m(&self, x_m: f64, y_m: f64, z_m: f64) -> Option<usize> {
        let (cx, cz) = (
            (x_m / self.column_m()).floor() as i64,
            (z_m / self.column_m()).floor() as i64,
        );
        let y_cell = y_m / CELL_SIZE_M;
        for i in self.column_range(cx, cz)? {
            let s = self.spans[i];
            if y_cell >= f64::from(s.y0) - 0.5 && y_cell < f64::from(s.y1) {
                return (self.volume[i].min(s.capacity()) > 0.02).then_some(i);
            }
        }
        None
    }

    /// Horizontal velocity `[x, z]` in m/s of the water in the span holding the
    /// point, or `None` where there is no water. It is the mean of the speeds
    /// through the span's two faces on each axis (`flow / wetted area` of its
    /// pipes), a face against a wall or a dry neighbour counting as still, so
    /// water running past a floating body pushes it and standing water does not.
    pub fn flow_at_m(&self, x_m: f64, y_m: f64, z_m: f64) -> Option<[f32; 2]> {
        Some(self.span_flow(self.wet_span_at_m(x_m, y_m, z_m)?))
    }

    /// [`Self::flow_at_m`] for span `i`.
    fn span_flow(&self, i: usize) -> [f32; 2] {
        let film = self.config.min_film_depth;
        let entries = &self.adj[self.adj_start[i] as usize..self.adj_start[i + 1] as usize];
        // Still water (most of any region) has no flow through any face.
        if entries
            .iter()
            .all(|&entry| self.q[(entry >> 1) as usize] == 0.0)
        {
            return [0.0; 2];
        }
        // [axis][side]: summed flow and wetted area through each face.
        let mut faces = [[(0.0f32, 0.0f32); 2]; 2];
        for &entry in entries {
            let pi = (entry >> 1) as usize;
            let p = &self.pipes[pi];
            let area = Self::wetted_area(
                p,
                self.effective_head(p.a as usize),
                self.effective_head(p.b as usize),
            );
            if area < film {
                continue;
            }
            // `a` looks towards +x or +z: this span is on the low side of the
            // pipe when it is `a`, so the pipe's face is its +axis face.
            let axis = usize::from(p.axis);
            let side = usize::from(entry & 1 == 1);
            faces[axis][side].0 += self.q[pi];
            faces[axis][side].1 += area;
        }
        let speed = |axis: usize| {
            let through = |(q, area): (f32, f32)| if area > 0.0 { q / area } else { 0.0 };
            // A pipe is a column wide: its face is that width times the area.
            0.5 * (through(faces[axis][0]) + through(faces[axis][1])) * CELL_SIZE_M as f32
                / self.config.column_cells as f32
        };
        [speed(0), speed(1)]
    }

    /// The spans of one column, bottom to top; empty if solid or unallocated.
    pub fn spans_in_column(&self, x: i64, z: i64) -> Vec<SpanView> {
        self.column_range(x, z)
            .map(|r| r.map(|i| self.view(i)).collect())
            .unwrap_or_default()
    }

    /// Visits a bounded number of spans in one column without allocating a
    /// temporary `Vec`. Returns false if the column has more spans than the
    /// caller's budget; in that case no visitor calls are made.
    pub fn visit_spans_in_column(
        &self,
        x: i64,
        z: i64,
        max_spans: usize,
        mut visit: impl FnMut(SpanView),
    ) -> bool {
        if let Some(range) = self.column_range(x, z) {
            if range.len() > max_spans {
                return false;
            }
            for index in range {
                visit(self.view(index));
            }
            return true;
        }

        let Some(dormant) = self.dormant.get(&region_of(x, z)) else {
            return true;
        };
        // Dormant records represent one still-water span per column. Derive its
        // current depth directly from the immutable record; do not revive or
        // allocate the solver region as a side effect of inspection.
        if max_spans == 0 {
            return false;
        }
        let local = (x.rem_euclid(REGION_EDGE) + REGION_EDGE * z.rem_euclid(REGION_EDGE)) as usize;
        let (floor_y, height) = dormant.cols[local];
        let depth = dormant.column_volume(local);
        visit(SpanView {
            // Dormant columns have no solver-span index.
            index: usize::MAX,
            floor_y: i64::from(floor_y),
            ceiling_y: i64::from(floor_y + height),
            sky: true,
            volume: depth,
            depth,
            overfill: 0.0,
            head_y: f64::from(floor_y) + depth,
        });
        true
    }

    // ---- stepping ------------------------------------------------------------

    fn rebuild_lists(&mut self) {
        let mut stamp = vec![false; self.spans.len()];
        self.act_spans.clear();
        if self.live.len() == self.pipes.len() {
            for &pi in &self.act_pipes {
                self.live[pi as usize] = false;
            }
        } else {
            self.live = vec![false; self.pipes.len()];
        }
        self.act_pipes.clear();
        self.touched.clear();
        if self.steady_span.len() == self.spans.len() {
            for &(a, b) in &self.steady_flagged {
                self.steady_span[a as usize..b as usize].fill(false);
            }
        } else {
            self.steady_span = vec![false; self.spans.len()];
        }
        self.steady_flagged.clear();
        for r in &self.regions {
            if r.steady {
                self.steady_span[r.span_start as usize..r.span_end as usize].fill(true);
                self.steady_flagged.push((r.span_start, r.span_end));
            }
        }
        self.steady_boundary.clear();
        let mut touched = vec![false; self.regions.len()];
        let part: Vec<bool> = (0..self.regions.len()).map(|s| self.stepping(s)).collect();
        // Active regions that sit this step out (resting, or not the one being
        // caught up): the water between them and a stepping region does not
        // move, or the edge of the one that rests would take water it cannot
        // pass on and the two would slosh against each other.
        let held: Vec<bool> = self
            .regions
            .iter()
            .enumerate()
            .map(|(s, r)| r.active && !r.frozen && !part[s])
            .collect();
        for (slot, r) in self.regions.iter().enumerate() {
            if part[slot] {
                for s in r.span_start..r.span_end {
                    stamp[s as usize] = true;
                    self.act_spans.push(s);
                }
            }
        }
        for (slot, r) in self.regions.iter().enumerate() {
            if !part[slot] {
                continue;
            }
            for &pi in &r.pipes {
                let p = self.pipes[pi as usize];
                // Each pipe is owned by its `a` region unless that one sleeps
                // (or rests).
                let owner_active = part[p.ra as usize];
                if p.ra as usize != slot && owner_active {
                    continue;
                }
                if held[p.ra as usize] || held[p.rb as usize] {
                    continue;
                }
                self.act_pipes.push(pi);
                self.live[pi as usize] = true;
                touched[p.ra as usize] = true;
                touched[p.rb as usize] = true;
                for (s, region) in [(p.a, p.ra), (p.b, p.rb)] {
                    if self.regions[region as usize].steady {
                        self.steady_boundary.push((s, region));
                    }
                }
                for s in [p.a, p.b] {
                    if !stamp[s as usize] {
                        stamp[s as usize] = true;
                        self.act_spans.push(s);
                    }
                }
            }
        }
        for (slot, stepping) in part.iter().enumerate() {
            if *stepping {
                touched[slot] = true;
            }
        }
        self.touched = (0..self.regions.len() as u32)
            .filter(|&s| touched[s as usize])
            .collect();
        self.steady_boundary.sort_unstable();
        self.steady_boundary.dedup();
        self.lists_dirty = false;
    }

    /// Advances the water by `dt` seconds using adaptive substeps. Every flow is
    /// computed from the previous state and applied together, so the result does
    /// not depend on iteration order or thread count. Sleeping regions cost
    /// nothing; a world with no active region returns immediately.
    pub fn step(&mut self, dt: f64) -> ColumnStepMetrics {
        if !self.dormant.is_empty() {
            self.revive_beside_activity();
        }
        if self.unplaced > 0.0 {
            self.release_unplaced();
        }
        self.update_resting(dt as f32);
        if self.lists_dirty {
            self.rebuild_lists();
        }
        let mut metrics = ColumnStepMetrics {
            active_regions: self.active_region_count(),
            resting_regions: self.resting_region_count(),
            active_spans: self.act_spans.len(),
            active_pipes: self.act_pipes.len(),
            ..ColumnStepMetrics::default()
        };
        self.step_counter += 1;
        if self.act_pipes.is_empty() && self.act_spans.is_empty() {
            return metrics;
        }
        let mut remaining = dt as f32;
        self.prepare_substeps();
        while remaining > 1.0e-9 && metrics.substeps < self.config.max_substeps {
            remaining -= self.substep(remaining);
            metrics.substeps += 1;
        }
        metrics.skipped_s = remaining.max(0.0);
        for &slot in &self.touched {
            self.dirty.insert(self.regions[slot as usize].key);
        }
        let advanced_s = dt as f32 - metrics.skipped_s;
        self.after_step(&mut metrics, advanced_s);
        metrics.steady_regions = self.steady_region_count();
        self.catch_up(&mut metrics);
        metrics
    }

    /// Decides which active regions rest this step and how long each has been
    /// resting (see [`ColumnConfig::throttle_from_cells`]).
    fn update_resting(&mut self, dt: f32) {
        const OWED_CAP_S: f32 = 1800.0;
        let bands = self.config.throttle_from_cells;
        let on = bands[0] > 0.0 && !self.focus.is_empty();
        let counter = self.step_counter;
        let mut changed = false;
        for (slot, r) in self.regions.iter_mut().enumerate() {
            let (period, rest) = if on && r.active && !r.frozen {
                let period = pace_at(&self.focus, bands, r.key);
                // Regions of a period all step on the same steps, and the periods
                // nest, so two neighbours step together whenever the slower of
                // them does and the water between them moves at its pace.
                let _ = slot;
                (period, !counter.is_multiple_of(period))
            } else {
                (1, false)
            };
            r.period = period as u8;
            if r.resting != rest {
                r.resting = rest;
                changed = true;
            }
            if rest {
                r.owed_s = (r.owed_s + dt).min(OWED_CAP_S);
            }
        }
        if changed {
            self.lists_dirty = true;
        }
    }

    /// Per-step set-up the substeps rely on, for whichever regions the lists
    /// now hold: the pressured spans, effective heads and the area hint.
    fn prepare_substeps(&mut self) {
        // Near-full spans carry a pressure head; the set is fixed for the step.
        self.nodes = self
            .act_spans
            .par_iter()
            .copied()
            .filter(|&i| self.pressure_ramp(i as usize) > 0.0)
            .collect();
        // The substep limit follows the largest wetted pipe area. The first
        // substep needs it up front; later ones take it from the flow update.
        self.refresh_effective_heads();
        let (pipes, eff) = (&self.pipes, &self.eff);
        self.a_hint = self
            .act_pipes
            .par_iter()
            .map(|&pi| {
                let p = &pipes[pi as usize];
                Self::wetted_area(p, eff[p.a as usize], eff[p.b as usize])
            })
            .reduce(|| 0.25f32, f32::max);
    }

    /// Spends a few substeps, region by region, on the nearest regions that
    /// rested while they were far: each is stepped alone for the time it owes
    /// (up to a limit) so the water catches up with real time as the player
    /// comes back, instead of staying where it stopped.
    fn catch_up(&mut self, metrics: &mut ColumnStepMetrics) {
        const MIN_OWED_S: f32 = 0.2;
        const MAX_PER_STEP: usize = 4;
        const MAX_SPAN_S: f32 = 4.0;
        let budget = self.config.catch_up_substeps;
        if budget == 0 || self.focus.is_empty() {
            return;
        }
        let mut due: Vec<(usize, f32)> = self
            .regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.active && !r.frozen && !r.resting && r.period == 1)
            .filter(|(_, r)| r.owed_s > MIN_OWED_S)
            .map(|(s, r)| (s, r.owed_s))
            .collect();
        if due.is_empty() {
            return;
        }
        due.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        due.truncate(MAX_PER_STEP);
        for (slot, owed) in due {
            self.only = Some(slot as u32);
            self.lists_dirty = true;
            self.rebuild_lists();
            self.prepare_substeps();
            let want = owed.min(MAX_SPAN_S);
            let mut remaining = want;
            let mut n = 0;
            while remaining > 1.0e-9 && n < budget {
                remaining -= self.substep(remaining);
                n += 1;
            }
            let r = &mut self.regions[slot];
            r.owed_s = (r.owed_s - (want - remaining)).max(0.0);
            for &t in &self.touched {
                self.dirty.insert(self.regions[t as usize].key);
            }
            // Water this region pushed into a sleeping neighbour wakes it, as
            // it would in an ordinary step; left asleep it would hold the water
            // as a ridge at the border.
            let cfg = self.config;
            let mut wake = BTreeSet::new();
            for &pi in &self.act_pipes {
                let p = self.pipes[pi as usize];
                let fast = self.q[pi as usize].abs() > cfg.sleep_flow;
                for (s, r) in [(p.a, p.ra), (p.b, p.rb)] {
                    let reg = &self.regions[r as usize];
                    if !reg.active
                        && !reg.steady
                        && (fast || self.border_out_of_level(s as usize, r))
                    {
                        wake.insert(r as usize);
                    }
                }
            }
            for s in wake {
                if self.wake_slot(s) {
                    metrics.regions_woken += 1;
                }
            }
            metrics.caught_up_regions += 1;
        }
        self.only = None;
        self.lists_dirty = true;
    }

    /// Regions frozen as steady: flowing, balanced, and not stepped.
    pub fn steady_region_count(&self) -> usize {
        self.regions.iter().filter(|r| r.steady).count()
    }

    /// Keys of the regions frozen as steady.
    pub fn steady_region_keys(&self) -> Vec<RegionKey> {
        self.regions
            .iter()
            .filter(|r| r.steady)
            .map(|r| r.key)
            .collect()
    }

    /// Per region, the largest |inflow - outflow| of any span, cells^3/s: how
    /// far from balanced it is. A source or sink span is never balanced.
    pub fn net_flow_imbalance(&self) -> Vec<(RegionKey, f32)> {
        self.regions
            .iter()
            .map(|r| {
                let worst = (r.span_start..r.span_end)
                    .map(|i| {
                        let i = i as usize;
                        let (a, b) = (self.adj_start[i] as usize, self.adj_start[i + 1] as usize);
                        let net: f32 = self.adj[a..b]
                            .iter()
                            .map(|&entry| {
                                let flow = self.q[(entry >> 1) as usize];
                                if entry & 1 == 1 { -flow } else { flow }
                            })
                            .sum();
                        net.abs()
                    })
                    .fold(0.0, f32::max);
                (r.key, worst)
            })
            .collect()
    }

    /// Quiet tracking, sleeping, waking and ring allocation.
    fn after_step(&mut self, metrics: &mut ColumnStepMetrics, advanced_s: f32) {
        let cfg = self.config;
        let n = self.regions.len();
        struct Acc {
            max_q: Vec<f32>,
            peak: f32,
            wake: BTreeSet<u32>,
        }
        let new_acc = || Acc {
            max_q: vec![0.0; n],
            peak: 0.0,
            wake: BTreeSet::new(),
        };
        let this = &*self;
        let (pipes, q, eff, regions) = (&self.pipes, &self.q, &self.eff, &self.regions);
        // Fixed-size chunks: a parallel fold would split into thousands of tasks,
        // each allocating and merging a vector the size of every allocated region.
        const CHUNK: usize = 16 * 1024;
        let acc = self
            .act_pipes
            .par_chunks(CHUNK)
            .map(|chunk| {
                chunk.iter().fold(new_acc(), |mut acc, &pi| {
                    let p = pipes[pi as usize];
                    let qa = q[pi as usize].abs();
                    acc.max_q[p.ra as usize] = acc.max_q[p.ra as usize].max(qa);
                    acc.max_q[p.rb as usize] = acc.max_q[p.rb as usize].max(qa);
                    let area = Self::wetted_area(&p, eff[p.a as usize], eff[p.b as usize]);
                    if area >= cfg.min_film_depth {
                        acc.peak = acc.peak.max(qa / area);
                    }
                    for (s, r) in [(p.a, p.ra), (p.b, p.rb)] {
                        let reg = &regions[r as usize];
                        if !reg.active
                            && !reg.steady
                            && (qa > cfg.sleep_flow || this.border_out_of_level(s as usize, r))
                        {
                            acc.wake.insert(r);
                        }
                    }
                    // A steady region wakes at once if a boundary flow jumps from its
                    // held value (the slower drift of a neighbour is caught by volume).
                    for r in [p.ra, p.rb] {
                        let reg = &regions[r as usize];
                        if !reg.steady {
                            continue;
                        }
                        let moved = match reg.steady_ref.binary_search_by_key(&pi, |e| e.0) {
                            Ok(k) => {
                                let (_, held, band) = reg.steady_ref[k];
                                (q[pi as usize] - held).abs() > band
                            }
                            // A pipe that did not exist when the region froze.
                            Err(_) => true,
                        };
                        if moved {
                            acc.wake.insert(r);
                        }
                    }
                    acc
                })
            })
            .reduce(new_acc, |mut a, b| {
                for (x, y) in a.max_q.iter_mut().zip(&b.max_q) {
                    *x = x.max(*y);
                }
                a.peak = a.peak.max(b.peak);
                a.wake.extend(b.wake);
                a
            });
        let max_q = acc.max_q;
        let wake: Vec<usize> = acc.wake.into_iter().map(|r| r as usize).collect();
        metrics.max_abs_flow = max_q.iter().fold(0.0, |m, q| m.max(*q));
        metrics.peak_speed_m_s = acc.peak * CELL_SIZE_M as f32 / cfg.column_cells as f32;
        // Steady flow: a region that keeps carrying water but whose span volumes
        // have not moved over a window stops being stepped, holding its mean flows.
        if cfg.steady_steps > 0 {
            for (s, r) in self.regions.iter_mut().enumerate() {
                if r.steady {
                    continue;
                }
                if !r.active || r.frozen || r.resting || max_q[s] < cfg.sleep_flow {
                    r.steady_snap.clear();
                    r.steady_qsum.clear();
                    r.steady_qsq.clear();
                    r.steady_steps = 0;
                    continue;
                }
                if r.steady_snap.is_empty() {
                    r.steady_snap =
                        self.volume[r.span_start as usize..r.span_end as usize].to_vec();
                    r.steady_qsum = vec![0.0; r.pipes.len()];
                    r.steady_qsq = vec![0.0; r.pipes.len()];
                    r.steady_steps = 0;
                    r.steady_time = 0.0;
                }
                r.steady_steps += 1;
                r.steady_time += advanced_s;
                for ((sum, sq), &pi) in r
                    .steady_qsum
                    .iter_mut()
                    .zip(r.steady_qsq.iter_mut())
                    .zip(&r.pipes)
                {
                    let flow = self.q[pi as usize];
                    *sum += flow;
                    *sq += flow * flow;
                }
            }
            let due: Vec<usize> = (0..n)
                .filter(|&s| self.regions[s].steady_steps >= cfg.steady_steps)
                .collect();
            let (pipes, q, volume) = (&self.pipes, &self.q, &self.volume);
            let verdicts: Vec<(usize, Option<Vec<f32>>)> = due
                .par_iter()
                .map(|&s| {
                    let r = &self.regions[s];
                    let (lo, hi) = (r.span_start as usize, r.span_end as usize);
                    let held = r
                        .steady_snap
                        .iter()
                        .zip(&volume[lo..hi])
                        .all(|(a, b)| (a - b).abs() <= cfg.steady_volume);
                    if !held || r.steady_time <= 0.0 {
                        return (s, None);
                    }
                    let samples = r.steady_steps as f32;
                    let mean: Vec<f32> = r.steady_qsum.iter().map(|sum| sum / samples).collect();
                    // Net and throughput of every span under the mean flows.
                    let mut net = vec![0.0f32; hi - lo];
                    let mut through = vec![0.0f32; hi - lo];
                    for (&pi, &m) in r.pipes.iter().zip(&mean) {
                        let p = pipes[pi as usize];
                        if (lo..hi).contains(&(p.a as usize)) {
                            net[p.a as usize - lo] -= m;
                            through[p.a as usize - lo] += 0.5 * m.abs();
                        }
                        if (lo..hi).contains(&(p.b as usize)) {
                            net[p.b as usize - lo] += m;
                            through[p.b as usize - lo] += 0.5 * m.abs();
                        }
                    }
                    let balanced = net.iter().all(|n| n.abs() <= cfg.steady_net);
                    // Volumes held while water passes: drift well below the throughput.
                    let steady = r.steady_snap.iter().zip(&volume[lo..hi]).zip(&through).all(
                        |((a, b), through)| {
                            let drift = (a - b).abs() as f32 / r.steady_time;
                            drift <= cfg.steady_drift_abs + cfg.steady_drift_rel * through
                        },
                    );
                    let _ = q;
                    (s, (balanced && steady).then_some(mean))
                })
                .collect();
            for (s, verdict) in verdicts {
                let Some(mean) = verdict else {
                    // Start a fresh window from where the water is now.
                    let r = &mut self.regions[s];
                    r.steady_snap.clear();
                    r.steady_qsum.clear();
                    r.steady_qsq.clear();
                    r.steady_steps = 0;
                    continue;
                };
                let pipe_ids = self.regions[s].pipes.clone();
                let samples = self.regions[s].steady_steps as f32;
                let mut held: Vec<(u32, f32, f32)> = Vec::new();
                for (k, (&pi, &m)) in pipe_ids.iter().zip(&mean).enumerate() {
                    let p = self.pipes[pi as usize];
                    // Interior flows are held at their window mean (they are not
                    // stepped again); a boundary pipe keeps its live flow, which
                    // the next step continues from.
                    if p.ra == p.rb {
                        self.q[pi as usize] = m;
                    } else {
                        let variance = (self.regions[s].steady_qsq[k] / samples - m * m).max(0.0);
                        let band = cfg.steady_wake_abs
                            + cfg.steady_wake_rel * m.abs()
                            + cfg.steady_wake_sigmas * variance.sqrt();
                        held.push((pi, m, band));
                    }
                }
                held.sort_by_key(|e| e.0);
                let (lo, hi) = (
                    self.regions[s].span_start as usize,
                    self.regions[s].span_end as usize,
                );
                let r = &mut self.regions[s];
                r.steady = true;
                r.steady_ledger = 0.0;
                r.active = false;
                r.steady_steps = 0;
                r.steady_snap = self.volume[lo..hi].to_vec();
                r.steady_qsum = Vec::new();
                r.steady_qsq = Vec::new();
                r.steady_ref = held;
                metrics.regions_steadied += 1;
                self.lists_dirty = true;
            }
        }
        // A steady region whose exchange with its neighbours has stopped
        // netting to zero is not steady any more: it wakes and flows again.
        {
            let wake: Vec<usize> = self
                .regions
                .iter()
                .enumerate()
                .filter(|(_, r)| r.steady && r.steady_ledger.abs() > cfg.steady_wake_volume)
                .map(|(s, _)| s)
                .collect();
            for s in wake {
                if self.wake_slot(s) {
                    metrics.regions_woken += 1;
                }
            }
        }
        let mut slept = Vec::new();
        for (s, r) in self.regions.iter_mut().enumerate() {
            if !r.active || r.frozen || r.resting {
                continue;
            }
            if max_q[s] < cfg.sleep_flow {
                r.steady_steps = 0;
                r.steady_snap.clear();
                r.steady_qsum.clear();
                r.steady_qsq.clear();
                // A far region is stepped one time in `period`: each quiet step
                // stands for that many, so it falls asleep at the real-time
                // pace a near one does.
                r.quiet_steps += u32::from(r.period);
                if r.quiet_steps >= cfg.sleep_steps {
                    r.active = false;
                    r.owed_s = 0.0;
                    slept.push(s);
                }
            } else {
                r.quiet_steps = 0;
            }
        }
        metrics.regions_slept = slept.len();
        for s in wake {
            if self.wake_slot(s) {
                metrics.regions_woken += 1;
            }
        }
        if !slept.is_empty() {
            // A pipe between two sleeping regions carries nothing.
            for &s in &slept {
                for &pi in &self.regions[s].pipes {
                    let p = self.pipes[pi as usize];
                    if !self.regions[p.ra as usize].active && !self.regions[p.rb as usize].active {
                        self.q[pi as usize] = 0.0;
                    }
                }
            }
            self.lists_dirty = true;
        }
        if self.regions.iter().any(|r| r.active && !r.ring_done) {
            self.ensure_rings();
        }
    }

    /// Relaxes the base pressure head of near-full spans toward their
    /// neighbours' heads, then refreshes the cached effective heads.
    fn relax(&mut self) {
        let nodes = std::mem::take(&mut self.nodes);
        // Base heads depend only on free surfaces and other base heads, never on
        // overfill, so overfill is a pure restoring spring on top.
        let sweeps = if nodes.is_empty() {
            0
        } else {
            self.config.relax_sweeps
        };
        for _ in 0..sweeps {
            let next: Vec<f32> = nodes
                .par_iter()
                .map(|&i| {
                    let iu = i as usize;
                    let (s, e) = (self.adj_start[iu] as usize, self.adj_start[iu + 1] as usize);
                    let ceiling = self.spans[iu].y1 as f32;
                    if s == e {
                        return ceiling;
                    }
                    let sum: f32 = self.adj[s..e]
                        .iter()
                        .map(|&entry| {
                            let p = self.pipes[(entry >> 1) as usize];
                            let j = if entry & 1 == 1 { p.b } else { p.a } as usize;
                            if self.pressure_ramp(j) > 0.0 {
                                self.head[j].max(self.spans[j].y1 as f32)
                            } else {
                                self.spans[j].y0 as f32 + self.volume[j] as f32
                            }
                        })
                        .sum();
                    ceiling.max(sum / (e - s) as f32)
                })
                .collect();
            for (&i, h) in nodes.iter().zip(next) {
                self.head[i as usize] = h;
            }
        }
        self.nodes = nodes;
        self.refresh_effective_heads();
    }

    /// Recomputes the cached effective head of every active span.
    fn refresh_effective_heads(&mut self) {
        let mut eff = std::mem::take(&mut self.eff);
        let out = Shared(eff.as_mut_ptr());
        let this = &*self;
        this.act_spans.par_iter().for_each(|&i| {
            // SAFETY: `act_spans` holds each span index once, so every write
            // targets a distinct element of `eff`, which nothing else touches.
            unsafe { out.write(i as usize, this.effective_head(i as usize)) }
        });
        self.eff = eff;
    }

    /// One explicit substep no longer than `max_dt`; returns the time used.
    fn substep(&mut self, max_dt: f32) -> f32 {
        self.relax();
        let cfg = self.config;
        let (pipes, eff) = (&self.pipes, &self.eff);

        // Areas move slowly between substeps, so the hint carries a 25% margin.
        // A wave crosses a wider column in proportionally more time.
        let width = cfg.column_cells as f32;
        let dt = max_dt.min(width * cfg.cfl / (cfg.gravity * self.a_hint * 1.25).sqrt());
        // Flows are volumes per second; a span's volume is its depth over the
        // column's area (1 at terrain resolution).
        let per_area = 1.0 / f64::from(cfg.column_cells * cfg.column_cells);

        // Flow update with damping; thin films carry nothing. Each pipe appears
        // once in `act_pipes`, so the writes are disjoint.
        let damp = (1.0 - cfg.damping_per_s * dt).max(0.0);
        let q_ptr = Shared(self.q.as_mut_ptr());
        let a_next = self
            .act_pipes
            .par_iter()
            .map(|&pi| {
                let p = &pipes[pi as usize];
                let (ha, hb) = (eff[p.a as usize], eff[p.b as usize]);
                let area = Self::wetted_area(p, ha, hb);
                // SAFETY: distinct pipe index per iteration; only this pipe is touched.
                unsafe {
                    let q = q_ptr.read(pi as usize);
                    q_ptr.write(
                        pi as usize,
                        if area < cfg.min_film_depth {
                            0.0
                        } else {
                            let lo = p.lo as f32;
                            (q + dt * cfg.gravity * area * (ha.max(lo) - hb.max(lo))) * damp
                        },
                    );
                }
                area
            })
            .reduce(|| 0.25f32, f32::max);
        self.a_hint = a_next;

        // Outflow limit: a donor never gives more than it holds.
        let (adj_start, adj, volume) = (&self.adj_start, &self.adj, &self.volume);
        let (q, live) = (&self.q, &self.live);
        let scale_ptr = Shared(self.scale.as_mut_ptr());
        self.act_spans.par_iter().for_each(|&i| {
            let i = i as usize;
            let (s, e) = (adj_start[i] as usize, adj_start[i + 1] as usize);
            let out: f64 = adj[s..e]
                .iter()
                .map(|&entry| {
                    let pi = (entry >> 1) as usize;
                    if !live[pi] {
                        return 0.0;
                    }
                    let flow = f64::from(q[pi]);
                    (if entry & 1 == 1 { flow } else { -flow }).max(0.0)
                })
                .sum::<f64>()
                * f64::from(dt)
                * per_area;
            let scale = if out > volume[i] {
                volume[i].max(0.0) / out
            } else {
                1.0
            };
            // SAFETY: one write per distinct span index.
            unsafe { scale_ptr.write(i, scale) }
        });
        let scale = &self.scale;
        let q_ptr = Shared(self.q.as_mut_ptr());
        self.act_pipes.par_iter().for_each(|&pi| {
            let p = &pipes[pi as usize];
            // SAFETY: distinct pipe per iteration.
            unsafe {
                let q = q_ptr.read(pi as usize);
                let s = scale[if q > 0.0 { p.a } else { p.b } as usize];
                if s < 1.0 {
                    q_ptr.write(pi as usize, (f64::from(q) * s) as f32);
                }
            }
        });

        // Apply all volume changes at once (Jacobi): each span reads the final
        // flows of its pipes and writes only its own volume.
        let (q, live, steady_span) = (&self.q, &self.live, &self.steady_span);
        let vol_ptr = Shared(self.volume.as_mut_ptr());
        self.act_spans.par_iter().for_each(|&i| {
            let i = i as usize;
            // A steady region's spans are fixed reservoirs: see `steady_ledger`.
            if steady_span[i] {
                return;
            }
            let (s, e) = (adj_start[i] as usize, adj_start[i + 1] as usize);
            let delta = adj[s..e]
                .iter()
                .map(|&entry| {
                    let pi = (entry >> 1) as usize;
                    if !live[pi] {
                        return 0.0;
                    }
                    let flow = f64::from(q[pi]);
                    if entry & 1 == 1 { -flow } else { flow }
                })
                .sum::<f64>()
                * f64::from(dt)
                * per_area;
            // SAFETY: one read-modify-write per distinct span index; `volume` is
            // not read through any other path in this pass.
            unsafe { vol_ptr.write(i, (vol_ptr.read(i) + delta).max(0.0)) }
        });
        // What the live boundary pipes moved across a steady region's edge goes
        // to its ledger, so the exchange is still counted exactly.
        if !self.steady_boundary.is_empty() {
            let deltas: Vec<(u32, f64)> = self
                .steady_boundary
                .iter()
                .map(|&(span, region)| {
                    let i = span as usize;
                    let (s, e) = (self.adj_start[i] as usize, self.adj_start[i + 1] as usize);
                    let delta: f64 = self.adj[s..e]
                        .iter()
                        .map(|&entry| {
                            let pi = (entry >> 1) as usize;
                            if !self.live[pi] {
                                return 0.0;
                            }
                            let flow = f64::from(self.q[pi]);
                            if entry & 1 == 1 { -flow } else { flow }
                        })
                        .sum::<f64>()
                        * f64::from(dt)
                        * per_area;
                    (region, delta)
                })
                .collect();
            for (region, delta) in deltas {
                self.regions[region as usize].steady_ledger += delta;
            }
        }
        dt
    }
}

/// Sum of `f(index, element)` over a slice, in parallel over fixed 64 Ki-element
/// chunks that are then added in order, so the result does not depend on the
/// number of threads. A world has hundreds of thousands of spans and these
/// totals ran every tick.
fn chunked_sum<T: Sync>(items: &[T], f: impl Fn(usize, &T) -> f64 + Sync) -> f64 {
    const CHUNK: usize = 1 << 16;
    let parts: Vec<f64> = items
        .par_chunks(CHUNK)
        .enumerate()
        .map(|(c, chunk)| {
            chunk
                .iter()
                .enumerate()
                .map(|(i, item)| f(c * CHUNK + i, item))
                .sum::<f64>()
        })
        .collect();
    parts.into_iter().sum()
}

/// A raw pointer that rayon tasks may share when each task touches its own
/// element, which every call site documents.
struct Shared<T>(*mut T);
unsafe impl<T: Send> Send for Shared<T> {}
unsafe impl<T: Send> Sync for Shared<T> {}

impl<T: Copy> Shared<T> {
    /// # Safety
    /// `i` is in bounds and no other thread accesses element `i` concurrently.
    unsafe fn write(&self, i: usize, v: T) {
        unsafe { self.0.add(i).write(v) }
    }

    /// # Safety
    /// As for `write`.
    unsafe fn read(&self, i: usize) -> T {
        unsafe { self.0.add(i).read() }
    }
}

/// Places the water of one old column into the new spans of the same column by
/// interval overlap. Returns the volume that did not fit and the height of the
/// old surface it should be relocated near.
fn remap_column(
    old_spans: &[Span],
    old_vol: &[f64],
    old: std::ops::Range<usize>,
    new_spans: &[Span],
    new_vol: &mut [f64],
    new: std::ops::Range<usize>,
) -> (f64, f64) {
    let old_total: f64 = old.clone().map(|o| old_vol[o]).sum();
    let mut placed_total = 0.0;
    for n in new {
        let ns = new_spans[n];
        let placed: f64 = old
            .clone()
            .map(|o| {
                let os = old_spans[o];
                let top = f64::from(os.y0) + old_vol[o].min(os.capacity());
                let lo = f64::from(os.y0).max(f64::from(ns.y0));
                let hi = top.min(f64::from(ns.y1));
                (hi - lo).max(0.0)
            })
            .sum::<f64>()
            .min(ns.capacity());
        new_vol[n] = placed;
        placed_total += placed;
    }
    let top_y = old
        .filter(|&o| old_vol[o] > 0.0)
        .map(|o| {
            let os = old_spans[o];
            f64::from(os.y0) + old_vol[o].min(os.capacity())
        })
        .fold(f64::MIN, f64::max);
    ((old_total - placed_total).max(0.0), top_y)
}

/// The span in `range` whose interval is closest to height `y`.
fn nearest_span(spans: &[Span], range: std::ops::Range<usize>, y: f64) -> Option<usize> {
    range.min_by(|&a, &b| {
        let dist = |i: usize| {
            let s = spans[i];
            (f64::from(s.y0) - y).max(y - f64::from(s.y1)).max(0.0)
        };
        dist(a).total_cmp(&dist(b))
    })
}

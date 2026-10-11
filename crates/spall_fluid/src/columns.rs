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
}

/// Read-only description of one span for tests and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpanView {
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

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ColumnStepMetrics {
    pub substeps: u32,
    /// Regions, spans and pipes that did work this step.
    pub active_regions: usize,
    pub active_spans: usize,
    pub active_pipes: usize,
    pub regions_slept: usize,
    pub regions_woken: usize,
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
    /// Near-full active spans for the current step.
    nodes: Vec<u32>,
    /// Regions whose water changed since `take_dirty_regions`.
    dirty: BTreeSet<RegionKey>,
    /// Regions touched by this step's active lists (rebuilt with them).
    touched: Vec<u32>,
    /// Largest wetted pipe area seen, for the substep limit.
    a_hint: f32,
    lists_dirty: bool,
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
            nodes: Vec::new(),
            dirty: BTreeSet::new(),
            touched: Vec::new(),
            a_hint: 0.25,
            lists_dirty: false,
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
            + (self.act_spans.len() + self.act_pipes.len()) * 4;
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
            (x_m / CELL_SIZE_M).floor() as i64,
            (z_m / CELL_SIZE_M).floor() as i64,
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
            0.5 * (through(faces[axis][0]) + through(faces[axis][1])) * CELL_SIZE_M as f32
        };
        [speed(0), speed(1)]
    }

    /// The spans of one column, bottom to top; empty if solid or unallocated.
    pub fn spans_in_column(&self, x: i64, z: i64) -> Vec<SpanView> {
        self.column_range(x, z)
            .map(|r| r.map(|i| self.view(i)).collect())
            .unwrap_or_default()
    }

    // ---- allocation and terrain -------------------------------------------

    /// Allocates the given regions (those that are resident) and re-reads every
    /// key in `refresh`, remapping its water column by column. New regions start
    /// asleep; refreshed ones wake with their neighbours.
    fn allocate(&mut self, add: &[RegionKey], refresh: &BTreeSet<RegionKey>) -> RemapReport {
        let (y_lo, y_hi) = self.terrain.y_range();
        let mut fresh: HashMap<RegionKey, RegionTerrain> = HashMap::new();
        let wanted: Vec<RegionKey> = add
            .iter()
            .copied()
            .filter(|k| !self.slot_of.contains_key(k))
            .chain(refresh.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let loaded: Vec<(RegionKey, Option<RegionTerrain>)> = wanted
            .par_iter()
            .map(|&k| (k, self.terrain.region(k)))
            .collect();
        let mut keys: BTreeSet<RegionKey> = self.regions.iter().map(|r| r.key).collect();
        let mut frozen_now = BTreeSet::new();
        for (k, t) in loaded {
            match t {
                Some(t) => {
                    keys.insert(k);
                    fresh.insert(k, t);
                }
                None if self.slot_of.contains_key(&k) => {
                    frozen_now.insert(k);
                }
                None => {
                    self.waiting.insert(k);
                }
            }
        }
        if fresh.is_empty() && frozen_now.is_empty() {
            return RemapReport::default();
        }
        if refresh.is_empty() && frozen_now.is_empty() {
            self.append_regions(fresh);
            return RemapReport::default();
        }

        let old_regions = std::mem::take(&mut self.regions);
        let old_slot = std::mem::take(&mut self.slot_of);
        let old_col = std::mem::take(&mut self.col_start);
        let old_spans = std::mem::take(&mut self.spans);
        let old_vol = std::mem::take(&mut self.volume);
        let old_head = std::mem::take(&mut self.head);
        let old_pipes = std::mem::take(&mut self.pipes);
        let old_q = std::mem::take(&mut self.q);
        let key_of_spans = |regions: &[Region], col: &[u32], spans: &[Span]| {
            let mut v = vec![0u64; spans.len()];
            for (slot, r) in regions.iter().enumerate() {
                for c in 0..REGION_COLUMNS {
                    let (x, z) = column_xz(r.key, c);
                    let ci = slot * REGION_COLUMNS + c;
                    for s in col[ci] as usize..col[ci + 1] as usize {
                        v[s] = span_key(x, z, spans[s].y0);
                    }
                }
            }
            v
        };
        let mut report = RemapReport::default();
        let mut pending: Vec<(i64, i64, f64, f64)> = Vec::new();
        let mut regions: Vec<Region> = Vec::with_capacity(keys.len());
        let mut col_start: Vec<u32> = Vec::with_capacity(keys.len() * REGION_COLUMNS + 1);
        let mut spans: Vec<Span> = Vec::new();
        let mut volume: Vec<f64> = Vec::new();
        let mut head: Vec<f32> = Vec::new();
        let mut slot_of = HashMap::new();

        for key in keys {
            let slot = regions.len() as u32;
            let base = spans.len();
            let old = old_slot.get(&key).map(|&s| s as usize);
            let col_base = col_start.len();
            let mut region = match old {
                Some(s) => old_regions[s].clone(),
                None => Region {
                    key,
                    span_start: 0,
                    span_end: 0,
                    pipes: Vec::new(),
                    active: false,
                    quiet_steps: 0,
                    ring_done: false,
                    frozen: false,
                },
            };
            region.pipes = Vec::new();
            if frozen_now.contains(&key) {
                region.frozen = true;
            }
            match (old, fresh.get(&key)) {
                (Some(old), Some(terrain)) => {
                    // Re-read terrain for an existing region: remap per column.
                    extract_columns(terrain, y_lo, y_hi, &mut spans, &mut col_start);
                    volume.resize(spans.len(), 0.0);
                    head.extend(spans[base..].iter().map(|s| s.y1 as f32));
                    for c in 0..REGION_COLUMNS {
                        let ci = old * REGION_COLUMNS + c;
                        let o = old_col[ci] as usize..old_col[ci + 1] as usize;
                        let n_end = if c + 1 < REGION_COLUMNS {
                            col_start[col_base + c + 1] as usize
                        } else {
                            spans.len()
                        };
                        let n = col_start[col_base + c] as usize..n_end;
                        if o.len() == n.len()
                            && o.clone()
                                .zip(n.clone())
                                .all(|(a, b)| old_spans[a] == spans[b])
                        {
                            for (a, b) in o.zip(n) {
                                volume[b] = old_vol[a];
                                head[b] = old_head[a];
                            }
                            continue;
                        }
                        report.columns_changed += 1;
                        let (x, z) = column_xz(key, c);
                        let (excess, top_y) =
                            remap_column(&old_spans, &old_vol, o, &spans, &mut volume, n.clone());
                        if excess > 0.0 {
                            report.relocated_volume += excess;
                            match nearest_span(&spans, n, top_y) {
                                Some(t) => volume[t] += excess,
                                None => pending.push((x, z, top_y, excess)),
                            }
                        }
                    }
                    region.active = true;
                    region.quiet_steps = 0;
                    region.ring_done = false;
                    region.frozen = false;
                }
                (Some(old), None) => {
                    // Unchanged region: copy its block.
                    let ci = old * REGION_COLUMNS;
                    let s0 = old_col[ci] as usize;
                    let s1 = old_col[ci + REGION_COLUMNS] as usize;
                    for c in 0..REGION_COLUMNS {
                        col_start.push(spans.len() as u32 + old_col[ci + c] - s0 as u32);
                    }
                    spans.extend_from_slice(&old_spans[s0..s1]);
                    volume.extend_from_slice(&old_vol[s0..s1]);
                    head.extend_from_slice(&old_head[s0..s1]);
                }
                (None, Some(terrain)) => {
                    extract_columns(terrain, y_lo, y_hi, &mut spans, &mut col_start);
                    volume.resize(spans.len(), 0.0);
                    head.extend(spans[base..].iter().map(|s| s.y1 as f32));
                }
                (None, None) => unreachable!("a new region always has terrain"),
            }
            region.span_start = base as u32;
            region.span_end = spans.len() as u32;
            slot_of.insert(key, slot);
            regions.push(region);
        }
        col_start.push(spans.len() as u32);

        self.regions = regions;
        self.slot_of = slot_of;
        self.col_start = col_start;
        self.spans = spans;
        self.volume = volume;
        self.head = head;

        // Columns that became solid hand their water to a neighbour column.
        for (x, z, y, excess) in pending {
            let mut target = None;
            for (dx, dz) in [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)] {
                if let Some(r) = self.column_range(x + dx, z + dz)
                    && let Some(t) = nearest_span(&self.spans, r, y)
                {
                    target = Some(t);
                    break;
                }
            }
            match target {
                Some(t) => self.volume[t] += excess,
                None => {
                    self.unplaced += excess;
                    report.unplaced_volume += excess;
                }
            }
        }

        // Rebuild pipes and keep the flow of every pipe that still exists. Only
        // regions whose terrain changed, and the owners of pipes running into
        // them, are rebuilt; every other pipe is carried over with its indices
        // shifted to the new span and slot layout.
        let touched: BTreeSet<RegionKey> = fresh
            .keys()
            .copied()
            .chain(frozen_now.iter().copied())
            .collect();
        let carried = self.carry_pipes(
            &touched,
            &old_regions,
            &old_slot,
            &old_col,
            &old_spans,
            &old_pipes,
            &old_q,
        );
        if !carried {
            let mut old_flow: HashMap<(u64, u64), f32> = HashMap::new();
            if old_q.iter().any(|q| *q != 0.0) {
                let keys_old = key_of_spans(&old_regions, &old_col, &old_spans);
                for (p, q) in old_pipes.iter().zip(&old_q) {
                    if *q != 0.0 {
                        old_flow.insert((keys_old[p.a as usize], keys_old[p.b as usize]), *q);
                    }
                }
            }

            self.build_pipes();
            self.q = vec![0.0; self.pipes.len()];
            if !old_flow.is_empty() {
                let keys_now = key_of_spans(&self.regions, &self.col_start, &self.spans);
                for (i, p) in self.pipes.iter().enumerate() {
                    if let Some(q) = old_flow.get(&(keys_now[p.a as usize], keys_now[p.b as usize]))
                    {
                        self.q[i] = *q;
                    }
                }
            }
        }
        self.eff = vec![0.0; self.spans.len()];
        self.scale = vec![1.0; self.spans.len()];

        // A refreshed region and its neighbours wake.
        for &k in refresh {
            if let Some(&s) = self.slot_of.get(&k) {
                self.wake_slot_and_neighbours(s as usize);
            }
        }
        self.lists_dirty = true;
        self.release_unplaced();
        report
    }

    /// Pipes whose `a` end is in region `slot` (towards its +x and +z
    /// neighbours), restricted to neighbour slots `accept` allows.
    fn pipes_from(&self, slot: usize, accept: impl Fn(u32) -> bool) -> Vec<Pipe> {
        let mut pipes = Vec::new();
        let r = &self.regions[slot];
        if r.frozen {
            return pipes;
        }
        let key = r.key;
        for lz in 0..REGION_EDGE {
            for lx in 0..REGION_EDGE {
                let (x, z) = (
                    i64::from(key.0) * REGION_EDGE + lx,
                    i64::from(key.1) * REGION_EDGE + lz,
                );
                let ca = slot * REGION_COLUMNS + (lx + REGION_EDGE * lz) as usize;
                for (axis, (ox, oz)) in [(1i64, 0i64), (0, 1)].into_iter().enumerate() {
                    let nb = region_of(x + ox, z + oz);
                    let Some(&ns) = self.slot_of.get(&nb) else {
                        continue;
                    };
                    if self.regions[ns as usize].frozen || !accept(ns) {
                        continue;
                    }
                    let cb = ns as usize * REGION_COLUMNS
                        + ((x + ox).rem_euclid(REGION_EDGE)
                            + REGION_EDGE * (z + oz).rem_euclid(REGION_EDGE))
                            as usize;
                    for a in self.col_start[ca] as usize..self.col_start[ca + 1] as usize {
                        for b in self.col_start[cb] as usize..self.col_start[cb + 1] as usize {
                            let lo = self.spans[a].y0.max(self.spans[b].y0);
                            let hi = self.spans[a].y1.min(self.spans[b].y1);
                            if hi > lo {
                                pipes.push(Pipe {
                                    a: a as u32,
                                    b: b as u32,
                                    lo,
                                    hi,
                                    ra: slot as u32,
                                    rb: ns,
                                    axis: axis as u8,
                                });
                            }
                        }
                    }
                }
            }
        }
        pipes
    }

    /// Rebuilds every pipe from the current spans. Each region's pipes are
    /// built independently and joined in slot order, so the result is
    /// deterministic.
    fn build_pipes(&mut self) {
        let this = &*self;
        let per_region: Vec<Vec<Pipe>> = (0..this.regions.len())
            .into_par_iter()
            .map(|slot| this.pipes_from(slot, |_| true))
            .collect();
        self.pipes = per_region.into_iter().flatten().collect();
        self.finish_pipes();
    }

    /// Fills each region's pipe list and the span adjacency from `self.pipes`.
    fn finish_pipes(&mut self) {
        for r in &mut self.regions {
            r.pipes.clear();
        }
        for (i, p) in self.pipes.iter().enumerate() {
            self.regions[p.ra as usize].pipes.push(i as u32);
            if p.rb != p.ra {
                self.regions[p.rb as usize].pipes.push(i as u32);
            }
        }
        self.rebuild_adjacency();
    }

    /// Rebuilds `pipes` and `q` after the span layout was reassembled, reusing
    /// the old pipes of every region that is untouched (`touched` holds the
    /// regions with new terrain or a changed frozen flag). A pipe belongs to
    /// the region at its `a` end, which looks towards +x and +z, so the owners
    /// to rebuild are the touched regions and their -x and -z neighbours.
    /// Returns false, changing nothing, if the old pipes are not grouped by
    /// owner (they are not after appended regions); the caller then rebuilds
    /// everything.
    #[allow(clippy::too_many_arguments)]
    fn carry_pipes(
        &mut self,
        touched: &BTreeSet<RegionKey>,
        old_regions: &[Region],
        old_slot: &HashMap<RegionKey, u32>,
        old_col: &[u32],
        old_spans: &[Span],
        old_pipes: &[Pipe],
        old_q: &[f32],
    ) -> bool {
        if old_q.len() != old_pipes.len() {
            return false;
        }
        // Old pipes by owner. They are grouped by owner unless regions were
        // appended since the last full build; then a counting sort groups them.
        let grouped = !old_pipes.windows(2).any(|w| w[0].ra > w[1].ra);
        let mut starts = vec![0usize; old_regions.len() + 1];
        for p in old_pipes {
            starts[p.ra as usize + 1] += 1;
        }
        for i in 0..old_regions.len() {
            starts[i + 1] += starts[i];
        }
        let order: Vec<u32> = if grouped {
            Vec::new()
        } else {
            let mut cursor = starts.clone();
            let mut order = vec![0u32; old_pipes.len()];
            for (i, p) in old_pipes.iter().enumerate() {
                order[cursor[p.ra as usize]] = i as u32;
                cursor[p.ra as usize] += 1;
            }
            order
        };
        let new_of_old: Vec<u32> = old_regions
            .iter()
            .map(|r| self.slot_of.get(&r.key).copied().unwrap_or(u32::MAX))
            .collect();
        let delta: Vec<i64> = old_regions
            .iter()
            .zip(&new_of_old)
            .map(|(r, &n)| {
                if n == u32::MAX {
                    0
                } else {
                    i64::from(self.regions[n as usize].span_start) - i64::from(r.span_start)
                }
            })
            .collect();
        let owner_dirty: Vec<bool> = self
            .regions
            .iter()
            .map(|r| {
                let (kx, kz) = r.key;
                touched.contains(&r.key)
                    || touched.contains(&(kx + 1, kz))
                    || touched.contains(&(kx, kz + 1))
            })
            .collect();
        // The span a pipe end sits in, by column position and floor.
        let key_in = |regions: &[Region], col: &[u32], spans: &[Span], idx: u32| {
            let ci = col.partition_point(|&c| c <= idx) - 1;
            let (x, z) = column_xz(regions[ci / REGION_COLUMNS].key, ci % REGION_COLUMNS);
            span_key(x, z, spans[idx as usize].y0)
        };
        // Flow of the pipes about to be rebuilt, by the spans they join.
        let mut old_flow: HashMap<(u64, u64), f32> = HashMap::new();
        for (so, r) in old_regions.iter().enumerate() {
            let rebuilt = self
                .slot_of
                .get(&r.key)
                .is_none_or(|&n| owner_dirty[n as usize]);
            if !rebuilt {
                continue;
            }
            let range = starts[so]..starts[so + 1];
            let ids: Vec<usize> = if grouped {
                range.collect()
            } else {
                order[range].iter().map(|&i| i as usize).collect()
            };
            for pi in ids {
                if old_q[pi] != 0.0 {
                    let p = old_pipes[pi];
                    old_flow.insert(
                        (
                            key_in(old_regions, old_col, old_spans, p.a),
                            key_in(old_regions, old_col, old_spans, p.b),
                        ),
                        old_q[pi],
                    );
                }
            }
        }
        let this = &*self;
        // Pipes (with their flow) of the owners being rebuilt.
        let rebuilt: Vec<Option<(Vec<Pipe>, Vec<f32>)>> = (0..this.regions.len())
            .into_par_iter()
            .map(|slot| {
                if !owner_dirty[slot] {
                    return None;
                }
                let pipes = this.pipes_from(slot, |_| true);
                let q = if old_flow.is_empty() {
                    vec![0.0; pipes.len()]
                } else {
                    pipes
                        .iter()
                        .map(|p| {
                            let k = (
                                key_in(&this.regions, &this.col_start, &this.spans, p.a),
                                key_in(&this.regions, &this.col_start, &this.spans, p.b),
                            );
                            old_flow.get(&k).copied().unwrap_or(0.0)
                        })
                        .collect()
                };
                Some((pipes, q))
            })
            .collect();
        let lens: Vec<usize> = (0..this.regions.len())
            .map(|slot| match &rebuilt[slot] {
                Some((pipes, _)) => pipes.len(),
                None => {
                    let so = old_slot[&this.regions[slot].key] as usize;
                    starts[so + 1] - starts[so]
                }
            })
            .collect();
        let total: usize = lens.iter().sum();
        let blank = Pipe {
            a: 0,
            b: 0,
            lo: 0,
            hi: 0,
            ra: 0,
            rb: 0,
            axis: 0,
        };
        let mut pipes = vec![blank; total];
        let mut q = vec![0.0f32; total];
        // One disjoint output slice per owner, filled in parallel.
        let mut slices: Vec<(&mut [Pipe], &mut [f32])> = Vec::with_capacity(lens.len());
        let (mut rest_p, mut rest_q) = (pipes.as_mut_slice(), q.as_mut_slice());
        for &len in &lens {
            let (hp, tp) = rest_p.split_at_mut(len);
            let (hq, tq) = rest_q.split_at_mut(len);
            slices.push((hp, hq));
            (rest_p, rest_q) = (tp, tq);
        }
        let carry = |p: &Pipe| Pipe {
            a: (i64::from(p.a) + delta[p.ra as usize]) as u32,
            b: (i64::from(p.b) + delta[p.rb as usize]) as u32,
            lo: p.lo,
            hi: p.hi,
            ra: new_of_old[p.ra as usize],
            rb: new_of_old[p.rb as usize],
            axis: p.axis,
        };
        slices
            .par_iter_mut()
            .enumerate()
            .for_each(|(slot, (out_p, out_q))| {
                if let Some((fresh_p, fresh_q)) = &rebuilt[slot] {
                    out_p.copy_from_slice(fresh_p);
                    out_q.copy_from_slice(fresh_q);
                    return;
                }
                let so = old_slot[&this.regions[slot].key] as usize;
                let range = starts[so]..starts[so + 1];
                if grouped {
                    for (k, i) in range.enumerate() {
                        out_p[k] = carry(&old_pipes[i]);
                        out_q[k] = old_q[i];
                    }
                } else {
                    for (k, &i) in order[range].iter().enumerate() {
                        out_p[k] = carry(&old_pipes[i as usize]);
                        out_q[k] = old_q[i as usize];
                    }
                }
            });
        self.pipes = pipes;
        self.q = q;
        self.finish_pipes();
        true
    }

    /// Span-to-pipe adjacency in compressed rows, ordered by pipe index.
    fn rebuild_adjacency(&mut self) {
        let n = self.spans.len();
        let mut start = vec![0u32; n + 1];
        for p in &self.pipes {
            start[p.a as usize + 1] += 1;
            start[p.b as usize + 1] += 1;
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let mut cursor = start.clone();
        let mut adj = vec![0u32; start[n] as usize];
        for (i, p) in self.pipes.iter().enumerate() {
            adj[cursor[p.a as usize] as usize] = (i as u32) << 1 | 1;
            cursor[p.a as usize] += 1;
            adj[cursor[p.b as usize] as usize] = (i as u32) << 1;
            cursor[p.b as usize] += 1;
        }
        self.adj_start = start;
        self.adj = adj;
    }

    /// Adds brand-new regions without disturbing existing spans, pipes or
    /// flows: new slots and pipes are appended, so every existing index stays.
    fn append_regions(&mut self, fresh: HashMap<RegionKey, RegionTerrain>) {
        let (y_lo, y_hi) = self.terrain.y_range();
        let mut new_keys: Vec<RegionKey> = fresh.keys().copied().collect();
        new_keys.sort_unstable();
        let first_new = self.regions.len() as u32;
        self.col_start.pop();
        for key in new_keys {
            let slot = self.regions.len() as u32;
            let base = self.spans.len();
            extract_columns(
                &fresh[&key],
                y_lo,
                y_hi,
                &mut self.spans,
                &mut self.col_start,
            );
            self.volume.resize(self.spans.len(), 0.0);
            self.head
                .extend(self.spans[base..].iter().map(|s| s.y1 as f32));
            self.slot_of.insert(key, slot);
            self.regions.push(Region {
                key,
                span_start: base as u32,
                span_end: self.spans.len() as u32,
                pipes: Vec::new(),
                active: false,
                quiet_steps: 0,
                ring_done: false,
                frozen: false,
            });
        }
        self.col_start.push(self.spans.len() as u32);

        // Existing regions whose +x/+z neighbour is new also gain pipes.
        let mut existing = BTreeSet::new();
        for slot in first_new..self.regions.len() as u32 {
            let key = self.regions[slot as usize].key;
            for (dx, dz) in [(-1, 0), (0, -1)] {
                if let Some(&e) = self.slot_of.get(&(key.0 + dx, key.1 + dz))
                    && e < first_new
                {
                    existing.insert(e);
                }
            }
        }
        let mut tasks: Vec<(u32, bool)> = (first_new..self.regions.len() as u32)
            .map(|s| (s, false))
            .collect();
        tasks.extend(existing.into_iter().map(|e| (e, true)));
        let this = &*self;
        let built: Vec<Vec<Pipe>> = tasks
            .par_iter()
            .map(|&(slot, only_new)| {
                this.pipes_from(slot as usize, |ns| !only_new || ns >= first_new)
            })
            .collect();
        for p in built.into_iter().flatten() {
            let i = self.pipes.len() as u32;
            self.regions[p.ra as usize].pipes.push(i);
            if p.rb != p.ra {
                self.regions[p.rb as usize].pipes.push(i);
            }
            self.pipes.push(p);
        }
        self.q.resize(self.pipes.len(), 0.0);
        self.eff.resize(self.spans.len(), 0.0);
        self.scale.resize(self.spans.len(), 1.0);
        self.rebuild_adjacency();
        self.lists_dirty = true;
    }

    fn wake_slot(&mut self, s: usize) -> bool {
        let r = &mut self.regions[s];
        r.quiet_steps = 0;
        if r.frozen || r.active {
            return false;
        }
        r.active = true;
        self.lists_dirty = true;
        true
    }

    fn wake_slot_and_neighbours(&mut self, s: usize) {
        let key = self.regions[s].key;
        self.wake_slot(s);
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            if let Some(&n) = self.slot_of.get(&(key.0 + dx, key.1 + dz)) {
                self.wake_slot(n as usize);
            }
        }
    }

    /// Wakes the region holding column `(x, z)`.
    pub fn wake_column(&mut self, x: i64, z: i64) {
        if let Some(&s) = self.slot_of.get(&region_of(x, z)) {
            self.wake_slot(s as usize);
        }
    }

    pub fn wake_all(&mut self) {
        for s in 0..self.regions.len() {
            self.wake_slot(s);
        }
    }

    /// Allocates every region touching the rectangle of columns (inclusive).
    pub fn allocate_rect(&mut self, x0: i64, z0: i64, x1: i64, z1: i64) {
        let (lo, hi) = (region_of(x0, z0), region_of(x1, z1));
        let keys: Vec<_> = (lo.1..=hi.1)
            .flat_map(|rz| (lo.0..=hi.0).map(move |rx| (rx, rz)))
            .collect();
        self.allocate(&keys, &BTreeSet::new());
    }

    /// Tells the world that these regions' terrain has become resident. Waiting
    /// neighbours of water are allocated and both sides wake, so water crosses
    /// the former frozen boundary.
    pub fn notify_resident(&mut self, keys: &[RegionKey]) {
        let mut refresh = BTreeSet::new();
        let mut add = Vec::new();
        for &k in keys {
            if self.slot_of.contains_key(&k) {
                refresh.insert(k);
            } else if self.waiting.remove(&k) {
                add.push(k);
            }
        }
        if !add.is_empty() || !refresh.is_empty() {
            self.allocate(&add, &refresh);
            for k in add {
                if let Some(&s) = self.slot_of.get(&k) {
                    self.wake_slot_and_neighbours(s as usize);
                }
            }
        }
    }

    /// Marks regions' terrain as non-resident: frozen, closed boundary. Their
    /// water is kept and counted but does not move.
    pub fn freeze_regions(&mut self, keys: &[RegionKey]) {
        let mut changed = false;
        for k in keys {
            if let Some(&s) = self.slot_of.get(k) {
                let r = &mut self.regions[s as usize];
                if !r.frozen {
                    r.frozen = true;
                    r.active = false;
                    changed = true;
                }
            }
        }
        if changed {
            self.build_pipes();
            self.q = vec![0.0; self.pipes.len()];
            self.lists_dirty = true;
        }
    }

    /// Re-reads the terrain of the given regions after a committed edit, remaps
    /// their water (volume conserved) and wakes them and their neighbours.
    pub fn refresh_regions(&mut self, keys: &[RegionKey]) -> RemapReport {
        let set: BTreeSet<_> = keys
            .iter()
            .copied()
            .filter(|k| self.slot_of.contains_key(k))
            .collect();
        self.allocate(&[], &set)
    }

    /// Swaps in a new terrain source and refreshes `changed` regions.
    pub fn replace_terrain(
        &mut self,
        terrain: Arc<dyn TerrainSource>,
        changed: &[RegionKey],
    ) -> RemapReport {
        self.terrain = terrain;
        self.dense = None;
        self.refresh_regions(changed)
    }

    /// Dense worlds: re-derives spans from a new committed boundary.
    pub fn apply_boundary(&mut self, boundary: &SolidBoundary) -> Result<RemapReport, ColumnError> {
        let Some((spec, old)) = self.dense.clone() else {
            return Err(ColumnError::DomainMismatch);
        };
        if boundary.spec() != spec {
            return Err(ColumnError::DomainMismatch);
        }
        if boundary.solid == *old {
            return Ok(RemapReport::default());
        }
        let [nx, ny, nz] = spec.dimensions().map(|d| d as usize);
        let o = spec.origin();
        let mut changed = BTreeSet::new();
        for cz in 0..nz {
            for cx in 0..nx {
                if (0..ny).any(|y| {
                    let i = cx + nx * (y + ny * cz);
                    boundary.solid[i] != old[i]
                }) {
                    changed.insert(region_of(o.x + cx as i64, o.z + cz as i64));
                }
            }
        }
        let terrain = BoundaryTerrain::new(boundary);
        self.dense = Some((spec, terrain.solid.clone()));
        self.terrain = Arc::new(terrain);
        let keys: Vec<_> = changed.into_iter().collect();
        Ok(self.refresh_regions(&keys))
    }

    // ---- water -------------------------------------------------------------

    /// Sets every span in the column to the depth reached by an absolute water
    /// level `level_y` (global cell units): spans below it fill, spans above it
    /// stay dry. Wakes the region. The region must be allocated.
    pub fn fill_column_to(&mut self, x: i64, z: i64, level_y: f64) {
        if let Some(range) = self.column_range(x, z) {
            for i in range {
                let s = self.spans[i];
                self.volume[i] = (level_y - f64::from(s.y0)).clamp(0.0, s.capacity());
            }
            self.wake_column(x, z);
        }
    }

    /// Adds water to the span containing `cell` (a source) and wakes its region.
    pub fn add_water(&mut self, cell: GlobalCell, volume: f64) -> Result<(), ColumnError> {
        if !volume.is_finite() || volume < 0.0 {
            return Err(ColumnError::InvalidAmount);
        }
        let i = self.span_at(cell).ok_or(ColumnError::NotOpen(cell))?;
        self.volume[i] += volume;
        self.wake_column(cell.x, cell.z);
        Ok(())
    }

    fn span_at(&self, cell: GlobalCell) -> Option<usize> {
        let y = cell.y;
        self.column_range(cell.x, cell.z)?
            .find(|&i| (i64::from(self.spans[i].y0)..i64::from(self.spans[i].y1)).contains(&y))
    }

    /// Seeds water at rest from a list of full cells (e.g. a worldgen water
    /// plan): allocates their regions, adds one cell^3 each, and leaves every
    /// region asleep. Cells in solid or non-resident terrain are returned.
    pub fn seed_cells(&mut self, cells: &[GlobalCell]) -> Vec<GlobalCell> {
        let keys: BTreeSet<RegionKey> = cells.iter().map(|c| region_of(c.x, c.z)).collect();
        let keys: Vec<_> = keys.into_iter().collect();
        self.allocate(&keys, &BTreeSet::new());
        let mut rejected = Vec::new();
        for &c in cells {
            match self.span_at(c) {
                Some(i) => self.volume[i] += 1.0,
                None => rejected.push(c),
            }
        }
        for r in &mut self.regions {
            r.active = false;
            r.quiet_steps = r.quiet_steps.max(self.config.sleep_steps);
        }
        self.ensure_rings();
        for r in &mut self.regions {
            r.active = false;
        }
        self.lists_dirty = true;
        rejected
    }

    /// Allocates the neighbours of every region that holds water, so water can
    /// always flow into an allocated region.
    fn ensure_rings(&mut self) {
        let mut add: BTreeSet<RegionKey> = BTreeSet::new();
        let mut marked = Vec::new();
        for (s, r) in self.regions.iter().enumerate() {
            if r.ring_done {
                continue;
            }
            let wet = self.volume[r.span_start as usize..r.span_end as usize]
                .iter()
                .any(|&v| v > 1.0e-6);
            if !wet {
                continue;
            }
            marked.push(s);
            for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let k = (r.key.0 + dx, r.key.1 + dz);
                if !self.slot_of.contains_key(&k) {
                    add.insert(k);
                }
            }
        }
        for s in marked {
            self.regions[s].ring_done = true;
        }
        if !add.is_empty() {
            let keys: Vec<_> = add.into_iter().collect();
            self.allocate(&keys, &BTreeSet::new());
        }
    }

    /// Total water including overfill and the unplaced ledger, cells^3.
    pub fn total_volume(&self) -> f64 {
        chunked_sum(&self.volume, |_, v| *v) + self.unplaced
    }

    pub fn total_volume_m3(&self) -> f64 {
        self.total_volume() * CELL_SIZE_M.powi(3)
    }

    /// Water in one column (all spans), cells^3.
    pub fn column_volume(&self, x: i64, z: i64) -> f64 {
        self.column_range(x, z)
            .map(|r| r.map(|i| self.volume[i]).sum())
            .unwrap_or(0.0)
    }

    pub fn unplaced_volume(&self) -> f64 {
        self.unplaced
    }

    /// Water volume above its span's capacity across the world, cells^3.
    pub fn overfill_volume(&self) -> f64 {
        chunked_sum(&self.volume, |i, v| {
            (*v - self.spans[i].capacity()).max(0.0)
        })
    }

    fn effective_heads(&self) -> Vec<f32> {
        (0..self.volume.len())
            .map(|i| self.effective_head(i))
            .collect()
    }

    /// Wetted area of a pipe given the two heads, in cells.
    fn wetted_area(p: &Pipe, ha: f32, hb: f32) -> f32 {
        (ha.max(hb) - p.lo as f32).clamp(0.0, (p.hi - p.lo) as f32)
    }

    /// Largest head difference across any wetted pipe: zero at true rest.
    pub fn max_wetted_head_difference(&self) -> f32 {
        let h = self.effective_heads();
        let film = self.config.min_film_depth;
        self.pipes
            .iter()
            .filter_map(|p| {
                let (ha, hb) = (h[p.a as usize], h[p.b as usize]);
                (Self::wetted_area(p, ha, hb) >= film)
                    .then(|| (ha.max(p.lo as f32) - hb.max(p.lo as f32)).abs())
            })
            .fold(0.0, f32::max)
    }

    pub fn max_abs_flow(&self) -> f32 {
        self.q.iter().fold(0.0, |m, q| m.max(q.abs()))
    }

    /// Largest pipe speed `|q| / A` in m/s over all pipes.
    pub fn peak_speed_m_s(&self) -> f32 {
        let h = self.effective_heads();
        let film = self.config.min_film_depth;
        let mut peak = 0.0f32;
        for (p, q) in self.pipes.iter().zip(&self.q) {
            let a = Self::wetted_area(p, h[p.a as usize], h[p.b as usize]);
            if a >= film {
                peak = peak.max(q.abs() / a);
            }
        }
        peak * CELL_SIZE_M as f32
    }

    fn release_unplaced(&mut self) {
        if self.unplaced > 0.0 && !self.volume.is_empty() {
            let key = |k: usize| f64::from(self.spans[k].y0) + self.volume[k];
            let mut best = 0;
            for i in 1..self.volume.len() {
                if key(i) < key(best) {
                    best = i;
                }
            }
            self.volume[best] += self.unplaced;
            self.unplaced = 0.0;
            let region = self
                .regions
                .partition_point(|r| (r.span_end as usize) <= best);
            if region < self.regions.len() {
                self.wake_slot(region);
            }
        }
    }

    /// Fills the span cell `cell` sits in up to the top of that cell, if it is
    /// open (a spring). Returns the volume added, or `None` if the cell is not
    /// open.
    pub fn fill_cell(&mut self, cell: GlobalCell) -> Option<f64> {
        let i = self.span_at(cell)?;
        let want = (cell.y + 1 - i64::from(self.spans[i].y0)) as f64;
        let added = (want - self.volume[i]).max(0.0);
        if added > 0.0 {
            self.volume[i] += added;
            self.dirty.insert(region_of(cell.x, cell.z));
            self.wake_column(cell.x, cell.z);
        }
        Some(added)
    }

    /// Removes the water that stands in `cell` (up to one cell^3) from its span
    /// (a drain). Returns the volume removed, or `None` if the cell is not open.
    pub fn drain_cell(&mut self, cell: GlobalCell) -> Option<f64> {
        let i = self.span_at(cell)?;
        let in_cell =
            (self.volume[i] - (cell.y - i64::from(self.spans[i].y0)) as f64).clamp(0.0, 1.0);
        if in_cell > 0.0 {
            self.volume[i] -= in_cell;
            self.dirty.insert(region_of(cell.x, cell.z));
            self.wake_column(cell.x, cell.z);
        }
        Some(in_cell)
    }

    /// Regions whose water changed since the last call, in key order.
    pub fn take_dirty_regions(&mut self) -> Vec<RegionKey> {
        std::mem::take(&mut self.dirty).into_iter().collect()
    }

    /// Marks every allocated region as changed (a full baseline is wanted).
    pub fn mark_all_dirty(&mut self) {
        self.dirty.extend(self.regions.iter().map(|r| r.key));
    }

    /// Allocated region keys in slot order.
    pub fn region_keys(&self) -> Vec<RegionKey> {
        self.regions.iter().map(|r| r.key).collect()
    }

    /// Water volume ledger of the world that is not inside any span: only the
    /// unplaced remainder; spans hold the rest.
    /// The wet spans of a region as `(local column, floor y, volume)`, keeping
    /// only spans holding more than `min_volume`.
    pub fn region_wet_spans(&self, key: RegionKey, min_volume: f64) -> Vec<(u16, i32, f64)> {
        let Some(&slot) = self.slot_of.get(&key) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for c in 0..REGION_COLUMNS {
            let ci = slot as usize * REGION_COLUMNS + c;
            for i in self.col_start[ci] as usize..self.col_start[ci + 1] as usize {
                if self.volume[i] > min_volume {
                    out.push((c as u16, self.spans[i].y0, self.volume[i]));
                }
            }
        }
        out
    }

    /// [`Self::region_wet_spans`] with each span's height in cells and the
    /// horizontal velocity `[x, z]` (m/s) of its water, as [`Self::flow_at_m`]
    /// reports it: `(column, floor y, volume, height, flow)`.
    pub fn region_wet_spans_with_flow(
        &self,
        key: RegionKey,
        min_volume: f64,
    ) -> Vec<(u16, i32, f64, i32, [f32; 2])> {
        let Some(&slot) = self.slot_of.get(&key) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for c in 0..REGION_COLUMNS {
            let ci = slot as usize * REGION_COLUMNS + c;
            for i in self.col_start[ci] as usize..self.col_start[ci + 1] as usize {
                if self.volume[i] > min_volume {
                    let span = self.spans[i];
                    out.push((
                        c as u16,
                        span.y0,
                        self.volume[i],
                        span.y1 - span.y0,
                        self.span_flow(i),
                    ));
                }
            }
        }
        out
    }

    /// Height of the span in `(key, column)` whose floor is `y0`, if any.
    pub fn span_height(&self, key: RegionKey, column: u16, y0: i32) -> Option<i32> {
        let slot = *self.slot_of.get(&key)? as usize;
        let ci = slot * REGION_COLUMNS + usize::from(column);
        (self.col_start[ci] as usize..self.col_start[ci + 1] as usize)
            .find(|&i| self.spans[i].y0 == y0)
            .map(|i| self.spans[i].y1 - y0)
    }

    /// Restores one span's volume by `(region, column, floor y)`; the region
    /// must be allocated. Returns false if terrain has no such span (the
    /// caller reports it; water is never silently dropped).
    pub fn restore_span(&mut self, key: RegionKey, column: u16, y0: i32, volume: f64) -> bool {
        let Some(&slot) = self.slot_of.get(&key) else {
            return false;
        };
        let ci = slot as usize * REGION_COLUMNS + usize::from(column);
        match (self.col_start[ci] as usize..self.col_start[ci + 1] as usize)
            .find(|&i| self.spans[i].y0 == y0)
        {
            Some(i) => {
                self.volume[i] = volume;
                self.dirty.insert(key);
                true
            }
            None => false,
        }
    }

    /// Allocates the given regions without adding water (restore). Regions
    /// that are not resident are skipped and returned.
    pub fn allocate_regions(&mut self, keys: &[RegionKey]) -> Vec<RegionKey> {
        self.allocate(keys, &BTreeSet::new());
        keys.iter()
            .copied()
            .filter(|k| !self.slot_of.contains_key(k))
            .collect()
    }

    /// Puts every region to sleep: restored or seeded water is at rest.
    pub fn sleep_all(&mut self) {
        for r in &mut self.regions {
            r.active = false;
            r.quiet_steps = self.config.sleep_steps;
        }
        self.lists_dirty = true;
        self.ensure_rings();
        for r in &mut self.regions {
            r.active = false;
        }
        self.lists_dirty = true;
    }

    /// Adds volume to the unplaced ledger (restore).
    pub fn add_unplaced(&mut self, volume: f64) {
        self.unplaced += volume;
    }

    // ---- stepping ------------------------------------------------------------

    fn rebuild_lists(&mut self) {
        let mut stamp = vec![false; self.spans.len()];
        self.act_spans.clear();
        self.act_pipes.clear();
        self.touched.clear();
        let mut touched = vec![false; self.regions.len()];
        for r in &self.regions {
            if r.active && !r.frozen {
                for s in r.span_start..r.span_end {
                    stamp[s as usize] = true;
                    self.act_spans.push(s);
                }
            }
        }
        for (slot, r) in self.regions.iter().enumerate() {
            if !r.active || r.frozen {
                continue;
            }
            for &pi in &r.pipes {
                let p = self.pipes[pi as usize];
                // Each pipe is owned by its `a` region unless that one sleeps.
                let owner_active = self.regions[p.ra as usize].active;
                if p.ra as usize != slot && owner_active {
                    continue;
                }
                self.act_pipes.push(pi);
                touched[p.ra as usize] = true;
                touched[p.rb as usize] = true;
                for s in [p.a, p.b] {
                    if !stamp[s as usize] {
                        stamp[s as usize] = true;
                        self.act_spans.push(s);
                    }
                }
            }
        }
        for (slot, r) in self.regions.iter().enumerate() {
            if r.active && !r.frozen {
                touched[slot] = true;
            }
        }
        self.touched = (0..self.regions.len() as u32)
            .filter(|&s| touched[s as usize])
            .collect();
        self.lists_dirty = false;
    }

    /// Advances the water by `dt` seconds using adaptive substeps. Every flow is
    /// computed from the previous state and applied together, so the result does
    /// not depend on iteration order or thread count. Sleeping regions cost
    /// nothing; a world with no active region returns immediately.
    pub fn step(&mut self, dt: f64) -> ColumnStepMetrics {
        if self.unplaced > 0.0 {
            self.release_unplaced();
        }
        if self.lists_dirty {
            self.rebuild_lists();
        }
        let mut metrics = ColumnStepMetrics {
            active_regions: self.active_region_count(),
            active_spans: self.act_spans.len(),
            active_pipes: self.act_pipes.len(),
            ..ColumnStepMetrics::default()
        };
        if metrics.active_regions == 0 {
            return metrics;
        }
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
        let mut remaining = dt as f32;
        while remaining > 1.0e-9 && metrics.substeps < self.config.max_substeps {
            remaining -= self.substep(remaining);
            metrics.substeps += 1;
        }
        metrics.skipped_s = remaining.max(0.0);
        for &slot in &self.touched {
            self.dirty.insert(self.regions[slot as usize].key);
        }
        self.after_step(&mut metrics);
        metrics
    }

    /// Quiet tracking, sleeping, waking and ring allocation.
    fn after_step(&mut self, metrics: &mut ColumnStepMetrics) {
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
        let (pipes, q, eff, regions) = (&self.pipes, &self.q, &self.eff, &self.regions);
        let acc = self
            .act_pipes
            .par_iter()
            .fold(new_acc, |mut acc, &pi| {
                let p = pipes[pi as usize];
                let qa = q[pi as usize].abs();
                acc.max_q[p.ra as usize] = acc.max_q[p.ra as usize].max(qa);
                acc.max_q[p.rb as usize] = acc.max_q[p.rb as usize].max(qa);
                let area = Self::wetted_area(&p, eff[p.a as usize], eff[p.b as usize]);
                if area >= cfg.min_film_depth {
                    acc.peak = acc.peak.max(qa / area);
                }
                if qa > cfg.sleep_flow {
                    for r in [p.ra, p.rb] {
                        if !regions[r as usize].active {
                            acc.wake.insert(r);
                        }
                    }
                }
                acc
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
        metrics.peak_speed_m_s = acc.peak * CELL_SIZE_M as f32;
        let mut slept = Vec::new();
        for (s, r) in self.regions.iter_mut().enumerate() {
            if !r.active || r.frozen {
                continue;
            }
            if max_q[s] < cfg.sleep_flow {
                r.quiet_steps += 1;
                if r.quiet_steps >= cfg.sleep_steps {
                    r.active = false;
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
        let dt = max_dt.min(cfg.cfl / (cfg.gravity * self.a_hint * 1.25).sqrt());

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
        let q = &self.q;
        let scale_ptr = Shared(self.scale.as_mut_ptr());
        self.act_spans.par_iter().for_each(|&i| {
            let i = i as usize;
            let (s, e) = (adj_start[i] as usize, adj_start[i + 1] as usize);
            let out: f64 = adj[s..e]
                .iter()
                .map(|&entry| {
                    let flow = f64::from(q[(entry >> 1) as usize]);
                    (if entry & 1 == 1 { flow } else { -flow }).max(0.0)
                })
                .sum::<f64>()
                * f64::from(dt);
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
        let q = &self.q;
        let vol_ptr = Shared(self.volume.as_mut_ptr());
        self.act_spans.par_iter().for_each(|&i| {
            let i = i as usize;
            let (s, e) = (adj_start[i] as usize, adj_start[i + 1] as usize);
            let delta = adj[s..e]
                .iter()
                .map(|&entry| {
                    let flow = f64::from(q[(entry >> 1) as usize]);
                    if entry & 1 == 1 { -flow } else { flow }
                })
                .sum::<f64>()
                * f64::from(dt);
            // SAFETY: one read-modify-write per distinct span index; `volume` is
            // not read through any other path in this pass.
            unsafe { vol_ptr.write(i, (vol_ptr.read(i) + delta).max(0.0)) }
        });
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

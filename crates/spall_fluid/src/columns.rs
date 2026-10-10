//! Stacked water columns with virtual pipes (ENG-165, W1).
//!
//! One column per terrain cell, each split into vertical open spans holding a
//! water volume. Spans in 4-neighbour columns whose open intervals overlap are
//! joined by a persistent flow `q` (cells^3/s) that accelerates with the head
//! difference. Water volume per span is the single conserved quantity; the
//! depth is `min(volume, capacity)` and any excess stays in the span as
//! "overfill", which raises its head and pushes the water out through its pipes.
//! Water is never deleted. Design: `docs/water-agent-handoff.md`.
//!
//! Lengths are in cells (0.25 m), times in seconds. A non-resident or
//! out-of-domain neighbour is a closed wall. This module is CPU-only and has
//! no dependency on bodies, networking or rendering.

use std::collections::HashMap;

use rayon::prelude::*;
use spall_core::GlobalCell;

use crate::{DomainSpec, SolidBoundary};

/// Edge length of one column cell in metres.
pub const CELL_SIZE_M: f64 = 0.25;

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
}

impl Default for ColumnConfig {
    fn default() -> Self {
        Self {
            gravity: (9.81 / CELL_SIZE_M) as f32,
            damping_per_s: 1.5,
            cfl: 0.5,
            min_film_depth: 1.0 / 64.0,
            relax_sweeps: 4,
            max_substeps: 64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ColumnError {
    #[error("terrain boundary covers a different domain than the column world")]
    DomainMismatch,
    #[error("cell {0:?} is solid or outside the column domain")]
    NotOpen(GlobalCell),
    #[error("water amount must be finite and non-negative")]
    InvalidAmount,
}

/// An open vertical run in one column. `y0..y1` are domain-relative cell rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    y0: i32,
    y1: i32,
    /// Reaches the domain top, which counts as open sky: never pressurised.
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
    /// Free-surface height as a global y; for a full span, the pressure head.
    pub head_y: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ColumnStepMetrics {
    pub substeps: u32,
    /// Largest |q| / wetted area over all pipes at the end of the step, m/s.
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

#[derive(Debug, Clone)]
struct Topology {
    col_start: Vec<u32>,
    spans: Vec<Span>,
    pipes: Vec<Pipe>,
    adj_start: Vec<u32>,
    /// (pipe index, true when the span is the pipe's `a` side).
    adj: Vec<(u32, bool)>,
}

#[derive(Debug, Clone)]
pub struct ColumnWorld {
    spec: DomainSpec,
    solid: Vec<bool>,
    config: ColumnConfig,
    topo: Topology,
    volume: Vec<f64>,
    head: Vec<f32>,
    q: Vec<f32>,
    unplaced: f64,
}

fn extract_topology(spec: DomainSpec, solid: &[bool]) -> Topology {
    let [nx, ny, nz] = spec.dimensions().map(|d| d as usize);
    let mut col_start = Vec::with_capacity(nx * nz + 1);
    let mut spans = Vec::new();
    for cz in 0..nz {
        for cx in 0..nx {
            col_start.push(spans.len() as u32);
            let mut y = 0;
            while y < ny {
                if solid[cx + nx * (y + ny * cz)] {
                    y += 1;
                    continue;
                }
                let y0 = y;
                while y < ny && !solid[cx + nx * (y + ny * cz)] {
                    y += 1;
                }
                spans.push(Span {
                    y0: y0 as i32,
                    y1: y as i32,
                    sky: y == ny,
                });
            }
        }
    }
    col_start.push(spans.len() as u32);

    let column = |cx: usize, cz: usize| {
        let c = cx + nx * cz;
        col_start[c] as usize..col_start[c + 1] as usize
    };
    let mut pipes = Vec::new();
    for cz in 0..nz {
        for cx in 0..nx {
            for (ox, oz) in [(1, 0), (0, 1)] {
                let (nbx, nbz) = (cx + ox, cz + oz);
                if nbx >= nx || nbz >= nz {
                    continue;
                }
                for a in column(cx, cz) {
                    for b in column(nbx, nbz) {
                        let lo = spans[a].y0.max(spans[b].y0);
                        let hi = spans[a].y1.min(spans[b].y1);
                        if hi > lo {
                            pipes.push(Pipe {
                                a: a as u32,
                                b: b as u32,
                                lo,
                                hi,
                            });
                        }
                    }
                }
            }
        }
    }
    let mut per_span: Vec<Vec<(u32, bool)>> = vec![Vec::new(); spans.len()];
    for (i, p) in pipes.iter().enumerate() {
        per_span[p.a as usize].push((i as u32, true));
        per_span[p.b as usize].push((i as u32, false));
    }
    let mut adj_start = Vec::with_capacity(spans.len() + 1);
    let mut adj = Vec::with_capacity(pipes.len() * 2);
    for list in per_span {
        adj_start.push(adj.len() as u32);
        adj.extend(list);
    }
    adj_start.push(adj.len() as u32);
    Topology {
        col_start,
        spans,
        pipes,
        adj_start,
        adj,
    }
}

impl ColumnWorld {
    /// Builds dry columns from a captured terrain boundary.
    pub fn new(boundary: &SolidBoundary, config: ColumnConfig) -> Self {
        let spec = boundary.spec();
        let topo = extract_topology(spec, &boundary.solid);
        let spans = topo.spans.len();
        let pipes = topo.pipes.len();
        Self {
            spec,
            solid: boundary.solid.clone(),
            config,
            topo,
            volume: vec![0.0; spans],
            head: vec![0.0; spans],
            q: vec![0.0; pipes],
            unplaced: 0.0,
        }
    }

    pub fn config(&self) -> ColumnConfig {
        self.config
    }

    pub fn spec(&self) -> DomainSpec {
        self.spec
    }

    pub fn span_count(&self) -> usize {
        self.topo.spans.len()
    }

    pub fn pipe_count(&self) -> usize {
        self.topo.pipes.len()
    }

    fn dims(&self) -> [usize; 3] {
        self.spec.dimensions().map(|d| d as usize)
    }

    fn column_range(&self, x: i64, z: i64) -> Option<std::ops::Range<usize>> {
        let [nx, _, nz] = self.dims();
        let o = self.spec.origin();
        let (cx, cz) = (x - o.x, z - o.z);
        if cx < 0 || cz < 0 || cx >= nx as i64 || cz >= nz as i64 {
            return None;
        }
        let c = cx as usize + nx * cz as usize;
        Some(self.topo.col_start[c] as usize..self.topo.col_start[c + 1] as usize)
    }

    /// 0 for a span below its top cell, rising to 1 as it fills; always 0 for
    /// sky spans. Pressure head is blended in over this ramp so the head is
    /// continuous in volume (a hard full/not-full switch chatters).
    fn pressure_ramp(&self, i: usize) -> f32 {
        let s = self.topo.spans[i];
        if s.sky {
            return 0.0;
        }
        let band = s.capacity().min(1.0);
        ((self.volume[i] - (s.capacity() - band)) / band).clamp(0.0, 1.0) as f32
    }

    fn view(&self, i: usize) -> SpanView {
        let s = self.topo.spans[i];
        let oy = self.spec.origin().y;
        let depth = self.volume[i].min(s.capacity());
        let head_rel = f64::from(self.effective_head(i));
        SpanView {
            index: i,
            floor_y: oy + i64::from(s.y0),
            ceiling_y: oy + i64::from(s.y1),
            sky: s.sky,
            volume: self.volume[i],
            depth,
            overfill: (self.volume[i] - s.capacity()).max(0.0),
            head_y: oy as f64 + head_rel,
        }
    }

    /// The spans of one column, bottom to top; empty if solid or outside.
    pub fn spans_in_column(&self, x: i64, z: i64) -> Vec<SpanView> {
        self.column_range(x, z)
            .map(|r| r.map(|i| self.view(i)).collect())
            .unwrap_or_default()
    }

    /// Sets every span in the column to the depth reached by an absolute water
    /// level `level_y` (global cell units): spans below it fill, spans above it
    /// stay dry. Used to seed bodies of water at rest.
    pub fn fill_column_to(&mut self, x: i64, z: i64, level_y: f64) {
        let oy = self.spec.origin().y as f64;
        if let Some(range) = self.column_range(x, z) {
            for i in range {
                let s = self.topo.spans[i];
                self.volume[i] = (level_y - oy - f64::from(s.y0)).clamp(0.0, s.capacity());
            }
        }
    }

    /// Adds water to the span containing `cell` (a source).
    pub fn add_water(&mut self, cell: GlobalCell, volume: f64) -> Result<(), ColumnError> {
        if !volume.is_finite() || volume < 0.0 {
            return Err(ColumnError::InvalidAmount);
        }
        let y = cell.y - self.spec.origin().y;
        let range = self
            .column_range(cell.x, cell.z)
            .ok_or(ColumnError::NotOpen(cell))?;
        let i = range
            .into_iter()
            .find(|&i| {
                (i64::from(self.topo.spans[i].y0)..i64::from(self.topo.spans[i].y1)).contains(&y)
            })
            .ok_or(ColumnError::NotOpen(cell))?;
        self.volume[i] += volume;
        Ok(())
    }

    /// Total water including overfill and the unplaced ledger, cells^3.
    pub fn total_volume(&self) -> f64 {
        self.volume.iter().sum::<f64>() + self.unplaced
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
        (0..self.volume.len())
            .map(|i| (self.volume[i] - self.topo.spans[i].capacity()).max(0.0))
            .sum()
    }

    /// Head of one span: its free surface plus, near full, the relaxed pressure
    /// head above its ceiling. Volume beyond capacity adds one cell of head per
    /// cell^3 (compliance of one cell^2, like a free surface), so water cannot
    /// hide in a pressurised span.
    fn effective_head(&self, i: usize) -> f32 {
        let s = self.topo.spans[i];
        let free = s.y0 as f32 + self.volume[i] as f32;
        let ramp = self.pressure_ramp(i);
        if ramp > 0.0 {
            free + ramp * (self.head[i] - s.y1 as f32).max(0.0)
        } else {
            free
        }
    }

    fn effective_heads(&self) -> Vec<f32> {
        (0..self.volume.len())
            .map(|i| self.effective_head(i))
            .collect()
    }

    /// Wetted area of a pipe given the two heads, in cells.
    fn wetted_area(p: &Pipe, ha: f32, hb: f32) -> f32 {
        let top = ha.max(hb);
        (top - p.lo as f32).clamp(0.0, (p.hi - p.lo) as f32)
    }

    /// Largest head difference across any wetted pipe: zero at true rest.
    pub fn max_wetted_head_difference(&self) -> f32 {
        let h = self.effective_heads();
        let film = self.config.min_film_depth;
        self.topo
            .pipes
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

    /// Largest pipe speed `|q| / A` in m/s.
    pub fn peak_speed_m_s(&self) -> f32 {
        let h = self.effective_heads();
        let film = self.config.min_film_depth;
        let mut peak = 0.0f32;
        for (p, q) in self.topo.pipes.iter().zip(&self.q) {
            let a = Self::wetted_area(p, h[p.a as usize], h[p.b as usize]);
            if a >= film {
                peak = peak.max(q.abs() / a);
            }
        }
        peak * CELL_SIZE_M as f32
    }

    fn release_unplaced(&mut self) {
        if self.unplaced > 0.0 && !self.volume.is_empty() {
            let mut best = 0;
            for i in 1..self.volume.len() {
                let key = |k: usize| f64::from(self.topo.spans[k].y0) + self.volume[k];
                if key(i) < key(best) {
                    best = i;
                }
            }
            self.volume[best] += self.unplaced;
            self.unplaced = 0.0;
        }
    }

    /// Advances the water by `dt` seconds using adaptive substeps. Every flow is
    /// computed from the previous state and applied together, so the result does
    /// not depend on iteration order or thread count.
    pub fn step(&mut self, dt: f64) -> ColumnStepMetrics {
        self.release_unplaced();
        let mut remaining = dt as f32;
        let mut substeps = 0;
        while remaining > 1.0e-9 && substeps < self.config.max_substeps {
            remaining -= self.substep(remaining);
            substeps += 1;
        }
        ColumnStepMetrics {
            substeps,
            peak_speed_m_s: self.peak_speed_m_s(),
            max_abs_flow: self.max_abs_flow(),
            skipped_s: remaining.max(0.0),
        }
    }

    /// Relaxes the base pressure head of full spans toward their neighbours'
    /// heads, seeded by free surfaces; returns every span's effective head.
    fn relax_full_heads(&mut self) -> Vec<f32> {
        let n = self.volume.len();
        let node: Vec<bool> = (0..n).map(|i| self.pressure_ramp(i) > 0.0).collect();
        let ceiling: Vec<f32> = self.topo.spans.iter().map(|s| s.y1 as f32).collect();
        for i in 0..n {
            self.head[i] = if node[i] {
                self.head[i].max(ceiling[i])
            } else {
                ceiling[i]
            };
        }
        // Base heads depend only on free surfaces and other base heads, never on
        // overfill, so overfill is a pure restoring spring on top.
        for _ in 0..self.config.relax_sweeps {
            let source: Vec<f32> = (0..n)
                .map(|i| {
                    if node[i] {
                        self.head[i]
                    } else {
                        self.topo.spans[i].y0 as f32 + self.volume[i] as f32
                    }
                })
                .collect();
            let topo = &self.topo;
            let next: Vec<f32> = (0..n)
                .into_par_iter()
                .map(|i| {
                    if !node[i] {
                        return f32::NAN;
                    }
                    let (s, e) = (topo.adj_start[i] as usize, topo.adj_start[i + 1] as usize);
                    if s == e {
                        return ceiling[i];
                    }
                    let sum: f32 = topo.adj[s..e]
                        .iter()
                        .map(|&(pipe, is_a)| {
                            let p = topo.pipes[pipe as usize];
                            source[if is_a { p.b } else { p.a } as usize]
                        })
                        .sum();
                    ceiling[i].max(sum / (e - s) as f32)
                })
                .collect();
            for i in 0..n {
                if node[i] {
                    self.head[i] = next[i];
                }
            }
        }
        self.effective_heads()
    }

    /// One explicit substep no longer than `max_dt`; returns the time used.
    fn substep(&mut self, max_dt: f32) -> f32 {
        let heads = self.relax_full_heads();
        let cfg = self.config;
        let pipes = &self.topo.pipes;

        let a_max = pipes
            .iter()
            .map(|p| Self::wetted_area(p, heads[p.a as usize], heads[p.b as usize]))
            .fold(0.25f32, f32::max);
        let dt = max_dt.min(cfg.cfl / (cfg.gravity * a_max).sqrt());

        // Flow update with damping; thin films carry nothing.
        let damp = (1.0 - cfg.damping_per_s * dt).max(0.0);
        self.q
            .par_iter_mut()
            .zip(pipes.par_iter())
            .for_each(|(q, p)| {
                let (ha, hb) = (heads[p.a as usize], heads[p.b as usize]);
                let area = Self::wetted_area(p, ha, hb);
                if area < cfg.min_film_depth {
                    *q = 0.0;
                    return;
                }
                let lo = p.lo as f32;
                let dh = ha.max(lo) - hb.max(lo);
                *q = (*q + dt * cfg.gravity * area * dh) * damp;
            });

        // Outflow limit: a donor never gives more than it holds.
        let topo = &self.topo;
        let q = &self.q;
        let volume = &self.volume;
        let scale: Vec<f64> = (0..volume.len())
            .into_par_iter()
            .map(|i| {
                let (s, e) = (topo.adj_start[i] as usize, topo.adj_start[i + 1] as usize);
                let out: f64 = topo.adj[s..e]
                    .iter()
                    .map(|&(pipe, is_a)| {
                        let flow = f64::from(q[pipe as usize]);
                        (if is_a { flow } else { -flow }).max(0.0)
                    })
                    .sum::<f64>()
                    * f64::from(dt);
                if out > volume[i] {
                    volume[i].max(0.0) / out
                } else {
                    1.0
                }
            })
            .collect();
        for (q, p) in self.q.iter_mut().zip(pipes.iter()) {
            let donor = if *q > 0.0 { p.a } else { p.b } as usize;
            if scale[donor] < 1.0 {
                *q = (f64::from(*q) * scale[donor]) as f32;
            }
        }

        // Apply all volume changes at once (Jacobi), spans in parallel.
        let q = &self.q;
        let delta: Vec<f64> = (0..self.volume.len())
            .into_par_iter()
            .map(|i| {
                let (s, e) = (topo.adj_start[i] as usize, topo.adj_start[i + 1] as usize);
                topo.adj[s..e]
                    .iter()
                    .map(|&(pipe, is_a)| {
                        let flow = f64::from(q[pipe as usize]);
                        if is_a { -flow } else { flow }
                    })
                    .sum::<f64>()
                    * f64::from(dt)
            })
            .collect();
        for (v, d) in self.volume.iter_mut().zip(delta) {
            *v = (*v + d).max(0.0);
        }
        dt
    }

    /// Re-derives spans from committed terrain and remaps the water, conserving
    /// volume exactly. Water that no longer fits stays as overfill in the span
    /// holding the old surface, or moves to a neighbouring column, and is pushed
    /// out by its own head over the following steps.
    pub fn apply_boundary(&mut self, boundary: &SolidBoundary) -> Result<RemapReport, ColumnError> {
        if boundary.spec() != self.spec {
            return Err(ColumnError::DomainMismatch);
        }
        if boundary.solid == self.solid {
            return Ok(RemapReport::default());
        }
        let [nx, _, nz] = self.dims();
        let new_topo = extract_topology(self.spec, &boundary.solid);
        let mut new_volume = vec![0.0; new_topo.spans.len()];
        let mut report = RemapReport::default();
        let mut pending: Vec<(usize, usize, f64, f64)> = Vec::new(); // (cx, cz, y, excess)

        for cz in 0..nz {
            for cx in 0..nx {
                let c = cx + nx * cz;
                let old = self.topo.col_start[c] as usize..self.topo.col_start[c + 1] as usize;
                let new = new_topo.col_start[c] as usize..new_topo.col_start[c + 1] as usize;
                let same = old.len() == new.len()
                    && old
                        .clone()
                        .zip(new.clone())
                        .all(|(o, n)| self.topo.spans[o] == new_topo.spans[n]);
                if same {
                    for (o, n) in old.zip(new) {
                        new_volume[n] = self.volume[o];
                    }
                    continue;
                }
                report.columns_changed += 1;
                let old_total: f64 = old.clone().map(|o| self.volume[o]).sum();
                let mut placed_total = 0.0;
                for n in new.clone() {
                    let ns = new_topo.spans[n];
                    let placed: f64 = old
                        .clone()
                        .map(|o| {
                            let os = self.topo.spans[o];
                            let top = f64::from(os.y0) + self.volume[o].min(os.capacity());
                            let lo = f64::from(os.y0).max(f64::from(ns.y0));
                            let hi = top.min(f64::from(ns.y1));
                            (hi - lo).max(0.0)
                        })
                        .sum::<f64>()
                        .min(ns.capacity());
                    new_volume[n] = placed;
                    placed_total += placed;
                }
                let excess = old_total - placed_total;
                if excess <= 0.0 {
                    continue;
                }
                report.relocated_volume += excess;
                let top_y = old
                    .clone()
                    .filter(|&o| self.volume[o] > 0.0)
                    .map(|o| {
                        let os = self.topo.spans[o];
                        f64::from(os.y0) + self.volume[o].min(os.capacity())
                    })
                    .fold(f64::MIN, f64::max);
                match nearest_span(&new_topo, new.clone(), top_y) {
                    Some(n) => new_volume[n] += excess,
                    None => pending.push((cx, cz, top_y, excess)),
                }
            }
        }
        // Columns with no open span left hand their water to a neighbour.
        for (cx, cz, y, excess) in pending {
            let mut target = None;
            for (dx, dz) in [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)] {
                let (bx, bz) = (cx as i64 + dx, cz as i64 + dz);
                if bx < 0 || bz < 0 || bx >= nx as i64 || bz >= nz as i64 {
                    continue;
                }
                let c = bx as usize + nx * bz as usize;
                let range = new_topo.col_start[c] as usize..new_topo.col_start[c + 1] as usize;
                if let Some(n) = nearest_span(&new_topo, range, y) {
                    target = Some(n);
                    break;
                }
            }
            match target {
                Some(n) => new_volume[n] += excess,
                None => {
                    self.unplaced += excess;
                    report.unplaced_volume += excess;
                }
            }
        }

        // Keep flow and pressure head where both endpoint spans are unchanged.
        let key = |topo: &Topology, i: usize| {
            let s = topo.spans[i];
            let c = topo.col_start.partition_point(|&start| start as usize <= i) - 1;
            (c as u32, s.y0, s.y1)
        };
        let old_q: HashMap<_, _> = self
            .topo
            .pipes
            .iter()
            .zip(&self.q)
            .map(|(p, q)| {
                (
                    (key(&self.topo, p.a as usize), key(&self.topo, p.b as usize)),
                    *q,
                )
            })
            .collect();
        let old_head: HashMap<_, _> = (0..self.topo.spans.len())
            .map(|i| (key(&self.topo, i), self.head[i]))
            .collect();
        self.q = new_topo
            .pipes
            .iter()
            .map(|p| {
                old_q
                    .get(&(key(&new_topo, p.a as usize), key(&new_topo, p.b as usize)))
                    .copied()
                    .unwrap_or(0.0)
            })
            .collect();
        self.head = (0..new_topo.spans.len())
            .map(|i| {
                old_head
                    .get(&key(&new_topo, i))
                    .copied()
                    .unwrap_or(new_topo.spans[i].y0 as f32)
            })
            .collect();
        self.topo = new_topo;
        self.volume = new_volume;
        self.solid = boundary.solid.clone();
        self.release_unplaced();
        Ok(report)
    }
}

/// The span in `range` whose interval is closest to height `y`.
fn nearest_span(topo: &Topology, range: std::ops::Range<usize>, y: f64) -> Option<usize> {
    range.min_by(|&a, &b| {
        let dist = |i: usize| {
            let s = topo.spans[i];
            (f64::from(s.y0) - y).max(y - f64::from(s.y1)).max(0.0)
        };
        dist(a).total_cmp(&dist(b))
    })
}

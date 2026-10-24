//! Region allocation, terrain refresh and waking for [`ColumnWorld`].

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use rayon::prelude::*;

use super::*;

impl ColumnWorld {
    /// Allocates the given regions (those that are resident) and re-reads every
    /// key in `refresh`, remapping its water column by column. New regions start
    /// asleep; refreshed ones wake with their neighbours.
    pub(super) fn allocate(
        &mut self,
        add: &[RegionKey],
        refresh: &BTreeSet<RegionKey>,
    ) -> RemapReport {
        if !self.dormant.is_empty() {
            let touched: Vec<RegionKey> = add
                .iter()
                .chain(refresh.iter())
                .copied()
                .filter(|k| self.dormant.contains_key(k))
                .collect();
            self.revive(&touched);
        }
        self.allocate_inner(add, refresh)
    }

    /// Turns dormant sea regions into real ones, asleep, holding the water
    /// they were recorded with. The terrain is read now, so an edited column
    /// takes its water into the span that holds the old floor cell and the
    /// region wakes to even it out; water with no span to go to is unplaced,
    /// never dropped.
    pub(super) fn revive(&mut self, keys: &[RegionKey]) {
        let mut revived = Vec::new();
        for k in keys {
            if let Some(d) = self.dormant.remove(k) {
                self.dormant_volume -= d.volume;
                revived.push((*k, d));
            }
        }
        if revived.is_empty() {
            return;
        }
        let want: Vec<RegionKey> = revived.iter().map(|(k, _)| *k).collect();
        self.allocate_inner(&want, &BTreeSet::new());
        for (key, d) in revived {
            let Some(&slot) = self.slot_of.get(&key) else {
                // Terrain not resident: keep it as it was.
                self.dormant_volume += d.volume;
                self.dormant.insert(key, d);
                continue;
            };
            let mut moved = false;
            for c in 0..REGION_COLUMNS {
                let v = d.column_volume(c);
                if v <= 0.0 {
                    continue;
                }
                let ci = slot as usize * REGION_COLUMNS + c;
                let y0 = i64::from(d.cols[c].0);
                let found =
                    (self.col_start[ci] as usize..self.col_start[ci + 1] as usize).find(|&i| {
                        (i64::from(self.spans[i].y0)..i64::from(self.spans[i].y1)).contains(&y0)
                    });
                match found {
                    Some(i) => {
                        moved |= self.spans[i].y0 != d.cols[c].0
                            || self.spans[i].y1 - self.spans[i].y0 != d.cols[c].1;
                        self.volume[i] += v;
                    }
                    None => {
                        moved = true;
                        self.unplaced += v;
                    }
                }
            }
            if moved {
                self.dirty.insert(key);
                self.wake_slot_and_neighbours(slot as usize);
            }
        }
        self.lists_dirty = true;
    }

    /// Revives the dormant region holding column `(x, z)`, if it is one.
    pub(super) fn revive_at(&mut self, x: i64, z: i64) {
        if !self.dormant.is_empty() {
            self.revive(&[region_of(x, z)]);
        }
    }

    /// Revives every dormant region next to a region that is moving: the
    /// disturbance can cross the border, so the neighbour must be real. Revived
    /// regions start asleep; the flow into them wakes them in turn, so a wave
    /// pulls in only the regions it actually reaches.
    pub(super) fn revive_beside_activity(&mut self) {
        let mut next: Vec<RegionKey> = Vec::new();
        for r in &self.regions {
            if !r.active || r.frozen {
                continue;
            }
            for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let k = (r.key.0 + dx, r.key.1 + dz);
                if self.dormant.contains_key(&k) {
                    next.push(k);
                }
            }
        }
        next.sort_unstable();
        next.dedup();
        self.revive(&next);
    }

    /// The dormant regions among the columns being seeded: regions wholly under
    /// one flat sea level with all eight neighbours the same, whose terrain has
    /// an open surface at that level over every column.
    pub(super) fn find_dormant(
        &self,
        columns: &[(i64, i64, i64, i64)],
    ) -> HashMap<RegionKey, Dormant> {
        // Per region: columns seen, lowest and highest sea top.
        let mut seen: HashMap<RegionKey, (u32, i64, i64)> = HashMap::new();
        for &(x, z, _, top) in columns {
            let e = seen.entry(region_of(x, z)).or_insert((0, top, top));
            e.0 += 1;
            e.1 = e.1.min(top);
            e.2 = e.2.max(top);
        }
        let uniform = |k: RegionKey| {
            seen.get(&k)
                .filter(|e| e.0 as usize == REGION_COLUMNS && e.1 == e.2)
                .map(|e| e.1)
        };
        let mut interior: Vec<(RegionKey, i64)> = seen
            .keys()
            .filter_map(|&k| {
                let top = uniform(k)?;
                for dx in -1..=1 {
                    for dz in -1..=1 {
                        if uniform((k.0 + dx, k.1 + dz)) != Some(top) {
                            return None;
                        }
                    }
                }
                Some((k, top))
            })
            .collect();
        if interior.is_empty() {
            return HashMap::new();
        }
        interior.sort_unstable();
        let mut beds: HashMap<RegionKey, Vec<i32>> = interior
            .iter()
            .map(|&(k, _)| (k, vec![i32::MIN; REGION_COLUMNS]))
            .collect();
        for &(x, z, bed, _) in columns {
            if let Some(b) = beds.get_mut(&region_of(x, z)) {
                let local =
                    (x.rem_euclid(REGION_EDGE) + REGION_EDGE * z.rem_euclid(REGION_EDGE)) as usize;
                b[local] = bed as i32;
            }
        }
        let (y_lo, y_hi) = self.terrain.y_range();
        let mut found = HashMap::new();
        // A bounded batch at a time: each check holds one region's solid mask.
        for chunk in interior.chunks(64) {
            let checked: Vec<Option<Dormant>> = chunk
                .par_iter()
                .map(|&(k, top)| {
                    let terrain = self.terrain.region(k)?;
                    let mut spans = Vec::new();
                    let mut col_start = Vec::new();
                    extract_columns(&terrain, y_lo, y_hi, &mut spans, &mut col_start);
                    let mut cols = Vec::with_capacity(REGION_COLUMNS);
                    let mut volume = 0.0;
                    for c in 0..REGION_COLUMNS {
                        let end = col_start.get(c + 1).map_or(spans.len(), |&e| e as usize);
                        let bed = beds[&k][c];
                        if bed == i32::MIN {
                            return None;
                        }
                        let at = i64::from(bed) + 1;
                        let s = spans[col_start[c] as usize..end]
                            .iter()
                            .find(|s| (i64::from(s.y0)..i64::from(s.y1)).contains(&at))?;
                        // An open surface at the sea level: not under a roof.
                        if top >= i64::from(s.y1) || top < i64::from(s.y0) {
                            return None;
                        }
                        cols.push((s.y0, s.y1 - s.y0));
                        volume += ((top + 1) as f64 - f64::from(s.y0)).clamp(0.0, s.capacity());
                    }
                    Some(Dormant { cols, top, volume })
                })
                .collect();
            for (&(k, _), d) in chunk.iter().zip(checked) {
                if let Some(d) = d {
                    found.insert(k, d);
                }
            }
        }
        found
    }

    /// The wet spans of a dormant region, `(column, floor y, volume, height)`.
    pub(super) fn dormant_spans(
        &self,
        key: RegionKey,
        min_volume: f64,
    ) -> Option<Vec<(u16, i32, f64, i32)>> {
        let d = self.dormant.get(&key)?;
        Some(
            (0..REGION_COLUMNS)
                .filter_map(|c| {
                    let v = d.column_volume(c);
                    (v > min_volume).then(|| (c as u16, d.cols[c].0, v, d.cols[c].1))
                })
                .collect(),
        )
    }

    pub(super) fn allocate_inner(
        &mut self,
        add: &[RegionKey],
        refresh: &BTreeSet<RegionKey>,
    ) -> RemapReport {
        // Every region's solid mask is loaded before any is built: half a MiB
        // each at 128 m of height, so thousands of ocean regions held 2.5 GiB at
        // once. In sorted batches the slots come out in the same order and only
        // one batch of masks is alive.
        const BATCH: usize = 256;
        let wanted: BTreeSet<RegionKey> = add.iter().copied().collect();
        if wanted.len() <= BATCH {
            return self.allocate_batch(add, refresh);
        }
        let keys: Vec<RegionKey> = wanted.into_iter().collect();
        let mut report = RemapReport::default();
        for (n, chunk) in keys.chunks(BATCH).enumerate() {
            let none = BTreeSet::new();
            let done = self.allocate_batch(chunk, if n == 0 { refresh } else { &none });
            report.columns_changed += done.columns_changed;
            report.relocated_volume += done.relocated_volume;
            report.unplaced_volume += done.unplaced_volume;
        }
        report
    }

    pub(super) fn allocate_batch(
        &mut self,
        add: &[RegionKey],
        refresh: &BTreeSet<RegionKey>,
    ) -> RemapReport {
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
                    resting: false,
                    period: 1,
                    owed_s: 0.0,
                    steady: false,
                    steady_steps: 0,
                    steady_time: 0.0,
                    steady_ledger: 0.0,
                    steady_snap: Vec::new(),
                    steady_qsum: Vec::new(),
                    steady_qsq: Vec::new(),
                    steady_ref: Vec::new(),
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
    pub(super) fn pipes_from(&self, slot: usize, accept: impl Fn(u32) -> bool) -> Vec<Pipe> {
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
    pub(super) fn build_pipes(&mut self) {
        let this = &*self;
        let per_region: Vec<Vec<Pipe>> = (0..this.regions.len())
            .into_par_iter()
            .map(|slot| this.pipes_from(slot, |_| true))
            .collect();
        self.pipes = per_region.into_iter().flatten().collect();
        self.finish_pipes();
    }

    /// Fills each region's pipe list and the span adjacency from `self.pipes`.
    pub(super) fn finish_pipes(&mut self) {
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
    pub(super) fn carry_pipes(
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
    pub(super) fn rebuild_adjacency(&mut self) {
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
    pub(super) fn append_regions(&mut self, fresh: HashMap<RegionKey, RegionTerrain>) {
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
                resting: false,
                period: 1,
                owed_s: 0.0,
                steady: false,
                steady_steps: 0,
                steady_time: 0.0,
                steady_ledger: 0.0,
                steady_snap: Vec::new(),
                steady_qsum: Vec::new(),
                steady_qsq: Vec::new(),
                steady_ref: Vec::new(),
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

    pub(super) fn wake_slot(&mut self, s: usize) -> bool {
        let r = &mut self.regions[s];
        r.quiet_steps = 0;
        r.steady_steps = 0;
        r.steady_snap.clear();
        r.steady_qsum.clear();
        r.steady_qsq.clear();
        if r.frozen || r.active {
            return false;
        }
        let was_steady = r.steady;
        r.steady = false;
        r.steady_ref.clear();
        r.active = true;
        if was_steady {
            self.settle_ledger(s);
        }
        self.lists_dirty = true;
        true
    }

    /// Puts a woken region's ledger into its fullest span: in a steady state the
    /// water that entered and left net to about zero, so what is left is small.
    pub(super) fn settle_ledger(&mut self, slot: usize) {
        let r = &mut self.regions[slot];
        let ledger = std::mem::take(&mut r.steady_ledger);
        if ledger == 0.0 {
            return;
        }
        let (lo, hi) = (r.span_start as usize, r.span_end as usize);
        let mut best = lo;
        for i in lo..hi {
            if self.volume[i] > self.volume[best] {
                best = i;
            }
        }
        let after = self.volume[best] + ledger;
        if after >= 0.0 {
            self.volume[best] = after;
        } else {
            // Not enough water in the region to give: the rest was never there.
            self.volume[best] = 0.0;
            self.unplaced += after;
        }
    }

    pub(super) fn wake_slot_and_neighbours(&mut self, s: usize) {
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
        self.revive_at(x, z);
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
            if self.slot_of.contains_key(&k) || self.dormant.contains_key(&k) {
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
            .filter(|k| self.slot_of.contains_key(k) || self.dormant.contains_key(k))
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
}

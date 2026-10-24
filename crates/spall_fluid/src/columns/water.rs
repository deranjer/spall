//! Adding, removing, seeding and reading water in a [`ColumnWorld`].

use std::collections::{BTreeSet, HashMap};

use rayon::prelude::*;

use super::*;

impl ColumnWorld {
    /// Sets every span in the column to the depth reached by an absolute water
    /// level `level_y` (global cell units): spans below it fill, spans above it
    /// stay dry. Wakes the region. The region must be allocated.
    pub fn fill_column_to(&mut self, x: i64, z: i64, level_y: f64) {
        self.revive_at(x, z);
        if let Some(range) = self.column_range(x, z) {
            for i in range {
                let s = self.spans[i];
                self.volume[i] = (level_y - f64::from(s.y0)).clamp(0.0, s.capacity());
            }
            self.wake_column(x, z);
        }
    }

    /// Adds a spring's flow to the span containing `cell`. In a steady region
    /// the flow joins the region's ledger instead and wakes nothing: the
    /// spring's water is part of the steady balance.
    pub fn add_flow(&mut self, cell: GlobalCell, volume: f64) -> Result<(), ColumnError> {
        if !volume.is_finite() || volume < 0.0 {
            return Err(ColumnError::InvalidAmount);
        }
        self.revive_at(cell.x, cell.z);
        let slot = self.slot_of.get(&region_of(cell.x, cell.z)).copied();
        if let Some(slot) = slot
            && self.regions[slot as usize].steady
        {
            self.span_at(cell).ok_or(ColumnError::NotOpen(cell))?;
            self.regions[slot as usize].steady_ledger += volume / self.column_area();
            return Ok(());
        }
        self.add_water(cell, volume)
    }

    /// Adds water to the span containing `cell` (a source) and wakes its region.
    pub fn add_water(&mut self, cell: GlobalCell, volume: f64) -> Result<(), ColumnError> {
        if !volume.is_finite() || volume < 0.0 {
            return Err(ColumnError::InvalidAmount);
        }
        self.revive_at(cell.x, cell.z);
        let i = self.span_at(cell).ok_or(ColumnError::NotOpen(cell))?;
        self.volume[i] += volume / self.column_area();
        self.wake_column(cell.x, cell.z);
        Ok(())
    }

    pub(super) fn span_at(&self, cell: GlobalCell) -> Option<usize> {
        let y = cell.y;
        self.column_range(cell.x, cell.z)?
            .find(|&i| (i64::from(self.spans[i].y0)..i64::from(self.spans[i].y1)).contains(&y))
    }

    /// Seeds water at rest from a list of full cells (e.g. a worldgen water
    /// plan): allocates their regions, adds one cell^3 each, and leaves every
    /// region asleep. Cells in solid or non-resident terrain are returned.
    pub fn seed_cells(&mut self, cells: &[GlobalCell]) -> Vec<GlobalCell> {
        let n = i64::from(self.config.column_cells);
        let keys: BTreeSet<RegionKey> = cells
            .iter()
            .map(|c| region_of(c.x.div_euclid(n), c.z.div_euclid(n)))
            .collect();
        let keys: Vec<_> = keys.into_iter().collect();
        self.allocate(&keys, &BTreeSet::new());
        let mut rejected = Vec::new();
        let per_cell = 1.0 / self.column_area();
        for &c in cells {
            match self.world_cell(c).and_then(|w| self.span_at(w)) {
                Some(i) => self.volume[i] += per_cell,
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

    /// Seeds water at rest from per-column levels, `(x, z, bed, top)`: the span
    /// that starts just above the terrain surface `bed` fills up to and
    /// including cell `top`. Spans below it (caves under a lake) stay dry.
    /// Allocates the regions and leaves every region asleep. Columns with no
    /// open span above their bed are returned, not dropped silently.
    pub fn seed_levels(&mut self, columns: &[(i64, i64, i64, i64)]) -> Vec<(i64, i64, i64, i64)> {
        if self.config.column_cells > 1 {
            return self.seed_levels_wide(columns);
        }
        if self.config.lazy_ocean {
            for (k, d) in self.find_dormant(columns) {
                self.dormant_volume += d.volume;
                self.dormant.insert(k, d);
            }
        }
        let keys: BTreeSet<RegionKey> = columns
            .iter()
            .map(|c| region_of(c.0, c.1))
            .filter(|k| !self.dormant.contains_key(k))
            .collect();
        let keys: Vec<_> = keys.into_iter().collect();
        self.allocate(&keys, &BTreeSet::new());
        let mut rejected = Vec::new();
        for &(x, z, bed, top) in columns {
            if !self.dormant.is_empty() && self.dormant.contains_key(&region_of(x, z)) {
                continue;
            }
            match self.span_at(GlobalCell::new(x, bed + 1, z)) {
                Some(i) => {
                    let s = self.spans[i];
                    self.volume[i] =
                        (((top + 1) as f64) - f64::from(s.y0)).clamp(0.0, s.capacity());
                }
                None => rejected.push((x, z, bed, top)),
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

    /// [`Self::seed_levels`] with wider columns: each column takes the highest
    /// planned surface among the terrain columns it covers, in the span holding
    /// that cell. Terrain columns whose column has no open span there (a solid
    /// cell closes it) are returned.
    fn seed_levels_wide(&mut self, columns: &[(i64, i64, i64, i64)]) -> Vec<(i64, i64, i64, i64)> {
        let n = i64::from(self.config.column_cells);
        let mut tops: HashMap<(i64, i64), i64> = HashMap::new();
        for &(x, z, _, top) in columns {
            let e = tops
                .entry((x.div_euclid(n), z.div_euclid(n)))
                .or_insert(top);
            *e = (*e).max(top);
        }
        let keys: BTreeSet<RegionKey> = tops.keys().map(|&(x, z)| region_of(x, z)).collect();
        let keys: Vec<_> = keys.into_iter().collect();
        self.allocate(&keys, &BTreeSet::new());
        let mut closed = std::collections::HashSet::new();
        for (&(x, z), &top) in &tops {
            match self.span_at(GlobalCell::new(x, top, z)) {
                Some(i) => {
                    let s = self.spans[i];
                    self.volume[i] = ((top + 1) as f64 - f64::from(s.y0)).clamp(0.0, s.capacity());
                }
                None => {
                    closed.insert((x, z));
                }
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
        columns
            .iter()
            .copied()
            .filter(|c| closed.contains(&(c.0.div_euclid(n), c.1.div_euclid(n))))
            .collect()
    }

    /// Allocates the neighbours of every region that holds water, so water can
    /// always flow into an allocated region.
    pub(super) fn ensure_rings(&mut self) {
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
                if !self.slot_of.contains_key(&k) && !self.dormant.contains_key(&k) {
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
    /// Terrain cells^2 one column covers (see [`ColumnConfig::column_cells`]).
    pub(super) fn column_area(&self) -> f64 {
        f64::from(self.config.column_cells * self.config.column_cells)
    }

    pub fn total_volume(&self) -> f64 {
        let ledgers: f64 = self.regions.iter().map(|r| r.steady_ledger).sum();
        (chunked_sum(&self.volume, |_, v| *v) + self.unplaced + ledgers + self.dormant_volume)
            * self.column_area()
    }

    pub fn total_volume_m3(&self) -> f64 {
        self.total_volume() * CELL_SIZE_M.powi(3)
    }

    /// Water in one column (all spans), cells^3.
    pub fn column_volume(&self, x: i64, z: i64) -> f64 {
        if let Some(d) = self.dormant.get(&region_of(x, z)) {
            let local =
                (x.rem_euclid(REGION_EDGE) + REGION_EDGE * z.rem_euclid(REGION_EDGE)) as usize;
            return d.column_volume(local);
        }
        self.column_range(x, z)
            .map(|r| r.map(|i| self.volume[i]).sum::<f64>() * self.column_area())
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

    pub(super) fn effective_heads(&self) -> Vec<f32> {
        (0..self.volume.len())
            .map(|i| self.effective_head(i))
            .collect()
    }

    /// Wetted area of a pipe given the two heads, in cells.
    pub(super) fn wetted_area(p: &Pipe, ha: f32, hb: f32) -> f32 {
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
        peak * CELL_SIZE_M as f32 / self.config.column_cells as f32
    }

    /// The cell of this world that terrain cell `c` falls in: itself at terrain
    /// resolution; with wider columns, the column holding it at the same height,
    /// or the floor of the first open span above if a solid cell closes it.
    pub fn world_cell(&self, c: GlobalCell) -> Option<GlobalCell> {
        let n = i64::from(self.config.column_cells);
        if n == 1 {
            return Some(c);
        }
        let (x, z) = (c.x.div_euclid(n), c.z.div_euclid(n));
        let range = self.column_range(x, z)?;
        let span = range
            .map(|i| self.spans[i])
            .find(|s| c.y < i64::from(s.y1))?;
        Some(GlobalCell::new(x, c.y.max(i64::from(span.y0)), z))
    }

    pub(super) fn release_unplaced(&mut self) {
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
        self.revive_at(cell.x, cell.z);
        let i = self.span_at(cell)?;
        let want = (cell.y + 1 - i64::from(self.spans[i].y0)) as f64;
        let added = (want - self.volume[i]).max(0.0);
        if added > 0.0 {
            self.volume[i] += added;
            self.dirty.insert(region_of(cell.x, cell.z));
            self.wake_column(cell.x, cell.z);
        }
        Some(added * self.column_area())
    }

    /// Removes the water that stands in `cell` (up to one cell^3) from its span
    /// (a drain). Returns the volume removed, or `None` if the cell is not open.
    pub fn drain_cell(&mut self, cell: GlobalCell) -> Option<f64> {
        self.revive_at(cell.x, cell.z);
        let i = self.span_at(cell)?;
        let in_cell =
            (self.volume[i] - (cell.y - i64::from(self.spans[i].y0)) as f64).clamp(0.0, 1.0);
        if in_cell > 0.0 {
            self.volume[i] -= in_cell;
            self.dirty.insert(region_of(cell.x, cell.z));
            self.wake_column(cell.x, cell.z);
        }
        Some(in_cell * self.column_area())
    }

    /// Regions whose water changed since the last call, in key order.
    pub fn take_dirty_regions(&mut self) -> Vec<RegionKey> {
        std::mem::take(&mut self.dirty).into_iter().collect()
    }

    /// Marks every allocated region as changed (a full baseline is wanted).
    pub fn mark_all_dirty(&mut self) {
        self.dirty.extend(self.regions.iter().map(|r| r.key));
        self.dirty.extend(self.dormant.keys().copied());
    }

    /// Allocated region keys in slot order, then the dormant sea regions
    /// (which hold water and report it like any other) in key order.
    pub fn region_keys(&self) -> Vec<RegionKey> {
        let mut keys: Vec<RegionKey> = self.regions.iter().map(|r| r.key).collect();
        let mut dormant: Vec<RegionKey> = self.dormant.keys().copied().collect();
        dormant.sort_unstable();
        keys.extend(dormant);
        keys
    }

    /// How many sea regions are held as records instead of solver regions.
    pub fn dormant_region_count(&self) -> usize {
        self.dormant.len()
    }

    /// Water volume ledger of the world that is not inside any span: only the
    /// unplaced remainder; spans hold the rest.
    /// The wet spans of a region as `(local column, floor y, volume)`, keeping
    /// only spans holding more than `min_volume`.
    pub fn region_wet_spans(&self, key: RegionKey, min_volume: f64) -> Vec<(u16, i32, f64)> {
        let Some(&slot) = self.slot_of.get(&key) else {
            return self
                .dormant_spans(key, min_volume)
                .map(|v| v.into_iter().map(|(c, y0, vol, _)| (c, y0, vol)).collect())
                .unwrap_or_default();
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
            return self
                .dormant_spans(key, min_volume)
                .map(|v| {
                    v.into_iter()
                        .map(|(c, y0, vol, h)| (c, y0, vol, h, [0.0; 2]))
                        .collect()
                })
                .unwrap_or_default();
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
        self.revive(&[key]);
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
            r.steady = false;
            r.steady_snap.clear();
            r.steady_qsum.clear();
            r.steady_qsq.clear();
            r.steady_ref.clear();
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

    /// The sea as an infinite body at a fixed level: every span the last step
    /// touched whose floor is below `top - 4` and whose surface stands within
    /// half a cell above `top` is cut back to `top`, so a river reaching the sea
    /// ends there instead of raising it and waking it far out. Returns the
    /// volume removed.
    pub fn hold_sea_level(&mut self, top: f64) -> f64 {
        let mut removed = 0.0;
        for &i in &self.act_spans {
            let i = i as usize;
            let span = self.spans[i];
            let floor = f64::from(span.y0);
            if !span.sky || floor >= top - 4.0 {
                continue;
            }
            let surface = floor + self.volume[i];
            if surface > top && surface <= top + 0.5 {
                removed += surface - top;
                self.volume[i] = top - floor;
            }
        }
        removed * self.column_area()
    }

    /// Evaporation: removes `depth` cells from every wet span open to the sky
    /// whose surface stands above `above_y`, in every allocated region, asleep
    /// or not (the sea below `above_y` and dormant sea records are left). Every
    /// column of a body loses the same depth, so water at rest stays level and
    /// nothing has to wake. Regions where a span dried out are marked changed.
    /// Returns the volume removed and the wet columns it was taken from.
    pub fn evaporate(&mut self, depth: f64, above_y: f64) -> (f64, usize) {
        // Far water runs in slow motion (see `ColumnConfig::throttle_from_cells`),
        // and so does its evaporation: a region far from every focus point loses
        // a half, a quarter or an eighth, as its springs add.
        let bands = self.config.throttle_from_cells;
        let share: Vec<f64> = self
            .regions
            .iter()
            .map(|r| 1.0 / super::pace_at(&self.focus, bands, r.key) as f64)
            .collect();
        let (spans, regions) = (&self.spans, &self.regions);
        let (removed, wet, dried): (f64, usize, Vec<u32>) = self
            .volume
            .par_chunks_mut(1 << 16)
            .enumerate()
            .map(|(chunk, vols)| {
                let base = chunk << 16;
                let (mut removed, mut wet, mut dried) = (0.0, 0usize, Vec::new());
                // Regions hold consecutive spans in slot order.
                let mut slot = regions.partition_point(|r| (r.span_end as usize) <= base);
                for (k, v) in vols.iter_mut().enumerate() {
                    let i = base + k;
                    while slot < regions.len() && regions[slot].span_end as usize <= i {
                        slot += 1;
                    }
                    let span = spans[i];
                    if *v <= 0.0 || !span.sky || f64::from(span.y0) + *v <= above_y {
                        continue;
                    }
                    wet += 1;
                    let take = (depth * share.get(slot).copied().unwrap_or(1.0)).min(*v);
                    *v -= take;
                    removed += take;
                    if *v <= 0.0 {
                        dried.push(i as u32);
                    }
                }
                (removed, wet, dried)
            })
            .reduce(
                || (0.0, 0, Vec::new()),
                |mut a, b| {
                    a.0 += b.0;
                    a.1 += b.1;
                    a.2.extend(b.2);
                    a
                },
            );
        for i in dried {
            let slot = self.regions.partition_point(|r| r.span_end <= i);
            if let Some(r) = self.regions.get(slot) {
                self.dirty.insert(r.key);
            }
        }
        (removed * self.column_area(), wet)
    }
}

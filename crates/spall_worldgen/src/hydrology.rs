//! Drainage on a coarse height grid: priority-flood depression filling, D8
//! flow, accumulation, lakes, rivers, springs and waterfalls.
//!
//! Everything here is integer arithmetic on fixed-point heights (`FP` units per
//! cell) with `(height, index)` tie-breaks, so the result is a pure function
//! of the heights and platform-exact. Grid samples are `x + n * z`.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Fixed-point units per world cell for grid heights.
pub const FP: i32 = 16;
/// No lake / no cell.
pub const NONE: u32 = u32::MAX;

/// Outlet kinds in the grid handed to [`priority_flood`]: where water leaves.
pub const OUTLET_NONE: u8 = 0;
/// Open sea: a cell below sea level. Also a fixed boundary for erosion.
pub const OUTLET_SEA: u8 = 1;
/// A terminal basin centre of a landlocked map: water ends here.
pub const OUTLET_BASIN: u8 = 2;

pub(crate) const DX: [i64; 8] = [1, -1, 0, 0, 1, 1, -1, -1];
pub(crate) const DZ: [i64; 8] = [0, 0, 1, -1, 1, -1, 1, -1];

/// Result of [`priority_flood`].
pub struct Flood {
    /// Height with every depression raised to its spill level.
    pub filled: Vec<i32>,
    /// Downstream neighbour; an outlet's receiver is itself.
    pub receiver: Vec<u32>,
    /// Cells in the order they were reached from the outlets: every cell comes
    /// after its receiver, so reverse order visits donors first.
    pub order: Vec<u32>,
}

/// Fills depressions from the outlets inward (Barnes' Priority-Flood) and
/// records flow directions. `outlet[i] != 0` marks drains. Panics if there is
/// no outlet: a grid with nowhere to drain has no defined flow.
pub fn priority_flood(h: &[i32], n: usize, outlet: &[u8]) -> Flood {
    let total = n * n;
    assert_eq!(h.len(), total);
    assert_eq!(outlet.len(), total);
    let mut filled = h.to_vec();
    let mut receiver = vec![NONE; total];
    let mut order = Vec::with_capacity(total);
    let mut heap: BinaryHeap<Reverse<(i32, u32)>> = BinaryHeap::new();
    let mut seen = vec![false; total];
    for (i, &o) in outlet.iter().enumerate() {
        if o != OUTLET_NONE {
            receiver[i] = i as u32;
            seen[i] = true;
            heap.push(Reverse((h[i], i as u32)));
        }
    }
    assert!(!heap.is_empty(), "priority_flood needs at least one outlet");
    while let Some(Reverse((level, i))) = heap.pop() {
        order.push(i);
        let (x, z) = ((i as usize % n) as i64, (i as usize / n) as i64);
        for d in 0..8 {
            let (nx, nz) = (x + DX[d], z + DZ[d]);
            if nx < 0 || nz < 0 || nx >= n as i64 || nz >= n as i64 {
                continue;
            }
            let j = nx as usize + n * nz as usize;
            if seen[j] {
                continue;
            }
            seen[j] = true;
            receiver[j] = i;
            filled[j] = h[j].max(level);
            heap.push(Reverse((filled[j], j as u32)));
        }
    }
    Flood {
        filled,
        receiver,
        order,
    }
}

/// Catchment area in samples (the cell itself counts) for every cell.
pub fn accumulate(flood: &Flood) -> Vec<u32> {
    let mut area = vec![1u32; flood.receiver.len()];
    for &i in flood.order.iter().rev() {
        let r = flood.receiver[i as usize];
        if r != i {
            area[r as usize] += area[i as usize];
        }
    }
    area
}

/// A closed depression: connected cells under one common fill level.
pub struct Pit {
    pub level: i32,
    /// Ascending cell indices.
    pub cells: Vec<u32>,
    /// The cell water leaves over (outside the pit, at `level`).
    pub spill: u32,
    /// Deepest point below `level`.
    pub depth: i32,
}

/// Every closed depression of `h` given a completed flood, in flood order of
/// their first cell, so the result is deterministic.
pub fn pits(h: &[i32], flood: &Flood, outlet: &[u8], n: usize) -> Vec<Pit> {
    let total = n * n;
    let mut rank = vec![0u32; total];
    for (k, &i) in flood.order.iter().enumerate() {
        rank[i as usize] = k as u32;
    }
    let is_pit = |i: usize| flood.filled[i] > h[i] && outlet[i] == OUTLET_NONE;
    let mut visited = vec![false; total];
    let mut out = Vec::new();
    for &start in &flood.order {
        let s = start as usize;
        if visited[s] || !is_pit(s) {
            continue;
        }
        let level = flood.filled[s];
        let mut cells = vec![start];
        visited[s] = true;
        let mut at = 0;
        while at < cells.len() {
            let i = cells[at] as usize;
            at += 1;
            let (x, z) = ((i % n) as i64, (i / n) as i64);
            for d in 0..8 {
                let (nx, nz) = (x + DX[d], z + DZ[d]);
                if nx < 0 || nz < 0 || nx >= n as i64 || nz >= n as i64 {
                    continue;
                }
                let j = nx as usize + n * nz as usize;
                if !visited[j] && is_pit(j) && flood.filled[j] == level {
                    visited[j] = true;
                    cells.push(j as u32);
                }
            }
        }
        cells.sort_unstable();
        // The first cell reached from outside is the one next to the pass.
        let first = *cells.iter().min_by_key(|&&c| rank[c as usize]).unwrap();
        let depth = cells
            .iter()
            .map(|&c| level - h[c as usize])
            .max()
            .unwrap_or(0);
        out.push(Pit {
            level,
            cells,
            spill: flood.receiver[first as usize],
            depth,
        });
    }
    out
}

/// Cuts a channel from the pass of every depression larger than `max_area`
/// down to its floor, `rounds` times, so broad basins drain through a channel instead of
/// filling into a lake. Run before erosion, which then widens the channel.
pub fn breach_large_pits(h: &mut [i32], n: usize, outlet: &[u8], max_area: u32, rounds: u32) {
    for _ in 0..rounds {
        let flood = priority_flood(h, n, outlet);
        let mut changed = false;
        for pit in pits(h, &flood, outlet, n) {
            if pit.cells.len() as u32 > max_area {
                // Cut a channel from the pass down the flow path until the ground
                // falls below the pit floor, so the pit drains instead of
                // growing into a bigger pit.
                let floor = pit.level - pit.depth.max(1);
                let mut c = pit.spill as usize;
                for _ in 0..n {
                    if outlet[c] != OUTLET_NONE || h[c] <= floor {
                        break;
                    }
                    h[c] = floor;
                    changed = true;
                    let next = flood.receiver[c] as usize;
                    if next == c {
                        break;
                    }
                    c = next;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

/// Thresholds for turning drainage into water features.
#[derive(Debug, Clone, Copy)]
pub struct HydroParams {
    /// Catchment (samples) from which a channel exists.
    pub river_threshold: u32,
    /// Smallest lake, in samples; shallower or smaller depressions are filled.
    pub min_lake_area: u32,
    /// Largest lake, in samples; a bigger depression is not a lake (it drains).
    pub max_lake_area: u32,
    /// Shallowest lake, in `FP` units of maximum depth.
    pub min_lake_depth: i32,
    /// Springs only rise above this height (`FP` units).
    pub min_spring_height: i32,
    /// A drop at least this large across one river step is a waterfall.
    pub waterfall_drop: i32,
}

impl HydroParams {
    /// Sensible thresholds for an `n x n` grid of 1 m samples.
    pub fn for_grid(n: usize, sea_level: i32) -> Self {
        let area = (n * n) as u32;
        Self {
            river_threshold: (area / 256).max(400),
            min_lake_area: (area / 4096).max(48),
            max_lake_area: (area / 100).max(2000),
            min_lake_depth: 2 * FP,
            min_spring_height: sea_level + 40 * FP,
            waterfall_drop: 4 * FP,
        }
    }
}

/// A body of standing water at one level.
#[derive(Debug, Clone)]
pub struct Lake {
    /// Surface height (`FP` units): the spill height of a basin lake, or the
    /// chosen level of a terminal lake.
    pub level: i32,
    /// Cells under water, ascending.
    pub cells: Vec<u32>,
    /// Where it overflows, or [`NONE`] for a terminal lake (drained, not spilled).
    pub spill: u32,
    /// Deepest point below the surface (`FP` units).
    pub depth: i32,
}

/// A terminal basin the caller wants a lake in: its outlet cell and level.
#[derive(Debug, Clone, Copy)]
pub struct TerminalBasin {
    pub cell: u32,
    pub level: i32,
}

pub struct Hydrology {
    pub n: usize,
    /// Heights with sub-threshold depressions filled in.
    pub height: Vec<i32>,
    pub receiver: Vec<u32>,
    pub order: Vec<u32>,
    /// Catchment area in samples.
    pub area: Vec<u32>,
    pub lakes: Vec<Lake>,
    /// Lake index per cell, or [`NONE`].
    pub lake_of: Vec<u32>,
    pub river: Vec<bool>,
    /// Channel bed: never rises downstream along the receiver chain.
    pub bed: Vec<i32>,
    /// River heads above the minimum height that feed a channel.
    pub springs: Vec<u32>,
    /// River cells whose step to the receiver drops at least the waterfall drop.
    pub falls: Vec<u32>,
    pub outlet: Vec<u8>,
}

impl Hydrology {
    pub fn compute(
        h: &[i32],
        n: usize,
        outlet: &[u8],
        basins: &[TerminalBasin],
        params: &HydroParams,
    ) -> Self {
        let total = n * n;
        let flood = priority_flood(h, n, outlet);
        let area = accumulate(&flood);
        let mut height = h.to_vec();

        // Standing water: connected cells that sit under one common fill level.
        let mut lake_of = vec![NONE; total];
        let mut lakes: Vec<Lake> = Vec::new();
        for pit in pits(h, &flood, outlet, n) {
            let keep = pit.cells.len() as u32 >= params.min_lake_area
                && pit.cells.len() as u32 <= params.max_lake_area
                && pit.depth >= params.min_lake_depth;
            if keep {
                let id = lakes.len() as u32;
                for &c in &pit.cells {
                    lake_of[c as usize] = id;
                }
                lakes.push(Lake {
                    level: pit.level,
                    cells: pit.cells,
                    spill: pit.spill,
                    depth: pit.depth,
                });
            } else {
                // Not a lake: fill it, it drains over the pass.
                for &c in &pit.cells {
                    height[c as usize] = pit.level;
                }
            }
        }

        // Terminal lakes: everything connected to the basin outlet below its level.
        for basin in basins {
            let mut cells = vec![basin.cell];
            let mut taken = vec![false; total];
            taken[basin.cell as usize] = true;
            let mut at = 0;
            while at < cells.len() {
                let i = cells[at] as usize;
                at += 1;
                let (x, z) = ((i % n) as i64, (i / n) as i64);
                for d in 0..8 {
                    let (nx, nz) = (x + DX[d], z + DZ[d]);
                    if nx < 0 || nz < 0 || nx >= n as i64 || nz >= n as i64 {
                        continue;
                    }
                    let j = nx as usize + n * nz as usize;
                    if !taken[j] && lake_of[j] == NONE && h[j] < basin.level {
                        taken[j] = true;
                        cells.push(j as u32);
                    }
                }
            }
            cells.sort_unstable();
            let id = lakes.len() as u32;
            for &c in &cells {
                lake_of[c as usize] = id;
            }
            let depth = cells
                .iter()
                .map(|&c| basin.level - h[c as usize])
                .max()
                .unwrap_or(0);
            lakes.push(Lake {
                level: basin.level,
                cells,
                spill: NONE,
                depth,
            });
        }

        // Channel candidates, then keep those fed from a spring or a lake outflow.
        let sea = |i: usize| outlet[i] == OUTLET_SEA;
        let candidate =
            |i: usize| area[i] >= params.river_threshold && lake_of[i] == NONE && !sea(i);
        let mut has_river_donor = vec![false; total];
        for i in 0..total {
            let r = flood.receiver[i] as usize;
            if r != i && candidate(i) {
                has_river_donor[r] = true;
            }
        }
        let mut fed = vec![false; total];
        let mut springs = Vec::new();
        for i in 0..total {
            if candidate(i) && !has_river_donor[i] && h[i] >= params.min_spring_height {
                fed[i] = true;
                springs.push(i as u32);
            }
        }
        // Donors before receivers: reverse of the flood order.
        for &i in flood.order.iter().rev() {
            let i = i as usize;
            let r = flood.receiver[i] as usize;
            if r == i {
                continue;
            }
            // A lake feeds its outflow; a fed cell feeds its receiver.
            let from_lake = lake_of[i] != NONE;
            if fed[i] || from_lake {
                fed[r] = true;
            }
        }
        let river: Vec<bool> = (0..total).map(|i| candidate(i) && fed[i]).collect();

        // Bed: the minimum of the ground and everything downstream.
        let mut bed = height.clone();
        for &i in &flood.order {
            let i = i as usize;
            let r = flood.receiver[i] as usize;
            if r != i {
                bed[i] = bed[i].min(bed[r]);
            }
        }
        let falls = (0..total)
            .filter(|&i| {
                let r = flood.receiver[i] as usize;
                river[i] && r != i && height[i] - height[r] >= params.waterfall_drop
            })
            .map(|i| i as u32)
            .collect();

        Self {
            n,
            height,
            receiver: flood.receiver,
            order: flood.order,
            area,
            lakes,
            lake_of,
            river,
            bed,
            springs,
            falls,
            outlet: outlet.to_vec(),
        }
    }

    /// The outlet a cell's water finally reaches, following receivers.
    pub fn terminus(&self, mut i: u32) -> u32 {
        while self.receiver[i as usize] != i {
            i = self.receiver[i as usize];
        }
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bowl with a rim notch: water must fill to the notch and spill there.
    fn bowl() -> (Vec<i32>, usize, Vec<u8>) {
        let n = 24;
        let mut h = vec![0i32; n * n];
        let mut outlet = vec![OUTLET_NONE; n * n];
        for z in 0..n {
            for x in 0..n {
                let (dx, dz) = (x as i32 - 12, z as i32 - 12);
                let r2 = dx * dx + dz * dz;
                h[x + n * z] = 100 * FP + (r2.min(81)) * FP / 2;
            }
        }
        // The pit sits on the dry plateau; the sea is a trench along z = 0.
        for x in 0..n {
            h[x] = 10 * FP;
            outlet[x] = OUTLET_SEA;
        }
        // Notch in the rim towards the trench.
        for z in 1..12 {
            h[12 + n * z] = 100 * FP + 30 * FP;
        }
        (h, n, outlet)
    }

    #[test]
    fn every_cell_drains_to_an_outlet_and_order_is_topological() {
        let (h, n, outlet) = bowl();
        let f = priority_flood(&h, n, &outlet);
        assert_eq!(f.order.len(), n * n);
        let mut pos = vec![0usize; n * n];
        for (k, &i) in f.order.iter().enumerate() {
            pos[i as usize] = k;
        }
        for i in 0..n * n {
            let r = f.receiver[i] as usize;
            assert!(pos[r] <= pos[i], "receiver reached after donor");
            assert!(f.filled[i] >= h[i]);
        }
    }

    #[test]
    fn a_pit_fills_to_its_spill_height() {
        let (h, n, outlet) = bowl();
        let hydro = Hydrology::compute(
            &h,
            n,
            &outlet,
            &[],
            &HydroParams {
                river_threshold: 1_000_000,
                min_lake_area: 4,
                max_lake_area: 10_000,
                min_lake_depth: FP,
                min_spring_height: 0,
                waterfall_drop: 1000 * FP,
            },
        );
        assert!(!hydro.lakes.is_empty(), "the bowl holds a lake");
        for lake in &hydro.lakes {
            if lake.spill != NONE {
                assert_eq!(lake.level, hydro.height[lake.spill as usize]);
            }
            for &c in &lake.cells {
                assert!(hydro.height[c as usize] < lake.level);
            }
        }
    }

    #[test]
    fn flooding_is_repeatable() {
        let (h, n, outlet) = bowl();
        let a = priority_flood(&h, n, &outlet);
        let b = priority_flood(&h, n, &outlet);
        assert_eq!(a.order, b.order);
        assert_eq!(a.receiver, b.receiver);
    }
}

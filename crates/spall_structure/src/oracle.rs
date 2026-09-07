//! A trivially-correct dense flood-fill reference for support analysis.
//!
//! This ignores bricks, local components, and the incremental graph entirely:
//! it collects every solid global cell, floods six-connected support out from
//! the anchor plane, and groups whatever is left into connected components by a
//! second flood. [`SupportGraph`](crate::graph::SupportGraph) results are
//! cross-checked against it in tests.
//!
//! It assumes every relevant brick is resident (the G1 "all resident" case): an
//! absent brick is simply not part of the cell set, so do not use the oracle
//! for scenarios that exercise [`Support::Unknown`](crate::support::Support).

use std::collections::{BTreeSet, VecDeque};

use spall_core::GlobalCell;
use spall_voxel::Volume;

use crate::graph::AnchorPlane;

type Cell = (i64, i64, i64);

/// The reference classification of a volume's solid cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenseSupport {
    /// Every solid cell reachable from the anchor plane through solid cells.
    pub supported: BTreeSet<Cell>,
    /// The remaining solid cells, grouped into six-connected components and
    /// ordered by their minimum `(z, y, x)` cell.
    pub unsupported_components: Vec<BTreeSet<Cell>>,
    /// Count of all solid cells.
    pub total_solid: usize,
}

impl DenseSupport {
    pub fn supported_count(&self) -> usize {
        self.supported.len()
    }

    pub fn unsupported_count(&self) -> usize {
        self.unsupported_components.iter().map(BTreeSet::len).sum()
    }

    /// The unsupported component containing `cell`, if any.
    pub fn component_containing(&self, cell: GlobalCell) -> Option<&BTreeSet<Cell>> {
        let key = (cell.x, cell.y, cell.z);
        self.unsupported_components
            .iter()
            .find(|c| c.contains(&key))
    }
}

const NEIGHBOURS: [Cell; 6] = [
    (1, 0, 0),
    (-1, 0, 0),
    (0, 1, 0),
    (0, -1, 0),
    (0, 0, 1),
    (0, 0, -1),
];

/// Runs the dense reference analysis over every resident brick of `volume`.
pub fn dense_support(volume: &Volume, anchor: AnchorPlane) -> DenseSupport {
    // 1. Collect all solid cells.
    let mut solid: BTreeSet<Cell> = BTreeSet::new();
    for coord in volume.resident_brick_coords() {
        let snap = volume
            .snapshot_brick(coord)
            .expect("coord from resident set is in bounds")
            .expect("coord from resident set is resident");
        for z in 0..32u8 {
            for y in 0..32u8 {
                for x in 0..32u8 {
                    let local = spall_core::LocalCell::new(x, y, z).unwrap();
                    if !snap.get(local).is_air() {
                        let g = GlobalCell::from_parts(coord, local)
                            .expect("fixture cell within i64 range");
                        solid.insert((g.x, g.y, g.z));
                    }
                }
            }
        }
    }
    let total_solid = solid.len();

    // 2. Flood support from the anchor plane.
    let mut supported: BTreeSet<Cell> = BTreeSet::new();
    let mut queue: VecDeque<Cell> = VecDeque::new();
    for &(x, y, z) in &solid {
        if y == anchor.y {
            supported.insert((x, y, z));
            queue.push_back((x, y, z));
        }
    }
    while let Some((x, y, z)) = queue.pop_front() {
        for (dx, dy, dz) in NEIGHBOURS {
            let n = (x + dx, y + dy, z + dz);
            if solid.contains(&n) && supported.insert(n) {
                queue.push_back(n);
            }
        }
    }

    // 3. Group the rest into connected components.
    let mut remaining: BTreeSet<Cell> = solid.difference(&supported).copied().collect();
    let mut unsupported_components: Vec<BTreeSet<Cell>> = Vec::new();
    while let Some(&seed) = remaining.iter().next() {
        remaining.remove(&seed);
        let mut component: BTreeSet<Cell> = BTreeSet::from([seed]);
        let mut queue: VecDeque<Cell> = VecDeque::from([seed]);
        while let Some((x, y, z)) = queue.pop_front() {
            for (dx, dy, dz) in NEIGHBOURS {
                let n = (x + dx, y + dy, z + dz);
                if remaining.remove(&n) {
                    component.insert(n);
                    queue.push_back(n);
                }
            }
        }
        unsupported_components.push(component);
    }
    // Canonical order: by minimum (z, y, x) cell.
    unsupported_components.sort_by_key(|c| {
        c.iter()
            .map(|&(x, y, z)| (z, y, x))
            .min()
            .expect("component is non-empty")
    });

    DenseSupport {
        supported,
        unsupported_components,
        total_solid,
    }
}

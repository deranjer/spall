//! A trivially-correct dense reference model, used to cross-check [`Volume`].
//!
//! Available to tests always, and to other crates via the `oracle` feature so
//! later foundation tasks reuse it instead of copying it.
//!
//! [`Volume`]: crate::volume::Volume

use std::collections::{BTreeMap, BTreeSet};

use spall_core::{BrickCoord, GlobalCell, MaterialId};

use crate::edit::CellEdit;

/// Every solid cell ever written, stored explicitly. Absent key == air.
#[derive(Debug, Default, Clone)]
pub struct DenseOracle {
    solid: BTreeMap<(i64, i64, i64), MaterialId>,
    edited_bricks: BTreeSet<(i64, i64, i64)>,
}

impl DenseOracle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies writes with the same last-write-wins, air-clears semantics as
    /// [`crate::volume::Volume::apply_edit`].
    pub fn apply(&mut self, writes: &[CellEdit]) {
        for write in writes {
            let (brick, _) = write.cell.split();
            self.edited_bricks.insert((brick.x, brick.y, brick.z));
            let key = (write.cell.x, write.cell.y, write.cell.z);
            if write.material.is_air() {
                self.solid.remove(&key);
            } else {
                self.solid.insert(key, write.material);
            }
        }
    }

    /// Material at a cell; `MaterialId::AIR` if never written or cleared.
    pub fn material_at(&self, cell: GlobalCell) -> MaterialId {
        self.solid
            .get(&(cell.x, cell.y, cell.z))
            .copied()
            .unwrap_or(MaterialId::AIR)
    }

    /// Whether any write has landed in this brick.
    pub fn brick_edited(&self, coord: BrickCoord) -> bool {
        self.edited_bricks.contains(&(coord.x, coord.y, coord.z))
    }

    /// Count of distinct solid cells (for accounting sanity checks).
    pub fn solid_cell_count(&self) -> usize {
        self.solid.len()
    }
}

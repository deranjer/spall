//! Canonical membership for a component that is about to be split off, plus the
//! conservation ledger that every split must balance.
//!
//! Membership is a sorted list of X-runs (`CellSpanX`): for a fixed `(z, y)` a
//! maximal inclusive `x0..=x1` range of member cells. Runs are ordered by
//! `(z, y, x0)` and never overlap or touch, so the encoding is unique for a
//! given cell set — "canonical source-cell ranges" in the architecture's terms.
//! A giant connected remainder is therefore one membership record with many
//! spans, never one body per cell.

use spall_core::GlobalCell;

use crate::graph::{GlobalComponent, GlobalComponentId, SupportGraph};

/// A maximal run of member cells along X at a fixed `(z, y)`. `x1 >= x0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellSpanX {
    pub z: i64,
    pub y: i64,
    pub x0: i64,
    pub x1: i64,
}

impl CellSpanX {
    /// Number of cells in the run (always at least one).
    pub fn cell_count(&self) -> u64 {
        (self.x1 - self.x0 + 1) as u64
    }

    /// Cells of the run in ascending X.
    pub fn cells(&self) -> impl Iterator<Item = GlobalCell> + '_ {
        (self.x0..=self.x1).map(move |x| GlobalCell::new(x, self.y, self.z))
    }
}

/// The full cell membership of one component, as canonical X-runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentMembership {
    pub id: GlobalComponentId,
    pub cell_count: u64,
    /// Sorted by `(z, y, x0)`, non-overlapping, non-adjacent.
    pub spans: Vec<CellSpanX>,
}

impl ComponentMembership {
    /// Builds membership for `component` by enumerating the member cells of
    /// every node from the graph's stored brick labels.
    pub fn from_component(graph: &SupportGraph, component: &GlobalComponent) -> Self {
        let mut cells: Vec<(i64, i64, i64)> = Vec::with_capacity(component.cell_count as usize);
        for key in &component.nodes {
            let labels = graph
                .brick_labels(key.brick)
                .expect("component node refers to a labelled brick");
            for local_cell in labels.cells(key.local) {
                let g = GlobalCell::from_parts(key.brick, local_cell)
                    .expect("structural cell within i64 range");
                cells.push((g.z, g.y, g.x));
            }
        }
        cells.sort_unstable();
        cells.dedup();

        let mut spans: Vec<CellSpanX> = Vec::new();
        for (z, y, x) in cells {
            match spans.last_mut() {
                Some(span) if span.z == z && span.y == y && span.x1 + 1 == x => {
                    span.x1 = x;
                }
                _ => spans.push(CellSpanX { z, y, x0: x, x1: x }),
            }
        }
        let cell_count = spans.iter().map(CellSpanX::cell_count).sum();
        Self {
            id: component.id,
            cell_count,
            spans,
        }
    }

    /// Member cells in canonical `(z, y, x)` order.
    pub fn cells(&self) -> impl Iterator<Item = GlobalCell> + '_ {
        self.spans.iter().flat_map(CellSpanX::cells)
    }

    /// Whether `cell` is a member.
    pub fn contains(&self, cell: GlobalCell) -> bool {
        self.spans
            .iter()
            .any(|s| s.z == cell.z && s.y == cell.y && (s.x0..=s.x1).contains(&cell.x))
    }
}

/// The split conservation identity:
/// `source_occupied == retained + child_cells + destroyed`.
///
/// For pure support analysis (T07) nothing is explicitly destroyed, `retained`
/// is the supported cell count, and `child_cells` is the sum over unsupported
/// component memberships. It only balances once no component is still
/// [`Unknown`](crate::support::Support::Unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConservationLedger {
    pub source_occupied: u64,
    pub retained: u64,
    pub child_cells: u64,
    pub destroyed: u64,
}

/// Ways a [`ConservationLedger`] can fail to balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConservationError {
    #[error(
        "conservation broken: source {source_occupied} != retained {retained} + child {child_cells} + destroyed {destroyed}"
    )]
    Unbalanced {
        source_occupied: u64,
        retained: u64,
        child_cells: u64,
        destroyed: u64,
    },
}

impl ConservationLedger {
    /// `Ok` iff `source_occupied == retained + child_cells + destroyed`.
    pub fn check(&self) -> Result<(), ConservationError> {
        let sum = self
            .retained
            .checked_add(self.child_cells)
            .and_then(|v| v.checked_add(self.destroyed));
        if sum == Some(self.source_occupied) {
            Ok(())
        } else {
            Err(ConservationError::Unbalanced {
                source_occupied: self.source_occupied,
                retained: self.retained,
                child_cells: self.child_cells,
                destroyed: self.destroyed,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, MaterialId, VolumeId};
    use spall_voxel::{EditPlan, Volume};

    use crate::graph::{AnchorPlane, CancelToken, ResidencyMode, SupportGraph};

    fn membership_of_only_component(v: &Volume, plane_y: i64) -> ComponentMembership {
        let graph = SupportGraph::build(
            v,
            AnchorPlane::at(plane_y),
            ResidencyMode::AllResident,
            &CancelToken::new(),
        )
        .unwrap();
        let component = graph
            .components()
            .iter()
            .find(|c| !c.anchored)
            .expect("one floating component");
        ComponentMembership::from_component(&graph, component)
    }

    #[test]
    fn membership_runs_are_canonical_and_merged() {
        // A 5-wide, 2-tall, 1-deep bar floating above the plane, spanning a
        // brick boundary on X (x = 30..34).
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let bar = EditPlan::filled_box(
            v.id(),
            GlobalCell::new(30, 10, 0),
            GlobalCell::new(34, 11, 0),
            MaterialId(1),
        );
        v.apply_edit(&bar).unwrap();

        let m = membership_of_only_component(&v, 0);
        assert_eq!(m.cell_count, 10);
        // One run per (z, y): two runs, each x = 30..=34, even across the seam.
        assert_eq!(
            m.spans,
            vec![
                CellSpanX {
                    z: 0,
                    y: 10,
                    x0: 30,
                    x1: 34
                },
                CellSpanX {
                    z: 0,
                    y: 11,
                    x0: 30,
                    x1: 34
                },
            ]
        );
        // Runs are strictly ordered by (z, y, x0).
        assert!(m.spans.windows(2).all(|w| w[0] < w[1]));
        assert!(m.contains(GlobalCell::new(32, 10, 0)));
        assert!(!m.contains(GlobalCell::new(35, 10, 0)));
        assert_eq!(m.cells().count(), 10);
    }

    #[test]
    fn conservation_detects_an_imbalance() {
        let ok = ConservationLedger {
            source_occupied: 100,
            retained: 60,
            child_cells: 40,
            destroyed: 0,
        };
        assert!(ok.check().is_ok());

        let bad = ConservationLedger {
            source_occupied: 100,
            retained: 60,
            child_cells: 30,
            destroyed: 0,
        };
        assert!(matches!(
            bad.check(),
            Err(ConservationError::Unbalanced { .. })
        ));
    }
}

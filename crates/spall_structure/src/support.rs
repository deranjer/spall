//! Support classification and the incremental index that carries it.
//!
//! [`StructureIndex`] owns a [`SupportGraph`] plus the [`JobToken`] describing
//! exactly which brick revisions (and which *absent* neighbour sentinels) the
//! current graph was computed from. A downstream consumer validates the token
//! against the live world before trusting the result: an edit that landed after
//! the analysis started makes the token stale and the result must be discarded,
//! exactly like any other off-tick job.
//!
//! A component is:
//! - [`Support::Supported`] — some member cell sits on the support plane;
//! - [`Support::Unsupported`] — fully resident, no anchor: it will be split off
//!   into a dynamic body;
//! - [`Support::Unknown`] — not anchored, but a member cell borders a brick that
//!   is not resident, so it might connect to the ground once that brick loads.
//!   Never treated as air and never as a permanent anchor.

use spall_core::{BrickCoord, GlobalCell, VolumeId};
use spall_jobs::{Generation, JobToken, Staleness, TopologyEpoch, WorldView};
use spall_voxel::{EditOutcome, Volume};

use crate::graph::{
    AnchorPlane, CancelToken, GlobalComponentId, Interrupted, ResidencyMode, SearchBudget,
    SupportGraph,
};
use crate::split::{ComponentMembership, ConservationLedger};

/// The support state of one global component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    /// Anchored to the support plane.
    Supported,
    /// Fully resident and not anchored — a split candidate.
    Unsupported,
    /// Not anchored and blocked on unresident neighbour bricks.
    Unknown { missing: Vec<BrickCoord> },
}

impl Support {
    pub fn is_supported(&self) -> bool {
        matches!(self, Support::Supported)
    }
    pub fn is_unsupported(&self) -> bool {
        matches!(self, Support::Unsupported)
    }
    pub fn is_unknown(&self) -> bool {
        matches!(self, Support::Unknown { .. })
    }
}

/// One component's classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedComponent {
    pub id: GlobalComponentId,
    pub support: Support,
    pub cell_count: u64,
}

/// The support classification of a whole volume at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportReport {
    pub components: Vec<ClassifiedComponent>,
    pub total_solid_cells: u64,
    pub supported_cells: u64,
    pub unsupported_cells: u64,
    pub unknown_cells: u64,
}

impl SupportReport {
    /// Component ids that are [`Support::Unsupported`], canonical order.
    pub fn unsupported_ids(&self) -> Vec<GlobalComponentId> {
        self.filtered(Support::is_unsupported)
    }

    /// Component ids that are [`Support::Unknown`], canonical order.
    pub fn unknown_ids(&self) -> Vec<GlobalComponentId> {
        self.filtered(Support::is_unknown)
    }

    /// Component ids that are [`Support::Supported`], canonical order.
    pub fn supported_ids(&self) -> Vec<GlobalComponentId> {
        self.filtered(Support::is_supported)
    }

    fn filtered(&self, pred: fn(&Support) -> bool) -> Vec<GlobalComponentId> {
        self.components
            .iter()
            .filter(|c| pred(&c.support))
            .map(|c| c.id)
            .collect()
    }

    /// `true` when no component is still [`Support::Unknown`].
    pub fn is_fully_resolved(&self) -> bool {
        self.unknown_cells == 0
    }

    /// The split conservation ledger for a fully-resolved report: supported
    /// cells are retained, unsupported cells become children, nothing is
    /// destroyed. `None` while any component is [`Support::Unknown`].
    pub fn conservation(&self) -> Option<ConservationLedger> {
        self.is_fully_resolved().then_some(ConservationLedger {
            source_occupied: self.total_solid_cells,
            retained: self.supported_cells,
            child_cells: self.unsupported_cells,
            destroyed: 0,
        })
    }
}

/// The incremental structural analysis of one volume.
#[derive(Debug, Clone)]
pub struct StructureIndex {
    volume: VolumeId,
    generation: Generation,
    topology_epoch: TopologyEpoch,
    graph: SupportGraph,
}

impl StructureIndex {
    /// Builds the index for every resident brick of `volume`.
    pub fn build(
        volume: &Volume,
        anchor: AnchorPlane,
        residency: ResidencyMode,
        generation: Generation,
        topology_epoch: TopologyEpoch,
        cancel: &CancelToken,
    ) -> Result<Self, Interrupted> {
        let graph = SupportGraph::build(volume, anchor, residency, cancel)?;
        Ok(Self {
            volume: volume.id(),
            generation,
            topology_epoch,
            graph,
        })
    }

    pub fn volume(&self) -> VolumeId {
        self.volume
    }

    pub fn graph(&self) -> &SupportGraph {
        &self.graph
    }

    /// `true` when a budget-limited pass left the component scan pending; call
    /// [`resume`](Self::resume) before reading a report.
    pub fn is_pending(&self) -> bool {
        self.graph.is_scan_pending()
    }

    /// The read-dependency token for the current graph: every labelled brick at
    /// the revision it was read, plus every absent neighbour that bounds an
    /// unresolved component as an *absent* sentinel.
    pub fn token(&self) -> JobToken {
        let mut token = JobToken::new(self.generation, self.topology_epoch);
        for (brick, revision) in self.graph.read_revisions() {
            token = token.reading(self.volume, brick, revision);
        }
        for brick in self.graph.absent_dependencies() {
            token = token.reading_absent(self.volume, brick);
        }
        token
    }

    /// Validates the current graph against the live world. Anything other than
    /// [`Staleness::Fresh`] means the analysis must be recomputed, not
    /// installed.
    pub fn validate<W: WorldView + ?Sized>(&self, world: &W) -> Staleness {
        self.token().check(world)
    }

    /// Re-labels the bricks named in `outcome`, reassembles the graph, and
    /// returns the fresh classification. This is the deletion / edit
    /// invalidation path: only the changed bricks pay the flood-fill cost.
    ///
    /// `topology_epoch` is the epoch the world moved to when this edit
    /// committed; the index adopts it so its token matches the post-edit world.
    pub fn apply_edit(
        &mut self,
        volume: &Volume,
        outcome: &EditOutcome,
        topology_epoch: TopologyEpoch,
        cancel: &CancelToken,
        budget: SearchBudget,
    ) -> Result<SupportReport, Interrupted> {
        assert_eq!(
            outcome.volume, self.volume,
            "edit outcome targets a different volume than this index"
        );
        let changed: Vec<BrickCoord> = outcome.bricks.iter().map(|b| b.coord).collect();
        // The world moved to this epoch when the edit committed; adopt it before
        // digesting the change so the token matches the post-edit world even if
        // the scan is interrupted and resumed.
        self.topology_epoch = topology_epoch;
        self.graph.apply_changes(volume, &changed, cancel, budget)?;
        Ok(self.report())
    }

    /// Continues a component scan left pending by an [`Interrupted::Budget`].
    pub fn resume(
        &mut self,
        cancel: &CancelToken,
        budget: SearchBudget,
    ) -> Result<(), Interrupted> {
        self.graph.resume(cancel, budget)
    }

    /// Classifies every global component. Panics if a scan is still pending
    /// (call [`resume`](Self::resume) first).
    pub fn report(&self) -> SupportReport {
        assert!(
            !self.graph.is_scan_pending(),
            "component scan is pending; call resume() before report()"
        );
        let mut components = Vec::with_capacity(self.graph.components().len());
        let (mut supported_cells, mut unsupported_cells, mut unknown_cells) = (0u64, 0u64, 0u64);
        for component in self.graph.components() {
            let support = if component.anchored {
                supported_cells += component.cell_count;
                Support::Supported
            } else if component.unresolved.is_empty() {
                unsupported_cells += component.cell_count;
                Support::Unsupported
            } else {
                unknown_cells += component.cell_count;
                Support::Unknown {
                    missing: component.unresolved.clone(),
                }
            };
            components.push(ClassifiedComponent {
                id: component.id,
                support,
                cell_count: component.cell_count,
            });
        }
        SupportReport {
            components,
            total_solid_cells: supported_cells + unsupported_cells + unknown_cells,
            supported_cells,
            unsupported_cells,
            unknown_cells,
        }
    }

    /// Canonical cell membership for every [`Support::Unsupported`] component,
    /// in canonical component-id order. These are the split plans.
    pub fn split_plans(&self) -> Vec<ComponentMembership> {
        let unsupported = self.report().unsupported_ids();
        self.graph
            .components()
            .iter()
            .filter(|c| unsupported.contains(&c.id))
            .map(|c| ComponentMembership::from_component(&self.graph, c))
            .collect()
    }

    /// The component a solid cell belongs to and its support state. `None` if
    /// the cell is not solid / not in a labelled brick.
    pub fn support_of_cell(&self, cell: GlobalCell) -> Option<(GlobalComponentId, Support)> {
        let component = self.graph.classify_cell(cell)?;
        let report = self.report();
        let classified = report.components.into_iter().find(|c| c.id == component)?;
        Some((component, classified.support))
    }
}

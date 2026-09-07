//! The cross-brick support graph.
//!
//! Nodes are `(brick, local component)` pairs. An edge joins two nodes whose
//! bricks are face-adjacent and whose boundary cells line up solid-to-solid
//! across the shared face (six-face connectivity — a diagonal boundary touch is
//! not an edge). Global components are the connected components of that node
//! graph, recomputed by a bounded, cancellable, resumable search rather than
//! trusted from an incremental union-find (union-find cannot undo a
//! disconnection).
//!
//! A node is *anchored* when any of its cells sits on the declared support
//! plane. A node also carries *unresolved* neighbour bricks: face directions
//! where it has a solid boundary cell but the neighbour brick is not resident,
//! so connectivity through that face is unknown, not absent.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use spall_core::{BRICK_EDGE, BrickCoord, GlobalCell, LocalCell, Revision, VolumeId};
use spall_voxel::{BrickState, Volume};

use crate::label::{BrickLabels, LocalComponent, label_brick};

const EDGE: i64 = BRICK_EDGE as i64;

/// Canonical order key for a node: `(z, y, x, local)`.
type NodeOrder = (i64, i64, i64, u16);

/// Cooperative cancellation flag for a structural search. Cheap to clone; all
/// clones share one atomic.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A work ceiling for one call into the component search. When a call settles
/// this many nodes it stops and leaves the graph resumable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchBudget {
    /// Maximum graph nodes this call may settle into a component.
    pub max_nodes: u64,
}

impl SearchBudget {
    /// Large enough never to interrupt a G1-scale graph in one call.
    pub const UNLIMITED: Self = Self {
        max_nodes: u64::MAX,
    };

    pub const fn new(max_nodes: u64) -> Self {
        Self { max_nodes }
    }
}

impl Default for SearchBudget {
    fn default() -> Self {
        Self::UNLIMITED
    }
}

/// Why a structural search stopped before completing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Interrupted {
    #[error("structural search was cancelled")]
    Cancelled,
    #[error("structural search reached its work budget; call resume() to continue")]
    Budget,
}

/// The declared lower support plane. A solid cell whose global `y` equals
/// [`AnchorPlane::y`] is an anchor: its component is supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorPlane {
    pub y: i64,
}

impl AnchorPlane {
    pub const fn at(y: i64) -> Self {
        Self { y }
    }
}

/// How to treat a face-adjacent brick that is not resident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResidencyMode {
    /// G1: the caller guarantees every relevant brick is loaded, so an *absent*
    /// neighbour brick is empty space, not a pending dependency. Connectivity is
    /// [`Unknown`](crate::support::Support::Unknown) only where a brick's load
    /// actually *failed*.
    #[default]
    AllResident,
    /// G3+: an absent neighbour brick may hold geometry that is simply not
    /// loaded yet, so connectivity through it is
    /// [`Unknown`](crate::support::Support::Unknown) until it arrives.
    Streamed,
}

/// Identity of one `(brick, local component)` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeKey {
    pub brick: BrickCoord,
    pub local: LocalComponent,
}

impl NodeKey {
    fn order(self) -> NodeOrder {
        let (z, y, x) = self.brick.sort_key();
        (z, y, x, self.local.get())
    }
}

#[derive(Debug, Clone)]
struct NodeData {
    key: NodeKey,
    cell_count: u32,
    anchored: bool,
    /// Face-adjacent neighbour bricks that are not resident but where this
    /// component has a solid boundary cell. Sorted canonical, de-duplicated.
    unresolved: Vec<BrickCoord>,
}

/// Stable id for a global component within one [`SupportGraph`] state, assigned
/// in canonical node order (the smallest `(z, y, x, local)` node in the
/// component fixes discovery order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GlobalComponentId(pub u32);

/// One connected component of the node graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalComponent {
    pub id: GlobalComponentId,
    /// Member nodes, canonical order.
    pub nodes: Vec<NodeKey>,
    /// Total solid cells across all member nodes.
    pub cell_count: u64,
    /// Any member node sits on the support plane.
    pub anchored: bool,
    /// Union of member nodes' unresolved neighbour bricks, sorted canonical.
    pub unresolved: Vec<BrickCoord>,
}

/// The support graph for one volume at one instant.
#[derive(Debug, Clone)]
pub struct SupportGraph {
    volume: VolumeId,
    anchor: AnchorPlane,
    residency: ResidencyMode,
    /// Per-brick labelling, keyed by raw `(x, y, z)`. Only bricks with at least
    /// one solid cell are kept.
    labels: BTreeMap<(i64, i64, i64), BrickLabels>,
    /// Revision each labelled brick was read at (feeds the job token).
    brick_revision: BTreeMap<(i64, i64, i64), Revision>,
    /// Neighbour bricks probed during assembly that were absent. This includes
    /// all-resident empty-space assumptions so a later load invalidates a
    /// completed analysis.
    absent_neighbours: BTreeSet<(i64, i64, i64)>,
    /// Neighbour bricks probed during assembly whose load had failed.
    failed_neighbours: BTreeSet<(i64, i64, i64)>,
    nodes: Vec<NodeData>,
    node_of: BTreeMap<NodeOrder, usize>,
    adj: Vec<BTreeSet<usize>>,
    components: Vec<GlobalComponent>,
    node_component: Vec<u32>,
    /// Set while a budget-limited component scan is partway done.
    stashed: Option<ScanState>,
}

impl SupportGraph {
    /// Builds the graph for every resident brick of `volume`. Uses an unlimited
    /// search budget, so it can only be interrupted by `cancel`.
    pub fn build(
        volume: &Volume,
        anchor: AnchorPlane,
        residency: ResidencyMode,
        cancel: &CancelToken,
    ) -> Result<Self, Interrupted> {
        let mut graph = Self {
            volume: volume.id(),
            anchor,
            residency,
            labels: BTreeMap::new(),
            brick_revision: BTreeMap::new(),
            absent_neighbours: BTreeSet::new(),
            failed_neighbours: BTreeSet::new(),
            nodes: Vec::new(),
            node_of: BTreeMap::new(),
            adj: Vec::new(),
            components: Vec::new(),
            node_component: Vec::new(),
            stashed: None,
        };
        for coord in volume.resident_brick_coords() {
            if cancel.is_cancelled() {
                return Err(Interrupted::Cancelled);
            }
            graph.relabel_brick(volume, coord);
        }
        graph.reassemble(volume, cancel, SearchBudget::UNLIMITED)?;
        Ok(graph)
    }

    pub fn volume(&self) -> VolumeId {
        self.volume
    }

    pub fn anchor(&self) -> AnchorPlane {
        self.anchor
    }

    /// Global components in canonical id order. Empty while a scan is stashed
    /// (i.e. after an [`Interrupted::Budget`]) until [`resume`](Self::resume).
    pub fn components(&self) -> &[GlobalComponent] {
        &self.components
    }

    /// `true` when a budget-limited scan is partway done and [`resume`] is
    /// required before the component list is meaningful.
    pub fn is_scan_pending(&self) -> bool {
        self.stashed.is_some()
    }

    /// The component a node belongs to, if the node exists and the scan is done.
    pub fn component_of(&self, key: NodeKey) -> Option<&GlobalComponent> {
        let &node = self.node_of.get(&key.order())?;
        let cid = *self.node_component.get(node)? as usize;
        self.components.get(cid)
    }

    /// `(brick coord, revision)` for every labelled brick, in canonical
    /// `(z, y, x)` order.
    pub fn read_revisions(&self) -> impl Iterator<Item = (BrickCoord, Revision)> + '_ {
        let mut out: Vec<(BrickCoord, Revision)> = self
            .brick_revision
            .iter()
            .map(|(&(x, y, z), &rev)| (BrickCoord::new(x, y, z), rev))
            .collect();
        out.sort_by_key(|(c, _)| c.sort_key());
        out.into_iter()
    }

    /// Neighbour bricks that were absent during assembly, in canonical `(z, y,
    /// x)` order. In `AllResident` mode they were read as known empty space;
    /// in `Streamed` mode they are also unresolved support dependencies.
    pub fn absent_dependencies(&self) -> impl Iterator<Item = BrickCoord> + '_ {
        let mut out: Vec<BrickCoord> = self
            .absent_neighbours
            .iter()
            .map(|&(x, y, z)| BrickCoord::new(x, y, z))
            .collect();
        out.sort_by_key(|c| c.sort_key());
        out.into_iter()
    }

    /// Neighbour bricks whose load had failed during assembly, in canonical
    /// `(z, y, x)` order.
    pub fn failed_dependencies(&self) -> impl Iterator<Item = BrickCoord> + '_ {
        let mut out: Vec<BrickCoord> = self
            .failed_neighbours
            .iter()
            .map(|&(x, y, z)| BrickCoord::new(x, y, z))
            .collect();
        out.sort_by_key(|c| c.sort_key());
        out.into_iter()
    }

    /// The labelling of one brick, if it carries any solid cell.
    pub fn brick_labels(&self, coord: BrickCoord) -> Option<&BrickLabels> {
        self.labels.get(&(coord.x, coord.y, coord.z))
    }

    /// The global component a solid cell belongs to, if any. Requires the scan
    /// to be complete.
    pub fn classify_cell(&self, cell: GlobalCell) -> Option<GlobalComponentId> {
        let (brick, local_cell) = cell.split();
        let labels = self.labels.get(&(brick.x, brick.y, brick.z))?;
        let local = labels.component_at(local_cell)?;
        self.component_of(NodeKey { brick, local }).map(|c| c.id)
    }

    /// Relabels the given bricks from `volume` (their labels are invalidated),
    /// then reassembles nodes, edges, and components. This is the deletion /
    /// edit invalidation path: only `changed` bricks pay the flood-fill cost.
    pub fn apply_changes(
        &mut self,
        volume: &Volume,
        changed: &[BrickCoord],
        cancel: &CancelToken,
        budget: SearchBudget,
    ) -> Result<(), Interrupted> {
        for &coord in changed {
            if cancel.is_cancelled() {
                return Err(Interrupted::Cancelled);
            }
            self.relabel_brick(volume, coord);
        }
        self.reassemble(volume, cancel, budget)
    }

    /// Continues a component scan left pending by an [`Interrupted::Budget`].
    /// Returns `Ok(())` when the scan completes.
    pub fn resume(
        &mut self,
        cancel: &CancelToken,
        budget: SearchBudget,
    ) -> Result<(), Interrupted> {
        let stashed = match self.stashed.take() {
            Some(state) => state,
            None => return Ok(()),
        };
        self.run_scan(cancel, budget, Some(stashed))
    }

    /// (Re)labels one brick from the live volume. A brick with no solid cell is
    /// dropped from the label / revision maps.
    fn relabel_brick(&mut self, volume: &Volume, coord: BrickCoord) {
        let key = (coord.x, coord.y, coord.z);
        match volume.brick_state(coord) {
            Ok(BrickState::Resident { revision, .. }) => {
                let snap = volume
                    .snapshot_brick(coord)
                    .expect("bounds checked by brick_state")
                    .expect("resident per brick_state");
                let labels = label_brick(&snap);
                if labels.is_empty() {
                    self.labels.remove(&key);
                } else {
                    self.labels.insert(key, labels);
                }
                self.brick_revision.insert(key, revision);
            }
            _ => {
                self.labels.remove(&key);
                self.brick_revision.remove(&key);
            }
        }
    }

    /// Rebuilds nodes, edges, and global components from the current label map.
    ///
    /// Labelling (per-brick flood fill) is incremental — only changed bricks are
    /// relabelled by the caller. Graph assembly from the label map is a full
    /// recompute; at G1 "all resident" scale that is a few hundred nodes. A true
    /// incremental node/edge patch is a later optimisation, to be added only
    /// against adversarial tests (docs/architecture.md).
    fn reassemble(
        &mut self,
        volume: &Volume,
        cancel: &CancelToken,
        budget: SearchBudget,
    ) -> Result<(), Interrupted> {
        self.nodes.clear();
        self.node_of.clear();
        self.adj.clear();
        self.absent_neighbours.clear();
        self.failed_neighbours.clear();
        self.components.clear();
        self.node_component.clear();
        self.stashed = None;

        let mut ordered: Vec<BrickCoord> = self
            .labels
            .keys()
            .map(|&(x, y, z)| BrickCoord::new(x, y, z))
            .collect();
        ordered.sort_by_key(|c| c.sort_key());

        for coord in &ordered {
            if cancel.is_cancelled() {
                return Err(Interrupted::Cancelled);
            }
            let labels = self.labels[&(coord.x, coord.y, coord.z)].clone();
            for local in labels.components() {
                let (anchored, unresolved) = self.node_attributes(volume, *coord, &labels, local);
                let node = self.nodes.len();
                let key = NodeKey {
                    brick: *coord,
                    local,
                };
                self.nodes.push(NodeData {
                    key,
                    cell_count: labels.cell_count(local),
                    anchored,
                    unresolved,
                });
                self.node_of.insert(key.order(), node);
            }
        }
        self.adj = vec![BTreeSet::new(); self.nodes.len()];

        for coord in &ordered {
            for axis in 0..3usize {
                let neighbour = axis_step(*coord, axis, 1);
                if self
                    .labels
                    .contains_key(&(neighbour.x, neighbour.y, neighbour.z))
                {
                    self.link_face(*coord, neighbour, axis);
                }
            }
        }

        self.run_scan(cancel, budget, None)
    }

    /// Anchored flag and unresolved-neighbour list for one node.
    fn node_attributes(
        &mut self,
        volume: &Volume,
        coord: BrickCoord,
        labels: &BrickLabels,
        local: LocalComponent,
    ) -> (bool, Vec<BrickCoord>) {
        let cells = labels.cells(local);
        let mut anchored = false;
        let mut faces = [false; 6];
        for cell in &cells {
            let g = GlobalCell::from_parts(coord, *cell).expect("structural cell within i64 range");
            if g.y == self.anchor.y {
                anchored = true;
            }
            let (x, y, z) = (
                i64::from(cell.x()),
                i64::from(cell.y()),
                i64::from(cell.z()),
            );
            faces[0] |= x == 0;
            faces[1] |= x == EDGE - 1;
            faces[2] |= y == 0;
            faces[3] |= y == EDGE - 1;
            faces[4] |= z == 0;
            faces[5] |= z == EDGE - 1;
        }

        let mut unresolved = Vec::new();
        for (face, &present) in faces.iter().enumerate() {
            if !present {
                continue;
            }
            let (axis, step): (usize, i64) = match face {
                0 => (0, -1),
                1 => (0, 1),
                2 => (1, -1),
                3 => (1, 1),
                4 => (2, -1),
                _ => (2, 1),
            };
            let neighbour = axis_step(coord, axis, step);
            if self
                .labels
                .contains_key(&(neighbour.x, neighbour.y, neighbour.z))
            {
                continue; // resident with solid cells: link_face handles it
            }
            match volume.brick_state(neighbour) {
                // Resident but not in the label map: no solid cell, so it can
                // carry no connectivity.
                Ok(BrickState::Resident { .. }) => {}
                // A failed load is always an unresolved dependency and must be
                // preserved in the exact read-dependency token.
                Ok(BrickState::Failed) => {
                    self.failed_neighbours
                        .insert((neighbour.x, neighbour.y, neighbour.z));
                    unresolved.push(neighbour);
                }
                // Absent: unknown only when the world is streamed; under
                // `AllResident` the caller guarantees this is empty space.
                Ok(BrickState::Absent) => {
                    self.absent_neighbours
                        .insert((neighbour.x, neighbour.y, neighbour.z));
                    if self.residency == ResidencyMode::Streamed {
                        unresolved.push(neighbour);
                    }
                }
                // Outside the volume's declared bounds: a hard world edge,
                // definitively empty — never an unresolved dependency.
                Err(_) => {}
            }
        }
        unresolved.sort_by_key(|c| c.sort_key());
        unresolved.dedup();
        (anchored, unresolved)
    }

    /// Adds edges between components of `a` and `b` (`b = a` stepped `+1` on
    /// `axis`) wherever their boundary cells are both solid at matching
    /// cross-face coordinates.
    fn link_face(&mut self, a: BrickCoord, b: BrickCoord, axis: usize) {
        let hi = (BRICK_EDGE - 1) as u8;
        let mut edges: Vec<(usize, usize)> = Vec::new();
        {
            let la = &self.labels[&(a.x, a.y, a.z)];
            let lb = &self.labels[&(b.x, b.y, b.z)];
            for u in 0..BRICK_EDGE as u8 {
                for v in 0..BRICK_EDGE as u8 {
                    let (ca, cb) = match axis {
                        0 => (
                            LocalCell::new(hi, u, v).unwrap(),
                            LocalCell::new(0, u, v).unwrap(),
                        ),
                        1 => (
                            LocalCell::new(u, hi, v).unwrap(),
                            LocalCell::new(u, 0, v).unwrap(),
                        ),
                        _ => (
                            LocalCell::new(u, v, hi).unwrap(),
                            LocalCell::new(u, v, 0).unwrap(),
                        ),
                    };
                    let (Some(comp_a), Some(comp_b)) = (la.component_at(ca), lb.component_at(cb))
                    else {
                        continue;
                    };
                    let na = self.node_of[&NodeKey {
                        brick: a,
                        local: comp_a,
                    }
                    .order()];
                    let nb = self.node_of[&NodeKey {
                        brick: b,
                        local: comp_b,
                    }
                    .order()];
                    edges.push((na, nb));
                }
            }
        }
        for (na, nb) in edges {
            self.adj[na].insert(nb);
            self.adj[nb].insert(na);
        }
    }

    /// Runs (or resumes) the connected-component pass. On success writes
    /// `components` / `node_component`. On [`Interrupted::Budget`] or
    /// [`Interrupted::Cancelled`] stashes progress for [`resume`](Self::resume).
    fn run_scan(
        &mut self,
        cancel: &CancelToken,
        budget: SearchBudget,
        resume: Option<ScanState>,
    ) -> Result<(), Interrupted> {
        let n = self.nodes.len();
        let mut state = match resume {
            Some(state) if state.node_component.len() == n => state,
            _ => ScanState {
                node_component: vec![u32::MAX; n],
                finished: Vec::new(),
                next_start: 0,
                current: None,
            },
        };

        let mut settled: u64 = 0;
        loop {
            if cancel.is_cancelled() {
                self.stashed = Some(state);
                return Err(Interrupted::Cancelled);
            }

            let mut part = match state.current.take() {
                Some(part) => part,
                None => {
                    while state.next_start < n && state.node_component[state.next_start] != u32::MAX
                    {
                        state.next_start += 1;
                    }
                    if state.next_start >= n {
                        break;
                    }
                    let start = state.next_start;
                    let id = state.finished.len() as u32;
                    state.node_component[start] = id;
                    PartialComponent {
                        id,
                        frontier: VecDeque::from([start]),
                        nodes: vec![start],
                    }
                }
            };

            loop {
                if settled >= budget.max_nodes {
                    state.current = Some(part);
                    self.stashed = Some(state);
                    return Err(Interrupted::Budget);
                }
                let Some(here) = part.frontier.pop_front() else {
                    break;
                };
                settled += 1;
                for &next in &self.adj[here] {
                    if state.node_component[next] == u32::MAX {
                        state.node_component[next] = part.id;
                        part.nodes.push(next);
                        part.frontier.push_back(next);
                    }
                }
            }

            part.nodes.sort_by_key(|&i| self.nodes[i].key.order());
            let mut cell_count = 0u64;
            let mut anchored = false;
            let mut unresolved: Vec<BrickCoord> = Vec::new();
            let mut keys = Vec::with_capacity(part.nodes.len());
            for &i in &part.nodes {
                let node = &self.nodes[i];
                cell_count += u64::from(node.cell_count);
                anchored |= node.anchored;
                unresolved.extend(node.unresolved.iter().copied());
                keys.push(node.key);
            }
            unresolved.sort_by_key(|c| c.sort_key());
            unresolved.dedup();
            state.finished.push(GlobalComponent {
                id: GlobalComponentId(part.id),
                nodes: keys,
                cell_count,
                anchored,
                unresolved,
            });
        }

        self.components = state.finished;
        self.node_component = state.node_component;
        self.stashed = None;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ScanState {
    node_component: Vec<u32>,
    finished: Vec<GlobalComponent>,
    next_start: usize,
    current: Option<PartialComponent>,
}

#[derive(Debug, Clone)]
struct PartialComponent {
    id: u32,
    frontier: VecDeque<usize>,
    nodes: Vec<usize>,
}

#[inline]
fn axis_step(coord: BrickCoord, axis: usize, step: i64) -> BrickCoord {
    match axis {
        0 => BrickCoord::new(coord.x + step, coord.y, coord.z),
        1 => BrickCoord::new(coord.x, coord.y + step, coord.z),
        _ => BrickCoord::new(coord.x, coord.y, coord.z + step),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, GlobalCell, MaterialId};
    use spall_voxel::{EditPlan, Volume};

    fn vol() -> Volume {
        Volume::new(spall_core::VolumeId::new(1).unwrap(), CellSizeCode::Quarter)
    }

    fn fill(v: &mut Volume, a: GlobalCell, b: GlobalCell) {
        v.apply_edit(&EditPlan::filled_box(v.id(), a, b, MaterialId(1)))
            .unwrap();
    }

    #[test]
    fn a_beam_across_four_bricks_is_one_component_spanning_four_nodes() {
        let mut v = vol();
        fill(
            &mut v,
            GlobalCell::new(-40, 5, 0),
            GlobalCell::new(39, 5, 0),
        );
        let g = SupportGraph::build(
            &v,
            AnchorPlane::at(-100),
            ResidencyMode::AllResident,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(g.components().len(), 1);
        assert_eq!(
            g.components()[0].nodes.len(),
            4,
            "one node per crossed brick"
        );
        assert_eq!(g.components()[0].cell_count, 80);
        assert!(!g.components()[0].anchored);
    }

    #[test]
    fn component_ids_follow_canonical_node_order() {
        // Three disjoint blobs; the one with the smallest (z, y, x) brick/cell
        // must be component 0.
        let mut v = vol();
        fill(
            &mut v,
            GlobalCell::new(40, 40, 40),
            GlobalCell::new(42, 42, 42),
        ); // brick (1,1,1)
        fill(&mut v, GlobalCell::new(0, 0, 0), GlobalCell::new(2, 2, 2)); // brick (0,0,0)
        fill(&mut v, GlobalCell::new(70, 5, 5), GlobalCell::new(72, 7, 7)); // brick (2,0,0)

        let g = SupportGraph::build(
            &v,
            AnchorPlane::at(-100),
            ResidencyMode::AllResident,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(g.components().len(), 3);
        assert_eq!(g.components()[0].id, GlobalComponentId(0));
        assert_eq!(g.components()[0].nodes[0].brick, BrickCoord::new(0, 0, 0));
        // Canonical (z, y, x): (0,0,0) < (0,0,2) < (1,1,1).
        assert_eq!(g.components()[1].nodes[0].brick, BrickCoord::new(2, 0, 0));
        assert_eq!(g.components()[2].nodes[0].brick, BrickCoord::new(1, 1, 1));

        // read_revisions covers exactly the labelled bricks, canonical order.
        let revs: Vec<BrickCoord> = g.read_revisions().map(|(c, _)| c).collect();
        assert_eq!(
            revs,
            vec![
                BrickCoord::new(0, 0, 0),
                BrickCoord::new(2, 0, 0),
                BrickCoord::new(1, 1, 1),
            ]
        );
    }

    #[test]
    fn a_failed_neighbour_brick_makes_a_bordering_component_unresolved() {
        let mut v = vol();
        fill(&mut v, GlobalCell::new(0, 10, 5), GlobalCell::new(5, 14, 5));
        v.mark_failed(BrickCoord::new(-1, 0, 0)).unwrap();

        let g = SupportGraph::build(
            &v,
            AnchorPlane::at(0),
            ResidencyMode::AllResident,
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(g.components().len(), 1);
        assert_eq!(
            g.components()[0].unresolved,
            vec![BrickCoord::new(-1, 0, 0)],
            "the -X face borders the failed brick"
        );
        assert_eq!(
            g.failed_dependencies().collect::<Vec<_>>(),
            vec![BrickCoord::new(-1, 0, 0)]
        );
    }
}

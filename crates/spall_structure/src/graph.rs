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
use std::sync::Arc;

use spall_core::{BRICK_EDGE, BrickCoord, GlobalCell, Revision, VolumeId};
use spall_voxel::{BrickState, Volume};

use crate::label::{BrickLabels, LabelCache, LocalComponent, label_brick};

const EDGE: i64 = BRICK_EDGE as i64;
const EDGE_US: usize = BRICK_EDGE as usize;

/// Linear index of a brick-local cell, `x + 32 * (y + 32 * z)`.
#[inline]
fn idx_of(x: usize, y: usize, z: usize) -> usize {
    x + EDGE_US * (y + EDGE_US * z)
}

/// Canonical order key for a node: `(z, y, x, local)`.
type NodeOrder = (i64, i64, i64, u16);

/// Raw `(x, y, z)` brick key.
type BrickKey = (i64, i64, i64);

/// A node's neighbours as slab indices, kept inline up to [`INLINE_NEIGHBOURS`] (a brick has six
/// faces, so almost every node fits) so cloning a graph copies memory instead of making one heap
/// allocation per node. Order is unspecified and an index appears at most once.
#[derive(Debug, Clone, Default)]
enum Neighbours {
    #[default]
    Empty,
    Inline {
        len: u8,
        slots: [u32; INLINE_NEIGHBOURS],
    },
    Heap(Vec<u32>),
}

const INLINE_NEIGHBOURS: usize = 6;

impl Neighbours {
    fn as_slice(&self) -> &[u32] {
        match self {
            Self::Empty => &[],
            Self::Inline { len, slots } => &slots[..usize::from(*len)],
            Self::Heap(items) => items,
        }
    }

    fn push(&mut self, node: u32) {
        match self {
            Self::Empty => {
                let mut slots = [0; INLINE_NEIGHBOURS];
                slots[0] = node;
                *self = Self::Inline { len: 1, slots };
            }
            Self::Inline { len, slots } => {
                if usize::from(*len) < INLINE_NEIGHBOURS {
                    slots[usize::from(*len)] = node;
                    *len += 1;
                } else {
                    let mut items = slots.to_vec();
                    items.push(node);
                    *self = Self::Heap(items);
                }
            }
            Self::Heap(items) => items.push(node),
        }
    }

    /// Removes `node` if present (order of the rest is not preserved).
    fn remove(&mut self, node: u32) {
        match self {
            Self::Empty => {}
            Self::Inline { len, slots } => {
                let at = slots[..usize::from(*len)].iter().position(|&n| n == node);
                if let Some(at) = at {
                    *len -= 1;
                    slots[at] = slots[usize::from(*len)];
                    if *len == 0 {
                        *self = Self::Empty;
                    }
                }
            }
            Self::Heap(items) => {
                if let Some(at) = items.iter().position(|&n| n == node) {
                    items.swap_remove(at);
                }
            }
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
}

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
    /// Absent / failed neighbour bricks this node probed while it was built; they are
    /// counted into [`SupportGraph::absent_neighbours`] / `failed_neighbours` and
    /// uncounted when the node is dropped.
    absent: Vec<BrickKey>,
    failed: Vec<BrickKey>,
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
    labels: BTreeMap<(i64, i64, i64), Arc<BrickLabels>>,
    /// Revision each labelled brick was read at (feeds the job token). Keyed `(z, y, x)`, not
    /// `(x, y, z)` like the other maps, so iteration is already in canonical order and building the
    /// token needs no sort.
    brick_revision: BTreeMap<(i64, i64, i64), Revision>,
    /// Neighbour bricks probed during assembly that were absent. This includes
    /// all-resident empty-space assumptions so a later load invalidates a
    /// completed analysis.
    /// Each key maps to how many nodes probed it.
    absent_neighbours: BTreeMap<BrickKey, u32>,
    /// Neighbour bricks probed during assembly whose load had failed (counted likewise).
    failed_neighbours: BTreeMap<BrickKey, u32>,
    /// Node slab: an index is a node's identity for as long as it lives, so adding or
    /// dropping a node never renumbers another (`None` marks a free slot).
    nodes: Vec<Option<NodeData>>,
    free_nodes: Vec<u32>,
    node_of: BTreeMap<NodeOrder, u32>,
    /// Undirected edges as slab indices, in no particular order, never duplicated.
    adj: Vec<Neighbours>,
    components: Vec<GlobalComponent>,
    /// Component of each slab slot (`u32::MAX` for a free slot).
    node_component: Vec<u32>,
    /// Set while a budget-limited component scan is partway done.
    stashed: Option<ScanState>,
    /// Bricks relabelled whose nodes and edges have not been refreshed yet, so an
    /// interrupted update is finished by the next one rather than lost.
    pending_dirty: BTreeSet<BrickKey>,
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
        Self::build_with(volume, anchor, residency, cancel, None)
    }

    /// [`Self::build`], reusing (and filling) `cache` for every brick whose
    /// revision it already holds. Pass a cache only when `volume` is the live
    /// volume, never a dry-run copy (see [`LabelCache`]). A volume's cache is
    /// pruned to its resident bricks.
    pub fn build_cached(
        volume: &Volume,
        anchor: AnchorPlane,
        residency: ResidencyMode,
        cancel: &CancelToken,
        cache: &LabelCache,
    ) -> Result<Self, Interrupted> {
        Self::build_with(volume, anchor, residency, cancel, Some(cache))
    }

    fn build_with(
        volume: &Volume,
        anchor: AnchorPlane,
        residency: ResidencyMode,
        cancel: &CancelToken,
        cache: Option<&LabelCache>,
    ) -> Result<Self, Interrupted> {
        let mut graph = Self {
            volume: volume.id(),
            anchor,
            residency,
            labels: BTreeMap::new(),
            brick_revision: BTreeMap::new(),
            absent_neighbours: BTreeMap::new(),
            failed_neighbours: BTreeMap::new(),
            nodes: Vec::new(),
            free_nodes: Vec::new(),
            node_of: BTreeMap::new(),
            adj: Vec::new(),
            components: Vec::new(),
            node_component: Vec::new(),
            stashed: None,
            pending_dirty: BTreeSet::new(),
        };
        for coord in volume.resident_brick_coords() {
            if cancel.is_cancelled() {
                return Err(Interrupted::Cancelled);
            }
            graph.relabel_brick(volume, coord, cache);
        }
        if let Some(cache) = cache {
            cache.retain_volume(volume.id(), |key| {
                graph.brick_revision.contains_key(&(key.2, key.1, key.0))
            });
        }
        let all: Vec<BrickKey> = graph.labels.keys().copied().collect();
        graph.refresh_nodes(volume, &all, true);
        graph.run_scan(cancel, SearchBudget::UNLIMITED, None)?;
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
        let cid = *self.node_component.get(node as usize)? as usize;
        self.components.get(cid)
    }

    /// `(brick coord, revision)` for every labelled brick, in canonical
    /// `(z, y, x)` order.
    pub fn read_revisions(&self) -> impl Iterator<Item = (BrickCoord, Revision)> + '_ {
        // The map is keyed `(z, y, x)`, so its iteration order is already canonical.
        self.brick_revision
            .iter()
            .map(|(&(z, y, x), &rev)| (BrickCoord::new(x, y, z), rev))
    }

    /// Neighbour bricks that were absent during assembly, in canonical `(z, y,
    /// x)` order. In `AllResident` mode they were read as known empty space;
    /// in `Streamed` mode they are also unresolved support dependencies.
    pub fn absent_dependencies(&self) -> impl Iterator<Item = BrickCoord> + '_ {
        let mut out: Vec<BrickCoord> = self
            .absent_neighbours
            .keys()
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
            .keys()
            .map(|&(x, y, z)| BrickCoord::new(x, y, z))
            .collect();
        out.sort_by_key(|c| c.sort_key());
        out.into_iter()
    }

    /// The labelling of one brick, if it carries any solid cell.
    pub fn brick_labels(&self, coord: BrickCoord) -> Option<&BrickLabels> {
        self.labels.get(&(coord.x, coord.y, coord.z)).map(|v| &**v)
    }

    /// The global component a solid cell belongs to, if any. Requires the scan
    /// to be complete.
    pub fn classify_cell(&self, cell: GlobalCell) -> Option<GlobalComponentId> {
        let (brick, local_cell) = cell.split();
        let labels = self.labels.get(&(brick.x, brick.y, brick.z))?;
        let local = labels.component_at(local_cell)?;
        self.component_of(NodeKey { brick, local }).map(|c| c.id)
    }

    /// Relabels the given bricks from `volume` (their labels are invalidated), then patches
    /// the nodes and edges of those bricks and their face neighbours and recomputes the
    /// global components. This is the deletion / edit invalidation path: only `changed`
    /// bricks pay the flood-fill cost, and only they and their neighbours pay node and edge
    /// construction. The result is identical to a fresh [`Self::build`] of `volume`.
    pub fn apply_changes(
        &mut self,
        volume: &Volume,
        changed: &[BrickCoord],
        cancel: &CancelToken,
        budget: SearchBudget,
    ) -> Result<(), Interrupted> {
        self.stashed = None;
        self.pending_dirty
            .extend(changed.iter().map(|c| (c.x, c.y, c.z)));
        // Relabel everything still pending, not just `changed`: an earlier update that was
        // cancelled before it finished left its bricks here.
        let pending: Vec<BrickKey> = self.pending_dirty.iter().copied().collect();
        for (x, y, z) in pending {
            if cancel.is_cancelled() {
                return Err(Interrupted::Cancelled);
            }
            self.relabel_brick(volume, BrickCoord::new(x, y, z), None);
        }
        self.components.clear();
        self.node_component.clear();
        let mut dirty: BTreeSet<BrickKey> = BTreeSet::new();
        for (x, y, z) in std::mem::take(&mut self.pending_dirty) {
            dirty.insert((x, y, z));
            for axis in 0..3usize {
                for step in [-1, 1] {
                    let n = axis_step(BrickCoord::new(x, y, z), axis, step);
                    dirty.insert((n.x, n.y, n.z));
                }
            }
        }
        let dirty: Vec<BrickKey> = dirty.into_iter().collect();
        self.refresh_nodes(volume, &dirty, false);
        self.run_scan(cancel, budget, None)
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
    fn relabel_brick(&mut self, volume: &Volume, coord: BrickCoord, cache: Option<&LabelCache>) {
        let key = (coord.x, coord.y, coord.z);
        match volume.brick_state(coord) {
            Ok(BrickState::Resident { revision, .. }) => {
                let cached = cache.and_then(|c| c.get(volume.id(), key, revision));
                let labels = cached.unwrap_or_else(|| {
                    let snap = volume
                        .snapshot_brick(coord)
                        .expect("bounds checked by brick_state")
                        .expect("resident per brick_state");
                    let labels = Arc::new(label_brick(&snap));
                    if let Some(cache) = cache {
                        return cache.put(volume.id(), key, revision, labels);
                    }
                    labels
                });
                if labels.is_empty() {
                    self.labels.remove(&key);
                } else {
                    self.labels.insert(key, labels);
                }
                self.brick_revision
                    .insert((coord.z, coord.y, coord.x), revision);
            }
            _ => {
                self.labels.remove(&key);
                self.brick_revision.remove(&(coord.z, coord.y, coord.x));
            }
        }
    }

    /// Rebuilds the nodes and edges of the `dirty` bricks (sorted) from the current label
    /// map: every node of a dirty brick is dropped together with its edges, then recreated
    /// for the brick's current labelling, and every face shared between a dirty brick and a
    /// labelled neighbour is linked again. `all` says every labelled brick is dirty.
    ///
    /// A brick's nodes depend on its own labelling and on the state of its six neighbours,
    /// so the caller passes every brick whose own state or neighbour's state changed.
    fn refresh_nodes(&mut self, volume: &Volume, dirty: &[BrickKey], all: bool) {
        if !self.node_of.is_empty() {
            for &(x, y, z) in dirty {
                let doomed: Vec<(NodeOrder, u32)> = self
                    .node_of
                    .range((z, y, x, 0)..=(z, y, x, u16::MAX))
                    .map(|(&order, &node)| (order, node))
                    .collect();
                for (order, node) in doomed {
                    self.remove_node(order, node);
                }
            }
        }

        for &key in dirty {
            let Some(labels) = self.labels.get(&key).cloned() else {
                continue;
            };
            let coord = BrickCoord::new(key.0, key.1, key.2);
            for local in labels.components() {
                let (anchored, unresolved, absent, failed) =
                    self.node_attributes(volume, coord, &labels, local);
                for &probe in &absent {
                    *self.absent_neighbours.entry(probe).or_insert(0) += 1;
                }
                for &probe in &failed {
                    *self.failed_neighbours.entry(probe).or_insert(0) += 1;
                }
                let node = match self.free_nodes.pop() {
                    Some(node) => node,
                    None => {
                        self.nodes.push(None);
                        self.adj.push(Neighbours::default());
                        (self.nodes.len() - 1) as u32
                    }
                };
                let node_key = NodeKey {
                    brick: coord,
                    local,
                };
                self.nodes[node as usize] = Some(NodeData {
                    key: node_key,
                    cell_count: labels.cell_count(local),
                    anchored,
                    unresolved,
                    absent,
                    failed,
                });
                self.node_of.insert(node_key.order(), node);
            }
        }

        // Each shared face is linked exactly once: from the lower brick when it is dirty,
        // otherwise from the dirty upper brick.
        for &key in dirty {
            if !self.labels.contains_key(&key) {
                continue;
            }
            let a = BrickCoord::new(key.0, key.1, key.2);
            for axis in 0..3usize {
                let upper = axis_step(a, axis, 1);
                if self.labels.contains_key(&(upper.x, upper.y, upper.z)) {
                    self.link_face(a, upper, axis);
                }
                let lower = axis_step(a, axis, -1);
                let lower_key = (lower.x, lower.y, lower.z);
                if self.labels.contains_key(&lower_key)
                    && !(all || dirty.binary_search(&lower_key).is_ok())
                {
                    self.link_face(lower, a, axis);
                }
            }
        }
    }

    /// Drops one node, its edges, and the neighbour probes it contributed.
    fn remove_node(&mut self, order: NodeOrder, node: u32) {
        let neighbours = std::mem::take(&mut self.adj[node as usize]);
        for &neighbour in neighbours.as_slice() {
            self.adj[neighbour as usize].remove(node);
        }
        let data = self.nodes[node as usize]
            .take()
            .expect("an indexed node is live");
        for probe in data.absent {
            uncount(&mut self.absent_neighbours, probe);
        }
        for probe in data.failed {
            uncount(&mut self.failed_neighbours, probe);
        }
        self.node_of.remove(&order);
        self.free_nodes.push(node);
    }

    /// Anchored flag and unresolved-neighbour list for one node.
    #[allow(clippy::type_complexity)]
    fn node_attributes(
        &self,
        volume: &Volume,
        coord: BrickCoord,
        labels: &BrickLabels,
        local: LocalComponent,
    ) -> (bool, Vec<BrickCoord>, Vec<BrickKey>, Vec<BrickKey>) {
        // Per-component facts were computed once when the brick was labelled: a
        // cell on the support plane is a cell at that height in this brick, and
        // a face is present when any cell touches it.
        let faces = labels.faces(local);
        let anchored = coord
            .y
            .checked_mul(EDGE)
            .and_then(|base| self.anchor.y.checked_sub(base))
            .filter(|local_y| (0..EDGE).contains(local_y))
            .is_some_and(|local_y| labels.has_layer(local, local_y as u8));

        let mut unresolved = Vec::new();
        let mut absent = Vec::new();
        let mut failed = Vec::new();
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
                    failed.push((neighbour.x, neighbour.y, neighbour.z));
                    unresolved.push(neighbour);
                }
                // Absent: unknown only when the world is streamed; under
                // `AllResident` the caller guarantees this is empty space.
                Ok(BrickState::Absent) => {
                    absent.push((neighbour.x, neighbour.y, neighbour.z));
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
        (anchored, unresolved, absent, failed)
    }

    /// Adds edges between components of `a` and `b` (`b = a` stepped `+1` on
    /// `axis`) wherever their boundary cells are both solid at matching
    /// cross-face coordinates.
    fn link_face(&mut self, a: BrickCoord, b: BrickCoord, axis: usize) {
        // `a`'s `+axis` face against `b`'s `-axis` face; the face masks share
        // `(u, v)` coordinates across the pair (see `BrickLabels::face_mask`).
        let (face_a, face_b) = (2 * axis + 1, 2 * axis);
        let mut pairs: BTreeSet<(LocalComponent, LocalComponent)> = BTreeSet::new();
        {
            let la = &self.labels[&(a.x, a.y, a.z)];
            let lb = &self.labels[&(b.x, b.y, b.z)];
            let (ma, mb) = (la.face_mask(face_a), lb.face_mask(face_b));
            if la.count() == 1 && lb.count() == 1 {
                // One component each: they bond iff any boundary cell pair is
                // solid on both sides.
                if ma.iter().zip(mb).any(|(x, y)| x & y != 0) {
                    pairs.insert((LocalComponent(1), LocalComponent(1)));
                }
            } else {
                // At least one brick has several components: compare the
                // labels of the cell pairs that are solid on both sides.
                for v in 0..BRICK_EDGE as usize {
                    let mut both = ma[v] & mb[v];
                    while both != 0 {
                        let u = both.trailing_zeros() as usize;
                        both &= both - 1;
                        let (ia, ib) = match axis {
                            0 => (idx_of(EDGE_US - 1, u, v), idx_of(0, u, v)),
                            1 => (idx_of(u, EDGE_US - 1, v), idx_of(u, 0, v)),
                            _ => (idx_of(u, v, EDGE_US - 1), idx_of(u, v, 0)),
                        };
                        pairs.insert((
                            LocalComponent(la.label_at_index(ia)),
                            LocalComponent(lb.label_at_index(ib)),
                        ));
                    }
                }
            }
        }
        for (comp_a, comp_b) in pairs {
            debug_assert!(
                comp_a.get() != 0 && comp_b.get() != 0,
                "masks are solid cells"
            );
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
            self.adj[na as usize].push(nb);
            self.adj[nb as usize].push(na);
        }
    }

    fn live(&self, node: usize) -> &NodeData {
        self.nodes[node].as_ref().expect("an indexed node is live")
    }

    /// Runs (or resumes) the connected-component pass. On success writes
    /// `components` / `node_component`. On [`Interrupted::Budget`] or
    /// [`Interrupted::Cancelled`] stashes progress for [`resume`](Self::resume).
    ///
    /// Components are discovered by breadth-first search starting from the first unassigned node
    /// in canonical order, so ids follow the smallest node of each component. Membership is
    /// assembled once the search has covered every node, by a single pass over the canonical node
    /// order, which yields each component's nodes already sorted.
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
                order: self.node_of.values().copied().collect(),
                next_id: 0,
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
                    while state.next_start < state.order.len()
                        && state.node_component[state.order[state.next_start] as usize] != u32::MAX
                    {
                        state.next_start += 1;
                    }
                    if state.next_start >= state.order.len() {
                        break;
                    }
                    let start = state.order[state.next_start] as usize;
                    let id = state.next_id;
                    state.next_id += 1;
                    state.node_component[start] = id;
                    PartialComponent {
                        id,
                        frontier: VecDeque::from([start]),
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
                for &next in self.adj[here].as_slice() {
                    let next = next as usize;
                    if state.node_component[next] == u32::MAX {
                        state.node_component[next] = part.id;
                        part.frontier.push_back(next);
                    }
                }
            }
        }

        // Every node belongs to a component: gather each component's nodes and totals in one
        // pass over the canonical order.
        let count = state.next_id as usize;
        let mut sizes = vec![0usize; count];
        for &node in &state.order {
            sizes[state.node_component[node as usize] as usize] += 1;
        }
        let mut components: Vec<GlobalComponent> = sizes
            .iter()
            .enumerate()
            .map(|(id, &size)| GlobalComponent {
                id: GlobalComponentId(id as u32),
                nodes: Vec::with_capacity(size),
                cell_count: 0,
                anchored: false,
                unresolved: Vec::new(),
            })
            .collect();
        for &node_index in &state.order {
            let node = self.live(node_index as usize);
            let component = &mut components[state.node_component[node_index as usize] as usize];
            component.cell_count += u64::from(node.cell_count);
            component.anchored |= node.anchored;
            component.unresolved.extend(node.unresolved.iter().copied());
            component.nodes.push(node.key);
        }
        for component in &mut components {
            component.unresolved.sort_by_key(|c| c.sort_key());
            component.unresolved.dedup();
        }

        self.components = components;
        self.node_component = state.node_component;
        self.stashed = None;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ScanState {
    node_component: Vec<u32>,
    /// Live node slots in canonical order: the order components are discovered in.
    order: Vec<u32>,
    /// Components started so far.
    next_id: u32,
    next_start: usize,
    current: Option<PartialComponent>,
}

#[derive(Debug, Clone)]
struct PartialComponent {
    id: u32,
    frontier: VecDeque<usize>,
}

/// Drops one reference to `probe`, forgetting it at zero.
fn uncount(counts: &mut BTreeMap<BrickKey, u32>, probe: BrickKey) {
    if let Some(count) = counts.get_mut(&probe) {
        *count -= 1;
        if *count == 0 {
            counts.remove(&probe);
        }
    }
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

    #[test]
    fn many_face_components_disconnect_and_resume_in_canonical_order() {
        let mut volume = vol();
        fill(
            &mut volume,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(31, 31, 31),
        );
        let mut buds = EditPlan::new(volume.id());
        for z in 0..32 {
            for y in 0..32 {
                if (y + z) % 2 == 0 {
                    buds.set(GlobalCell::new(32, y, z), MaterialId(1));
                }
            }
        }
        volume.apply_edit(&buds).unwrap();
        let cancel = CancelToken::new();
        let mut graph = SupportGraph::build(
            &volume,
            AnchorPlane::at(0),
            ResidencyMode::AllResident,
            &cancel,
        )
        .unwrap();
        assert_eq!(graph.components().len(), 1);
        assert_eq!(graph.components()[0].nodes.len(), 513);
        assert_eq!(graph.components()[0].cell_count, 32768 + 512);
        assert!(graph.components()[0].anchored);
        let cut = volume
            .apply_edit(&EditPlan::filled_box(
                volume.id(),
                GlobalCell::new(31, 0, 0),
                GlobalCell::new(31, 31, 31),
                MaterialId::AIR,
            ))
            .unwrap();
        graph
            .apply_changes(
                &volume,
                &cut.bricks.iter().map(|b| b.coord).collect::<Vec<_>>(),
                &cancel,
                SearchBudget::UNLIMITED,
            )
            .unwrap();
        let expected = graph.components().to_vec();
        assert_eq!(expected.len(), 513);
        assert_eq!(expected.iter().filter(|c| c.anchored).count(), 17);
        assert_eq!(
            graph.apply_changes(&volume, &[], &cancel, SearchBudget { max_nodes: 1 }),
            Err(Interrupted::Budget)
        );
        while graph.is_scan_pending() {
            match graph.resume(&cancel, SearchBudget { max_nodes: 1 }) {
                Ok(()) | Err(Interrupted::Budget) => {}
                other => panic!("unexpected resume: {other:?}"),
            }
        }
        assert_eq!(graph.components(), expected);
    }

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

    /// Everything observable about a graph, canonicalised so two graphs that describe the
    /// same volume compare equal regardless of how they were built.
    #[derive(Debug, PartialEq)]
    struct View {
        components: Vec<GlobalComponent>,
        nodes: Vec<(NodeOrder, u32, bool, Vec<BrickCoord>)>,
        edges: BTreeSet<(NodeOrder, NodeOrder)>,
        node_components: Vec<(NodeOrder, u32)>,
        absent: Vec<BrickCoord>,
        failed: Vec<BrickCoord>,
        revisions: Vec<(BrickCoord, Revision)>,
        labels: Vec<(BrickCoord, u16)>,
    }

    fn view(g: &SupportGraph) -> View {
        let order_of = |node: u32| g.nodes[node as usize].as_ref().unwrap().key.order();
        let mut edges = BTreeSet::new();
        for (node, neighbours) in g.adj.iter().enumerate() {
            if g.nodes[node].is_none() {
                assert!(neighbours.is_empty(), "a free slot keeps no edges");
                continue;
            }
            let mut seen = BTreeSet::new();
            for &other in neighbours.as_slice() {
                assert!(seen.insert(other), "an edge is never duplicated");
                edges.insert((order_of(node as u32), order_of(other)));
            }
        }
        let mut nodes: Vec<_> = g
            .node_of
            .iter()
            .map(|(&order, &node)| {
                let data = g.nodes[node as usize].as_ref().unwrap();
                assert_eq!(data.key.order(), order);
                (
                    order,
                    data.cell_count,
                    data.anchored,
                    data.unresolved.clone(),
                )
            })
            .collect();
        nodes.sort();
        let mut node_components: Vec<_> = g
            .node_of
            .iter()
            .map(|(&order, &node)| (order, g.node_component[node as usize]))
            .collect();
        node_components.sort();
        View {
            components: g.components().to_vec(),
            nodes,
            edges,
            node_components,
            absent: g.absent_dependencies().collect(),
            failed: g.failed_dependencies().collect(),
            revisions: g.read_revisions().collect(),
            labels: g
                .labels
                .iter()
                .map(|(&(x, y, z), l)| (BrickCoord::new(x, y, z), l.count()))
                .collect(),
        }
    }

    #[test]
    fn neighbour_lists_behave_as_sets_across_the_inline_and_heap_representations() {
        let mut list = Neighbours::default();
        assert!(list.is_empty());
        let mut model: BTreeSet<u32> = BTreeSet::new();
        // Grow past the inline capacity, then shrink back below it, checking against a set at
        // every step; remove absent entries and the last entry too.
        for step in 0..40u32 {
            let value = (step * 7) % 23;
            if model.insert(value) {
                list.push(value);
            }
            if step % 3 == 2 {
                let victim = (step * 5) % 23;
                model.remove(&victim);
                list.remove(victim);
            }
            let got: BTreeSet<u32> = list.as_slice().iter().copied().collect();
            assert_eq!(got, model, "step {step}");
            assert_eq!(
                list.as_slice().len(),
                model.len(),
                "no duplicates at step {step}"
            );
        }
        for value in model.clone() {
            list.remove(value);
        }
        assert!(list.is_empty());
        list.remove(99);
        assert!(list.is_empty());
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> i64 {
            (self.next() % n) as i64
        }
    }

    fn assert_matches_fresh(
        g: &SupportGraph,
        volume: &Volume,
        anchor: AnchorPlane,
        mode: ResidencyMode,
        what: &str,
    ) {
        let fresh = SupportGraph::build(volume, anchor, mode, &CancelToken::new()).unwrap();
        assert_eq!(view(g), view(&fresh), "incremental != fresh after {what}");
    }

    #[test]
    fn an_incrementally_updated_graph_equals_a_fresh_build_after_every_change() {
        for (seed, anchor_y, mode) in [
            (0x9E37_79B9_7F4A_7C15_u64, 0, ResidencyMode::AllResident),
            (0xD1B5_4A32_D192_ED03, 33, ResidencyMode::Streamed),
            (0x2545_F491_4F6C_DD1D, 5, ResidencyMode::AllResident),
            (0x8CB9_2BA7_2F3D_8DD7, 64, ResidencyMode::Streamed),
        ] {
            let mut rng = Rng(seed);
            let mut volume = vol();
            let anchor = AnchorPlane::at(anchor_y);
            let cancel = CancelToken::new();
            // Start from some terrain so edits have something to cut.
            for _ in 0..6 {
                let (x, y, z) = (rng.below(90), rng.below(90), rng.below(90));
                fill(
                    &mut volume,
                    GlobalCell::new(x, y, z),
                    GlobalCell::new(x + rng.below(40), y + rng.below(40), z + rng.below(40)),
                );
            }
            let mut graph = SupportGraph::build(&volume, anchor, mode, &cancel).unwrap();
            assert_matches_fresh(&graph, &volume, anchor, mode, "the initial build");

            for step in 0..60 {
                let mut changed: Vec<BrickCoord> = Vec::new();
                let what = match rng.below(10) {
                    0 => {
                        let coord = BrickCoord::new(rng.below(4), rng.below(4), rng.below(4));
                        volume.evict_brick(coord);
                        changed.push(coord);
                        "an eviction"
                    }
                    1 => {
                        let coord = BrickCoord::new(rng.below(5) - 1, rng.below(4), rng.below(4));
                        if volume.mark_failed(coord).is_ok() {
                            changed.push(coord);
                        }
                        "a load failure"
                    }
                    n => {
                        let (x, y, z) =
                            (rng.below(110) - 5, rng.below(110) - 5, rng.below(110) - 5);
                        let (b0, b1) = (
                            GlobalCell::new(x, y, z),
                            GlobalCell::new(
                                x + rng.below(30),
                                y + rng.below(30),
                                z + rng.below(30),
                            ),
                        );
                        let material = if n < 6 {
                            MaterialId::AIR
                        } else {
                            MaterialId(1)
                        };
                        if let Ok(outcome) =
                            volume.apply_edit(&EditPlan::filled_box(volume.id(), b0, b1, material))
                        {
                            changed.extend(outcome.bricks.iter().map(|b| b.coord));
                        }
                        "a box edit"
                    }
                };
                graph
                    .apply_changes(&volume, &changed, &cancel, SearchBudget::UNLIMITED)
                    .unwrap();
                assert_matches_fresh(
                    &graph,
                    &volume,
                    anchor,
                    mode,
                    &format!("{what} (step {step})"),
                );
            }
        }
    }

    #[test]
    fn a_cancelled_update_is_finished_by_the_next_one() {
        let mut volume = vol();
        let anchor = AnchorPlane::at(0);
        let mode = ResidencyMode::AllResident;
        fill(
            &mut volume,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(70, 5, 70),
        );
        let mut graph = SupportGraph::build(&volume, anchor, mode, &CancelToken::new()).unwrap();
        let cut = volume
            .apply_edit(&EditPlan::filled_box(
                volume.id(),
                GlobalCell::new(30, 0, 0),
                GlobalCell::new(33, 5, 70),
                MaterialId::AIR,
            ))
            .unwrap();
        let changed: Vec<BrickCoord> = cut.bricks.iter().map(|b| b.coord).collect();
        let cancelled = CancelToken::new();
        cancelled.cancel();
        assert_eq!(
            graph.apply_changes(&volume, &changed, &cancelled, SearchBudget::UNLIMITED),
            Err(Interrupted::Cancelled)
        );
        // The interrupted bricks are remembered: a later update that names nothing new
        // still produces the exact graph.
        graph
            .apply_changes(&volume, &[], &CancelToken::new(), SearchBudget::UNLIMITED)
            .unwrap();
        assert_matches_fresh(&graph, &volume, anchor, mode, "a cancelled update");
    }
}

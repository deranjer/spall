//! Bounded source-cell connectivity planning for frozen corpse aggregates.
//!
//! The planner keeps authored source coordinates and identities. It creates no
//! Rapier objects and mutates no owner; callers must revalidate the captured
//! tokens before applying the immutable partition at a tick boundary.

use std::collections::{BTreeMap, BTreeSet};

use spall_core::{MaterialId, VolumeId};
use thiserror::Error;

use crate::articulated::{PartTransform, SourcePart};

pub const FRACTURE_PLAN_VERSION: u16 = 1;
pub const MAX_FRACTURE_SOURCE_CELLS: usize = 1_000_000;
pub const MAX_FRACTURE_PARTS: usize = 256;
pub const MAX_FRACTURE_DENSE_CELLS: usize = MAX_FRACTURE_SOURCE_CELLS;
pub const MAX_FRACTURE_COMPONENTS: usize = 256;
/// All cross-volume cell pairs examined, including broadphase rejects.
pub const MAX_FRACTURE_CELL_PAIR_TESTS: usize = 1_000_000;
pub const MAX_FRACTURE_SAT_AXIS_TESTS: usize = MAX_FRACTURE_CELL_PAIR_TESTS * 15;

/// Stable identity of one authoritatively owned source cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceCellRef {
    pub volume_id: VolumeId,
    pub part_id: u32,
    pub cell: [i32; 3],
    pub material: MaterialId,
    pub source_revision: u64,
}

/// Immutable source geometry and its frozen pose in the corpse root frame.
#[derive(Clone)]
pub struct FracturePartSnapshot {
    pub source: SourcePart,
    pub root_local_pose: PartTransform,
}

/// Version tokens which must still match when a prepared plan is committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FractureSnapshotToken {
    pub corpse_key: u128,
    pub topology_revision: u64,
    pub pose_epoch: u64,
    pub source_generation: u64,
    /// `None` selects the initial unsplit logical owner. Once fractured, a
    /// snapshot names exactly one independently moving physical component.
    pub component_key: Option<DerivedFractureComponentId>,
}

/// Complete read-only snapshot consumed by [`prepare_fracture`].
pub struct FractureSnapshot {
    pub token: FractureSnapshotToken,
    pub parts: Vec<FracturePartSnapshot>,
}

/// One physically connected set of source cells. Cells are canonical sorted
/// provenance; source coordinates are never rebased or regenerated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FractureComponent {
    pub cells: Vec<SourceCellRef>,
}

/// Stable transaction-owned identity for one derived physical component.
/// `serial` is allocated monotonically by the logical corpse owner and is
/// never a Rapier handle or source VolumeId.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DerivedFractureComponentId {
    pub owner_key: u128,
    pub serial: u64,
}

/// Versioned immutable one-to-many ownership proposal. Component keys are
/// deliberately allocated by the owning simulation transaction at commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFracturePlan {
    pub version: u16,
    pub token: FractureSnapshotToken,
    pub source_cells: Vec<SourceCellRef>,
    pub components: Vec<FractureComponent>,
    pub candidate_pairs_tested: usize,
    /// Number of certified interval SAT axes evaluated, bounded by 15 per
    /// non-lattice candidate pair.
    pub sat_axes_tested: usize,
    pub certified_cross_volume_edges: usize,
    pub redundant_ambiguous_pairs_ignored: usize,
    pub elapsed: std::time::Duration,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FractureError {
    #[error("fracture snapshot is empty, malformed, or has duplicate source identity")]
    InvalidSnapshot,
    #[error("fracture source cell or cross-volume pair budget exceeded")]
    WorkBudget,
    #[error("prepared fracture plan is stale, forged, or does not cover the selected owner cells")]
    InvalidPlan,
    #[error("fracture owner topology, pose, or source generation changed before commit")]
    StaleOwner,
    #[error("fracture component mass properties are invalid")]
    InvalidMass,
    #[error(
        "cross-volume source-cell geometry is too close to classify safely: {left:?} / {right:?}"
    )]
    AmbiguousContact {
        left: SourceCellRef,
        right: SourceCellRef,
        /// Absolute pose-origin separation per axis as f64 bit patterns.
        axis_gap_m_bits: [u64; 3],
        /// Conservative sphere-radius sum as f64 bits.
        required_gap_m_bits: u64,
    },
}

#[derive(Clone)]
struct CellGeometry {
    source: SourceCellRef,
    source_cell: [i32; 3],
    pose_translation: [f32; 3],
    rotation: [[i32; 3]; 3],
    rotation_xyzw: [f32; 4],
    cell_m: f32,
    bound_radius_m: f64,
    cached_box: Option<CachedBox>,
}

/// Prepare deterministic connected components from an immutable corpse pose.
/// Same-volume connectivity uses exact integer six-face adjacency. Cross-volume
/// edges use exact shared-lattice proofs, exact coplanar positive-area contact,
/// or interval-certified positive-volume
/// overlap of the canonical affine cell boxes. The latter describes the stored
/// normalized-f32 quaternion coefficients interpreted as exact affine inputs;
/// it does not claim bit-for-bit equivalence with Rapier's contact arithmetic.
/// Uncertain cases fail closed.
pub fn prepare_fracture(
    snapshot: &FractureSnapshot,
) -> Result<PreparedFracturePlan, FractureError> {
    let start = std::time::Instant::now();
    if snapshot.parts.is_empty()
        || snapshot.parts.len() > MAX_FRACTURE_PARTS
        || snapshot.token.corpse_key == 0
    {
        return Err(FractureError::InvalidSnapshot);
    }

    // Admit source and dense geometry budgets before allocating per-cell lists.
    let mut solid_count = 0u64;
    let mut dense_count = 0usize;
    for part in &snapshot.parts {
        let [x, y, z] = part.source.grid.dims();
        let dense = usize::try_from(x)
            .ok()
            .and_then(|x| usize::try_from(y).ok().and_then(|y| x.checked_mul(y)))
            .and_then(|xy| usize::try_from(z).ok().and_then(|z| xy.checked_mul(z)))
            .ok_or(FractureError::WorkBudget)?;
        if dense > MAX_FRACTURE_SOURCE_CELLS {
            return Err(FractureError::WorkBudget);
        }
        dense_count = dense_count
            .checked_add(dense)
            .ok_or(FractureError::WorkBudget)?;
        if dense_count > MAX_FRACTURE_DENSE_CELLS {
            return Err(FractureError::WorkBudget);
        }
        solid_count = solid_count
            .checked_add(part.source.grid.solid_count())
            .ok_or(FractureError::WorkBudget)?;
        if solid_count > MAX_FRACTURE_SOURCE_CELLS as u64 {
            return Err(FractureError::WorkBudget);
        }
    }

    let mut cells = Vec::new();
    let mut geometry = Vec::new();
    let mut part_ids = BTreeSet::new();
    let mut volume_ids = BTreeSet::new();
    for part in &snapshot.parts {
        if part.source.part_id == 0
            || !part_ids.insert(part.source.part_id)
            || !volume_ids.insert(part.source.volume_id)
            || part.source.source_revision == 0
            || !part.source.cell_m.is_finite()
            || part.source.cell_m <= 0.0
            || !part.root_local_pose.is_valid()
        {
            return Err(FractureError::InvalidSnapshot);
        }
        let pose = part
            .root_local_pose
            .normalized()
            .ok_or(FractureError::InvalidSnapshot)?;
        let origin = part.source.grid.origin();
        let axes = [
            rotate64(pose.rotation_xyzw, [1.0, 0.0, 0.0]),
            rotate64(pose.rotation_xyzw, [0.0, 1.0, 0.0]),
            rotate64(pose.rotation_xyzw, [0.0, 0.0, 1.0]),
        ];
        for [x, y, z] in part.source.grid.solid_indices() {
            let source_cell = [
                i32::try_from(
                    origin
                        .x
                        .checked_add(i64::from(x))
                        .ok_or(FractureError::InvalidSnapshot)?,
                )
                .map_err(|_| FractureError::InvalidSnapshot)?,
                i32::try_from(
                    origin
                        .y
                        .checked_add(i64::from(y))
                        .ok_or(FractureError::InvalidSnapshot)?,
                )
                .map_err(|_| FractureError::InvalidSnapshot)?,
                i32::try_from(
                    origin
                        .z
                        .checked_add(i64::from(z))
                        .ok_or(FractureError::InvalidSnapshot)?,
                )
                .map_err(|_| FractureError::InvalidSnapshot)?,
            ];
            let material = part
                .source
                .grid
                .material(x as u32, y as u32, z as u32)
                .ok_or(FractureError::InvalidSnapshot)?;
            let reference = SourceCellRef {
                volume_id: part.source.volume_id,
                part_id: part.source.part_id,
                cell: source_cell,
                material,
                source_revision: part.source.source_revision,
            };
            cells.push(reference);
            let mut cell_geometry = CellGeometry {
                source: reference,
                source_cell,
                pose_translation: pose.translation_m,
                rotation: signed_permutation(axes).unwrap_or([[0; 3]; 3]),
                rotation_xyzw: pose.rotation_xyzw,
                cell_m: part.source.cell_m,
                // Loose sphere around the part origin enclosing every rotated
                // cell. This does not use rounded quaternion axes.
                bound_radius_m: (2.0
                    * f64::from(part.source.cell_m)
                    * (f64::from(source_cell[0]).abs()
                        + f64::from(source_cell[1]).abs()
                        + f64::from(source_cell[2]).abs()
                        + 2.0))
                    .next_up(),
                cached_box: None,
            };
            cell_geometry.cached_box = cache_cell_box(&cell_geometry);
            geometry.push(cell_geometry);
        }
    }
    if cells.is_empty() {
        return Err(FractureError::InvalidSnapshot);
    }
    cells.sort_unstable();
    if cells.windows(2).any(|pair| {
        pair[0] == pair[1]
            || (pair[0].volume_id, pair[0].part_id, pair[0].cell)
                == (pair[1].volume_id, pair[1].part_id, pair[1].cell)
    }) {
        return Err(FractureError::InvalidSnapshot);
    }
    // The geometry array follows input part/cell order, while the union-find is
    // indexed by canonical source identity.
    geometry.sort_unstable_by_key(|cell| cell.source);
    let indices = cells
        .iter()
        .enumerate()
        .map(|(index, cell)| (*cell, index))
        .collect::<BTreeMap<_, _>>();
    let geometric_indices = cells
        .iter()
        .enumerate()
        .map(|(index, cell)| ((cell.volume_id, cell.part_id, cell.cell), index))
        .collect::<BTreeMap<_, _>>();
    let mut union = UnionFind::new(cells.len());

    for (index, cell) in cells.iter().enumerate() {
        for delta in [[1, 0, 0], [0, 1, 0], [0, 0, 1]] {
            let Some(neighbor) = cell.cell[0]
                .checked_add(delta[0])
                .zip(cell.cell[1].checked_add(delta[1]))
                .zip(cell.cell[2].checked_add(delta[2]))
                .map(|((x, y), z)| [x, y, z])
            else {
                continue;
            };
            let key = (cell.volume_id, cell.part_id, neighbor);
            if let Some(other) = geometric_indices.get(&key) {
                union.join(index, *other);
            }
        }
    }

    let mut candidate_pairs_tested = 0usize;
    let mut sat_axes_tested = 0usize;
    let mut certified_cross_volume_edges = 0usize;
    let mut ambiguous_pairs = Vec::new();
    let mut by_volume = BTreeMap::<VolumeId, Vec<usize>>::new();
    for (index, cell) in geometry.iter().enumerate() {
        by_volume
            .entry(cell.source.volume_id)
            .or_default()
            .push(index);
    }
    let volume_groups = by_volume.into_values().collect::<Vec<_>>();
    // Bound all cross-volume work. Same-volume adjacency is handled above by
    // one geometric lookup per cell and is not part of this pair budget.
    for left_group in 0..volume_groups.len() {
        for right_group in left_group + 1..volume_groups.len() {
            for &left in &volume_groups[left_group] {
                let a = &geometry[left];
                for &right in &volume_groups[right_group] {
                    let b = &geometry[right];
                    candidate_pairs_tested = candidate_pairs_tested
                        .checked_add(1)
                        .ok_or(FractureError::WorkBudget)?;
                    if candidate_pairs_tested > MAX_FRACTURE_CELL_PAIR_TESTS {
                        return Err(FractureError::WorkBudget);
                    }
                    // Prefer the exact lattice certificate. A loose origin sphere
                    // cheaply rejects distant pairs; unresolved pairs use bounded
                    // interval SAT and certified positive-area coplanar contacts.
                    let classification = if let Some(lattice) = classify_lattice_pair(a, b) {
                        lattice
                    } else {
                        let coarse = classify_conservative_separation(a, b);
                        if coarse != ContactClass::Ambiguous {
                            coarse
                        } else {
                            let (sat, axes_tested) = classify_parallelepiped_pair(a, b);
                            sat_axes_tested = sat_axes_tested
                                .checked_add(axes_tested)
                                .filter(|count| *count <= MAX_FRACTURE_SAT_AXIS_TESTS)
                                .ok_or(FractureError::WorkBudget)?;
                            sat
                        }
                    };
                    match classification {
                        ContactClass::Separated => {}
                        ContactClass::PositiveContact => {
                            let left_index = indices[&a.source];
                            let right_index = indices[&b.source];
                            union.join(left_index, right_index);
                            certified_cross_volume_edges += 1;
                        }
                        ContactClass::Ambiguous => ambiguous_pairs.push((
                            indices[&a.source],
                            indices[&b.source],
                            a.source,
                            b.source,
                            std::array::from_fn(|axis| {
                                (f64::from(a.pose_translation[axis])
                                    - f64::from(b.pose_translation[axis]))
                                .abs()
                                .to_bits()
                            }),
                            (a.bound_radius_m + b.bound_radius_m).next_up().to_bits(),
                        )),
                    }
                }
            }
        }
    }

    // An uncertain edge cannot alter the partition if its endpoints already
    // belong to one independently certified component. Defer ambiguity until
    // the complete positive-proof graph exists so redundant joint surfaces do
    // not reject a result unnecessarily.
    let mut redundant_ambiguous_pairs_ignored = 0;
    for (left, right, left_ref, right_ref, axis_gap_m_bits, required_gap_m_bits) in ambiguous_pairs
    {
        if union.root(left) != union.root(right) {
            return Err(FractureError::AmbiguousContact {
                left: left_ref,
                right: right_ref,
                axis_gap_m_bits,
                required_gap_m_bits,
            });
        }
        redundant_ambiguous_pairs_ignored += 1;
    }

    let mut groups = BTreeMap::<usize, Vec<SourceCellRef>>::new();
    for (index, cell) in cells.iter().copied().enumerate() {
        groups.entry(union.root(index)).or_default().push(cell);
    }
    if groups.len() > MAX_FRACTURE_COMPONENTS {
        return Err(FractureError::WorkBudget);
    }
    let mut components = groups
        .into_values()
        .map(|mut cells| {
            cells.sort_unstable();
            FractureComponent { cells }
        })
        .collect::<Vec<_>>();
    components.sort_by_key(|component| component.cells[0]);
    Ok(PreparedFracturePlan {
        version: FRACTURE_PLAN_VERSION,
        token: snapshot.token,
        source_cells: cells,
        components,
        candidate_pairs_tested,
        sat_axes_tested,
        certified_cross_volume_edges,
        redundant_ambiguous_pairs_ignored,
        elapsed: start.elapsed(),
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContactClass {
    Separated,
    PositiveContact,
    Ambiguous,
}

fn classify_conservative_separation(a: &CellGeometry, b: &CellGeometry) -> ContactClass {
    if let (Some(a_box), Some(b_box)) = (a.cached_box, b.cached_box)
        && (0..3).any(|axis| {
            a_box.aabb[axis].hi < b_box.aabb[axis].lo || b_box.aabb[axis].hi < a_box.aabb[axis].lo
        })
    {
        return ContactClass::Separated;
    }
    let radius_sum = (a.bound_radius_m + b.bound_radius_m).next_up();
    for axis in 0..3 {
        let delta = f64::from(a.pose_translation[axis]) - f64::from(b.pose_translation[axis]);
        let lower_gap = delta.abs().next_down();
        if lower_gap > radius_sum {
            return ContactClass::Separated;
        }
    }
    ContactClass::Ambiguous
}

#[derive(Clone, Copy, Debug)]
struct Interval {
    lo: f64,
    hi: f64,
}

impl Interval {
    const ZERO: Self = Self { lo: 0.0, hi: 0.0 };

    fn point(value: f64) -> Self {
        Self {
            lo: value,
            hi: value,
        }
    }

    fn finite(self) -> bool {
        self.lo.is_finite() && self.hi.is_finite() && self.lo <= self.hi
    }

    fn add(self, rhs: Self) -> Self {
        let (lo, lo_err) = two_sum(self.lo, rhs.lo);
        let (hi, hi_err) = two_sum(self.hi, rhs.hi);
        Self {
            lo: if lo_err < 0.0 { lo.next_down() } else { lo },
            hi: if hi_err > 0.0 { hi.next_up() } else { hi },
        }
    }

    fn neg(self) -> Self {
        Self {
            lo: -self.hi,
            hi: -self.lo,
        }
    }

    fn sub(self, rhs: Self) -> Self {
        self.add(rhs.neg())
    }

    fn mul(self, rhs: Self) -> Self {
        let pairs = [
            (self.lo, rhs.lo),
            (self.lo, rhs.hi),
            (self.hi, rhs.lo),
            (self.hi, rhs.hi),
        ];
        let mut lower = f64::INFINITY;
        let mut upper = f64::NEG_INFINITY;
        for (a, b) in pairs {
            let product = a * b;
            if !product.is_finite() {
                return Self {
                    lo: f64::NEG_INFINITY,
                    hi: f64::INFINITY,
                };
            }
            if a == 0.0 || b == 0.0 {
                lower = lower.min(product);
                upper = upper.max(product);
                continue;
            }
            let error = a.mul_add(b, -product);
            if !error.is_finite() {
                return Self {
                    lo: f64::NEG_INFINITY,
                    hi: f64::INFINITY,
                };
            }
            // The exact FMA residual can round to zero when it is below the
            // binary64 subnormal range, even for normal operands. Below 2^-969
            // the residual's worst-case exponent may be unrepresentable; widen
            // one full product ULP unconditionally instead of trusting zero.
            let residual_safe_floor = f64::MIN_POSITIVE * (1_u64 << 53) as f64;
            let underflow_guard = product.abs() < residual_safe_floor;
            let product_lo = if error < 0.0 || underflow_guard {
                product.next_down()
            } else {
                product
            };
            let product_hi = if error > 0.0 || underflow_guard {
                product.next_up()
            } else {
                product
            };
            lower = lower.min(product_lo);
            upper = upper.max(product_hi);
        }
        Self {
            lo: lower,
            hi: upper,
        }
    }

    fn abs(self) -> Self {
        if self.lo >= 0.0 {
            self
        } else if self.hi <= 0.0 {
            self.neg()
        } else {
            Self {
                lo: 0.0,
                hi: (-self.lo).max(self.hi),
            }
        }
    }

    fn exact_zero(self) -> bool {
        self.lo == 0.0 && self.hi == 0.0
    }
}

fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let sum = a + b;
    let b_virtual = sum - a;
    let error = (a - (sum - b_virtual)) + (b - b_virtual);
    (sum, error)
}

type IVec3 = [Interval; 3];

fn ivec_add(a: IVec3, b: IVec3) -> IVec3 {
    std::array::from_fn(|i| a[i].add(b[i]))
}

fn ivec_sub(a: IVec3, b: IVec3) -> IVec3 {
    std::array::from_fn(|i| a[i].sub(b[i]))
}

fn ivec_scale(a: IVec3, scale: Interval) -> IVec3 {
    std::array::from_fn(|i| a[i].mul(scale))
}

fn ivec_dot(a: IVec3, b: IVec3) -> Interval {
    a[0].mul(b[0]).add(a[1].mul(b[1])).add(a[2].mul(b[2]))
}

fn ivec_cross(a: IVec3, b: IVec3) -> IVec3 {
    [
        a[1].mul(b[2]).sub(a[2].mul(b[1])),
        a[2].mul(b[0]).sub(a[0].mul(b[2])),
        a[0].mul(b[1]).sub(a[1].mul(b[0])),
    ]
}

fn rotate_interval(q: [f32; 4], value: IVec3) -> IVec3 {
    let qv = [
        Interval::point(f64::from(q[0])),
        Interval::point(f64::from(q[1])),
        Interval::point(f64::from(q[2])),
    ];
    let twice_cross = ivec_scale(ivec_cross(qv, value), Interval::point(2.0));
    ivec_add(
        value,
        ivec_add(
            ivec_scale(twice_cross, Interval::point(f64::from(q[3]))),
            ivec_cross(qv, twice_cross),
        ),
    )
}

fn cell_box(geometry: &CellGeometry) -> Option<(IVec3, [IVec3; 3])> {
    let cell_m = Interval::point(f64::from(geometry.cell_m));
    let source_center: IVec3 = std::array::from_fn(|axis| {
        Interval::point(f64::from(geometry.source_cell[axis]))
            .add(Interval::point(0.5))
            .mul(cell_m)
    });
    let translation = geometry
        .pose_translation
        .map(|v| Interval::point(f64::from(v)));
    let center = ivec_add(
        translation,
        rotate_interval(geometry.rotation_xyzw, source_center),
    );
    let half = cell_m.mul(Interval::point(0.5));
    let edges = [
        rotate_interval(
            geometry.rotation_xyzw,
            [half, Interval::ZERO, Interval::ZERO],
        ),
        rotate_interval(
            geometry.rotation_xyzw,
            [Interval::ZERO, half, Interval::ZERO],
        ),
        rotate_interval(
            geometry.rotation_xyzw,
            [Interval::ZERO, Interval::ZERO, half],
        ),
    ];
    if center
        .into_iter()
        .chain(edges.into_iter().flatten())
        .all(Interval::finite)
    {
        Some((center, edges))
    } else {
        None
    }
}

#[derive(Clone, Copy)]
struct CachedBox {
    center: IVec3,
    edges: [IVec3; 3],
    aabb: IVec3,
}

fn cache_cell_box(cell: &CellGeometry) -> Option<CachedBox> {
    let (center, edges) = cell_box(cell)?;
    let aabb = std::array::from_fn(|axis| {
        let radius = edges
            .iter()
            .map(|edge| edge[axis].abs())
            .fold(Interval::ZERO, Interval::add);
        Interval {
            lo: center[axis].sub(radius).lo,
            hi: center[axis].add(radius).hi,
        }
    });
    aabb.into_iter().all(Interval::finite).then_some(CachedBox {
        center,
        edges,
        aabb,
    })
}

fn classify_parallelepiped_pair(a: &CellGeometry, b: &CellGeometry) -> (ContactClass, usize) {
    let Some(CachedBox {
        center: center_a,
        edges: edges_a,
        ..
    }) = a.cached_box
    else {
        return (ContactClass::Ambiguous, 0);
    };
    let Some(CachedBox {
        center: center_b,
        edges: edges_b,
        ..
    }) = b.cached_box
    else {
        return (ContactClass::Ambiguous, 0);
    };
    let mut axes = [[Interval::ZERO; 3]; 15];
    axes[0] = ivec_cross(edges_a[1], edges_a[2]);
    axes[1] = ivec_cross(edges_a[2], edges_a[0]);
    axes[2] = ivec_cross(edges_a[0], edges_a[1]);
    axes[3] = ivec_cross(edges_b[1], edges_b[2]);
    axes[4] = ivec_cross(edges_b[2], edges_b[0]);
    axes[5] = ivec_cross(edges_b[0], edges_b[1]);
    let mut axis_index = 6;
    for (index_a, edge_a) in edges_a.into_iter().enumerate() {
        for (index_b, edge_b) in edges_b.into_iter().enumerate() {
            // Identical canonical affine directions have an exactly zero
            // cross-axis. Interval dependency alone cannot recover that
            // algebraic identity from two independently enclosed products.
            let same_direction = index_a == index_b
                && a.rotation_xyzw.map(f32::to_bits) == b.rotation_xyzw.map(f32::to_bits)
                && a.cell_m.to_bits() == b.cell_m.to_bits();
            axes[axis_index] = if same_direction {
                [Interval::ZERO; 3]
            } else {
                ivec_cross(edge_a, edge_b)
            };
            axis_index += 1;
        }
    }
    let delta = ivec_sub(center_b, center_a);
    let mut tested = 0;
    let mut strict_overlap = true;
    let mut strict_axes = [false; 15];
    for (axis_index, axis) in axes.into_iter().enumerate() {
        if axis.into_iter().all(Interval::exact_zero) {
            continue;
        }
        tested += 1;
        if !axis.into_iter().all(Interval::finite) {
            return (ContactClass::Ambiguous, tested);
        }
        let projection = ivec_dot(delta, axis).abs();
        let radius_a = edges_a
            .iter()
            .map(|edge| ivec_dot(axis, *edge).abs())
            .fold(Interval::ZERO, Interval::add);
        let radius_b = edges_b
            .iter()
            .map(|edge| ivec_dot(axis, *edge).abs())
            .fold(Interval::ZERO, Interval::add);
        let radius = radius_a.add(radius_b);
        if !projection.finite() || !radius.finite() {
            return (ContactClass::Ambiguous, tested);
        }
        if projection.lo > radius.hi {
            return (ContactClass::Separated, tested);
        }
        strict_axes[axis_index] = projection.hi < radius.lo;
        if !strict_axes[axis_index] {
            strict_overlap = false;
        }
    }
    if (strict_overlap && tested > 0)
        || certify_axis_aligned_face(center_a, edges_a, center_b, edges_b, axes, strict_axes)
    {
        (ContactClass::PositiveContact, tested)
    } else {
        (ContactClass::Ambiguous, tested)
    }
}

/// An exact shared supporting plane plus strict projected polygon overlap proves
/// positive-area material contact. Only prisms with a certified pure world-axis
/// edge and zero axial components on the other edges qualify. The four side-face
/// normals are already present in the complete SAT set; no extra axis budget or
/// tolerance is introduced. A nonzero axial face normal certifies rank two.
fn certify_axis_aligned_face(
    center_a: IVec3,
    edges_a: [IVec3; 3],
    center_b: IVec3,
    edges_b: [IVec3; 3],
    axes: [IVec3; 15],
    strict_axes: [bool; 15],
) -> bool {
    let pure_edge = |edges: [IVec3; 3], axis: usize| {
        (0..3).find(|&i| {
            edges[i][axis].abs().lo > 0.0
                && (0..3)
                    .filter(|&j| j != axis)
                    .all(|j| edges[i][j].exact_zero())
                && (0..3)
                    .filter(|&j| j != i)
                    .all(|j| edges[j][axis].exact_zero())
        })
    };
    let same_point = |a: Interval, b: Interval| {
        a.finite() && b.finite() && a.lo == a.hi && b.lo == b.hi && a.lo == b.lo
    };
    for axis in 0..3 {
        let (Some(i), Some(j)) = (pure_edge(edges_a, axis), pure_edge(edges_b, axis)) else {
            continue;
        };
        let ra = edges_a[i][axis].abs();
        let rb = edges_b[j][axis].abs();
        let coincident = same_point(center_a[axis].add(ra), center_b[axis].sub(rb))
            || same_point(center_b[axis].add(rb), center_a[axis].sub(ra));
        if coincident
            && axes[i][axis].abs().lo > 0.0
            && axes[3 + j][axis].abs().lo > 0.0
            && (0..3).filter(|&k| k != i).all(|k| strict_axes[k])
            && (0..3).filter(|&k| k != j).all(|k| strict_axes[3 + k])
        {
            return true;
        }
    }
    false
}

fn signed_permutation(axes: [[f64; 3]; 3]) -> Option<[[i32; 3]; 3]> {
    let mut rotation = [[0; 3]; 3];
    for column in 0..3 {
        let mut found = None;
        for (row, &value) in axes[column].iter().enumerate() {
            match value {
                -1.0 => {
                    if found.is_some() {
                        return None;
                    }
                    found = Some((row, -1));
                }
                0.0 => {}
                1.0 => {
                    if found.is_some() {
                        return None;
                    }
                    found = Some((row, 1));
                }
                _ => return None,
            }
        }
        let (row, sign) = found?;
        if rotation[row].iter().any(|value| *value != 0) {
            return None;
        }
        rotation[row][column] = sign;
    }
    Some(rotation)
}

/// Exact common-lattice predicate for signed-permutation part frames. The
/// relative translation must be an exactly represented integer number of
/// cells. Integer interval arithmetic proves positive-area face contact;
/// edge/corner contacts are separated.
fn classify_lattice_pair(a: &CellGeometry, b: &CellGeometry) -> Option<ContactClass> {
    if a.rotation == [[0; 3]; 3]
        || b.rotation == [[0; 3]; 3]
        || a.cell_m.to_bits() != b.cell_m.to_bits()
    {
        return None;
    }
    let cell_m = a.cell_m;
    // Promotion precedes subtraction: f64 represents each finite f32 exactly,
    // but the subtraction itself is accepted only when TwoDiff proves no
    // residual was lost.
    let translation_delta = std::array::from_fn::<_, 3, _>(|axis| {
        exact_difference(
            f64::from(b.pose_translation[axis]),
            f64::from(a.pose_translation[axis]),
        )
    });
    let cell_m = f64::from(cell_m);
    let mut delta = [0_i64; 3];
    for axis in 0..3 {
        let cells = translation_delta[axis]? / cell_m;
        let rounded = cells.round();
        if !rounded.is_finite()
            || rounded < i64::MIN as f64
            || rounded >= 9_007_199_254_740_992.0
            || exact_product(rounded, cell_m)? != translation_delta[axis]?
        {
            return None;
        }
        delta[axis] = rounded as i64;
    }
    let world_cell =
        |rotation: [[i32; 3]; 3], source: [i32; 3], offset: [i64; 3]| -> Option<[i64; 3]> {
            let mut result = [0_i64; 3];
            for axis in 0..3 {
                let mut coordinate = offset[axis];
                for input_axis in 0..3 {
                    let sign = rotation[axis][input_axis];
                    if sign == -1 {
                        coordinate = coordinate.checked_sub(1)?;
                    }
                    coordinate =
                        coordinate.checked_add(i64::from(sign) * i64::from(source[input_axis]))?;
                }
                result[axis] = coordinate;
            }
            Some(result)
        };
    let a_cell = world_cell(a.rotation, a.source_cell, [0; 3])?;
    let b_cell = world_cell(b.rotation, b.source_cell, delta)?;
    let diff = std::array::from_fn::<_, 3, _>(|axis| b_cell[axis].checked_sub(a_cell[axis]));
    let diff = [diff[0]?, diff[1]?, diff[2]?];
    let absolute = [
        diff[0].checked_abs()?,
        diff[1].checked_abs()?,
        diff[2].checked_abs()?,
    ];
    if absolute.iter().any(|value| *value > 1) {
        Some(ContactClass::Separated)
    } else {
        match absolute.iter().map(|value| *value as usize).sum::<usize>() {
            0 => Some(ContactClass::PositiveContact),
            1 => Some(ContactClass::PositiveContact),
            _ => Some(ContactClass::Separated),
        }
    }
}

fn exact_difference(a: f64, b: f64) -> Option<f64> {
    let difference = a - b;
    let b_virtual = a - difference;
    let a_virtual = difference + b_virtual;
    let b_roundoff = b_virtual - b;
    let a_roundoff = a - a_virtual;
    let residual = a_roundoff + b_roundoff;
    (residual == 0.0).then_some(difference)
}

fn exact_product(a: f64, b: f64) -> Option<f64> {
    const SPLITTER: f64 = 134_217_729.0; // 2^27 + 1 for IEEE binary64.
    let product = a * b;
    if !product.is_finite() {
        return None;
    }
    let a_split = SPLITTER * a;
    let a_high = a_split - (a_split - a);
    let a_low = a - a_high;
    let b_split = SPLITTER * b;
    let b_high = b_split - (b_split - b);
    let b_low = b - b_high;
    let residual = ((a_high * b_high - product) + a_high * b_low + a_low * b_high) + a_low * b_low;
    (residual == 0.0).then_some(product)
}

fn rotate64(q: [f32; 4], v: [f64; 3]) -> [f64; 3] {
    let q = q.map(f64::from);
    let qv = [q[0], q[1], q[2]];
    let t = scale(cross(qv, v), 2.0);
    add(v, add(scale(t, q[3]), cross(qv, t)))
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|axis| a[axis] + b[axis])
}
fn scale(a: [f64; 3], factor: f64) -> [f64; 3] {
    a.map(|value| value * factor)
}
fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}
impl UnionFind {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
            rank: vec![0; len],
        }
    }
    fn root(&mut self, index: usize) -> usize {
        if self.parent[index] != index {
            self.parent[index] = self.root(self.parent[index]);
        }
        self.parent[index]
    }
    fn join(&mut self, a: usize, b: usize) {
        let mut a = self.root(a);
        let mut b = self.root(b);
        if a == b {
            return;
        }
        if self.rank[a] < self.rank[b] {
            std::mem::swap(&mut a, &mut b);
        }
        self.parent[b] = a;
        if self.rank[a] == self.rank[b] {
            self.rank[a] += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use spall_core::{GlobalCell, MaterialId, VolumeId};

    use super::*;
    use crate::occupancy::OccupancyGrid;

    fn volume(id: u64) -> VolumeId {
        VolumeId::new(id).unwrap()
    }

    fn part(
        part_id: u32,
        volume_id: u64,
        origin: [i64; 3],
        dims: [u32; 3],
        solid: Vec<bool>,
        pose: PartTransform,
    ) -> FracturePartSnapshot {
        let materials = vec![MaterialId(1); solid.len()];
        FracturePartSnapshot {
            source: SourcePart {
                part_id,
                parent_id: None,
                volume_id: volume(volume_id),
                source_revision: 7,
                grid: OccupancyGrid::from_solid_mask(
                    GlobalCell::new(origin[0], origin[1], origin[2]),
                    dims,
                    solid,
                    materials,
                )
                .unwrap(),
                cell_m: 1.0,
                local_transform: PartTransform::IDENTITY,
            },
            root_local_pose: pose,
        }
    }

    fn snapshot(parts: Vec<FracturePartSnapshot>) -> FractureSnapshot {
        FractureSnapshot {
            token: FractureSnapshotToken {
                corpse_key: 9,
                topology_revision: 11,
                pose_epoch: 13,
                source_generation: 17,
                component_key: None,
            },
            parts,
        }
    }

    fn z_rotation_45(translation: [f32; 3]) -> PartTransform {
        let half_angle = std::f32::consts::FRAC_PI_8;
        PartTransform {
            translation_m: translation,
            rotation_xyzw: [0.0, 0.0, half_angle.sin(), half_angle.cos()],
        }
    }

    #[test]
    fn tiny_normal_product_with_underflowed_fma_residual_is_enclosed() {
        let a = (1.0 + 2.0_f64.powi(-52)) * 2.0_f64.powi(-500);
        let product = a * a;
        // Exact expansion is 2^-1000 + 2^-1051 + 2^-1104. Binary64 rounds
        // away the final term, and its FMA residual itself underflows to zero.
        assert_eq!(a.mul_add(a, -product), 0.0);
        let enclosed = Interval::point(a).mul(Interval::point(a));
        assert_eq!(enclosed.lo, product.next_down());
        assert_eq!(enclosed.hi, product.next_up());
    }

    #[test]
    fn same_volume_uses_integer_face_adjacency_and_bridge_removal_partitions_exactly() {
        let before = snapshot(vec![part(
            1,
            101,
            [-3, -1, 2],
            [3, 1, 1],
            vec![true, true, true],
            PartTransform::IDENTITY,
        )]);
        let plan = prepare_fracture(&before).unwrap();
        assert_eq!(plan.components.len(), 1);
        assert_eq!(plan.source_cells[0].cell, [-3, -1, 2]);
        assert_eq!(plan.source_cells[2].cell, [-1, -1, 2]);

        let after = snapshot(vec![part(
            1,
            101,
            [-3, -1, 2],
            [3, 1, 1],
            vec![true, false, true],
            PartTransform::IDENTITY,
        )]);
        let plan = prepare_fracture(&after).unwrap();
        assert_eq!(plan.components.len(), 2);
        assert_eq!(plan.components[0].cells[0].cell, [-3, -1, 2]);
        assert_eq!(plan.components[1].cells[0].cell, [-1, -1, 2]);
    }

    #[test]
    fn arbitrary_rotated_cross_volume_overlap_requires_certified_material_intersection() {
        let result = prepare_fracture(&snapshot(vec![
            part(
                1,
                101,
                [0, 0, 0],
                [1, 1, 1],
                vec![true],
                PartTransform::IDENTITY,
            ),
            part(
                2,
                102,
                [0, 0, 0],
                [1, 1, 1],
                vec![true],
                z_rotation_45([0.55, 0.0, 0.0]),
            ),
        ]));
        assert_eq!(result.unwrap().components.len(), 1);
    }

    #[test]
    fn rotated_aabb_overlap_does_not_create_a_false_connection() {
        let result = prepare_fracture(&snapshot(vec![
            part(
                1,
                101,
                [0, 0, 0],
                [1, 1, 1],
                vec![true],
                PartTransform {
                    translation_m: [-0.5, -0.5, -0.5],
                    rotation_xyzw: PartTransform::IDENTITY.rotation_xyzw,
                },
            ),
            part(
                2,
                102,
                [0, 0, 0],
                [1, 1, 1],
                vec![true],
                z_rotation_45([1.1, 0.393, -0.5]),
            ),
        ]));
        // SAT proves separation along a diagonal despite overlapping AABBs.
        assert_eq!(result.unwrap().components.len(), 2);
    }

    #[test]
    fn exact_common_lattice_face_contact_is_connected() {
        let result = prepare_fracture(&snapshot(vec![
            part(
                1,
                101,
                [0, 0, 0],
                [1, 1, 1],
                vec![true],
                PartTransform::IDENTITY,
            ),
            part(
                2,
                102,
                [0, 0, 0],
                [1, 1, 1],
                vec![true],
                PartTransform {
                    translation_m: [1.0, 0.0, 0.0],
                    rotation_xyzw: PartTransform::IDENTITY.rotation_xyzw,
                },
            ),
        ]));
        let plan = result.unwrap();
        assert_eq!(plan.certified_cross_volume_edges, 1);
        assert_eq!(plan.components.len(), 1);
    }

    #[test]
    fn plan_preserves_source_revision_material_and_full_identity() {
        let plan = prepare_fracture(&snapshot(vec![part(
            3,
            503,
            [-9, 2, -4],
            [1, 1, 1],
            vec![true],
            PartTransform::IDENTITY,
        )]))
        .unwrap();
        assert_eq!(plan.version, FRACTURE_PLAN_VERSION);
        assert_eq!(plan.token.topology_revision, 11);
        assert_eq!(plan.source_cells[0].volume_id, volume(503));
        assert_eq!(plan.source_cells[0].part_id, 3);
        assert_eq!(plan.source_cells[0].cell, [-9, 2, -4]);
        assert_eq!(plan.source_cells[0].material, MaterialId(1));
        assert_eq!(plan.source_cells[0].source_revision, 7);
    }

    #[test]
    fn duplicate_or_empty_source_parts_reject() {
        let duplicate = snapshot(vec![
            part(
                1,
                101,
                [0, 0, 0],
                [1, 1, 1],
                vec![true],
                PartTransform::IDENTITY,
            ),
            part(
                2,
                101,
                [2, 0, 0],
                [1, 1, 1],
                vec![true],
                PartTransform::IDENTITY,
            ),
        ]);
        assert_eq!(
            prepare_fracture(&duplicate).unwrap_err(),
            FractureError::InvalidSnapshot
        );
        let empty = snapshot(vec![part(
            1,
            101,
            [0, 0, 0],
            [1, 1, 1],
            vec![false],
            PartTransform::IDENTITY,
        )]);
        assert_eq!(
            prepare_fracture(&empty).unwrap_err(),
            FractureError::InvalidSnapshot
        );
    }
}

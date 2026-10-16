//! Exact per-source-part collision for authored, unit-scale voxel assemblies.
//!
//! This is a bounded feasibility adapter. Geometry stays as an `OccupancyGrid`
//! per source volume; the world attaches each exact shape to one kinematic root.

use spall_core::VolumeId;
use thiserror::Error;

/// Opaque handle for an assembly managed by its articulated APIs. It cannot be
/// passed to single-grid body rebuild/dormancy methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArticulatedBodyId(pub(crate) crate::world::BodyId);

use crate::occupancy::OccupancyGrid;

pub const MAX_ARTICULATED_PARTS: usize = 1024;
pub const MAX_ARTICULATED_DENSE_CELLS: u64 = 4_000_000;
pub const MAX_ARTICULATED_SOLID_CELLS: u64 = 1_000_000;
/// Maximum supported per-axis collider extent and source-origin offset (metres).
/// This is a finite-shape guard, not a claim that cell precision is exact at the limit.
pub const MAX_ARTICULATED_GEOMETRY_EXTENT_M: f64 = 1_000.0;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ArticulatedError {
    #[error("articulated part count is outside 1..={MAX_ARTICULATED_PARTS}")]
    PartCount,
    #[error("part IDs must be nonzero and unique")]
    DuplicatePart,
    #[error("each source volume must be owned by only one articulated assembly")]
    DuplicateVolume,
    #[error("articulated hierarchy must have one root and no missing parents or cycles")]
    Hierarchy,
    #[error("all parts must have finite unit-scale transforms and matching positive cell sizes")]
    InvalidTransform,
    #[error("part {0} has no occupied source cells")]
    EmptyPart(u32),
    #[error("articulated source geometry exceeds the bounded cell-work budget")]
    WorkBudget,
    #[error(
        "articulated terrain transition budget exceeded: {source_cells} cells × at least {terrain_candidates_at_least} current fixed terrain candidates"
    )]
    TerrainTransitionWorkBudget {
        source_cells: usize,
        terrain_candidates_at_least: usize,
    },
    #[error("unsupported exact terrain transition cast for articulated part {part_id}")]
    UnsupportedTerrainTransitionCast { part_id: u32 },
    #[error("source coordinates exceed i32 hit provenance or the bounded finite collider extent")]
    GeometryRange,
    #[error("pose update topology/base token is stale, duplicated, or out of order")]
    StalePose,
    #[error("articulated topology or source revision is stale")]
    StaleTopology,
    #[error("the ray is invalid or no exact source collider was hit")]
    Miss,
    #[error("root pose is non-finite or puts attached geometry outside finite solver coordinates")]
    InvalidRootPose,
    #[error("another articulated root pose is awaiting the next physics step")]
    PendingRootPose,
    #[error("dynamic corpse aggregates do not accept authored pose or sweep updates")]
    DynamicCorpsePose,
    #[error("aggregate source-cell mass properties are invalid or singular")]
    InvalidMassProperties,
    #[error("solid source material has an invalid or non-positive density")]
    InvalidDensity,
    #[error("a dynamic corpse aggregate must retain at least one occupied source part")]
    EmptyAggregate,
}

/// A unit-scale part-local transform in metres and `(x, y, z, w)` form.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartTransform {
    pub translation_m: [f32; 3],
    pub rotation_xyzw: [f32; 4],
}

impl PartTransform {
    pub const IDENTITY: Self = Self {
        translation_m: [0.0; 3],
        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
    };

    pub fn is_valid(self) -> bool {
        let norm_squared = self.rotation_xyzw.iter().map(|v| v * v).sum::<f32>();
        self.translation_m.iter().all(|v| v.is_finite())
            && self.rotation_xyzw.iter().all(|v| v.is_finite())
            && norm_squared.is_finite()
            && norm_squared > 1.0e-12
    }

    pub(crate) fn normalized(self) -> Option<Self> {
        self.is_valid().then(|| Self {
            translation_m: self.translation_m,
            rotation_xyzw: normalize(self.rotation_xyzw),
        })
    }
}

/// Immutable source geometry and hierarchy for one articulated part.
#[derive(Clone)]
pub struct SourcePart {
    pub part_id: u32,
    /// `None` only for the assembly root part.
    pub parent_id: Option<u32>,
    pub volume_id: VolumeId,
    pub source_revision: u64,
    pub grid: OccupancyGrid,
    pub cell_m: f32,
    pub local_transform: PartTransform,
}

/// A complete sampled local-pose set. The token identifies the source topology
/// and the monotonically increasing sampled pose installed at a tick boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ArticulatedPose {
    pub topology_token: u64,
    pub base_pose_token: u64,
    pub pose_token: u64,
    pub parts: Vec<(u32, PartTransform)>,
}

/// Authoritative result of a Rapier ray hit on an articulated source collider.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceCellHit {
    pub part_id: u32,
    pub volume_id: VolumeId,
    pub source_revision: u64,
    pub cell: [i32; 3],
    pub toi_m: f32,
    pub topology_token: u64,
    pub pose_token: u64,
}

/// Result of the experimental assembly-root sweep. The sweep moves a sphere
/// enclosing every current source part, so it is conservative and may reject
/// clearances an exact articulated shape cast would allow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArticulatedMove {
    pub requested_translation_m: [f32; 3],
    pub applied_translation_m: [f32; 3],
    pub fraction: f32,
    pub blocked: bool,
}

/// Result of sweeping one complete sampled assembly pose against current fixed
/// terrain. A hit rejects the entire sample before any body or pose token is
/// staged; it is not a partial interpolation or a solver contact response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArticulatedTransition {
    pub blocked: bool,
    pub part_id: Option<u32>,
    /// Earliest normalized time in the declared one-tick interpolation path.
    pub time_of_impact: Option<f32>,
    /// Current fixed nonsensor colliders retained by the swept-AABB broadphase.
    pub terrain_candidates: usize,
    /// Exact source-cell nonlinear casts actually issued for this transition.
    pub cell_casts: usize,
    /// Time spent refreshing live query structures before broadphase lookup.
    pub query_sync_time: std::time::Duration,
    /// Time spent collecting candidates from the refreshed broadphase.
    pub candidate_query_time: std::time::Duration,
    /// Time spent in exact source-cell nonlinear casts.
    pub exact_cast_time: std::time::Duration,
}

/// Compose a child-local transform with its parent's assembly-local transform.
pub(crate) fn compose(parent: PartTransform, local: PartTransform) -> PartTransform {
    let p = parent.rotation_xyzw;
    let l = local.rotation_xyzw;
    let rotated = rotate(p, local.translation_m);
    let q = [
        p[3] * l[0] + p[0] * l[3] + p[1] * l[2] - p[2] * l[1],
        p[3] * l[1] - p[0] * l[2] + p[1] * l[3] + p[2] * l[0],
        p[3] * l[2] + p[0] * l[1] - p[1] * l[0] + p[2] * l[3],
        p[3] * l[3] - p[0] * l[0] - p[1] * l[1] - p[2] * l[2],
    ];
    PartTransform {
        translation_m: [
            parent.translation_m[0] + rotated[0],
            parent.translation_m[1] + rotated[1],
            parent.translation_m[2] + rotated[2],
        ],
        rotation_xyzw: normalize(q),
    }
}

pub(crate) fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let qv = [q[0], q[1], q[2]];
    let t = cross(qv, v).map(|x| x * 2.0);
    let c = cross(qv, t);
    [
        v[0] + q[3] * t[0] + c[0],
        v[1] + q[3] * t[1] + c[1],
        v[2] + q[3] * t[2] + c[2],
    ]
}

pub(crate) fn normalize(q: [f32; 4]) -> [f32; 4] {
    let length = q.iter().map(|v| v * v).sum::<f32>().sqrt();
    q.map(|v| v / length)
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_transform_rotates_child_translation_and_normalizes_rotation() {
        let parent = PartTransform {
            translation_m: [2.0, 0.0, 0.0],
            rotation_xyzw: [
                0.0,
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
            ],
        };
        let child = PartTransform {
            translation_m: [1.0, 0.0, 0.0],
            rotation_xyzw: PartTransform::IDENTITY.rotation_xyzw,
        };
        let result = compose(parent, child);
        assert!((result.translation_m[0] - 2.0).abs() < 1.0e-6);
        assert!((result.translation_m[1] - 1.0).abs() < 1.0e-6);
        assert!((result.rotation_xyzw.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn transform_rejects_non_finite_and_zero_quaternion() {
        assert!(
            !PartTransform {
                translation_m: [f32::NAN, 0.0, 0.0],
                ..PartTransform::IDENTITY
            }
            .is_valid()
        );
        assert!(
            !PartTransform {
                rotation_xyzw: [0.0; 4],
                ..PartTransform::IDENTITY
            }
            .is_valid()
        );
    }
}

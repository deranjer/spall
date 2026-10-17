//! The narrow Rapier adapter: a fixed-step physics world that speaks in engine
//! terms only. Rapier handles never leave this module; callers address bodies
//! by an opaque [`BodyId`].

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use rapier3d::control::CharacterCollision;
use rapier3d::prelude::*;
use spall_core::{GlobalCell, MaterialId};

use crate::articulated::{
    ArticulatedBodyId, ArticulatedPose, MAX_ARTICULATED_PARTS, PartTransform, SourceCellHit,
    SourcePart, compose, normalize, rotate,
};
use crate::collider::{Representation, build_collider};
use crate::fracture::{
    DerivedFractureComponentId, FRACTURE_PLAN_VERSION, FractureError, FracturePartSnapshot,
    FractureSnapshot, FractureSnapshotToken, PreparedFracturePlan, SourceCellRef, prepare_fracture,
};
use crate::mass::BodyMassProperties;
use crate::occupancy::OccupancyGrid;

const MAX_TRANSITION_CELL_CASTS: usize = 1_000_000;

fn transition_candidate_limit(source_cells: usize) -> usize {
    MAX_TRANSITION_CELL_CASTS / source_cells.max(1)
}

fn require_supported_transition_cast<T>(
    part_id: u32,
    result: Result<Option<T>, rapier3d::parry::query::Unsupported>,
) -> Result<Option<T>, crate::articulated::ArticulatedError> {
    result.map_err(
        |_| crate::articulated::ArticulatedError::UnsupportedTerrainTransitionCast { part_id },
    )
}

/// Opaque, stable identifier for a body in a [`PhysicsWorld`]. The high 32 bits
/// namespace bodies across coordinated physics regions; the low 32 bits are the
/// body's stable slot within its region. Never a Rapier handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BodyId(u64);

impl BodyId {
    /// The region-local slot, for report keys only.
    pub fn index(self) -> u32 {
        self.0 as u32
    }

    /// The physics-region namespace used to route this id to its owner.
    pub fn namespace(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

impl From<&BodyId> for BodyId {
    fn from(value: &BodyId) -> Self {
        *value
    }
}

fn entry_index<I: Into<BodyId>>(id: I, namespace: u32) -> usize {
    let id = id.into();
    assert_eq!(
        id.namespace(),
        namespace,
        "body id belongs to another physics region"
    );
    id.index() as usize
}

/// Fixed simulation configuration.
#[derive(Debug, Clone, Copy)]
pub struct PhysicsConfig {
    /// Gravity, m/s².
    pub gravity_m_s2: [f32; 3],
    /// Fixed timestep, seconds.
    pub dt_s: f32,
    /// Constraint solver iterations per step.
    pub solver_iterations: usize,
    /// Number of coordinated solver substeps per outer physics tick (1–64).
    /// Every dynamic body advances the same total `dt_s`; position-based
    /// kinematics are interpolated across these substeps. Values above one
    /// enable a 2.5 cm per-substep swept-surface admission limit for
    /// articulated pose updates.
    pub solver_substeps: usize,
    /// Skip rapier's CCD solver pass entirely (`max_ccd_substeps = 0`). Callers
    /// that never enable per-body CCD should set this: rapier's CCD broad-phase
    /// BVH can retain a stale proxy for a collider that was removed and
    /// re-inserted in the same step (as [`PhysicsWorld::rebuild_collider`] does
    /// on every voxel edit), then panic with "No element at index" mid-sweep.
    /// Disabling the pass is a no-op when no body has `ccd` set.
    pub disable_ccd: bool,
}

impl Default for PhysicsConfig {
    fn default() -> Self {
        Self {
            gravity_m_s2: [0.0, -9.81, 0.0],
            dt_s: 1.0 / 60.0,
            solver_iterations: 4,
            solver_substeps: 1,
            disable_ccd: false,
        }
    }
}

/// Whether a body is simulated or immovable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// Immovable terrain.
    Fixed,
    /// Simulated body. `ccd` enables continuous collision detection for fast
    /// movers.
    Dynamic { ccd: bool },
}

/// Everything needed to add one body.
pub struct BodySpec {
    /// Fixed or dynamic.
    pub kind: BodyKind,
    /// Collision representation to build.
    pub representation: Representation,
    /// Solid occupancy of the body, in its own local grid.
    pub grid: OccupancyGrid,
    /// Cell edge length, metres.
    pub cell_m: f32,
    /// Uniform collider density, kg/m³. Used **only** when [`Self::mass_properties`]
    /// is `None`; then mass / COM / inertia are whatever Rapier derives from the
    /// collision shape at this density.
    pub density_kg_m3: f32,
    /// Exact rigid mass properties derived from the fine material grid. When
    /// `Some`, they are installed into the body verbatim, the collider is given
    /// zero density so it contributes no mass, and these values — not the
    /// collision shape — are authoritative for the solver. Coarse collider
    /// inflation therefore cannot change the physical mass, COM, or inertia.
    pub mass_properties: Option<BodyMassProperties>,
    /// World translation of the body-local origin, metres.
    pub translation_m: [f32; 3],
    /// Initial linear velocity, m/s (ignored for `Fixed`).
    pub linvel_m_s: [f32; 3],
}

/// A body whose collider was built ahead of time. Everything in [`BodySpec`] except the grid.
pub struct PreparedBodySpec {
    pub kind: BodyKind,
    pub representation: Representation,
    pub cell_m: f32,
    pub density_kg_m3: f32,
    pub mass_properties: Option<BodyMassProperties>,
    pub translation_m: [f32; 3],
    pub linvel_m_s: [f32; 3],
}

/// A collider built from an occupancy grid, ready to attach to a body.
///
/// Building is a pure function of the grid, the cell size and the representation, so it can run
/// on any thread and for many bodies at once; only attaching it to the solver needs the world.
pub struct PreparedCollider {
    build: crate::collider::ColliderBuild,
    offset_m: [f32; 3],
}

impl PreparedCollider {
    pub fn new(grid: &OccupancyGrid, cell_m: f32, representation: Representation) -> Self {
        Self {
            build: build_collider(grid, cell_m, representation),
            offset_m: grid_origin_offset_m(grid, cell_m),
        }
    }
}

/// Kinematic snapshot of one body after a step.
#[derive(Debug, Clone, Copy)]
pub struct BodyState {
    /// World translation, metres.
    pub translation_m: [f32; 3],
    /// Orientation quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// Linear velocity, m/s.
    pub linvel_m_s: [f32; 3],
    /// Angular velocity, rad/s.
    pub angvel_rad_s: [f32; 3],
    /// Whether Rapier has put the body to sleep.
    pub sleeping: bool,
    /// Mass Rapier derived from the collider, kg (0 for `Fixed`).
    pub mass_kg: f32,
}

/// Opaque read fence for selected ordinary physics bodies. Runtime handles
/// and shared-shape identity stay inside this module and are never encoded.
pub struct BodyCommitFence {
    world_identity: std::sync::Arc<()>,
    step_count: u64,
    entry_count: usize,
    body_count: usize,
    bodies: Vec<BodyCommitFenceEntry>,
}

struct BodyCommitFenceEntry {
    id: BodyId,
    body: RigidBodyHandle,
    body_type: RigidBodyType,
    position: Pose,
    linvel: Vector,
    angvel: Vector,
    mass: f32,
    center_of_mass: Vector,
    retired: bool,
    dormant: bool,
    collider: Option<ColliderCommitFenceState>,
    representation: Representation,
    query_only: bool,
    density: f32,
    mass_properties: Option<BodyMassProperties>,
}

struct ColliderCommitFenceState {
    handle: ColliderHandle,
    shape: SharedShape,
    position: Pose,
    enabled: bool,
    sensor: bool,
    active_collision_types: ActiveCollisionTypes,
    collision_groups: InteractionGroups,
    solver_groups: InteractionGroups,
    friction: f32,
    restitution: f32,
}

impl BodyState {
    /// True if every kinematic field is finite (no NaN / infinity blow-up).
    pub fn is_finite(&self) -> bool {
        self.translation_m.iter().all(|v| v.is_finite())
            && self.rotation.iter().all(|v| v.is_finite())
            && self.linvel_m_s.iter().all(|v| v.is_finite())
            && self.angvel_rad_s.iter().all(|v| v.is_finite())
    }

    /// Linear speed, m/s.
    pub fn speed_m_s(&self) -> f32 {
        let v = self.linvel_m_s;
        (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
    }
}

/// Value-only live snapshot for an articulated owner. It contains no physics
/// handles and lets an owner validate a prepared lifecycle transition against
/// the current topology and sampled pose.
#[derive(Debug, Clone, Copy)]
pub struct ArticulatedOwnerSnapshot {
    pub topology_token: u64,
    pub pose_token: u64,
    pub pending_pose: bool,
    pub root: BodyState,
}

/// Per-step timing.
#[derive(Debug, Clone, Copy)]
pub struct StepTiming {
    /// 1-based index of the step just run.
    pub step_index: u64,
    /// Wall-clock time inside `PhysicsPipeline::step`.
    pub pipeline: Duration,
    /// Additional wall-clock time to snapshot and attribute articulated
    /// solver-point witnesses across all outer-step substeps.
    pub contact_witness_capture: Duration,
    /// Number of coordinated world-pipeline substeps used for this outer tick.
    pub substeps: usize,
    /// Sum of positive solver normal impulses across active contact pairs and
    /// internal substeps (N·s). This is an outer-tick total, not a unique-pair
    /// count and not an energy/work estimate.
    pub normal_impulse_n_s: f32,
    /// Number of active contact-pair/substep observations contributing a
    /// positive normal impulse.
    pub impulse_pair_substeps: usize,
}

/// The solved normal impulse over one contact pair after a step — the raw
/// physical input the T21 contact-damage rules threshold and convert into
/// bounded server edit intents. Read *after* [`PhysicsWorld::step`]; the values
/// are the accumulated solver impulses for the step just run. This adapter never
/// mutates anything in a contact callback: it only reports.
#[derive(Debug, Clone, Copy)]
pub struct ContactImpulse {
    /// The two bodies in contact (adapter ids, not Rapier handles).
    pub bodies: [BodyId; 2],
    /// Authored source-part owners when a participating collider belongs to an
    /// articulated assembly. This follows the live collider binding exactly.
    pub source_parts: [Option<u32>; 2],
    /// Whether each of `bodies` is a dynamic (simulated) body. `false` is a
    /// fixed body — terrain. A dynamic/fixed pair is a body striking terrain; a
    /// dynamic/dynamic pair is debris-on-debris.
    pub dynamic: [bool; 2],
    /// World-space contact point, metres — the mean of the manifold points,
    /// suitable as a damage brush centre.
    pub point_m: [f32; 3],
    /// World-space contact normal (unit), pointing from body 0 toward body 1.
    pub normal: [f32; 3],
    /// Accumulated normal impulse over the pair this step, newton-seconds
    /// (always `>= 0`). A resting body contributes roughly `m · g · dt` every
    /// step; a hard impact spikes well above that.
    pub normal_impulse_n_s: f32,
    /// Solver manifold points do not identify an authoritative occupied voxel.
    /// Source-cell damage therefore remains unattributed at this layer.
    pub source_cells: [Option<[i32; 3]>; 2],
}

/// Versioned geometric source-cell witness captured from one solver contact
/// at its actual kinematic substep. A resolved cell identifies the unique
/// exposed voxel face containing the source-side witness; it does not imply
/// that an application should apply damage to that cell.
pub const CONTACT_WITNESS_VERSION: u16 = 1;
/// Hard cap on retained per-step point witnesses. Overflow is reported rather
/// than growing memory in proportion to hostile contact churn.
pub const MAX_CONTACT_WITNESSES_PER_STEP: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactWitnessUnattributed {
    NonFinite,
    Penetrating,
    NotAxisAlignedFace,
    FacePositionUncertain,
    EdgeOrCorner,
    EmptySourceCell,
    InternalSharedFace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactCellAttribution {
    Resolved {
        cell: [i32; 3],
        material: spall_core::MaterialId,
    },
    Unattributed(ContactWitnessUnattributed),
}

/// One source side of a solver contact point. `pose_token` names the authored
/// target pose for this outer tick; `substep_fraction` and the captured source
/// witness preserve the intermediate physical pose without publishing a live
/// query token mid-step. `normal_impulse_n_s` is Rapier's per-point solver
/// impulse, not a share inferred from a pair aggregate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceContactWitness {
    pub part_id: u32,
    pub volume_id: spall_core::VolumeId,
    pub source_revision: u64,
    pub topology_token: u64,
    pub pose_token: u64,
    pub source_witness_world_m: [f32; 3],
    pub source_outward_normal_world: [f32; 3],
    pub separation_m: f32,
    pub normal_impulse_n_s: Option<f32>,
    pub attribution: ContactCellAttribution,
}

/// A solver point involving an articulated source collider, retained across
/// all solver substeps of one outer step. It is published only after the whole
/// tick; internal Rapier handles and intermediate query tokens never escape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArticulatedContactWitness {
    pub version: u16,
    pub outer_step: u64,
    pub solver_substep: u16,
    pub substep_fraction: f32,
    pub bodies: [BodyId; 2],
    pub world_contact_point_m: [f32; 3],
    pub source_sides: [Option<SourceContactWitness>; 2],
}

struct Entry {
    body: RigidBodyHandle,
    collider: ColliderHandle,
    cell_m: f32,
    representation: Representation,
    density: f32,
    /// Body-local translation applied to the collider shape so grid cell
    /// `(0, 0, 0)` sits at `grid.origin() * cell_m` rather than at body-local
    /// zero. Preserves the authoritative volume transform: the rigid body still
    /// sits at the volume's pose, and the off-origin occupancy is carried as a
    /// collider offset (`ENG-55`).
    collider_offset_m: [f32; 3],
    /// Exact fine-grid mass properties installed on the body, if any. While this
    /// is `Some` the collider carries zero density and these values are the
    /// authoritative mass / COM / inertia; a collider rebuild preserves them and
    /// [`PhysicsWorld::set_mass_properties`] replaces them after a geometry edit.
    mass_properties: Option<BodyMassProperties>,
    /// Set once the body's authoritative volume became empty and it was retired
    /// (`ENG-56`): its Rapier rigid body and collider have been removed, so it
    /// no longer collides, steps, or answers queries. The [`BodyId`] index is
    /// kept so other bodies' ids do not shift; every accessor for it is now a
    /// guarded no-op.
    retired: bool,
    /// Set while the body is **dormant** (T21): a settled body whose whole
    /// interaction region went quiet has had its Rapier rigid body and collider
    /// removed to save step cost, but — unlike [`Self::retired`] — this is
    /// reversible. [`PhysicsWorld::reactivate_body`] rebuilds it in this same
    /// slot from the caller's grid and stored pose before any contact or edit.
    dormant: bool,
    /// Set by [`PhysicsWorld::set_query_only`]: every subsequent
    /// [`PhysicsWorld::rebuild_collider`] call reapplies zeroed
    /// `solver_groups` to the fresh collider it builds, so the setting
    /// survives a rebuild rather than needing the caller to reapply it every
    /// time. See that method's doc for what this actually does.
    query_only: bool,
    /// Contact restitution retained across collider rebuilds and dormancy.
    restitution: f32,
}

#[derive(Clone)]
struct ArticulatedPartEntry {
    part_id: u32,
    parent_id: Option<u32>,
    volume_id: spall_core::VolumeId,
    source_revision: u64,
    grid: OccupancyGrid,
    cell_m: f32,
    local_pose: PartTransform,
    assembly_pose: PartTransform,
    /// Physical occupied-cell corner radius measured from the part-body origin.
    part_surface_radius_m: f32,
    /// Physical occupied-cell corner radius measured from the assembly root.
    root_surface_radius_m: f32,
    body: RigidBodyHandle,
    collider: ColliderHandle,
    primitives: usize,
    component_id: Option<DerivedFractureComponentId>,
}

/// Maximum Euclidean distance from `body_origin` to any corner of an occupied
/// source cell. Unlike Rapier's `Voxels::compute_local_aabb`, this uses the
/// authoritative cell coordinates and exact cell size, so broad-phase storage
/// padding cannot inflate or shrink the admitted physical surface travel.
fn source_surface_radius_m(
    grid: &OccupancyGrid,
    cell_m: f32,
    body_origin_m: [f32; 3],
    part_pose: Option<PartTransform>,
) -> f32 {
    let origin = grid.origin();
    let mut radius_sq = 0.0_f64;
    for [x, y, z] in grid.solid_indices() {
        let global = [
            origin.x + i64::from(x),
            origin.y + i64::from(y),
            origin.z + i64::from(z),
        ];
        let lo = global.map(|coord| coord as f64 * f64::from(cell_m));
        let hi = global.map(|coord| (coord + 1) as f64 * f64::from(cell_m));
        for mask in 0..8 {
            let corner = [
                if mask & 1 == 0 { lo[0] } else { hi[0] },
                if mask & 2 == 0 { lo[1] } else { hi[1] },
                if mask & 4 == 0 { lo[2] } else { hi[2] },
            ];
            let local = if let Some(pose) = part_pose {
                let rotated = rotate(
                    pose.rotation_xyzw,
                    [corner[0] as f32, corner[1] as f32, corner[2] as f32],
                );
                [
                    f64::from(pose.translation_m[0] + rotated[0] - body_origin_m[0]),
                    f64::from(pose.translation_m[1] + rotated[1] - body_origin_m[1]),
                    f64::from(pose.translation_m[2] + rotated[2] - body_origin_m[2]),
                ]
            } else {
                [
                    corner[0] - f64::from(body_origin_m[0]),
                    corner[1] - f64::from(body_origin_m[1]),
                    corner[2] - f64::from(body_origin_m[2]),
                ]
            };
            let squared = local.iter().map(|value| value * value).sum::<f64>();
            radius_sq = radius_sq.max(squared);
        }
    }
    radius_sq.sqrt() as f32
}

/// Maximum logical owners in one cold reconstruction batch. Dense/source
/// cell and primitive limits apply cumulatively across the entire batch.
pub const MAX_CORPSE_RESTORE_BATCH_OWNERS: usize = 1024;
pub const MAX_CORPSE_RESTORE_BATCH_DENSE_CELLS: u64 = 16 * 1024 * 1024;
pub const MAX_CORPSE_RESTORE_BATCH_SOLID_CELLS: u64 = 4_000_000;
pub const MAX_CORPSE_RESTORE_BATCH_PRIMITIVES: usize = 4_000_000;

/// One lifecycle operation prepared and published atomically with its batch.
/// Tags are caller-owned correlation IDs and must be unique within a batch.
pub enum ArticulatedBatchOperation {
    /// Create a live kinematic articulated owner from a complete source snapshot.
    AliveBirth {
        operation_tag: u64,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
    },
    /// Convert one live kinematic articulated owner into its dynamic corpse.
    CheckedDeathTransfer {
        operation_tag: u64,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        expected_pose_token: u64,
    },
    /// Reconstruct one exact portable owner from canonical source and masks.
    ColdRestore {
        operation_tag: u64,
        restore: crate::articulated::DynamicCorpseRestore,
        representation: Representation,
    },
}

/// Result for one operation in a committed articulated batch.
#[derive(Debug)]
pub struct ArticulatedBatchOutcome {
    pub operation_tag: u64,
    pub owner_id: ArticulatedBodyId,
    pub transfer: Option<CorpseTransferOutcome>,
}

/// Owned immutable solver preparation. It is not serializable and cannot be
/// committed to another world, reused after a step, or published twice.
pub struct PreparedArticulatedBatch {
    world_identity: std::sync::Arc<()>,
    expected_step: u64,
    expected_entries: usize,
    changes: Vec<PreparedArticulatedChange>,
}

/// One-shot publication capability produced only after every batch member
/// and cross-owner reservation passed validation.
pub struct ValidatedArticulatedBatch {
    prepared: PreparedArticulatedBatch,
}

/// Compatibility name for callers using the original restore-only batch.
pub type PreparedDynamicCorpseBatch = PreparedArticulatedBatch;

enum PreparedArticulatedChange {
    AliveBirth {
        operation_tag: u64,
        birth: PreparedArticulatedBirth,
    },
    DynamicCorpseBirth {
        operation_tag: u64,
        birth: PreparedArticulatedBirth,
    },
    ColdRestore {
        operation_tag: u64,
        owner: PreparedCorpseRestoreOwner,
    },
    CheckedDeathTransfer {
        operation_tag: u64,
        transfer: PreparedCorpseTransfer,
    },
}

struct PreparedArticulatedBirth {
    translation_m: [f32; 3],
    rotation_xyzw: [f32; 4],
    topology_token: u64,
    parts: Vec<SourcePart>,
    flattened: Vec<PartTransform>,
    collider_poses: Vec<([f32; 3], [f32; 4])>,
    builds: Vec<crate::collider::ColliderBuild>,
    source_volume_ids: std::collections::BTreeSet<spall_core::VolumeId>,
    mass: Option<BodyMassProperties>,
    manifest: Option<HashMap<MaterialId, u64>>,
    kinematic_part_bodies: bool,
    representation: Representation,
    primitive_count: usize,
}

struct PreparedCorpseTransferPartSnapshot {
    body: RigidBodyHandle,
    collider: ColliderHandle,
    position: Pose,
    linvel: Vector,
    angvel: Vector,
    part_id: u32,
    volume_id: spall_core::VolumeId,
    source_revision: u64,
    cell_m: f32,
    grid_origin: GlobalCell,
    grid_dims: [u32; 3],
    source_solid_count: u64,
    source_cells: Vec<(u32, u32, u32, MaterialId)>,
    local_pose: PartTransform,
    assembly_pose: PartTransform,
}

struct PreparedCorpseTransfer {
    id: ArticulatedBodyId,
    expected_topology_token: u64,
    expected_pose_token: u64,
    source_generation: u64,
    source_volume_ids: std::collections::BTreeSet<spall_core::VolumeId>,
    parts: Vec<PreparedCorpseTransferPartSnapshot>,
    entry_body: RigidBodyHandle,
    entry_position: Pose,
    entry_linvel: Vector,
    entry_angvel: Vector,
    entry_collider: ColliderHandle,
    props: BodyMassProperties,
    manifest: HashMap<MaterialId, u64>,
    prepared: Vec<(ColliderHandle, RigidBodyHandle, Pose)>,
    replacement_colliders: Vec<Collider>,
    primitive_count: usize,
    outcome: CorpseTransferOutcome,
}

fn add_batch_grid_budget(
    grid: &OccupancyGrid,
    dense: &mut u64,
    solids: &mut u64,
    materials: &mut std::collections::BTreeSet<MaterialId>,
) -> Result<(), crate::articulated::ArticulatedError> {
    let dims = grid.dims();
    let cells = u64::from(dims[0])
        .checked_mul(u64::from(dims[1]))
        .and_then(|value| value.checked_mul(u64::from(dims[2])))
        .ok_or(crate::articulated::ArticulatedError::WorkBudget)?;
    *dense = dense
        .checked_add(cells)
        .ok_or(crate::articulated::ArticulatedError::WorkBudget)?;
    *solids = solids
        .checked_add(grid.solid_count())
        .ok_or(crate::articulated::ArticulatedError::WorkBudget)?;
    if *dense > MAX_CORPSE_RESTORE_BATCH_DENSE_CELLS
        || *solids > MAX_CORPSE_RESTORE_BATCH_SOLID_CELLS
    {
        return Err(crate::articulated::ArticulatedError::WorkBudget);
    }
    grid.for_each_solid(|_, _, _, material| {
        materials.insert(material);
    });
    Ok(())
}

struct PreparedCorpsePartIdentity {
    part_id: u32,
    volume_id: spall_core::VolumeId,
    source_revision: u64,
    cell_m: f32,
}

struct PreparedCorpseComponentState {
    component_id: Option<DerivedFractureComponentId>,
    linear_velocity_m_s: [f32; 3],
    angular_velocity_rad_s: [f32; 3],
}

type PreparedCorpsePart = (
    PreparedCorpsePartIdentity,
    OccupancyGrid,
    PartTransform,
    usize,
    Collider,
);
type PreparedCorpseComponent = (
    PreparedCorpseComponentState,
    BodyMassProperties,
    [f32; 4],
    [f32; 3],
    Vec<PreparedCorpsePart>,
);

struct PreparedCorpseRestoreOwner {
    owner_key: u128,
    source_generation: u64,
    topology_revision: u64,
    pose_epoch: u64,
    next_component_serial: u64,
    source_volumes: std::collections::BTreeSet<spall_core::VolumeId>,
    manifest: HashMap<MaterialId, u64>,
    representation: Representation,
    prepared: Vec<PreparedCorpseComponent>,
    primitive_count: usize,
    derived_dense: u64,
}

struct ArticulatedEntry {
    topology_token: u64,
    pose_token: u64,
    root_pose: PartTransform,
    pending_root_pose_token: Option<u64>,
    pending_pose: Option<PendingArticulatedPose>,
    kinematic_part_bodies: bool,
    dynamic_corpse: bool,
    source_generation: u64,
    fracture_owner_key: Option<u128>,
    next_component_serial: u64,
    density_manifest: Option<HashMap<MaterialId, u64>>,
    source_volume_ids: std::collections::BTreeSet<spall_core::VolumeId>,
    parts: Vec<ArticulatedPartEntry>,
}

fn density_manifest(
    parts: &[SourcePart],
    density: &(impl Fn(MaterialId) -> f64 + ?Sized),
) -> Result<HashMap<MaterialId, u64>, crate::articulated::ArticulatedError> {
    let mut manifest = HashMap::new();
    for part in parts {
        part.grid.for_each_solid(|_, _, _, material| {
            manifest
                .entry(material)
                .or_insert_with(|| density(material).to_bits());
        });
    }
    if manifest.values().any(|bits| {
        let value = f64::from_bits(*bits);
        !value.is_finite() || value <= 0.0
    }) {
        return Err(crate::articulated::ArticulatedError::InvalidDensity);
    }
    Ok(manifest)
}

fn manifest_density(manifest: &HashMap<MaterialId, u64>, material: MaterialId) -> f64 {
    manifest
        .get(&material)
        .map_or(f64::NAN, |bits| f64::from_bits(*bits))
}

/// Measurements from an atomic articulated-rig to corpse-owner transfer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CorpseTransferOutcome {
    pub preparation_time: Duration,
    pub mass_kg: f64,
    pub com_world_m: [f64; 3],
    pub linear_momentum_kg_m_s: [f64; 3],
    pub angular_momentum_about_com_kg_m2_s: [f64; 3],
    /// Resultant rigid-body twist installed on the corpse owner.
    pub linear_velocity_m_s: [f64; 3],
    pub angular_velocity_rad_s: [f64; 3],
}

/// Measurements from one all-or-nothing physical corpse component split.
#[derive(Debug, Clone, PartialEq)]
pub struct FractureCommitOutcome {
    pub elapsed: Duration,
    pub source_cells: usize,
    pub components: Vec<(DerivedFractureComponentId, f64)>,
}

/// Bounded live diagnostics for one stable physical fracture component.
/// Rapier handles remain private to the adapter.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CorpseEditOutcome {
    Rebuilt {
        elapsed: Duration,
        removed_mass_kg: f64,
    },
    PartRetired {
        elapsed: Duration,
        removed_mass_kg: f64,
    },
    AggregateRetired {
        elapsed: Duration,
        removed_mass_kg: f64,
    },
}

fn fracture_snapshots_equal(a: &FractureSnapshot, b: &FractureSnapshot) -> bool {
    if a.token != b.token || a.parts.len() != b.parts.len() {
        return false;
    }
    a.parts.iter().zip(&b.parts).all(|(left, right)| {
        if left.source.part_id != right.source.part_id
            || left.source.parent_id != right.source.parent_id
            || left.source.volume_id != right.source.volume_id
            || left.source.source_revision != right.source.source_revision
            || left.source.cell_m.to_bits() != right.source.cell_m.to_bits()
            || left.source.grid.origin() != right.source.grid.origin()
            || left.source.grid.dims() != right.source.grid.dims()
            || left.root_local_pose.translation_m.map(f32::to_bits)
                != right.root_local_pose.translation_m.map(f32::to_bits)
            || left.root_local_pose.rotation_xyzw.map(f32::to_bits)
                != right.root_local_pose.rotation_xyzw.map(f32::to_bits)
        {
            return false;
        }
        let mut left_cells = Vec::new();
        let mut right_cells = Vec::new();
        let left_origin = left.source.grid.origin();
        let right_origin = right.source.grid.origin();
        left.source.grid.for_each_solid(|x, y, z, material| {
            left_cells.push((
                left_origin.x + i64::from(x),
                left_origin.y + i64::from(y),
                left_origin.z + i64::from(z),
                material,
            ));
        });
        right.source.grid.for_each_solid(|x, y, z, material| {
            right_cells.push((
                right_origin.x + i64::from(x),
                right_origin.y + i64::from(y),
                right_origin.z + i64::from(z),
                material,
            ));
        });
        left_cells == right_cells
    })
}

fn solve_symmetric3(a: [[f64; 3]; 3], b: [f64; 3]) -> Option<[f64; 3]> {
    let det = a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]);
    if !det.is_finite() || det <= 0.0 {
        return None;
    }
    let inv = [
        [
            a[1][1] * a[2][2] - a[1][2] * a[2][1],
            a[0][2] * a[2][1] - a[0][1] * a[2][2],
            a[0][1] * a[1][2] - a[0][2] * a[1][1],
        ],
        [
            a[1][2] * a[2][0] - a[1][0] * a[2][2],
            a[0][0] * a[2][2] - a[0][2] * a[2][0],
            a[0][2] * a[1][0] - a[0][0] * a[1][2],
        ],
        [
            a[1][0] * a[2][1] - a[1][1] * a[2][0],
            a[0][1] * a[2][0] - a[0][0] * a[2][1],
            a[0][0] * a[1][1] - a[0][1] * a[1][0],
        ],
    ];
    let x = inv.map(|row| (row[0] * b[0] + row[1] * b[1] + row[2] * b[2]) / det);
    x.iter().all(|v| v.is_finite()).then_some(x)
}

fn cross64(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

struct PendingArticulatedPose {
    pose_token: u64,
    parts: Vec<(u32, PartTransform, PartTransform)>,
}

/// `solver_groups` for [`PhysicsWorld::set_query_only`]: membership *and*
/// filter both zeroed, so Rapier's interaction test — `(a.memberships &
/// b.filter) != 0` (`And` mode additionally requires the symmetric term too)
/// — is false against literally any other collider's groups, regardless of
/// their own configuration or the test mode in effect. `collision_groups`
/// (a separate field, left at its default) is untouched, so ordinary
/// queries still see this collider normally.
fn query_only_solver_groups() -> InteractionGroups {
    InteractionGroups::new(Group::NONE, Group::NONE, InteractionTestMode::And)
}

/// The body-local offset that places a tight occupancy grid's cell `(0, 0, 0)`
/// at its global-cell [`OccupancyGrid::origin`] scaled to metres.
fn grid_origin_offset_m(grid: &OccupancyGrid, cell_m: f32) -> [f32; 3] {
    let o = grid.origin();
    [
        o.x as f32 * cell_m,
        o.y as f32 * cell_m,
        o.z as f32 * cell_m,
    ]
}

/// Resolves only a finite, non-penetrating witness lying well inside one
/// exterior axis-aligned voxel face. This deliberately leaves diagonal,
/// shared, edge, corner, and penetrating contacts unattributed; no rounding or
/// nearest-cell fallback is used.
fn resolve_source_cell_witness(
    grid: &OccupancyGrid,
    cell_m: f32,
    point: Vector,
    outward_normal: Vector,
    separation_m: f32,
) -> ContactCellAttribution {
    use ContactWitnessUnattributed as Why;
    if !cell_m.is_finite()
        || cell_m <= 0.0
        || ![point.x, point.y, point.z]
            .iter()
            .all(|value| value.is_finite())
        || ![outward_normal.x, outward_normal.y, outward_normal.z]
            .iter()
            .all(|value| value.is_finite())
        || !separation_m.is_finite()
    {
        return ContactCellAttribution::Unattributed(Why::NonFinite);
    }
    // Rapier contacts with negative separation represent overlap. Even if a
    // nearby face can be guessed, that witness does not uniquely establish
    // which exposed source boundary generated the solver constraint.
    if separation_m < -1.0e-5 {
        return ContactCellAttribution::Unattributed(Why::Penetrating);
    }
    let normal_len = outward_normal.length();
    if !normal_len.is_finite() || normal_len <= f32::EPSILON {
        return ContactCellAttribution::Unattributed(Why::NonFinite);
    }
    let n = outward_normal / normal_len;
    let Some(axis) = (0..3).max_by(|&a, &b| n[a].abs().total_cmp(&n[b].abs())) else {
        return ContactCellAttribution::Unattributed(Why::NonFinite);
    };
    if n[axis].abs() < 0.9999 {
        return ContactCellAttribution::Unattributed(Why::NotAxisAlignedFace);
    }

    // Explicit geometric tolerance: 10 micrometres or 1e-4 of a cell,
    // whichever is larger. Witnesses closer than this to an edge are ambiguous.
    let eps_m = (cell_m * 1.0e-4).max(1.0e-5);
    let eps_cell = eps_m / cell_m;
    let q = [point.x / cell_m, point.y / cell_m, point.z / cell_m];
    let boundary = q[axis].round();
    if (q[axis] - boundary).abs() > eps_cell {
        return ContactCellAttribution::Unattributed(Why::FacePositionUncertain);
    }
    let mut cell = [0_i32; 3];
    for tangent in 0..3 {
        if tangent == axis {
            continue;
        }
        let floor = q[tangent].floor();
        let fraction = q[tangent] - floor;
        if fraction <= eps_cell || 1.0 - fraction <= eps_cell {
            return ContactCellAttribution::Unattributed(Why::EdgeOrCorner);
        }
        if floor < i32::MIN as f32 || floor > i32::MAX as f32 {
            return ContactCellAttribution::Unattributed(Why::NonFinite);
        }
        cell[tangent] = floor as i32;
    }
    if boundary < i32::MIN as f32 || boundary > i32::MAX as f32 {
        return ContactCellAttribution::Unattributed(Why::NonFinite);
    }
    let boundary = boundary as i32;
    let outward_sign = if n[axis] > 0.0 { 1 } else { -1 };
    cell[axis] = if outward_sign > 0 {
        boundary - 1
    } else {
        boundary
    };
    if cell.iter().any(|&coord| coord < 0) {
        return ContactCellAttribution::Unattributed(Why::EmptySourceCell);
    }
    let local = cell.map(|coord| coord as u32);
    let Some(material) = grid.material(local[0], local[1], local[2]) else {
        return ContactCellAttribution::Unattributed(Why::EmptySourceCell);
    };
    let mut neighbor = cell;
    neighbor[axis] += outward_sign;
    if neighbor.iter().all(|&coord| coord >= 0)
        && grid.is_solid(neighbor[0] as u32, neighbor[1] as u32, neighbor[2] as u32)
    {
        return ContactCellAttribution::Unattributed(Why::InternalSharedFace);
    }
    let origin = grid.origin();
    let Some(cell_x) = origin.x.checked_add(i64::from(cell[0])) else {
        return ContactCellAttribution::Unattributed(Why::NonFinite);
    };
    let Some(cell_y) = origin.y.checked_add(i64::from(cell[1])) else {
        return ContactCellAttribution::Unattributed(Why::NonFinite);
    };
    let Some(cell_z) = origin.z.checked_add(i64::from(cell[2])) else {
        return ContactCellAttribution::Unattributed(Why::NonFinite);
    };
    let (Ok(cell_x), Ok(cell_y), Ok(cell_z)) = (
        i32::try_from(cell_x),
        i32::try_from(cell_y),
        i32::try_from(cell_z),
    ) else {
        return ContactCellAttribution::Unattributed(Why::NonFinite);
    };
    ContactCellAttribution::Resolved {
        cell: [cell_x, cell_y, cell_z],
        material,
    }
}

fn articulated_grid_range_valid(grid: &OccupancyGrid, cell_m: f32) -> bool {
    use crate::articulated::MAX_ARTICULATED_GEOMETRY_EXTENT_M;
    if !cell_m.is_finite() || cell_m <= 0.0 {
        return false;
    }
    let origin = grid.origin();
    let dims = grid.dims();
    let origins = [origin.x, origin.y, origin.z];
    (0..3).all(|axis| {
        let end = origins[axis].checked_add(i64::from(dims[axis]) - 1);
        let Some(end) = end else {
            return false;
        };
        if origins[axis] < i32::MIN as i64 || end > i32::MAX as i64 {
            return false;
        }
        let start_m = origins[axis] as f64 * f64::from(cell_m);
        let end_m = (end as f64 + 1.0) * f64::from(cell_m);
        let extent_m = f64::from(dims[axis]) * f64::from(cell_m);
        start_m.is_finite()
            && end_m.is_finite()
            && extent_m.is_finite()
            && start_m.abs() <= MAX_ARTICULATED_GEOMETRY_EXTENT_M
            && end_m.abs() <= MAX_ARTICULATED_GEOMETRY_EXTENT_M
            && extent_m <= MAX_ARTICULATED_GEOMETRY_EXTENT_M
    })
}

fn articulated_translation_in_range(translation_m: [f32; 3]) -> bool {
    const LIMIT_M: f32 = 1_000.0;
    translation_m
        .iter()
        .all(|value| value.is_finite() && value.abs() <= LIMIT_M)
}

fn flatten_source_parts(
    parts: &[SourcePart],
) -> Result<Vec<PartTransform>, crate::articulated::ArticulatedError> {
    use crate::articulated::ArticulatedError;
    let mut flattened: HashMap<u32, PartTransform> = HashMap::with_capacity(parts.len());
    let mut locals: Vec<_> = parts
        .iter()
        .map(|part| (part.part_id, part.parent_id, part.local_transform))
        .collect();
    let mut progressed = true;
    while !locals.is_empty() && progressed {
        progressed = false;
        let mut index = 0;
        while index < locals.len() {
            let (part_id, parent_id, local) = locals[index];
            let world = if let Some(parent_id) = parent_id {
                let Some(parent) = flattened.get(&parent_id).copied() else {
                    index += 1;
                    continue;
                };
                compose(parent, local)
            } else {
                local
            };
            if !world.is_valid() {
                return Err(ArticulatedError::InvalidTransform);
            }
            flattened.insert(part_id, world);
            locals.remove(index);
            progressed = true;
        }
    }
    if !locals.is_empty() {
        return Err(ArticulatedError::Hierarchy);
    }
    Ok(parts.iter().map(|part| flattened[&part.part_id]).collect())
}

fn flatten_local_poses(
    parts: &[ArticulatedPartEntry],
    locals: &[(u32, PartTransform)],
) -> Result<Vec<PartTransform>, crate::articulated::ArticulatedError> {
    use crate::articulated::ArticulatedError;
    let mut by_id = HashMap::with_capacity(locals.len());
    for (part_id, pose) in locals {
        let pose = pose
            .normalized()
            .ok_or(ArticulatedError::InvalidTransform)?;
        if by_id.insert(*part_id, pose).is_some() {
            return Err(ArticulatedError::StalePose);
        }
    }
    if by_id.len() != parts.len() || parts.iter().any(|part| !by_id.contains_key(&part.part_id)) {
        return Err(ArticulatedError::StalePose);
    }
    let mut flattened: HashMap<u32, PartTransform> = HashMap::with_capacity(parts.len());
    let mut pending = parts.iter().collect::<Vec<_>>();
    let mut progressed = true;
    while !pending.is_empty() && progressed {
        progressed = false;
        let mut index = 0;
        while index < pending.len() {
            let part = pending[index];
            let local = by_id[&part.part_id];
            let world = if let Some(parent_id) = part.parent_id {
                let Some(parent) = flattened.get(&parent_id).copied() else {
                    index += 1;
                    continue;
                };
                compose(parent, local)
            } else {
                local
            };
            if !world.is_valid() {
                return Err(ArticulatedError::InvalidTransform);
            }
            flattened.insert(part.part_id, world);
            pending.remove(index);
            progressed = true;
        }
    }
    if !pending.is_empty() {
        return Err(ArticulatedError::Hierarchy);
    }
    Ok(parts.iter().map(|part| flattened[&part.part_id]).collect())
}

/// Builds the Rapier mass properties for `props` in the body-local frame the
/// collider is built in: the grid-local centre of mass that
/// `analytic_mass_properties` produces (`ENG-41`) is shifted by the body's
/// grid-origin collider offset (`ENG-55`) so the installed centre of mass and
/// the collision geometry share one origin. The inertia tensor is taken about
/// the centre of mass, so it is translation-invariant and passes through
/// unchanged.
fn rapier_mass_properties(
    props: BodyMassProperties,
    collider_offset_m: [f32; 3],
) -> MassProperties {
    MassProperties::with_inertia_matrix(
        Vector::new(
            props.local_com_m[0] + collider_offset_m[0],
            props.local_com_m[1] + collider_offset_m[1],
            props.local_com_m[2] + collider_offset_m[2],
        ),
        props.mass_kg.max(f32::MIN_POSITIVE),
        Matrix::from_cols_array_2d(&props.inertia_com_kg_m2),
    )
}

fn validate_body_mass_properties(
    props: BodyMassProperties,
) -> Result<(), crate::articulated::ArticulatedError> {
    use crate::articulated::ArticulatedError;
    let finite = props.mass_kg.is_finite()
        && props.local_com_m.iter().all(|value| value.is_finite())
        && props
            .inertia_com_kg_m2
            .iter()
            .flatten()
            .all(|value| value.is_finite());
    if !finite || props.mass_kg <= 0.0 {
        return Err(ArticulatedError::InvalidMassProperties);
    }
    let m = props.inertia_com_kg_m2;
    let scale = m.iter().flatten().map(|v| v.abs()).fold(0.0_f32, f32::max);
    let eps = (scale * 1.0e-6).max(f32::MIN_POSITIVE);
    if (m[0][1] - m[1][0]).abs() > eps
        || (m[0][2] - m[2][0]).abs() > eps
        || (m[1][2] - m[2][1]).abs() > eps
    {
        return Err(ArticulatedError::InvalidMassProperties);
    }
    let minor2 = m[0][0] * m[1][1] - m[0][1] * m[1][0];
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if m[0][0] <= eps || minor2 <= eps * eps || det <= eps * eps * eps {
        return Err(ArticulatedError::InvalidMassProperties);
    }
    Ok(())
}

/// Preserve the rigid velocity field when an open-system edit changes COM.
/// Rapier's linear velocity is the world velocity of COM, while the body's
/// origin and angular velocity remain unchanged.
fn corpse_edit_velocity(
    body: &RigidBody,
    root_pose: Pose,
    new_mass: BodyMassProperties,
) -> Result<(Vector, Vector), crate::articulated::ArticulatedError> {
    let old_com = body.center_of_mass();
    let new_com = root_pose.transform_point(Vector::new(
        new_mass.local_com_m[0],
        new_mass.local_com_m[1],
        new_mass.local_com_m[2],
    ));
    let angular = body.angvel();
    let linear = body.linvel() + angular.cross(new_com - old_com);
    if ![
        linear.x, linear.y, linear.z, angular.x, angular.y, angular.z,
    ]
    .iter()
    .all(|value| value.is_finite())
    {
        return Err(crate::articulated::ArticulatedError::InvalidTransform);
    }
    Ok((linear, angular))
}

/// The principal (eigenvalue) inertia triple of a symmetric tensor about the
/// centre of mass — the diagonal in the frame where the tensor is diagonal.
fn principal_inertia(inertia_com_kg_m2: [[f32; 3]; 3]) -> [f32; 3] {
    let mp = MassProperties::with_inertia_matrix(
        Vector::ZERO,
        1.0,
        Matrix::from_cols_array_2d(&inertia_com_kg_m2),
    );
    mp.principal_inertia().into()
}

/// A fixed-step rigid-body world over voxel colliders.
pub struct PhysicsWorld {
    preparation_identity: std::sync::Arc<()>,
    id_namespace: u32,
    gravity: Vector,
    params: IntegrationParameters,
    solver_substeps: usize,
    pipeline: PhysicsPipeline,
    islands: IslandManager,
    broad_phase: DefaultBroadPhase,
    narrow_phase: NarrowPhase,
    bodies: RigidBodySet,
    colliders: ColliderSet,
    impulse_joints: ImpulseJointSet,
    multibody_joints: MultibodyJointSet,
    ccd_solver: CCDSolver,
    entries: Vec<Entry>,
    articulated: HashMap<BodyId, ArticulatedEntry>,
    /// Logical fracture keys are never reusable during this world lifetime,
    /// including after their owner is retired.
    fracture_owner_keys: HashMap<u128, BodyId>,
    step_count: u64,
    articulated_contact_witnesses: Vec<ArticulatedContactWitness>,
    contact_witness_overflow: u64,
    /// Collider handles added/rebuilt/removed since the last [`Self::step`]
    /// or [`Self::sync_queries`] — see [`Self::sync_queries`]'s doc for why
    /// this exists alongside `step`, not instead of it.
    pending_modified: Vec<ColliderHandle>,
    pending_removed: Vec<ColliderHandle>,
}

/// How close (metres) a dynamic body must be to the character's capsule to be
/// carried along by a push even when the tick reported no contact.
const CARRY_MARGIN_M: f32 = 0.05;
/// Slowest horizontal character speed (m/s) that still carries bodies.
const CARRY_MIN_SPEED_M_S: f32 = 0.1;
/// Largest admitted kinematic surface displacement within one configured
/// solver substep for the bounded articulated-contact experiment.
const MAX_KINEMATIC_SURFACE_TRAVEL_PER_SUBSTEP_M: f32 = 0.025;

/// A prebuilt set of bodies whose colliders are left out of character queries (see
/// [`PhysicsWorld::exclusion_for`]). Colliders carry their owning body's id, so the set stays
/// valid while a body's collider is rebuilt (which gives it a new Rapier handle); only adding or
/// removing bodies changes it.
#[derive(Debug, Clone, Default)]
pub struct ColliderExclusion {
    bodies: std::collections::HashSet<BodyId>,
}

impl ColliderExclusion {
    pub fn len(&self) -> usize {
        self.bodies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    /// `user_data` is the owner's id plus one, so a collider that was never tagged (zero) is never
    /// mistaken for the body with id 0.
    fn excludes(&self, collider: &Collider) -> bool {
        collider.user_data != 0
            && self
                .bodies
                .contains(&BodyId((collider.user_data - 1) as u64))
    }
}

struct CombinedExclusion<'a> {
    base: &'a ColliderExclusion,
    extra: &'a ColliderExclusion,
}

impl CombinedExclusion<'_> {
    fn excludes(&self, collider: &Collider) -> bool {
        self.base.excludes(collider) || self.extra.excludes(collider)
    }

    fn is_empty(&self) -> bool {
        self.base.is_empty() && self.extra.is_empty()
    }
}

impl PhysicsWorld {
    /// Creates an empty world.
    pub fn new(cfg: PhysicsConfig) -> Self {
        Self::new_in_namespace(cfg, 0)
    }

    /// Creates an empty region-local world whose body IDs are disjoint from
    /// worlds using another namespace. Namespace zero preserves `new` IDs.
    pub fn new_in_namespace(cfg: PhysicsConfig, id_namespace: u32) -> Self {
        assert!(
            (1..=64).contains(&cfg.solver_substeps),
            "solver_substeps must be in 1..=64"
        );
        let mut params = IntegrationParameters {
            dt: cfg.dt_s,
            ..Default::default()
        };
        params.num_solver_iterations = cfg.solver_iterations.max(1);
        if cfg.disable_ccd {
            params.max_ccd_substeps = 0;
        }
        Self {
            preparation_identity: std::sync::Arc::new(()),
            id_namespace,
            gravity: Vector::new(
                cfg.gravity_m_s2[0],
                cfg.gravity_m_s2[1],
                cfg.gravity_m_s2[2],
            ),
            params,
            solver_substeps: cfg.solver_substeps,
            pipeline: PhysicsPipeline::new(),
            islands: IslandManager::new(),
            broad_phase: DefaultBroadPhase::new(),
            narrow_phase: NarrowPhase::new(),
            bodies: RigidBodySet::new(),
            colliders: ColliderSet::new(),
            impulse_joints: ImpulseJointSet::new(),
            multibody_joints: MultibodyJointSet::new(),
            ccd_solver: CCDSolver::new(),
            entries: Vec::new(),
            articulated: HashMap::new(),
            fracture_owner_keys: HashMap::new(),
            step_count: 0,
            articulated_contact_witnesses: Vec::new(),
            contact_witness_overflow: 0,
            pending_modified: Vec::new(),
            pending_removed: Vec::new(),
        }
    }

    /// Adds one position-controlled assembly root and one exact collider per
    /// source part. Parent transforms are flattened into assembly-local poses;
    /// source grids remain separate even when their integer coordinates overlap.
    pub fn add_articulated_body(
        &mut self,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
    ) -> Result<ArticulatedBodyId, crate::articulated::ArticulatedError> {
        self.add_articulated_body_mode(
            translation_m,
            rotation_xyzw,
            topology_token,
            parts,
            representation,
            false,
            None,
        )
    }

    /// Experimental feasibility path: each exact source-part collider has an
    /// independent position-controlled body, while all parts retain one
    /// conceptual owner and source identity. No joints are inferred from the
    /// authored part hierarchy. This remains a bounded movement/contact rig,
    /// not a dynamic ragdoll or corpse representation.
    pub fn add_articulated_kinematic_parts(
        &mut self,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
    ) -> Result<ArticulatedBodyId, crate::articulated::ArticulatedError> {
        let birth = self.prepare_articulated_birth(
            translation_m,
            rotation_xyzw,
            topology_token,
            parts,
            representation,
            true,
            None,
        )?;
        let prepared = PreparedArticulatedBatch {
            world_identity: self.preparation_identity.clone(),
            expected_step: self.step_count,
            expected_entries: self.entries.len(),
            changes: vec![PreparedArticulatedChange::AliveBirth {
                operation_tag: 0,
                birth,
            }],
        };
        let mut outcomes = self.commit_articulated_batch(prepared)?;
        Ok(outcomes
            .pop()
            .expect("singleton kinematic birth yields one result")
            .owner_id)
    }

    /// Bounded corpse candidate: exact frozen per-source-part colliders under
    /// one dynamic solver body. PART hierarchy defines pose only, never bonds.
    pub fn add_dynamic_corpse_aggregate(
        &mut self,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<ArticulatedBodyId, crate::articulated::ArticulatedError> {
        let mut materials = std::collections::BTreeSet::new();
        for part in &parts {
            part.grid.for_each_solid(|_, _, _, material| {
                materials.insert(material);
            });
        }
        let mut manifest = HashMap::new();
        for material in materials {
            let rho = density(material);
            if !rho.is_finite() || rho <= 0.0 {
                return Err(crate::articulated::ArticulatedError::InvalidDensity);
            }
            manifest.insert(material, rho.to_bits());
        }
        let stable_density = |material| manifest_density(&manifest, material);
        let birth = self.prepare_articulated_birth(
            translation_m,
            rotation_xyzw,
            topology_token,
            parts,
            representation,
            false,
            Some(&stable_density),
        )?;
        let prepared = PreparedArticulatedBatch {
            world_identity: self.preparation_identity.clone(),
            expected_step: self.step_count,
            expected_entries: self.entries.len(),
            changes: vec![PreparedArticulatedChange::DynamicCorpseBirth {
                operation_tag: 0,
                birth,
            }],
        };
        let mut outcomes = self.commit_articulated_batch(prepared)?;
        Ok(outcomes
            .pop()
            .expect("singleton birth yields one result")
            .owner_id)
    }

    /// Reconstructs one exact owner using the same preparation/publication
    /// boundary as a multi-owner cold restore.
    pub fn restore_dynamic_corpse(
        &mut self,
        restore: crate::articulated::DynamicCorpseRestore,
        representation: Representation,
        density: impl Fn(MaterialId) -> f64,
    ) -> Result<ArticulatedBodyId, crate::articulated::ArticulatedError> {
        let prepared =
            self.prepare_dynamic_corpse_restore_batch(vec![restore], representation, density)?;
        let mut owners = self.commit_dynamic_corpse_restore_batch(prepared)?;
        Ok(owners
            .pop()
            .expect("singleton restore prepares exactly one owner"))
    }

    /// Prepares all sources, masks, shapes, masses and identities without
    /// inserting any solver object. The returned value is bound to this world.
    /// Input order is the order of the returned owner handles at commit.
    pub fn prepare_dynamic_corpse_restore_batch(
        &self,
        restores: Vec<crate::articulated::DynamicCorpseRestore>,
        representation: Representation,
        density: impl Fn(MaterialId) -> f64,
    ) -> Result<PreparedDynamicCorpseBatch, crate::articulated::ArticulatedError> {
        self.prepare_articulated_batch(
            restores
                .into_iter()
                .enumerate()
                .map(|(index, restore)| ArticulatedBatchOperation::ColdRestore {
                    operation_tag: index as u64,
                    restore,
                    representation,
                })
                .collect(),
            density,
        )
    }

    /// Prepares all supported articulated lifecycle operations without
    /// inserting solver objects. Density values are sampled once per material
    /// for the complete batch. Operation tags must be unique.
    pub fn prepare_articulated_batch(
        &self,
        operations: Vec<ArticulatedBatchOperation>,
        density: impl Fn(MaterialId) -> f64,
    ) -> Result<PreparedArticulatedBatch, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        if operations.is_empty() || operations.len() > MAX_CORPSE_RESTORE_BATCH_OWNERS {
            return Err(ArticulatedError::WorkBudget);
        }
        let mut dense = 0_u64;
        let mut solids = 0_u64;
        let mut mask_cells = 0_usize;
        let mut materials = std::collections::BTreeSet::new();
        let mut tags = std::collections::BTreeSet::new();
        for operation in &operations {
            let tag = match operation {
                ArticulatedBatchOperation::AliveBirth { operation_tag, .. }
                | ArticulatedBatchOperation::CheckedDeathTransfer { operation_tag, .. }
                | ArticulatedBatchOperation::ColdRestore { operation_tag, .. } => *operation_tag,
            };
            if !tags.insert(tag) {
                return Err(ArticulatedError::StalePreparedBatch);
            }
            let parts = match operation {
                ArticulatedBatchOperation::AliveBirth { parts, .. } => parts.as_slice(),
                ArticulatedBatchOperation::ColdRestore { restore, .. } => {
                    if restore.components.len() > crate::fracture::MAX_FRACTURE_COMPONENTS {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    for component in &restore.components {
                        mask_cells = mask_cells
                            .checked_add(component.cells.len())
                            .ok_or(ArticulatedError::WorkBudget)?;
                    }
                    if mask_cells as u64 > MAX_CORPSE_RESTORE_BATCH_SOLID_CELLS {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    restore.source_parts.as_slice()
                }
                ArticulatedBatchOperation::CheckedDeathTransfer { id, .. } => {
                    let assembly = self
                        .articulated
                        .get(&id.0)
                        .ok_or(ArticulatedError::Hierarchy)?;
                    let source = assembly
                        .parts
                        .iter()
                        .map(|part| SourcePart {
                            part_id: part.part_id,
                            parent_id: part.parent_id,
                            volume_id: part.volume_id,
                            source_revision: part.source_revision,
                            grid: part.grid.clone(),
                            cell_m: part.cell_m,
                            local_transform: part.local_pose,
                        })
                        .collect::<Vec<_>>();
                    if source.len() > MAX_ARTICULATED_PARTS {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    for part in &source {
                        add_batch_grid_budget(&part.grid, &mut dense, &mut solids, &mut materials)?;
                    }
                    continue;
                }
            };
            if parts.len() > MAX_ARTICULATED_PARTS {
                return Err(ArticulatedError::WorkBudget);
            }
            for part in parts {
                add_batch_grid_budget(&part.grid, &mut dense, &mut solids, &mut materials)?;
            }
        }
        // Snapshot each material density once for the complete batch. Shapes
        // and mass preparation cannot observe different catalog values per owner.
        let mut material_densities = HashMap::new();
        for material in materials {
            let rho = density(material);
            if !rho.is_finite() || rho <= 0.0 {
                return Err(ArticulatedError::InvalidDensity);
            }
            material_densities.insert(material, rho.to_bits());
        }
        let stable_density = |material| manifest_density(&material_densities, material);
        let mut changes = Vec::with_capacity(operations.len());
        let mut owner_keys = std::collections::BTreeSet::new();
        let mut volume_ids = std::collections::BTreeSet::new();
        let mut primitives = 0_usize;
        let mut derived_dense = 0_u64;
        let mut transfer_ids = HashSet::new();
        for operation in operations {
            match operation {
                ArticulatedBatchOperation::AliveBirth {
                    operation_tag,
                    translation_m,
                    rotation_xyzw,
                    topology_token,
                    parts,
                    representation,
                } => {
                    let birth = self.prepare_articulated_birth(
                        translation_m,
                        rotation_xyzw,
                        topology_token,
                        parts,
                        representation,
                        true,
                        Some(&stable_density),
                    )?;
                    if birth
                        .source_volume_ids
                        .iter()
                        .any(|id| !volume_ids.insert(*id))
                    {
                        return Err(ArticulatedError::DuplicateVolume);
                    }
                    primitives = primitives
                        .checked_add(birth.primitive_count)
                        .ok_or(ArticulatedError::WorkBudget)?;
                    if primitives > MAX_CORPSE_RESTORE_BATCH_PRIMITIVES {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    changes.push(PreparedArticulatedChange::AliveBirth {
                        operation_tag,
                        birth,
                    });
                }
                ArticulatedBatchOperation::ColdRestore {
                    operation_tag,
                    restore,
                    representation,
                } => {
                    let owner = self.prepare_corpse_restore_owner(
                        restore,
                        representation,
                        &stable_density,
                    )?;
                    if !owner_keys.insert(owner.owner_key)
                        || owner
                            .source_volumes
                            .iter()
                            .any(|id| !volume_ids.insert(*id))
                    {
                        return Err(ArticulatedError::InvalidRestore);
                    }
                    primitives = primitives
                        .checked_add(owner.primitive_count)
                        .ok_or(ArticulatedError::WorkBudget)?;
                    derived_dense = derived_dense
                        .checked_add(owner.derived_dense)
                        .ok_or(ArticulatedError::WorkBudget)?;
                    if primitives > MAX_CORPSE_RESTORE_BATCH_PRIMITIVES
                        || derived_dense > MAX_CORPSE_RESTORE_BATCH_DENSE_CELLS
                    {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    changes.push(PreparedArticulatedChange::ColdRestore {
                        operation_tag,
                        owner,
                    });
                }
                ArticulatedBatchOperation::CheckedDeathTransfer {
                    operation_tag,
                    id,
                    expected_topology_token,
                    expected_pose_token,
                } => {
                    if !transfer_ids.insert(id.0) {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                    let transfer = self.prepare_dynamic_corpse_transfer(
                        id,
                        expected_topology_token,
                        expected_pose_token,
                        stable_density,
                    )?;
                    if transfer
                        .source_volume_ids
                        .iter()
                        .any(|source| !volume_ids.insert(*source))
                    {
                        return Err(ArticulatedError::DuplicateVolume);
                    }
                    primitives = primitives
                        .checked_add(transfer.primitive_count)
                        .ok_or(ArticulatedError::WorkBudget)?;
                    if primitives > MAX_CORPSE_RESTORE_BATCH_PRIMITIVES {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    changes.push(PreparedArticulatedChange::CheckedDeathTransfer {
                        operation_tag,
                        transfer,
                    });
                }
            }
        }
        Ok(PreparedArticulatedBatch {
            world_identity: self.preparation_identity.clone(),
            expected_step: self.step_count,
            expected_entries: self.entries.len(),
            changes,
        })
    }

    /// Revalidates the entire prepared batch before the first insertion.
    /// Stale or conflicting batches publish no owners and consume no handles.
    pub fn commit_dynamic_corpse_restore_batch(
        &mut self,
        prepared: PreparedDynamicCorpseBatch,
    ) -> Result<Vec<ArticulatedBodyId>, crate::articulated::ArticulatedError> {
        self.commit_articulated_batch(prepared).map(|outcomes| {
            outcomes
                .into_iter()
                .map(|outcome| outcome.owner_id)
                .collect()
        })
    }

    /// Revalidates every operation and cross-operation reservation before
    /// publishing any member. Publication has no fallible checks or Result
    /// paths after the first Rapier mutation.
    pub fn commit_articulated_batch(
        &mut self,
        prepared: PreparedArticulatedBatch,
    ) -> Result<Vec<ArticulatedBatchOutcome>, crate::articulated::ArticulatedError> {
        let validated = self.validate_articulated_batch(prepared)?;
        Ok(self.publish_validated_articulated_batch(validated))
    }

    /// Performs every fallible freshness, identity, source and cross-member
    /// check. The returned private token may be published after a separately
    /// prepared geometry batch mutates unrelated physics entries.
    pub fn validate_articulated_batch(
        &self,
        prepared: PreparedArticulatedBatch,
    ) -> Result<ValidatedArticulatedBatch, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        if !std::sync::Arc::ptr_eq(&prepared.world_identity, &self.preparation_identity)
            || prepared.expected_step != self.step_count
            || prepared.expected_entries != self.entries.len()
        {
            return Err(ArticulatedError::StalePreparedBatch);
        }
        let additions = prepared
            .changes
            .iter()
            .filter(|change| {
                !matches!(
                    change,
                    PreparedArticulatedChange::CheckedDeathTransfer { .. }
                )
            })
            .count();
        let final_entries = self
            .entries
            .len()
            .checked_add(additions)
            .ok_or(ArticulatedError::WorkBudget)?;
        if final_entries > u32::MAX as usize {
            return Err(ArticulatedError::WorkBudget);
        }
        let live_volumes: std::collections::BTreeSet<_> = self
            .articulated
            .values()
            .flat_map(|owner| owner.source_volume_ids.iter().copied())
            .collect();
        let mut batch_owner_keys = std::collections::BTreeSet::new();
        let mut batch_volumes = std::collections::BTreeSet::new();
        for change in &prepared.changes {
            match change {
                PreparedArticulatedChange::ColdRestore { owner, .. } => {
                    if self.fracture_owner_keys.contains_key(&owner.owner_key)
                        || self
                            .articulated
                            .values()
                            .any(|live| live.fracture_owner_key == Some(owner.owner_key))
                        || owner
                            .source_volumes
                            .iter()
                            .any(|id| live_volumes.contains(id) || !batch_volumes.insert(*id))
                        || !batch_owner_keys.insert(owner.owner_key)
                    {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                }
                PreparedArticulatedChange::AliveBirth { birth, .. } => {
                    if birth
                        .source_volume_ids
                        .iter()
                        .any(|id| live_volumes.contains(id) || !batch_volumes.insert(*id))
                    {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                }
                PreparedArticulatedChange::DynamicCorpseBirth { birth, .. } => {
                    if birth
                        .source_volume_ids
                        .iter()
                        .any(|id| live_volumes.contains(id) || !batch_volumes.insert(*id))
                    {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                }
                PreparedArticulatedChange::CheckedDeathTransfer { transfer, .. } => {
                    if !self.validate_prepared_corpse_transfer(transfer) {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                    if transfer
                        .source_volume_ids
                        .iter()
                        .any(|id| !batch_volumes.insert(*id))
                    {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                }
            }
        }
        Ok(ValidatedArticulatedBatch { prepared })
    }

    /// Publishes a previously validated batch without another fallible path.
    /// The token must be published on its creating world at the same tick,
    /// after joint validation and before unrelated mutation. When composing
    /// with geometry, only the already-validated geometry publication may
    /// intervene. Publishing on another world is programmer error and panics
    /// before this batch mutates either solver structures or owner indexes.
    pub fn publish_validated_articulated_batch(
        &mut self,
        validated: ValidatedArticulatedBatch,
    ) -> Vec<ArticulatedBatchOutcome> {
        assert!(
            std::sync::Arc::ptr_eq(
                &validated.prepared.world_identity,
                &self.preparation_identity
            ) && validated.prepared.expected_step == self.step_count,
            "validated articulated batch belongs to another PhysicsWorld or tick"
        );
        let prepared = validated.prepared;
        let mut outcomes = Vec::with_capacity(prepared.changes.len());
        // All bounds, reservations and freshness checks have completed.
        // Publication below contains no fallible preparation or validation.
        for change in prepared.changes {
            match change {
                PreparedArticulatedChange::AliveBirth {
                    operation_tag,
                    birth,
                } => {
                    let owner_id = self.publish_articulated_birth(birth);
                    outcomes.push(ArticulatedBatchOutcome {
                        operation_tag,
                        owner_id,
                        transfer: None,
                    });
                }
                PreparedArticulatedChange::DynamicCorpseBirth {
                    operation_tag,
                    birth,
                } => {
                    let owner_id = self.publish_articulated_birth(birth);
                    outcomes.push(ArticulatedBatchOutcome {
                        operation_tag,
                        owner_id,
                        transfer: None,
                    });
                }
                PreparedArticulatedChange::ColdRestore {
                    operation_tag,
                    owner,
                } => {
                    let owner_id = self.publish_corpse_restore_owner(owner);
                    outcomes.push(ArticulatedBatchOutcome {
                        operation_tag,
                        owner_id,
                        transfer: None,
                    });
                }
                PreparedArticulatedChange::CheckedDeathTransfer {
                    operation_tag,
                    transfer,
                } => {
                    let outcome = self.publish_prepared_corpse_transfer(transfer);
                    outcomes.push(ArticulatedBatchOutcome {
                        operation_tag,
                        owner_id: outcome.0,
                        transfer: Some(outcome.1),
                    });
                }
            }
        }
        outcomes
    }

    fn prepare_articulated_birth(
        &self,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
        kinematic_part_bodies: bool,
        density: Option<&dyn Fn(MaterialId) -> f64>,
    ) -> Result<PreparedArticulatedBirth, crate::articulated::ArticulatedError> {
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_SOLID_CELLS,
        };
        if parts.is_empty()
            || parts.len() > MAX_ARTICULATED_PARTS
            || !articulated_translation_in_range(translation_m)
            || !(PartTransform {
                translation_m: [0.0; 3],
                rotation_xyzw,
            })
            .is_valid()
            || representation == Representation::SmoothConvex
        {
            return Err(ArticulatedError::InvalidTransform);
        }
        let root_rotation = normalize(rotation_xyzw);
        let live_volumes: std::collections::BTreeSet<_> = self
            .articulated
            .values()
            .flat_map(|owner| owner.source_volume_ids.iter().copied())
            .collect();
        let mut sorted = parts;
        sorted.sort_by_key(|part| part.part_id);
        let mut ids = std::collections::BTreeSet::new();
        let mut source_volume_ids = std::collections::BTreeSet::new();
        let mut roots = 0usize;
        let mut dense = 0u64;
        let mut solids = 0u64;
        let cell_m = sorted[0].cell_m;
        for part in &mut sorted {
            if part.part_id == 0 || !ids.insert(part.part_id) {
                return Err(ArticulatedError::DuplicatePart);
            }
            if !source_volume_ids.insert(part.volume_id) || live_volumes.contains(&part.volume_id) {
                return Err(ArticulatedError::DuplicateVolume);
            }
            roots += usize::from(part.parent_id.is_none());
            if !part.cell_m.is_finite()
                || part.cell_m <= 0.0
                || (part.cell_m - cell_m).abs() > 1e-7
                || !part.local_transform.is_valid()
            {
                return Err(ArticulatedError::InvalidTransform);
            }
            if !articulated_translation_in_range(part.local_transform.translation_m)
                || !articulated_grid_range_valid(&part.grid, part.cell_m)
            {
                return Err(ArticulatedError::GeometryRange);
            }
            part.local_transform = part
                .local_transform
                .normalized()
                .ok_or(ArticulatedError::InvalidTransform)?;
            let dims = part.grid.dims();
            let part_dense = u64::from(dims[0])
                .checked_mul(u64::from(dims[1]))
                .and_then(|n| n.checked_mul(u64::from(dims[2])))
                .ok_or(ArticulatedError::WorkBudget)?;
            dense = dense
                .checked_add(part_dense)
                .ok_or(ArticulatedError::WorkBudget)?;
            solids = solids
                .checked_add(part.grid.solid_count())
                .ok_or(ArticulatedError::WorkBudget)?;
            if part.grid.solid_count() == 0 {
                return Err(ArticulatedError::EmptyPart(part.part_id));
            }
            if dense > MAX_ARTICULATED_DENSE_CELLS || solids > MAX_ARTICULATED_SOLID_CELLS {
                return Err(ArticulatedError::WorkBudget);
            }
        }
        if roots != 1
            || sorted
                .iter()
                .any(|part| part.parent_id.is_some_and(|parent| !ids.contains(&parent)))
        {
            return Err(ArticulatedError::Hierarchy);
        }
        let flattened = flatten_source_parts(&sorted)?;
        if flattened
            .iter()
            .any(|pose| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }
        let manifest = density
            .map(|density| density_manifest(&sorted, density))
            .transpose()?;
        let mass = if kinematic_part_bodies {
            None
        } else {
            let manifest_ref = manifest.as_ref().ok_or(ArticulatedError::InvalidDensity)?;
            let stable_density = |material| manifest_density(manifest_ref, material);
            let mass =
                crate::mass::articulated_mass_properties(&sorted, &flattened, stable_density)
                    .ok_or(ArticulatedError::InvalidTransform)?
                    .to_body_properties();
            validate_body_mass_properties(mass)?;
            Some(mass)
        };
        let builds = sorted
            .iter()
            .map(|part| build_collider(&part.grid, cell_m, representation))
            .collect::<Vec<_>>();
        let primitive_count = builds.iter().map(|build| build.primitives).sum::<usize>();
        if builds
            .iter()
            .any(|build| build.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET)
            || primitive_count > 65_536
        {
            return Err(ArticulatedError::WorkBudget);
        }
        let collider_poses = sorted
            .iter()
            .zip(&flattened)
            .map(|(part, pose)| {
                let origin = part.grid.origin();
                let origin_m = [
                    origin.x as f32 * cell_m,
                    origin.y as f32 * cell_m,
                    origin.z as f32 * cell_m,
                ];
                let offset = rotate(pose.rotation_xyzw, origin_m);
                (
                    [
                        pose.translation_m[0] + offset[0],
                        pose.translation_m[1] + offset[1],
                        pose.translation_m[2] + offset[2],
                    ],
                    pose.rotation_xyzw,
                )
            })
            .collect::<Vec<_>>();
        if collider_poses.iter().any(|(local, rotation)| {
            !articulated_translation_in_range(*local)
                || !rotation.iter().all(|v| v.is_finite())
                || {
                    let world = rotate(root_rotation, *local);
                    !articulated_translation_in_range([
                        translation_m[0] + world[0],
                        translation_m[1] + world[1],
                        translation_m[2] + world[2],
                    ])
                }
        }) {
            return Err(ArticulatedError::GeometryRange);
        }
        Ok(PreparedArticulatedBirth {
            translation_m,
            rotation_xyzw: root_rotation,
            topology_token,
            parts: sorted,
            flattened,
            collider_poses,
            builds,
            source_volume_ids,
            mass,
            manifest,
            kinematic_part_bodies,
            representation,
            primitive_count,
        })
    }

    fn publish_articulated_birth(&mut self, birth: PreparedArticulatedBirth) -> ArticulatedBodyId {
        let PreparedArticulatedBirth {
            translation_m,
            rotation_xyzw,
            topology_token,
            parts,
            flattened,
            collider_poses,
            builds,
            source_volume_ids,
            mass,
            manifest,
            kinematic_part_bodies,
            representation,
            ..
        } = birth;
        let pose = Pose::new(
            Vector::from(translation_m),
            Rotation::from_xyzw(
                rotation_xyzw[0],
                rotation_xyzw[1],
                rotation_xyzw[2],
                rotation_xyzw[3],
            )
            .to_scaled_axis(),
        );
        let root_builder = if let Some(mass) = mass {
            RigidBodyBuilder::dynamic()
                .additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]))
        } else {
            RigidBodyBuilder::kinematic_position_based()
        };
        let root = self.bodies.insert(root_builder.pose(pose).build());
        let id = BodyId((u64::from(self.id_namespace) << 32) | self.entries.len() as u64);
        let cell_m = parts[0].cell_m;
        let mut built_parts = Vec::with_capacity(parts.len());
        for (((part, assembly_pose), (local_translation, local_rotation)), build) in parts
            .into_iter()
            .zip(flattened)
            .zip(collider_poses)
            .zip(builds)
        {
            let local_rotation = Rotation::from_xyzw(
                local_rotation[0],
                local_rotation[1],
                local_rotation[2],
                local_rotation[3],
            );
            let (part_body, collider_pose) = if kinematic_part_bodies {
                let world_pose = compose(
                    PartTransform {
                        translation_m,
                        rotation_xyzw,
                    },
                    assembly_pose,
                );
                let part_body = self.bodies.insert(
                    RigidBodyBuilder::kinematic_position_based()
                        .pose(Pose::new(
                            Vector::from(world_pose.translation_m),
                            Rotation::from_xyzw(
                                world_pose.rotation_xyzw[0],
                                world_pose.rotation_xyzw[1],
                                world_pose.rotation_xyzw[2],
                                world_pose.rotation_xyzw[3],
                            )
                            .to_scaled_axis(),
                        ))
                        .build(),
                );
                let origin = part.grid.origin();
                (
                    part_body,
                    Pose::from_translation(Vector::new(
                        origin.x as f32 * cell_m,
                        origin.y as f32 * cell_m,
                        origin.z as f32 * cell_m,
                    )),
                )
            } else {
                (
                    root,
                    Pose::new(
                        Vector::from(local_translation),
                        local_rotation.to_scaled_axis(),
                    ),
                )
            };
            let mut collider_builder = ColliderBuilder::new(build.collider.shared_shape().clone())
                .density(0.0)
                .position(collider_pose);
            if kinematic_part_bodies {
                collider_builder = collider_builder.active_collision_types(
                    ActiveCollisionTypes::default() | ActiveCollisionTypes::KINEMATIC_FIXED,
                );
            }
            let collider = collider_builder.build();
            let handle = self
                .colliders
                .insert_with_parent(collider, part_body, &mut self.bodies);
            self.pending_modified.push(handle);
            let part_surface_radius_m = source_surface_radius_m(&part.grid, cell_m, [0.0; 3], None);
            let root_surface_radius_m =
                source_surface_radius_m(&part.grid, cell_m, [0.0; 3], Some(assembly_pose));
            built_parts.push(ArticulatedPartEntry {
                part_id: part.part_id,
                parent_id: part.parent_id,
                volume_id: part.volume_id,
                source_revision: part.source_revision,
                grid: part.grid,
                cell_m,
                local_pose: part.local_transform,
                assembly_pose,
                part_surface_radius_m,
                root_surface_radius_m,
                body: part_body,
                collider: handle,
                primitives: build.primitives,
                component_id: None,
            });
        }
        let first_origin = built_parts[0].grid.origin();
        let primary = built_parts[0].collider;
        self.entries.push(Entry {
            body: root,
            collider: primary,
            cell_m,
            representation,
            density: 0.0,
            collider_offset_m: [
                first_origin.x as f32 * cell_m,
                first_origin.y as f32 * cell_m,
                first_origin.z as f32 * cell_m,
            ],
            mass_properties: mass,
            retired: false,
            dormant: false,
            query_only: false,
            restitution: 0.0,
        });
        for part in &built_parts {
            self.tag_collider(part.collider, id);
        }
        self.articulated.insert(
            id,
            ArticulatedEntry {
                topology_token,
                pose_token: 0,
                root_pose: PartTransform {
                    translation_m,
                    rotation_xyzw,
                },
                pending_root_pose_token: None,
                pending_pose: None,
                kinematic_part_bodies,
                dynamic_corpse: mass.is_some(),
                source_generation: 1,
                fracture_owner_key: None,
                next_component_serial: 1,
                density_manifest: manifest,
                source_volume_ids,
                parts: built_parts,
            },
        );
        if let Some(mass) = mass {
            let pose = *self.bodies[root].position();
            self.bodies[root]
                .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
            self.bodies[root].recompute_mass_properties_from_colliders(&self.colliders);
            self.bodies[root].set_position(pose, true);
        }
        ArticulatedBodyId(id)
    }

    fn prepare_corpse_restore_owner(
        &self,
        restore: crate::articulated::DynamicCorpseRestore,
        representation: Representation,
        density: &impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<PreparedCorpseRestoreOwner, crate::articulated::ArticulatedError> {
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_PARTS,
            MAX_ARTICULATED_SOLID_CELLS,
        };
        use std::collections::{BTreeMap, BTreeSet};

        if restore.owner_key == 0
            || restore.source_generation == 0
            || restore.topology_revision == 0
            || restore.pose_epoch == 0
            || restore.next_component_serial == 0
            || restore.source_parts.is_empty()
            || restore.source_parts.len() > MAX_ARTICULATED_PARTS
            || restore.components.is_empty()
            || restore.components.len() > crate::fracture::MAX_FRACTURE_COMPONENTS
            || self.fracture_owner_keys.contains_key(&restore.owner_key)
            || self
                .articulated
                .values()
                .any(|assembly| assembly.fracture_owner_key == Some(restore.owner_key))
            || representation == Representation::SmoothConvex
        {
            return Err(ArticulatedError::InvalidRestore);
        }

        let mut sources = restore.source_parts;
        sources.sort_by_key(|part| part.part_id);
        let mut source_part_ids = BTreeSet::new();
        let mut source_volumes = BTreeSet::new();
        let mut roots = 0;
        let mut source_dense = 0_u64;
        let mut source_solid = 0_u64;
        for part in &mut sources {
            if part.part_id == 0
                || !source_part_ids.insert(part.part_id)
                || !source_volumes.insert(part.volume_id)
                || !part.local_transform.is_valid()
                || part.source_revision == 0
                || !part.cell_m.is_finite()
                || part.cell_m <= 0.0
                || !articulated_grid_range_valid(&part.grid, part.cell_m)
            {
                return Err(ArticulatedError::InvalidRestore);
            }
            roots += usize::from(part.parent_id.is_none());
            part.local_transform = part
                .local_transform
                .normalized()
                .ok_or(ArticulatedError::InvalidRestore)?;
            let dims = part.grid.dims();
            let dense = u64::from(dims[0])
                .checked_mul(u64::from(dims[1]))
                .and_then(|value| value.checked_mul(u64::from(dims[2])))
                .ok_or(ArticulatedError::WorkBudget)?;
            source_dense = source_dense
                .checked_add(dense)
                .ok_or(ArticulatedError::WorkBudget)?;
            source_solid = source_solid
                .checked_add(part.grid.solid_count())
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        if roots != 1
            || sources.iter().any(|part| {
                part.parent_id
                    .is_some_and(|parent| !source_part_ids.contains(&parent))
            })
            || source_dense > MAX_ARTICULATED_DENSE_CELLS
            || source_solid == 0
            || source_solid > MAX_ARTICULATED_SOLID_CELLS
        {
            return Err(ArticulatedError::InvalidRestore);
        }
        let frozen_poses = flatten_source_parts(&sources)?;
        if frozen_poses
            .iter()
            .any(|pose| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }

        // A source VolumeId can be repeated by different derived components,
        // but never by two original source parts or another live owner.
        let live_volumes: BTreeSet<_> = self
            .articulated
            .values()
            .flat_map(|assembly| assembly.source_volume_ids.iter().copied())
            .collect();
        if source_volumes
            .iter()
            .any(|volume| live_volumes.contains(volume))
        {
            return Err(ArticulatedError::DuplicateVolume);
        }

        if restore.components.iter().any(|component| {
            component
                .component_id
                .is_some_and(|id| id.owner_key != restore.owner_key)
                || !component.translation_m.iter().all(|v| v.is_finite())
                || !component.rotation_xyzw.iter().all(|v| v.is_finite())
                || !component.linear_velocity_m_s.iter().all(|v| v.is_finite())
                || !component
                    .angular_velocity_rad_s
                    .iter()
                    .all(|v| v.is_finite())
                || !articulated_translation_in_range(component.translation_m)
                || !(PartTransform {
                    translation_m: [0.0; 3],
                    rotation_xyzw: component.rotation_xyzw,
                })
                .is_valid()
                || component.cells.is_empty()
        }) {
            return Err(ArticulatedError::InvalidRestore);
        }
        let intact = restore.components.len() == 1 && restore.components[0].component_id.is_none();
        if (intact && restore.next_component_serial != 1)
            || restore.components.iter().any(|component| {
                component.component_id.is_none() != intact
                    || component.component_id.is_some_and(|id| {
                        id.owner_key != restore.owner_key
                            || id.serial == 0
                            || id.serial >= restore.next_component_serial
                    })
            })
        {
            return Err(ArticulatedError::InvalidRestore);
        }
        let mut component_ids = BTreeSet::new();
        if restore
            .components
            .iter()
            .filter_map(|c| c.component_id)
            .any(|id| !component_ids.insert(id))
        {
            return Err(ArticulatedError::InvalidRestore);
        }

        // Read the canonical source occupancy once. Component masks are routed
        // through this index, so work scales with source cells plus mask cells,
        // not source cells multiplied by the number of components.
        let mut expected = BTreeMap::new();
        for (part_index, part) in sources.iter().enumerate() {
            let origin = part.grid.origin();
            let mut range_error = false;
            part.grid.for_each_solid(|x, y, z, material| {
                let xyz = [
                    origin.x.checked_add(i64::from(x)),
                    origin.y.checked_add(i64::from(y)),
                    origin.z.checked_add(i64::from(z)),
                ];
                let [Some(cx), Some(cy), Some(cz)] =
                    xyz.map(|coord| coord.and_then(|value| i32::try_from(value).ok()))
                else {
                    range_error = true;
                    return;
                };
                let key = SourceCellRef {
                    volume_id: part.volume_id,
                    part_id: part.part_id,
                    cell: [cx, cy, cz],
                    material,
                    source_revision: part.source_revision,
                };
                if expected.insert(key, (part_index, [x, y, z])).is_some() {
                    range_error = true;
                }
            });
            if range_error {
                return Err(ArticulatedError::GeometryRange);
            }
        }
        let mut assigned = BTreeSet::new();
        let mut component_owned = Vec::with_capacity(restore.components.len());
        for component in &restore.components {
            let mut local = BTreeSet::new();
            let mut by_part = vec![Vec::new(); sources.len()];
            for cell in &component.cells {
                let Some((part_index, local_coord)) = expected.get(cell).copied() else {
                    return Err(ArticulatedError::InvalidRestore);
                };
                if !local.insert(*cell) || !assigned.insert(*cell) {
                    return Err(ArticulatedError::InvalidRestore);
                }
                by_part[part_index].push((
                    local_coord[0],
                    local_coord[1],
                    local_coord[2],
                    cell.material,
                ));
            }
            component_owned.push(by_part);
        }
        if assigned.len() != expected.len() {
            return Err(ArticulatedError::InvalidRestore);
        }

        let manifest = density_manifest(&sources, &density)?;
        let stable_density = |material| manifest_density(&manifest, material);
        let mut prepared = Vec::with_capacity(restore.components.len());
        let mut primitive_count = 0_usize;
        let mut derived_dense = 0_u64;
        let mut total_shards = 0_usize;
        for (component_index, component) in restore.components.iter().enumerate() {
            let rotation = normalize(component.rotation_xyzw);
            let mut source_shards = Vec::new();
            let mut flat_poses = Vec::new();
            let mut parts = Vec::new();
            for (part_index, (part, frozen_pose)) in sources.iter().zip(&frozen_poses).enumerate() {
                let origin = part.grid.origin();
                let owned = &component_owned[component_index][part_index];
                if owned.is_empty() {
                    continue;
                }
                let min = std::array::from_fn::<_, 3, _>(|axis| {
                    owned.iter().map(|c| [c.0, c.1, c.2][axis]).min().unwrap()
                });
                let max = std::array::from_fn::<_, 3, _>(|axis| {
                    owned.iter().map(|c| [c.0, c.1, c.2][axis]).max().unwrap()
                });
                let dims = [
                    max[0] - min[0] + 1,
                    max[1] - min[1] + 1,
                    max[2] - min[2] + 1,
                ];
                let dense = u64::from(dims[0])
                    .checked_mul(u64::from(dims[1]))
                    .and_then(|v| v.checked_mul(u64::from(dims[2])))
                    .ok_or(ArticulatedError::WorkBudget)?;
                derived_dense = derived_dense
                    .checked_add(dense)
                    .ok_or(ArticulatedError::WorkBudget)?;
                if derived_dense > crate::fracture::MAX_FRACTURE_DENSE_CELLS as u64 {
                    return Err(ArticulatedError::WorkBudget);
                }
                let volume = usize::try_from(dense).map_err(|_| ArticulatedError::WorkBudget)?;
                let mut mask = vec![false; volume];
                let mut materials = vec![MaterialId::AIR; volume];
                for &(x, y, z, material) in owned {
                    let local = [x - min[0], y - min[1], z - min[2]];
                    let index = local[0] as usize
                        + dims[0] as usize
                            * (local[1] as usize + dims[1] as usize * local[2] as usize);
                    mask[index] = true;
                    materials[index] = material;
                }
                let grid = OccupancyGrid::from_solid_mask(
                    GlobalCell::new(
                        origin.x + i64::from(min[0]),
                        origin.y + i64::from(min[1]),
                        origin.z + i64::from(min[2]),
                    ),
                    dims,
                    mask,
                    materials,
                )
                .map_err(|_| ArticulatedError::InvalidRestore)?;
                source_shards.push(SourcePart {
                    part_id: part.part_id,
                    parent_id: None,
                    volume_id: part.volume_id,
                    source_revision: part.source_revision,
                    grid: grid.clone(),
                    cell_m: part.cell_m,
                    local_transform: *frozen_pose,
                });
                flat_poses.push(*frozen_pose);
                let built = build_collider(&grid, part.cell_m, representation);
                primitive_count = primitive_count
                    .checked_add(built.primitives)
                    .ok_or(ArticulatedError::WorkBudget)?;
                if built.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET
                    || primitive_count > 65_536
                {
                    return Err(ArticulatedError::WorkBudget);
                }
                let origin = grid.origin();
                let origin_m = [
                    origin.x as f32 * part.cell_m,
                    origin.y as f32 * part.cell_m,
                    origin.z as f32 * part.cell_m,
                ];
                let offset = rotate(frozen_pose.rotation_xyzw, origin_m);
                let local_translation = [
                    frozen_pose.translation_m[0] + offset[0],
                    frozen_pose.translation_m[1] + offset[1],
                    frozen_pose.translation_m[2] + offset[2],
                ];
                let world_offset = rotate(rotation, local_translation);
                if !articulated_translation_in_range(local_translation)
                    || !articulated_translation_in_range([
                        component.translation_m[0] + world_offset[0],
                        component.translation_m[1] + world_offset[1],
                        component.translation_m[2] + world_offset[2],
                    ])
                {
                    return Err(ArticulatedError::GeometryRange);
                }
                let local_pose = Pose::new(
                    Vector::new(
                        local_translation[0],
                        local_translation[1],
                        local_translation[2],
                    ),
                    Rotation::from_xyzw(
                        frozen_pose.rotation_xyzw[0],
                        frozen_pose.rotation_xyzw[1],
                        frozen_pose.rotation_xyzw[2],
                        frozen_pose.rotation_xyzw[3],
                    )
                    .to_scaled_axis(),
                );
                let collider = ColliderBuilder::new(built.collider.shared_shape().clone())
                    .density(0.0)
                    .position(local_pose)
                    .build();
                total_shards = total_shards
                    .checked_add(1)
                    .ok_or(ArticulatedError::WorkBudget)?;
                if total_shards > MAX_ARTICULATED_PARTS {
                    return Err(ArticulatedError::WorkBudget);
                }
                parts.push((
                    PreparedCorpsePartIdentity {
                        part_id: part.part_id,
                        volume_id: part.volume_id,
                        source_revision: part.source_revision,
                        cell_m: part.cell_m,
                    },
                    grid,
                    *frozen_pose,
                    built.primitives,
                    collider,
                ));
            }
            if parts.is_empty() {
                return Err(ArticulatedError::EmptyAggregate);
            }
            let mass = crate::mass::articulated_mass_properties(
                &source_shards,
                &flat_poses,
                stable_density,
            )
            .ok_or(ArticulatedError::InvalidMassProperties)?
            .to_body_properties();
            validate_body_mass_properties(mass)?;
            let translation = component.translation_m;
            if !articulated_translation_in_range(translation) {
                return Err(ArticulatedError::GeometryRange);
            }
            prepared.push((
                PreparedCorpseComponentState {
                    component_id: component.component_id,
                    linear_velocity_m_s: component.linear_velocity_m_s,
                    angular_velocity_rad_s: component.angular_velocity_rad_s,
                },
                mass,
                rotation,
                translation,
                parts,
            ));
        }

        Ok(PreparedCorpseRestoreOwner {
            owner_key: restore.owner_key,
            source_generation: restore.source_generation,
            topology_revision: restore.topology_revision,
            pose_epoch: restore.pose_epoch,
            next_component_serial: restore.next_component_serial,
            source_volumes,
            manifest,
            prepared,
            representation,
            primitive_count,
            derived_dense,
        })
    }

    fn publish_corpse_restore_owner(
        &mut self,
        restore: PreparedCorpseRestoreOwner,
    ) -> ArticulatedBodyId {
        let PreparedCorpseRestoreOwner {
            owner_key,
            source_generation,
            topology_revision,
            pose_epoch,
            next_component_serial,
            source_volumes,
            manifest,
            prepared,
            representation,
            ..
        } = restore;
        let first_translation = prepared[0].3;
        let first_rotation = prepared[0].2;
        let first_mass = prepared[0].1;
        let first_linear = prepared[0].0.linear_velocity_m_s;
        let first_angular = prepared[0].0.angular_velocity_rad_s;
        let first_body = self.bodies.insert(
            RigidBodyBuilder::dynamic()
                .translation(Vector::new(
                    first_translation[0],
                    first_translation[1],
                    first_translation[2],
                ))
                .rotation(
                    Rotation::from_xyzw(
                        first_rotation[0],
                        first_rotation[1],
                        first_rotation[2],
                        first_rotation[3],
                    )
                    .to_scaled_axis(),
                )
                .additional_mass_properties(rapier_mass_properties(first_mass, [0.0; 3]))
                .linvel(Vector::from(first_linear))
                .angvel(Vector::from(first_angular))
                .build(),
        );
        let id = BodyId((u64::from(self.id_namespace) << 32) | self.entries.len() as u64);
        let mut built_parts = Vec::new();
        let mut primary_collider = None;
        let mut primary_cell_m = 1.0;
        let mut primary_origin = [0.0; 3];
        let mut first_body_handle = first_body;
        for (component_index, (component, mass, rotation, translation, parts)) in
            prepared.into_iter().enumerate()
        {
            let body = if component_index == 0 {
                first_body_handle
            } else {
                self.bodies.insert(
                    RigidBodyBuilder::dynamic()
                        .translation(Vector::new(translation[0], translation[1], translation[2]))
                        .rotation(
                            Rotation::from_xyzw(rotation[0], rotation[1], rotation[2], rotation[3])
                                .to_scaled_axis(),
                        )
                        .additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]))
                        .linvel(Vector::from(component.linear_velocity_m_s))
                        .angvel(Vector::from(component.angular_velocity_rad_s))
                        .build(),
                )
            };
            if component_index == 0 {
                first_body_handle = body;
            }
            let component_id = component.component_id;
            for (part, grid, frozen_pose, primitives, collider) in parts {
                let collider = self
                    .colliders
                    .insert_with_parent(collider, body, &mut self.bodies);
                self.tag_collider(collider, id);
                self.pending_modified.push(collider);
                if primary_collider.is_none() {
                    primary_collider = Some(collider);
                    primary_cell_m = part.cell_m;
                    let origin = grid.origin();
                    primary_origin = [
                        origin.x as f32 * part.cell_m,
                        origin.y as f32 * part.cell_m,
                        origin.z as f32 * part.cell_m,
                    ];
                }
                built_parts.push(ArticulatedPartEntry {
                    part_id: part.part_id,
                    parent_id: None,
                    volume_id: part.volume_id,
                    source_revision: part.source_revision,
                    grid: grid.clone(),
                    cell_m: part.cell_m,
                    local_pose: frozen_pose,
                    assembly_pose: frozen_pose,
                    part_surface_radius_m: source_surface_radius_m(
                        &grid,
                        part.cell_m,
                        [0.0; 3],
                        None,
                    ),
                    root_surface_radius_m: source_surface_radius_m(
                        &grid,
                        part.cell_m,
                        [0.0; 3],
                        Some(frozen_pose),
                    ),
                    body,
                    collider,
                    primitives,
                    component_id,
                });
            }
            self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
            self.bodies[body]
                .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
            self.bodies[body].set_linvel(Vector::from(component.linear_velocity_m_s), true);
            self.bodies[body].set_angvel(Vector::from(component.angular_velocity_rad_s), true);
        }
        self.entries.push(Entry {
            body: first_body_handle,
            collider: primary_collider.expect("nonempty prepared restore has a primary collider"),
            cell_m: primary_cell_m,
            representation,
            density: 0.0,
            collider_offset_m: primary_origin,
            mass_properties: Some(first_mass),
            retired: false,
            dormant: false,
            query_only: false,
            restitution: 0.0,
        });
        self.articulated.insert(
            id,
            ArticulatedEntry {
                topology_token: topology_revision,
                pose_token: pose_epoch,
                root_pose: PartTransform {
                    translation_m: first_translation,
                    rotation_xyzw: first_rotation,
                },
                pending_root_pose_token: None,
                pending_pose: None,
                kinematic_part_bodies: false,
                dynamic_corpse: true,
                source_generation,
                fracture_owner_key: Some(owner_key),
                next_component_serial,
                density_manifest: Some(manifest),
                source_volume_ids: source_volumes,
                parts: built_parts,
            },
        );
        self.fracture_owner_keys.insert(owner_key, id);
        self.ccd_solver = CCDSolver::new();
        ArticulatedBodyId(id)
    }

    /// Converts the existing ENG-210 multi-body kinematic owner in place. It
    /// derives exact source-cell mass and momenta from each part body's current
    /// pose and velocity; an invalid/stale/pending transfer mutates nothing.
    pub fn transfer_articulated_to_dynamic_corpse(
        &mut self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        expected_pose_token: u64,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<CorpseTransferOutcome, crate::articulated::ArticulatedError> {
        self.transfer_articulated_to_dynamic_corpse_checked(
            id,
            expected_topology_token,
            expected_pose_token,
            density,
            |_| true,
        )
    }

    /// As [`Self::transfer_articulated_to_dynamic_corpse`], with a final
    /// value-only owner validation hook. The hook runs after exact mass,
    /// resultant motion, and every replacement shape are prepared, but before
    /// any collider/body ownership mutation. Returning false rejects atomically.
    pub fn transfer_articulated_to_dynamic_corpse_checked(
        &mut self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        expected_pose_token: u64,
        density: impl Fn(spall_core::MaterialId) -> f64,
        validate_final: impl FnOnce(&CorpseTransferOutcome) -> bool,
    ) -> Result<CorpseTransferOutcome, crate::articulated::ArticulatedError> {
        let prepared = self.prepare_articulated_batch(
            vec![ArticulatedBatchOperation::CheckedDeathTransfer {
                operation_tag: 0,
                id,
                expected_topology_token,
                expected_pose_token,
            }],
            density,
        )?;
        let transfer_outcome = prepared
            .changes
            .iter()
            .find_map(|change| {
                if let PreparedArticulatedChange::CheckedDeathTransfer { transfer, .. } = change {
                    Some(&transfer.outcome)
                } else {
                    None
                }
            })
            .expect("singleton transfer prepares one transfer");
        let accepted = validate_final(transfer_outcome);
        if !accepted {
            return Err(crate::articulated::ArticulatedError::TransferRejected);
        }
        let mut outcomes = self.commit_articulated_batch(prepared)?;
        Ok(outcomes
            .pop()
            .and_then(|outcome| outcome.transfer)
            .expect("singleton transfer includes its witness"))
    }

    fn prepare_dynamic_corpse_transfer(
        &self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        expected_pose_token: u64,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<PreparedCorpseTransfer, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let started = Instant::now();
        let body_id = id.0;
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if !assembly.kinematic_part_bodies || assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if assembly.topology_token != expected_topology_token
            || assembly.pose_token != expected_pose_token
        {
            return Err(ArticulatedError::StaleTopology);
        }
        if assembly.pending_root_pose_token.is_some() || assembly.pending_pose.is_some() {
            return Err(ArticulatedError::PendingRootPose);
        }
        let other_volumes: std::collections::BTreeSet<_> = self
            .articulated
            .iter()
            .filter(|(owner, _)| **owner != body_id)
            .flat_map(|(_, a)| a.source_volume_ids.iter().copied())
            .collect();
        if assembly.source_volume_ids.is_empty()
            || assembly
                .source_volume_ids
                .iter()
                .any(|volume| other_volumes.contains(volume))
        {
            return Err(ArticulatedError::DuplicateVolume);
        }
        if assembly.parts.is_empty()
            || assembly
                .parts
                .iter()
                .map(|p| p.grid.solid_count())
                .sum::<u64>()
                > crate::articulated::MAX_ARTICULATED_SOLID_CELLS
            || assembly.parts.iter().map(|p| p.primitives).sum::<usize>() > 65_536
        {
            return Err(ArticulatedError::WorkBudget);
        }

        let source_parts = assembly
            .parts
            .iter()
            .map(|p| SourcePart {
                part_id: p.part_id,
                parent_id: p.parent_id,
                volume_id: p.volume_id,
                source_revision: p.source_revision,
                grid: p.grid.clone(),
                cell_m: p.cell_m,
                local_transform: p.local_pose,
            })
            .collect::<Vec<_>>();
        let manifest = density_manifest(&source_parts, &density)?;
        if assembly.density_manifest.as_ref().is_some_and(|bound| {
            manifest
                .iter()
                .any(|(material, bits)| bound.get(material) != Some(bits))
        }) {
            return Err(ArticulatedError::InvalidDensity);
        }
        let local_poses = source_parts
            .iter()
            .map(|p| p.local_transform)
            .collect::<Vec<_>>();
        let stable_density = |material| manifest_density(&manifest, material);
        let mass64 =
            crate::mass::articulated_mass_properties(&source_parts, &local_poses, stable_density)
                .ok_or(ArticulatedError::InvalidDensity)?;
        let mass = mass64.to_body_properties();
        validate_body_mass_properties(mass)?;

        let entry_index = entry_index(body_id, self.id_namespace);
        let root_body = self.entries[entry_index].body;
        let root_pose = *self.bodies[root_body].position();
        let mut cell_samples = Vec::with_capacity(
            assembly
                .parts
                .iter()
                .map(|p| p.grid.solid_count())
                .sum::<u64>() as usize,
        );
        let mut total_mass = 0.0_f64;
        let mut first_moment = [0.0_f64; 3];
        let mut linear_momentum = [0.0_f64; 3];
        for part in &assembly.parts {
            let rb = &self.bodies[part.body];
            let pose = *rb.position();
            let linear = rb.linvel();
            let angular = rb.angvel();
            if ![
                linear.x, linear.y, linear.z, angular.x, angular.y, angular.z,
            ]
            .iter()
            .all(|v| v.is_finite())
            {
                return Err(ArticulatedError::InvalidTransform);
            }
            let origin = part.grid.origin();
            let s = part.cell_m;
            let mut density_error = false;
            part.grid.for_each_solid(|x, y, z, material| {
                let rho = stable_density(material);
                if !rho.is_finite() || rho <= 0.0 {
                    density_error = true;
                    return;
                }
                let m = rho * f64::from(s).powi(3);
                let local = Vector::new(
                    (origin.x as f32 + x as f32 + 0.5) * s,
                    (origin.y as f32 + y as f32 + 0.5) * s,
                    (origin.z as f32 + z as f32 + 0.5) * s,
                );
                let point = pose.transform_point(local);
                let r = point - pose.translation;
                let velocity = linear + angular.cross(r);
                let position = [point.x as f64, point.y as f64, point.z as f64];
                let v = [velocity.x as f64, velocity.y as f64, velocity.z as f64];
                total_mass += m;
                for axis in 0..3 {
                    first_moment[axis] += m * position[axis];
                    linear_momentum[axis] += m * v[axis];
                }
                cell_samples.push((
                    position,
                    v,
                    m,
                    f64::from(s),
                    [
                        f64::from(angular.x),
                        f64::from(angular.y),
                        f64::from(angular.z),
                    ],
                ));
            });
            if density_error {
                return Err(ArticulatedError::InvalidDensity);
            }
        }
        if !total_mass.is_finite() || (total_mass - mass64.mass_kg).abs() > mass64.mass_kg * 1e-5 {
            return Err(ArticulatedError::InvalidMassProperties);
        }
        let com_world = first_moment.map(|v| v / total_mass);
        let mut inertia_world = [[0.0_f64; 3]; 3];
        let mut angular_momentum = [0.0_f64; 3];
        for (position, velocity, m, s, omega) in cell_samples {
            let r = [
                position[0] - com_world[0],
                position[1] - com_world[1],
                position[2] - com_world[2],
            ];
            let p = velocity.map(|v| v * m);
            let spin = cross64(r, p);
            for axis in 0..3 {
                angular_momentum[axis] += spin[axis];
            }
            let r2 = r.iter().map(|v| v * v).sum::<f64>();
            for a in 0..3 {
                for b in 0..3 {
                    let diagonal = if a == b { 1.0 } else { 0.0 };
                    inertia_world[a][b] +=
                        m * s * s / 6.0 * diagonal + m * (diagonal * r2 - r[a] * r[b]);
                }
            }
            // The body's point velocity contains only orbital motion; a cube's
            // spin contribution is its isotropic self-inertia times omega.
            let self_i = m * s * s / 6.0;
            for axis in 0..3 {
                angular_momentum[axis] += self_i * omega[axis];
            }
        }
        let omega_world = solve_symmetric3(inertia_world, angular_momentum)
            .ok_or(ArticulatedError::InvalidMassProperties)?;
        let linear_velocity = linear_momentum.map(|v| v / total_mass);
        let com_local = root_pose.inverse_transform_point(Vector::new(
            com_world[0] as f32,
            com_world[1] as f32,
            com_world[2] as f32,
        ));
        let props = BodyMassProperties {
            mass_kg: mass.mass_kg,
            local_com_m: [com_local.x, com_local.y, com_local.z],
            inertia_com_kg_m2: mass.inertia_com_kg_m2,
        };
        validate_body_mass_properties(props)?;

        // Prepare every replacement collider and its root-local pose before
        // changing ownership. Insertion is infallible; no fallible check below
        // this point can leave a partially transferred rig.
        let prepared = assembly
            .parts
            .iter()
            .map(|part| {
                let origin = part.grid.origin();
                let origin_m = [
                    origin.x as f32 * part.cell_m,
                    origin.y as f32 * part.cell_m,
                    origin.z as f32 * part.cell_m,
                ];
                let offset = rotate(part.assembly_pose.rotation_xyzw, origin_m);
                let translation = [
                    part.assembly_pose.translation_m[0] + offset[0],
                    part.assembly_pose.translation_m[1] + offset[1],
                    part.assembly_pose.translation_m[2] + offset[2],
                ];
                let pose = Pose::new(
                    Vector::new(translation[0], translation[1], translation[2]),
                    Rotation::from_xyzw(
                        part.assembly_pose.rotation_xyzw[0],
                        part.assembly_pose.rotation_xyzw[1],
                        part.assembly_pose.rotation_xyzw[2],
                        part.assembly_pose.rotation_xyzw[3],
                    )
                    .to_scaled_axis(),
                );
                (part.collider, part.body, pose)
            })
            .collect::<Vec<_>>();
        let prepared_colliders = prepared
            .iter()
            .map(|(old, _, pose)| {
                let old_collider = &self.colliders[*old];
                let restitution = old_collider.restitution();
                let restitution_rule = old_collider.restitution_combine_rule();
                let friction = old_collider.friction();
                let friction_rule = old_collider.friction_combine_rule();
                let collision_groups = old_collider.collision_groups();
                let solver_groups = old_collider.solver_groups();
                let active_collision_types = old_collider.active_collision_types();
                let sensor = old_collider.is_sensor();
                let mut collider = ColliderBuilder::new(old_collider.shared_shape().clone())
                    .density(0.0)
                    .position(*pose)
                    .friction(friction)
                    .active_collision_types(active_collision_types)
                    .sensor(sensor)
                    .build();
                collider.set_restitution(restitution);
                collider.set_restitution_combine_rule(restitution_rule);
                collider.set_friction_combine_rule(friction_rule);
                collider.set_collision_groups(collision_groups);
                collider.set_solver_groups(solver_groups);
                collider
            })
            .collect::<Vec<_>>();
        let mut outcome = CorpseTransferOutcome {
            preparation_time: started.elapsed(),
            mass_kg: total_mass,
            com_world_m: com_world,
            linear_momentum_kg_m_s: linear_momentum,
            angular_momentum_about_com_kg_m2_s: angular_momentum,
            linear_velocity_m_s: linear_velocity,
            angular_velocity_rad_s: omega_world,
        };
        let source_volume_ids = assembly.source_volume_ids.clone();
        let primitive_count = assembly.parts.iter().try_fold(0_usize, |total, part| {
            total
                .checked_add(part.primitives)
                .ok_or(ArticulatedError::WorkBudget)
        })?;
        let snapshots = assembly
            .parts
            .iter()
            .map(|part| {
                let body = &self.bodies[part.body];
                let mut source_cells = Vec::with_capacity(part.grid.solid_count() as usize);
                part.grid
                    .for_each_solid(|x, y, z, material| source_cells.push((x, y, z, material)));
                PreparedCorpseTransferPartSnapshot {
                    body: part.body,
                    collider: part.collider,
                    position: *body.position(),
                    linvel: body.linvel(),
                    angvel: body.angvel(),
                    part_id: part.part_id,
                    volume_id: part.volume_id,
                    source_revision: part.source_revision,
                    cell_m: part.cell_m,
                    grid_origin: part.grid.origin(),
                    grid_dims: part.grid.dims(),
                    source_solid_count: part.grid.solid_count(),
                    source_cells,
                    local_pose: part.local_pose,
                    assembly_pose: part.assembly_pose,
                }
            })
            .collect();
        let mut old_bodies = prepared
            .iter()
            .map(|(_, body, _)| *body)
            .collect::<Vec<_>>();
        old_bodies.sort_by_key(|body| body.into_raw_parts());
        old_bodies.dedup();
        outcome.preparation_time = started.elapsed();
        Ok(PreparedCorpseTransfer {
            id,
            expected_topology_token,
            expected_pose_token,
            source_generation: assembly.source_generation,
            source_volume_ids,
            parts: snapshots,
            entry_body: root_body,
            entry_position: *self.bodies[root_body].position(),
            entry_linvel: self.bodies[root_body].linvel(),
            entry_angvel: self.bodies[root_body].angvel(),
            entry_collider: self.entries[entry_index].collider,
            props,
            manifest,
            prepared,
            replacement_colliders: prepared_colliders,
            primitive_count,
            outcome,
        })
    }

    fn validate_prepared_corpse_transfer(&self, transfer: &PreparedCorpseTransfer) -> bool {
        let body_id = transfer.id.0;
        let Some(assembly) = self.articulated.get(&body_id) else {
            return false;
        };
        if !assembly.kinematic_part_bodies
            || assembly.dynamic_corpse
            || assembly.topology_token != transfer.expected_topology_token
            || assembly.pose_token != transfer.expected_pose_token
            || assembly.source_generation != transfer.source_generation
            || assembly.pending_root_pose_token.is_some()
            || assembly.pending_pose.is_some()
            || assembly.source_volume_ids != transfer.source_volume_ids
            || assembly.parts.len() != transfer.parts.len()
        {
            return false;
        }
        let index = entry_index(body_id, self.id_namespace);
        let Some(entry) = self.entries.get(index) else {
            return false;
        };
        if entry.body != transfer.entry_body || entry.collider != transfer.entry_collider {
            return false;
        }
        let Some(entry_body) = self.bodies.get(transfer.entry_body) else {
            return false;
        };
        if *entry_body.position() != transfer.entry_position
            || entry_body.linvel() != transfer.entry_linvel
            || entry_body.angvel() != transfer.entry_angvel
        {
            return false;
        }
        for (part, snapshot) in assembly.parts.iter().zip(&transfer.parts) {
            let Some(body) = self.bodies.get(part.body) else {
                return false;
            };
            if part.body != snapshot.body
                || part.collider != snapshot.collider
                || part.part_id != snapshot.part_id
                || part.volume_id != snapshot.volume_id
                || part.source_revision != snapshot.source_revision
                || part.cell_m.to_bits() != snapshot.cell_m.to_bits()
                || part.grid.origin() != snapshot.grid_origin
                || part.grid.dims() != snapshot.grid_dims
                || part.grid.solid_count() != snapshot.source_solid_count
                || part.local_pose.translation_m != snapshot.local_pose.translation_m
                || part.local_pose.rotation_xyzw != snapshot.local_pose.rotation_xyzw
                || part.assembly_pose.translation_m != snapshot.assembly_pose.translation_m
                || part.assembly_pose.rotation_xyzw != snapshot.assembly_pose.rotation_xyzw
                || *body.position() != snapshot.position
                || body.linvel() != snapshot.linvel
                || body.angvel() != snapshot.angvel
            {
                return false;
            }
            let mut idx = 0usize;
            let mut same = true;
            part.grid.for_each_solid(|x, y, z, material| {
                if snapshot.source_cells.get(idx).copied() != Some((x, y, z, material)) {
                    same = false;
                }
                idx += 1;
            });
            if !same || idx != snapshot.source_cells.len() {
                return false;
            }
        }
        true
    }

    fn publish_prepared_corpse_transfer(
        &mut self,
        transfer: PreparedCorpseTransfer,
    ) -> (ArticulatedBodyId, CorpseTransferOutcome) {
        let PreparedCorpseTransfer {
            id,
            props,
            manifest,
            prepared,
            replacement_colliders,
            outcome,
            ..
        } = transfer;
        let body_id = id.0;
        let index = entry_index(body_id, self.id_namespace);
        let root_body = self.entries[index].body;
        let mut replacement_handles = Vec::with_capacity(prepared.len());
        for collider in replacement_colliders {
            let handle = self
                .colliders
                .insert_with_parent(collider, root_body, &mut self.bodies);
            self.tag_collider(handle, body_id);
            replacement_handles.push(handle);
        }
        let old_root_pose = *self.bodies[root_body].position();
        self.bodies[root_body].set_body_type(RigidBodyType::Dynamic, true);
        self.bodies[root_body]
            .set_additional_mass_properties(rapier_mass_properties(props, [0.0; 3]), true);
        self.bodies[root_body].recompute_mass_properties_from_colliders(&self.colliders);
        self.bodies[root_body].set_position(old_root_pose, true);
        self.bodies[root_body].set_linvel(
            Vector::new(
                outcome.linear_velocity_m_s[0] as f32,
                outcome.linear_velocity_m_s[1] as f32,
                outcome.linear_velocity_m_s[2] as f32,
            ),
            true,
        );
        self.bodies[root_body].set_angvel(
            Vector::new(
                outcome.angular_velocity_rad_s[0] as f32,
                outcome.angular_velocity_rad_s[1] as f32,
                outcome.angular_velocity_rad_s[2] as f32,
            ),
            true,
        );
        let mut old_bodies = prepared
            .iter()
            .map(|(_, body, _)| *body)
            .collect::<Vec<_>>();
        old_bodies.sort_by_key(|body| body.into_raw_parts());
        old_bodies.dedup();
        for (old, _, _) in &prepared {
            self.colliders
                .remove(*old, &mut self.islands, &mut self.bodies, true);
            self.pending_removed.push(*old);
        }
        for old in old_bodies {
            self.bodies.remove(
                old,
                &mut self.islands,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                true,
            );
        }
        self.ccd_solver = CCDSolver::new();
        self.pending_modified
            .extend(replacement_handles.iter().copied());
        let assembly = self
            .articulated
            .get_mut(&body_id)
            .expect("batch transfer was prevalidated");
        for (part, handle) in assembly
            .parts
            .iter_mut()
            .zip(replacement_handles.iter().copied())
        {
            part.body = root_body;
            part.collider = handle;
        }
        assembly.kinematic_part_bodies = false;
        assembly.dynamic_corpse = true;
        assembly.density_manifest = Some(manifest);
        let entry = &mut self.entries[index];
        entry.body = root_body;
        entry.collider = replacement_handles[0];
        entry.collider_offset_m = [0.0; 3];
        entry.density = 0.0;
        entry.mass_properties = Some(props);
        (id, outcome)
    }

    /// Captures one live physical component for bounded immutable fracture
    /// planning. After the initial split every capture targets one child ID.
    pub fn capture_dynamic_corpse_fracture_snapshot(
        &self,
        id: ArticulatedBodyId,
        corpse_key: u128,
        component_key: Option<DerivedFractureComponentId>,
    ) -> Result<FractureSnapshot, FractureError> {
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(FractureError::StaleOwner)?;
        if !assembly.dynamic_corpse
            || corpse_key == 0
            || self
                .fracture_owner_keys
                .get(&corpse_key)
                .is_some_and(|owner| *owner != id.0)
            || assembly
                .fracture_owner_key
                .is_some_and(|key| key != corpse_key)
            || component_key.is_some_and(|key| {
                key.owner_key != corpse_key
                    || key.serial == 0
                    || key.serial >= assembly.next_component_serial
            })
        {
            return Err(FractureError::StaleOwner);
        }
        let selected = assembly
            .parts
            .iter()
            .filter(|part| part.component_id == component_key)
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Err(FractureError::InvalidSnapshot);
        }
        let mut part_ids = HashSet::new();
        let mut volume_ids = HashSet::new();
        if selected
            .iter()
            .any(|part| !part_ids.insert(part.part_id) || !volume_ids.insert(part.volume_id))
        {
            return Err(FractureError::InvalidSnapshot);
        }
        Ok(FractureSnapshot {
            token: FractureSnapshotToken {
                corpse_key,
                topology_revision: assembly.topology_token,
                pose_epoch: self.step_count,
                source_generation: assembly.source_generation,
                component_key,
            },
            parts: selected
                .into_iter()
                .map(|part| FracturePartSnapshot {
                    source: SourcePart {
                        part_id: part.part_id,
                        parent_id: None,
                        volume_id: part.volume_id,
                        source_revision: part.source_revision,
                        grid: part.grid.clone(),
                        cell_m: part.cell_m,
                        local_transform: part.assembly_pose,
                    },
                    root_local_pose: part.assembly_pose,
                })
                .collect(),
        })
    }

    /// Atomically replaces exactly one frozen physical component with its
    /// prepared connected components. Source volumes stay under this one
    /// logical owner; each collider shard preserves original cell provenance.
    pub fn commit_dynamic_corpse_fracture(
        &mut self,
        id: ArticulatedBodyId,
        snapshot: &FractureSnapshot,
        plan: &PreparedFracturePlan,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<FractureCommitOutcome, FractureError> {
        let started = Instant::now();
        let body_id = id.0;
        let current = self.capture_dynamic_corpse_fracture_snapshot(
            id,
            snapshot.token.corpse_key,
            snapshot.token.component_key,
        )?;
        if snapshot.token != current.token || !fracture_snapshots_equal(snapshot, &current) {
            return Err(FractureError::StaleOwner);
        }
        if plan.version != FRACTURE_PLAN_VERSION
            || plan.token != snapshot.token
            || plan.components.len() < 2
        {
            return Err(FractureError::InvalidPlan);
        }
        let canonical = prepare_fracture(snapshot)?;
        if canonical.source_cells != plan.source_cells || canonical.components != plan.components {
            return Err(FractureError::InvalidPlan);
        }
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(FractureError::StaleOwner)?;
        let manifest = assembly
            .density_manifest
            .clone()
            .ok_or(FractureError::InvalidMass)?;
        if snapshot.parts.iter().any(|part| {
            let mut mismatch = false;
            part.source.grid.for_each_solid(|_, _, _, material| {
                mismatch |= manifest
                    .get(&material)
                    .is_none_or(|bits| density(material).to_bits() != *bits);
            });
            mismatch
        }) {
            return Err(FractureError::InvalidMass);
        }
        let stable_density = |material| manifest_density(&manifest, material);
        let target = assembly
            .parts
            .iter()
            .filter(|part| part.component_id == snapshot.token.component_key)
            .cloned()
            .collect::<Vec<_>>();
        if assembly.pending_pose.is_some()
            || assembly.pending_root_pose_token.is_some()
            || target.len() != snapshot.parts.len()
            || target.iter().zip(&snapshot.parts).any(|(part, snapshot)| {
                part.part_id != snapshot.source.part_id || part.body != target[0].body
            })
            || assembly
                .fracture_owner_key
                .is_some_and(|key| key != snapshot.token.corpse_key)
            || self.articulated.iter().any(|(other_id, other)| {
                *other_id != body_id && other.fracture_owner_key == Some(snapshot.token.corpse_key)
            })
            || self
                .fracture_owner_keys
                .get(&snapshot.token.corpse_key)
                .is_some_and(|owner| *owner != body_id)
        {
            return Err(FractureError::StaleOwner);
        }
        let next_serial = assembly
            .next_component_serial
            .checked_add(plan.components.len() as u64)
            .ok_or(FractureError::WorkBudget)?;
        let next_source_generation = assembly
            .source_generation
            .checked_add(1)
            .ok_or(FractureError::WorkBudget)?;
        let next_topology_revision = assembly
            .topology_token
            .checked_add(1)
            .ok_or(FractureError::WorkBudget)?;
        let component_ids = (0..plan.components.len())
            .map(|offset| DerivedFractureComponentId {
                owner_key: snapshot.token.corpse_key,
                serial: assembly.next_component_serial + offset as u64,
            })
            .collect::<Vec<_>>();
        if component_ids.iter().any(|component| component.serial == 0) {
            return Err(FractureError::InvalidPlan);
        }
        let live_components = assembly
            .parts
            .iter()
            .filter_map(|part| part.component_id)
            .collect::<HashSet<_>>()
            .len()
            .max(1);
        if live_components
            .checked_sub(1)
            .and_then(|count| count.checked_add(plan.components.len()))
            .is_none_or(|count| count > crate::fracture::MAX_FRACTURE_COMPONENTS)
        {
            return Err(FractureError::WorkBudget);
        }
        let entry_idx = entry_index(body_id, self.id_namespace);
        let entry = &self.entries[entry_idx];
        let representation = entry.representation;
        let query_only = entry.query_only;
        let old_body = target[0].body;
        let old_pose = *self.bodies[old_body].position();
        let old_sources = target
            .iter()
            .map(|part| SourcePart {
                part_id: part.part_id,
                parent_id: None,
                volume_id: part.volume_id,
                source_revision: part.source_revision,
                grid: part.grid.clone(),
                cell_m: part.cell_m,
                local_transform: part.assembly_pose,
            })
            .collect::<Vec<_>>();
        let old_poses = old_sources
            .iter()
            .map(|part| part.local_transform)
            .collect::<Vec<_>>();
        let old_mass =
            crate::mass::articulated_mass_properties(&old_sources, &old_poses, stable_density)
                .ok_or(FractureError::InvalidMass)?
                .to_body_properties();
        validate_body_mass_properties(old_mass).map_err(|_| FractureError::InvalidMass)?;
        // Re-derive from the canonical source cells. Rapier's cached COM can
        // be uninitialized on a newly-created aggregate and is not a source
        // ownership oracle.
        let old_com = old_pose.transform_point(Vector::new(
            old_mass.local_com_m[0],
            old_mass.local_com_m[1],
            old_mass.local_com_m[2],
        ));
        let old_linear = self.bodies[old_body].linvel();
        let old_angular = self.bodies[old_body].angvel();

        struct PreparedShard {
            part: ArticulatedPartEntry,
            collider: Collider,
        }
        struct PreparedChild {
            component_id: DerivedFractureComponentId,
            mass: BodyMassProperties,
            linear: Vector,
            angular: Vector,
            shards: Vec<PreparedShard>,
        }
        let mut prepared = Vec::with_capacity(plan.components.len());
        let mut primitive_count = 0usize;
        let mut derived_dense_count = 0usize;
        for (component_index, component) in plan.components.iter().enumerate() {
            let refs = component.cells.iter().copied().collect::<HashSet<_>>();
            if refs.len() != component.cells.len() || refs.is_empty() {
                return Err(FractureError::InvalidPlan);
            }
            let mut sources = Vec::new();
            let mut poses = Vec::new();
            let mut shards = Vec::new();
            for (part_index, old_part) in target.iter().enumerate() {
                let source = &snapshot.parts[part_index].source;
                let origin = old_part.grid.origin();
                let mut owned = Vec::new();
                old_part.grid.for_each_solid(|x, y, z, material| {
                    let coord = [
                        origin.x.checked_add(i64::from(x)),
                        origin.y.checked_add(i64::from(y)),
                        origin.z.checked_add(i64::from(z)),
                    ];
                    let Ok(cx) = i32::try_from(coord[0].unwrap_or(i64::MIN)) else {
                        return;
                    };
                    let Ok(cy) = i32::try_from(coord[1].unwrap_or(i64::MIN)) else {
                        return;
                    };
                    let Ok(cz) = i32::try_from(coord[2].unwrap_or(i64::MIN)) else {
                        return;
                    };
                    let cell = SourceCellRef {
                        volume_id: old_part.volume_id,
                        part_id: old_part.part_id,
                        cell: [cx, cy, cz],
                        material,
                        source_revision: old_part.source_revision,
                    };
                    if refs.contains(&cell) {
                        owned.push((x, y, z, material));
                    }
                });
                if owned.is_empty() {
                    continue;
                }
                let min = std::array::from_fn::<_, 3, _>(|axis| {
                    owned
                        .iter()
                        .map(|cell| [cell.0, cell.1, cell.2][axis])
                        .min()
                        .unwrap()
                });
                let max = std::array::from_fn::<_, 3, _>(|axis| {
                    owned
                        .iter()
                        .map(|cell| [cell.0, cell.1, cell.2][axis])
                        .max()
                        .unwrap()
                });
                let dims = [
                    max[0] - min[0] + 1,
                    max[1] - min[1] + 1,
                    max[2] - min[2] + 1,
                ];
                let dense = (dims[0] as usize)
                    .checked_mul(dims[1] as usize)
                    .and_then(|v| v.checked_mul(dims[2] as usize))
                    .ok_or(FractureError::WorkBudget)?;
                derived_dense_count = derived_dense_count
                    .checked_add(dense)
                    .ok_or(FractureError::WorkBudget)?;
                if derived_dense_count > crate::fracture::MAX_FRACTURE_DENSE_CELLS {
                    return Err(FractureError::WorkBudget);
                }
                let mut mask = vec![false; dense];
                let mut materials = vec![MaterialId::AIR; dense];
                for (x, y, z, material) in owned {
                    let local = [x - min[0], y - min[1], z - min[2]];
                    let index = local[0] as usize
                        + dims[0] as usize
                            * (local[1] as usize + dims[1] as usize * local[2] as usize);
                    mask[index] = true;
                    materials[index] = material;
                }
                let cropped_origin = GlobalCell::new(
                    origin.x + i64::from(min[0]),
                    origin.y + i64::from(min[1]),
                    origin.z + i64::from(min[2]),
                );
                let grid = OccupancyGrid::from_solid_mask(cropped_origin, dims, mask, materials)
                    .map_err(|_| FractureError::InvalidPlan)?;
                let source_part = SourcePart {
                    part_id: source.part_id,
                    parent_id: None,
                    volume_id: source.volume_id,
                    source_revision: source.source_revision,
                    grid: grid.clone(),
                    cell_m: source.cell_m,
                    local_transform: snapshot.parts[part_index].root_local_pose,
                };
                sources.push(source_part);
                poses.push(snapshot.parts[part_index].root_local_pose);
                let built = build_collider(&grid, old_part.cell_m, representation);
                primitive_count = primitive_count
                    .checked_add(built.primitives)
                    .ok_or(FractureError::WorkBudget)?;
                if built.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET
                    || primitive_count > 65_536
                {
                    return Err(FractureError::WorkBudget);
                }
                let old_collider = self
                    .colliders
                    .get(old_part.collider)
                    .ok_or(FractureError::StaleOwner)?;
                let origin = grid.origin();
                let origin_m = [
                    origin.x as f32 * old_part.cell_m,
                    origin.y as f32 * old_part.cell_m,
                    origin.z as f32 * old_part.cell_m,
                ];
                let pose = snapshot.parts[part_index].root_local_pose;
                let offset = rotate(pose.rotation_xyzw, origin_m);
                let local = Pose::new(
                    Vector::new(
                        pose.translation_m[0] + offset[0],
                        pose.translation_m[1] + offset[1],
                        pose.translation_m[2] + offset[2],
                    ),
                    Rotation::from_xyzw(
                        pose.rotation_xyzw[0],
                        pose.rotation_xyzw[1],
                        pose.rotation_xyzw[2],
                        pose.rotation_xyzw[3],
                    )
                    .to_scaled_axis(),
                );
                let mut collider = ColliderBuilder::new(built.collider.shared_shape().clone())
                    .density(0.0)
                    .position(local)
                    .friction(old_collider.friction())
                    .active_collision_types(old_collider.active_collision_types())
                    .sensor(old_collider.is_sensor())
                    .build();
                collider.set_restitution(old_collider.restitution());
                collider.set_restitution_combine_rule(old_collider.restitution_combine_rule());
                collider.set_friction_combine_rule(old_collider.friction_combine_rule());
                collider.set_collision_groups(old_collider.collision_groups());
                collider.set_solver_groups(if query_only {
                    query_only_solver_groups()
                } else {
                    old_collider.solver_groups()
                });
                let part_surface_radius_m =
                    source_surface_radius_m(&grid, old_part.cell_m, [0.0; 3], None);
                let root_surface_radius_m =
                    source_surface_radius_m(&grid, old_part.cell_m, [0.0; 3], Some(pose));
                shards.push(PreparedShard {
                    part: ArticulatedPartEntry {
                        part_id: old_part.part_id,
                        parent_id: None,
                        volume_id: old_part.volume_id,
                        source_revision: old_part.source_revision,
                        grid,
                        cell_m: old_part.cell_m,
                        local_pose: pose,
                        assembly_pose: pose,
                        part_surface_radius_m,
                        root_surface_radius_m,
                        body: old_body,
                        collider: old_part.collider,
                        primitives: built.primitives,
                        component_id: Some(component_ids[component_index]),
                    },
                    collider,
                });
            }
            let mass = crate::mass::articulated_mass_properties(&sources, &poses, stable_density)
                .ok_or(FractureError::InvalidMass)?
                .to_body_properties();
            validate_body_mass_properties(mass).map_err(|_| FractureError::InvalidMass)?;
            let com = old_pose.transform_point(Vector::new(
                mass.local_com_m[0],
                mass.local_com_m[1],
                mass.local_com_m[2],
            ));
            let linear = old_linear + old_angular.cross(com - old_com);
            if shards.is_empty()
                || !linear.x.is_finite()
                || !linear.y.is_finite()
                || !linear.z.is_finite()
            {
                return Err(FractureError::InvalidMass);
            }
            prepared.push(PreparedChild {
                component_id: component_ids[component_index],
                mass,
                linear,
                angular: old_angular,
                shards,
            });
        }

        let mut total_parts = prepared
            .iter()
            .map(|child| child.shards.len())
            .sum::<usize>();
        let mut total_dense = derived_dense_count as u64;
        let mut total_solid = plan.source_cells.len() as u64;
        let mut total_primitives = primitive_count;
        for part in assembly
            .parts
            .iter()
            .filter(|part| part.component_id != snapshot.token.component_key)
        {
            total_parts = total_parts
                .checked_add(1)
                .ok_or(FractureError::WorkBudget)?;
            let dims = part.grid.dims();
            let dense = u64::from(dims[0])
                .checked_mul(u64::from(dims[1]))
                .and_then(|v| v.checked_mul(u64::from(dims[2])))
                .ok_or(FractureError::WorkBudget)?;
            total_dense = total_dense
                .checked_add(dense)
                .ok_or(FractureError::WorkBudget)?;
            total_solid = total_solid
                .checked_add(part.grid.solid_count())
                .ok_or(FractureError::WorkBudget)?;
            total_primitives = total_primitives
                .checked_add(part.primitives)
                .ok_or(FractureError::WorkBudget)?;
        }
        if total_parts > MAX_ARTICULATED_PARTS
            || total_dense > crate::articulated::MAX_ARTICULATED_DENSE_CELLS
            || total_solid > crate::articulated::MAX_ARTICULATED_SOLID_CELLS
            || total_primitives > 65_536
        {
            return Err(FractureError::WorkBudget);
        }

        // All partitions, shapes, masses, transforms, and stable IDs are now
        // validated. Only infallible Rapier insert/remove operations follow.
        let old_colliders = target.iter().map(|part| part.collider).collect::<Vec<_>>();
        let old_bodies = target.iter().map(|part| part.body).collect::<HashSet<_>>();
        let entry_body_was_replaced = old_bodies.contains(&self.entries[entry_idx].body);
        let mut committed = Vec::with_capacity(prepared.len());
        for child in prepared {
            let child_linear = child.linear;
            let child_angular = child.angular;
            let body = self.bodies.insert(
                RigidBodyBuilder::dynamic()
                    .pose(old_pose)
                    .additional_mass_properties(rapier_mass_properties(child.mass, [0.0; 3]))
                    .linvel(child.linear)
                    .angvel(child.angular)
                    .build(),
            );
            let mut parts = Vec::with_capacity(child.shards.len());
            for shard in child.shards {
                let handle =
                    self.colliders
                        .insert_with_parent(shard.collider, body, &mut self.bodies);
                self.tag_collider(handle, body_id);
                self.pending_modified.push(handle);
                let mut part = shard.part;
                part.body = body;
                part.collider = handle;
                parts.push(part);
            }
            self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
            // Collider insertion/recomputation may shift Rapier's COM and
            // adjust the body's origin velocity. Install the prepared COM
            // velocity field only after the final mass frame is known.
            self.bodies[body].set_linvel(child_linear, true);
            self.bodies[body].set_angvel(child_angular, true);
            committed.push((child.component_id, child.mass, body, parts));
        }
        for collider in old_colliders {
            self.colliders
                .remove(collider, &mut self.islands, &mut self.bodies, true);
            self.pending_removed.push(collider);
        }
        for body in old_bodies {
            self.bodies.remove(
                body,
                &mut self.islands,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                true,
            );
        }
        if entry_body_was_replaced {
            self.entries[entry_idx].body = committed[0].2;
            self.entries[entry_idx].collider = committed[0].3[0].collider;
            self.entries[entry_idx].mass_properties = Some(committed[0].1);
        }
        let assembly = self
            .articulated
            .get_mut(&body_id)
            .expect("validated assembly");
        assembly
            .parts
            .retain(|part| part.component_id != snapshot.token.component_key);
        let mut outcomes = Vec::with_capacity(committed.len());
        for (component_id, mass, _, parts) in committed {
            assembly.parts.extend(parts);
            outcomes.push((component_id, f64::from(mass.mass_kg)));
        }
        assembly.fracture_owner_key = Some(snapshot.token.corpse_key);
        assembly.next_component_serial = next_serial;
        assembly.dynamic_corpse = true;
        assembly.kinematic_part_bodies = false;
        assembly.source_generation = next_source_generation;
        assembly.topology_token = next_topology_revision;
        self.fracture_owner_keys
            .insert(snapshot.token.corpse_key, body_id);
        self.ccd_solver = CCDSolver::new();
        Ok(FractureCommitOutcome {
            elapsed: started.elapsed(),
            source_cells: plan.source_cells.len(),
            components: outcomes,
        })
    }

    fn add_articulated_body_mode(
        &mut self,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
        kinematic_part_bodies: bool,
        dynamic_mass: Option<BodyMassProperties>,
    ) -> Result<ArticulatedBodyId, crate::articulated::ArticulatedError> {
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_SOLID_CELLS,
        };
        if parts.is_empty() || parts.len() > MAX_ARTICULATED_PARTS {
            return Err(ArticulatedError::PartCount);
        }
        if !articulated_translation_in_range(translation_m)
            || !(PartTransform {
                translation_m: [0.0; 3],
                rotation_xyzw,
            })
            .is_valid()
            || representation == Representation::SmoothConvex
        {
            return Err(ArticulatedError::InvalidTransform);
        }
        let rotation_xyzw = normalize(rotation_xyzw);
        let already_owned_volumes: std::collections::BTreeSet<_> = self
            .articulated
            .values()
            .flat_map(|assembly| assembly.source_volume_ids.iter().copied())
            .collect();
        let mut sorted = parts;
        sorted.sort_by_key(|part| part.part_id);
        let mut ids = std::collections::BTreeSet::new();
        let mut volume_ids = std::collections::BTreeSet::new();
        let mut roots = 0usize;
        let mut dense_cells = 0u64;
        let mut solid_cells = 0u64;
        let cell_m = sorted[0].cell_m;
        for part in &mut sorted {
            if part.part_id == 0 || !ids.insert(part.part_id) {
                return Err(ArticulatedError::DuplicatePart);
            }
            if !volume_ids.insert(part.volume_id) || already_owned_volumes.contains(&part.volume_id)
            {
                return Err(ArticulatedError::DuplicateVolume);
            }
            if part.parent_id.is_none() {
                roots += 1;
            }
            if !part.local_transform.is_valid()
                || !part.cell_m.is_finite()
                || part.cell_m <= 0.0
                || (part.cell_m - cell_m).abs() > 1.0e-7
            {
                return Err(ArticulatedError::InvalidTransform);
            }
            if !articulated_translation_in_range(part.local_transform.translation_m)
                || !articulated_grid_range_valid(&part.grid, part.cell_m)
            {
                return Err(ArticulatedError::GeometryRange);
            }
            part.local_transform = part
                .local_transform
                .normalized()
                .ok_or(ArticulatedError::InvalidTransform)?;
            let dims = part.grid.dims();
            let dense = u64::from(dims[0])
                .checked_mul(u64::from(dims[1]))
                .and_then(|v| v.checked_mul(u64::from(dims[2])))
                .ok_or(ArticulatedError::WorkBudget)?;
            dense_cells = dense_cells
                .checked_add(dense)
                .ok_or(ArticulatedError::WorkBudget)?;
            solid_cells = solid_cells
                .checked_add(part.grid.solid_count())
                .ok_or(ArticulatedError::WorkBudget)?;
            if part.grid.solid_count() == 0 {
                return Err(ArticulatedError::EmptyPart(part.part_id));
            }
            if dense_cells > MAX_ARTICULATED_DENSE_CELLS
                || solid_cells > MAX_ARTICULATED_SOLID_CELLS
            {
                return Err(ArticulatedError::WorkBudget);
            }
        }
        if roots != 1
            || sorted
                .iter()
                .any(|part| part.parent_id.is_some_and(|parent| !ids.contains(&parent)))
        {
            return Err(ArticulatedError::Hierarchy);
        }
        let flattened = flatten_source_parts(&sorted)?;
        if flattened
            .iter()
            .any(|pose| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }
        let source_volume_ids = sorted.iter().map(|part| part.volume_id).collect();
        let builds: Vec<_> = sorted
            .iter()
            .map(|part| build_collider(&part.grid, cell_m, representation))
            .collect();
        let primitive_count = builds.iter().map(|build| build.primitives).sum::<usize>();
        if builds
            .iter()
            .any(|build| build.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET)
            || primitive_count > 65_536
        {
            return Err(ArticulatedError::WorkBudget);
        }
        let collider_poses: Vec<_> = sorted
            .iter()
            .zip(&flattened)
            .map(|(part, pose)| {
                let origin = part.grid.origin();
                let origin_m = [
                    origin.x as f32 * cell_m,
                    origin.y as f32 * cell_m,
                    origin.z as f32 * cell_m,
                ];
                let offset = rotate(pose.rotation_xyzw, origin_m);
                let translation = [
                    pose.translation_m[0] + offset[0],
                    pose.translation_m[1] + offset[1],
                    pose.translation_m[2] + offset[2],
                ];
                (translation, pose.rotation_xyzw)
            })
            .collect();
        if collider_poses.iter().any(|(translation, rotation)| {
            !translation.iter().all(|v| v.is_finite())
                || !rotation.iter().all(|v| v.is_finite())
                || !rotation.iter().map(|v| v * v).sum::<f32>().is_finite()
        }) {
            return Err(ArticulatedError::InvalidTransform);
        }
        if collider_poses.iter().any(|(local_translation, _)| {
            let rotated = rotate(rotation_xyzw, *local_translation);
            !articulated_translation_in_range(*local_translation)
                || !articulated_translation_in_range([
                    translation_m[0] + rotated[0],
                    translation_m[1] + rotated[1],
                    translation_m[2] + rotated[2],
                ])
        }) {
            return Err(ArticulatedError::GeometryRange);
        }

        let root_rotation = Rotation::from_xyzw(
            rotation_xyzw[0],
            rotation_xyzw[1],
            rotation_xyzw[2],
            rotation_xyzw[3],
        );
        let root_builder = if let Some(mass) = dynamic_mass {
            RigidBodyBuilder::dynamic()
                .additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]))
        } else {
            RigidBodyBuilder::kinematic_position_based()
        };
        let root = self.bodies.insert(
            root_builder
                .translation(Vector::new(
                    translation_m[0],
                    translation_m[1],
                    translation_m[2],
                ))
                .rotation(root_rotation.to_scaled_axis())
                .build(),
        );
        let id = BodyId((u64::from(self.id_namespace) << 32) | self.entries.len() as u64);
        let mut built_parts = Vec::with_capacity(sorted.len());
        for (((part, pose), (local_translation, local_rotation_xyzw)), build) in sorted
            .into_iter()
            .zip(flattened)
            .zip(collider_poses)
            .zip(builds)
        {
            let part_surface_radius_m = source_surface_radius_m(&part.grid, cell_m, [0.0; 3], None);
            let root_surface_radius_m =
                source_surface_radius_m(&part.grid, cell_m, [0.0; 3], Some(pose));
            let local_rotation = Rotation::from_xyzw(
                local_rotation_xyzw[0],
                local_rotation_xyzw[1],
                local_rotation_xyzw[2],
                local_rotation_xyzw[3],
            );
            let (part_body, collider_pose) = if kinematic_part_bodies {
                let world_pose = compose(
                    PartTransform {
                        translation_m,
                        rotation_xyzw,
                    },
                    pose,
                );
                let body_pose = Pose::new(
                    Vector::new(
                        world_pose.translation_m[0],
                        world_pose.translation_m[1],
                        world_pose.translation_m[2],
                    ),
                    Rotation::from_xyzw(
                        world_pose.rotation_xyzw[0],
                        world_pose.rotation_xyzw[1],
                        world_pose.rotation_xyzw[2],
                        world_pose.rotation_xyzw[3],
                    )
                    .to_scaled_axis(),
                );
                let body = self.bodies.insert(
                    RigidBodyBuilder::kinematic_position_based()
                        .pose(body_pose)
                        .build(),
                );
                let origin = part.grid.origin();
                let origin_m = [
                    origin.x as f32 * cell_m,
                    origin.y as f32 * cell_m,
                    origin.z as f32 * cell_m,
                ];
                (body, Pose::from_translation(Vector::from(origin_m)))
            } else {
                (
                    root,
                    Pose::new(
                        Vector::new(
                            local_translation[0],
                            local_translation[1],
                            local_translation[2],
                        ),
                        local_rotation.to_scaled_axis(),
                    ),
                )
            };
            let mut collider_builder = ColliderBuilder::new(build.collider.shared_shape().clone())
                .density(0.0)
                .position(collider_pose);
            if kinematic_part_bodies {
                collider_builder = collider_builder.active_collision_types(
                    ActiveCollisionTypes::default() | ActiveCollisionTypes::KINEMATIC_FIXED,
                );
            }
            let collider = collider_builder.build();
            let handle = self
                .colliders
                .insert_with_parent(collider, part_body, &mut self.bodies);
            self.pending_modified.push(handle);
            built_parts.push(ArticulatedPartEntry {
                part_id: part.part_id,
                parent_id: part.parent_id,
                volume_id: part.volume_id,
                source_revision: part.source_revision,
                grid: part.grid,
                cell_m,
                local_pose: part.local_transform,
                assembly_pose: pose,
                part_surface_radius_m,
                root_surface_radius_m,
                body: part_body,
                collider: handle,
                primitives: build.primitives,
                component_id: None,
            });
        }
        let primary = built_parts[0].collider;
        let first_origin = built_parts[0].grid.origin();
        self.entries.push(Entry {
            body: root,
            collider: primary,
            cell_m,
            representation,
            density: 0.0,
            collider_offset_m: [
                first_origin.x as f32 * cell_m,
                first_origin.y as f32 * cell_m,
                first_origin.z as f32 * cell_m,
            ],
            mass_properties: dynamic_mass,
            retired: false,
            dormant: false,
            query_only: false,
            restitution: 0.0,
        });
        for part in &built_parts {
            self.tag_collider(part.collider, id);
        }
        self.articulated.insert(
            id,
            ArticulatedEntry {
                topology_token,
                pose_token: 0,
                root_pose: PartTransform {
                    translation_m,
                    rotation_xyzw,
                },
                pending_root_pose_token: None,
                pending_pose: None,
                kinematic_part_bodies,
                dynamic_corpse: dynamic_mass.is_some(),
                source_generation: 1,
                fracture_owner_key: None,
                next_component_serial: 1,
                density_manifest: None,
                source_volume_ids,
                parts: built_parts,
            },
        );
        if let Some(mass) = dynamic_mass {
            let pose = *self.bodies[root].position();
            self.bodies[root]
                .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
            self.bodies[root].recompute_mass_properties_from_colliders(&self.colliders);
            self.bodies[root].set_position(pose, true);
        }
        Ok(ArticulatedBodyId(id))
    }

    /// Atomically installs a complete sampled part-pose set after validating
    /// both version tokens and all transforms. Invalid/stale updates do not
    /// mutate any collider or token.
    pub fn update_articulated_pose(
        &mut self,
        id: ArticulatedBodyId,
        update: ArticulatedPose,
    ) -> Result<(), crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let id = id.0;
        let current = self
            .articulated
            .get(&id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if current.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if update.topology_token != current.topology_token
            || current.pending_root_pose_token.is_some()
            || current.pending_pose.is_some()
            || update.base_pose_token != current.pose_token
            || update.pose_token <= current.pose_token
            || update.parts.len() != current.parts.len()
            || update
                .parts
                .iter()
                .zip(&current.parts)
                .any(|((part_id, pose), part)| {
                    *part_id != part.part_id || pose.normalized().is_none()
                })
        {
            return Err(ArticulatedError::StalePose);
        }
        if update
            .parts
            .iter()
            .any(|(_, pose)| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }
        let locals: Vec<_> = update
            .parts
            .iter()
            .map(|(id, pose)| {
                pose.normalized()
                    .map(|pose| (*id, pose))
                    .ok_or(ArticulatedError::InvalidTransform)
            })
            .collect::<Result<_, _>>()?;
        let flattened = flatten_local_poses(&current.parts, &locals)?;
        if flattened.iter().any(|pose| !pose.is_valid()) {
            return Err(ArticulatedError::InvalidTransform);
        }
        if flattened
            .iter()
            .any(|pose| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }
        let prepared: Vec<_> = current
            .parts
            .iter()
            .zip(locals.iter().zip(flattened.iter()))
            .map(|(part, ((part_id, local), world))| {
                let origin = part.grid.origin();
                let origin_m = [
                    origin.x as f32 * part.cell_m,
                    origin.y as f32 * part.cell_m,
                    origin.z as f32 * part.cell_m,
                ];
                let offset = rotate(world.rotation_xyzw, origin_m);
                let translation = [
                    world.translation_m[0] + offset[0],
                    world.translation_m[1] + offset[1],
                    world.translation_m[2] + offset[2],
                ];
                (*part_id == part.part_id && articulated_translation_in_range(translation))
                    .then_some((*local, *world, translation))
            })
            .collect();
        if prepared.len() != current.parts.len() || prepared.iter().any(Option::is_none) {
            return Err(ArticulatedError::InvalidTransform);
        }
        if current.kinematic_part_bodies {
            let root =
                self.bodies[self.entries[entry_index(id, self.id_namespace)].body].position();
            let targets = current
                .parts
                .iter()
                .zip(prepared.iter().flatten())
                .map(|(part, (_, world, _))| {
                    let local = Pose::new(
                        Vector::new(
                            world.translation_m[0],
                            world.translation_m[1],
                            world.translation_m[2],
                        ),
                        Rotation::from_xyzw(
                            world.rotation_xyzw[0],
                            world.rotation_xyzw[1],
                            world.rotation_xyzw[2],
                            world.rotation_xyzw[3],
                        )
                        .to_scaled_axis(),
                    );
                    (part.body, *root * local)
                })
                .collect::<Vec<_>>();
            if targets.iter().any(|(_, pose)| {
                !articulated_translation_in_range([
                    pose.translation.x,
                    pose.translation.y,
                    pose.translation.z,
                ])
            }) {
                return Err(ArticulatedError::GeometryRange);
            }
            self.validate_kinematic_surface_travel(&targets)?;
            for (body, pose) in targets {
                self.bodies[body].set_next_kinematic_position(pose);
            }
            let pending = locals
                .into_iter()
                .zip(flattened)
                .map(|((part_id, local), world)| (part_id, local, world))
                .collect::<Vec<_>>();
            self.articulated
                .get_mut(&id)
                .expect("validated assembly exists")
                .pending_pose = Some(PendingArticulatedPose {
                pose_token: update.pose_token,
                parts: pending,
            });
            return Ok(());
        }
        let root = self.bodies[self.entries[entry_index(id, self.id_namespace)].body].position();
        if prepared.iter().flatten().any(|(_, _, local_translation)| {
            let world_offset = root.transform_vector(Vector::new(
                local_translation[0],
                local_translation[1],
                local_translation[2],
            ));
            let world = root.translation + world_offset;
            !articulated_translation_in_range([world.x, world.y, world.z])
        }) {
            return Err(ArticulatedError::InvalidTransform);
        }
        let assembly = self
            .articulated
            .get_mut(&id)
            .expect("validated assembly exists");
        for (part, prepared) in assembly
            .parts
            .iter_mut()
            .zip(prepared.into_iter().flatten())
        {
            let (local, world, translation) = prepared;
            let rotation = Rotation::from_xyzw(
                world.rotation_xyzw[0],
                world.rotation_xyzw[1],
                world.rotation_xyzw[2],
                world.rotation_xyzw[3],
            );
            let collider_local = Pose::new(
                Vector::new(translation[0], translation[1], translation[2]),
                rotation.to_scaled_axis(),
            );
            let collider = &mut self.colliders[part.collider];
            collider.set_position_wrt_parent(collider_local);
            // Rapier propagates collider-local changes during its next step.
            // Queries at this tick boundary need the installed pose now, even
            // when the unchanged parent is no longer in its modified-body list.
            // Preserve the native dirty flags for the real solver/broad phase.
            collider.set_position(*root * collider_local);
            part.local_pose = local.normalized().expect("validated local pose");
            part.assembly_pose = world;
            self.pending_modified.push(part.collider);
        }
        assembly.pose_token = update.pose_token;
        Ok(())
    }

    /// Rebuilds one source-part shape after an authoritative cell edit. The
    /// new collider is fully prepared and budget-checked before removing the
    /// old shape; stale topology/revision tokens leave the old shape untouched.
    pub fn rebuild_articulated_part_grid(
        &mut self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        new_topology_token: u64,
        part_id: u32,
        expected_source_revision: u64,
        new_source_revision: u64,
        new_grid: OccupancyGrid,
    ) -> Result<Duration, crate::articulated::ArticulatedError> {
        self.rebuild_articulated_part_grid_impl(
            id,
            expected_topology_token,
            new_topology_token,
            part_id,
            expected_source_revision,
            new_source_revision,
            new_grid,
            false,
        )
    }

    fn rebuild_articulated_part_grid_impl(
        &mut self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        new_topology_token: u64,
        part_id: u32,
        expected_source_revision: u64,
        new_source_revision: u64,
        new_grid: OccupancyGrid,
        dynamic_allowed: bool,
    ) -> Result<Duration, crate::articulated::ArticulatedError> {
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_SOLID_CELLS,
        };
        let id = id.0;
        let assembly = self
            .articulated
            .get(&id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse != dynamic_allowed {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        let part_index = assembly
            .parts
            .iter()
            .position(|part| part.part_id == part_id)
            .ok_or(ArticulatedError::StaleTopology)?;
        let part = &assembly.parts[part_index];
        if assembly.topology_token != expected_topology_token
            || assembly.pending_root_pose_token.is_some()
            || assembly.pending_pose.is_some()
            || new_topology_token <= expected_topology_token
            || part.source_revision != expected_source_revision
            || new_source_revision <= expected_source_revision
        {
            return Err(ArticulatedError::StaleTopology);
        }
        if !articulated_grid_range_valid(&new_grid, part.cell_m) {
            return Err(ArticulatedError::GeometryRange);
        }
        let dims = new_grid.dims();
        let dense = u64::from(dims[0])
            .checked_mul(u64::from(dims[1]))
            .and_then(|v| v.checked_mul(u64::from(dims[2])))
            .ok_or(ArticulatedError::WorkBudget)?;
        let old_dense = u64::from(part.grid.dims()[0])
            .checked_mul(u64::from(part.grid.dims()[1]))
            .and_then(|v| v.checked_mul(u64::from(part.grid.dims()[2])))
            .ok_or(ArticulatedError::WorkBudget)?;
        let dense_total = assembly
            .parts
            .iter()
            .try_fold(0u64, |sum, part| {
                let dims = part.grid.dims();
                sum.checked_add(
                    u64::from(dims[0])
                        .checked_mul(u64::from(dims[1]))?
                        .checked_mul(u64::from(dims[2]))?,
                )
            })
            .ok_or(ArticulatedError::WorkBudget)?;
        let solid_total = assembly
            .parts
            .iter()
            .try_fold(0u64, |sum, part| sum.checked_add(part.grid.solid_count()))
            .ok_or(ArticulatedError::WorkBudget)?;
        let new_dense_total = dense_total
            .checked_sub(old_dense)
            .and_then(|v| v.checked_add(dense))
            .ok_or(ArticulatedError::WorkBudget)?;
        let new_solid_total = solid_total
            .checked_sub(part.grid.solid_count())
            .and_then(|v| v.checked_add(new_grid.solid_count()))
            .ok_or(ArticulatedError::WorkBudget)?;
        if new_dense_total > MAX_ARTICULATED_DENSE_CELLS
            || new_solid_total > MAX_ARTICULATED_SOLID_CELLS
        {
            return Err(ArticulatedError::WorkBudget);
        }
        if new_grid.solid_count() == 0 {
            return Err(ArticulatedError::EmptyPart(part_id));
        }
        let part_surface_radius_m = source_surface_radius_m(&new_grid, part.cell_m, [0.0; 3], None);
        let root_surface_radius_m =
            source_surface_radius_m(&new_grid, part.cell_m, [0.0; 3], Some(part.assembly_pose));
        let representation = self.entries[entry_index(id, self.id_namespace)].representation;
        let start = Instant::now();
        let built = build_collider(&new_grid, part.cell_m, representation);
        if built.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET
            || assembly
                .parts
                .iter()
                .map(|part| part.primitives)
                .sum::<usize>()
                - part.primitives
                + built.primitives
                > 65_536
        {
            return Err(ArticulatedError::WorkBudget);
        }
        let origin = new_grid.origin();
        let origin_m = [
            origin.x as f32 * part.cell_m,
            origin.y as f32 * part.cell_m,
            origin.z as f32 * part.cell_m,
        ];
        let offset = rotate(part.assembly_pose.rotation_xyzw, origin_m);
        let translation = [
            part.assembly_pose.translation_m[0] + offset[0],
            part.assembly_pose.translation_m[1] + offset[1],
            part.assembly_pose.translation_m[2] + offset[2],
        ];
        if !translation.iter().all(|value| value.is_finite()) {
            return Err(ArticulatedError::InvalidTransform);
        }
        let root = self.bodies[self.entries[entry_index(id, self.id_namespace)].body].position();
        let world_offset =
            root.transform_vector(Vector::new(translation[0], translation[1], translation[2]));
        let world_position = root.translation + world_offset;
        if !world_position.x.is_finite()
            || !world_position.y.is_finite()
            || !world_position.z.is_finite()
        {
            return Err(ArticulatedError::InvalidTransform);
        }
        let rotation = Rotation::from_xyzw(
            part.assembly_pose.rotation_xyzw[0],
            part.assembly_pose.rotation_xyzw[1],
            part.assembly_pose.rotation_xyzw[2],
            part.assembly_pose.rotation_xyzw[3],
        );
        let entry = &self.entries[entry_index(id, self.id_namespace)];
        let (mut body, query_only, restitution) = (entry.body, entry.query_only, entry.restitution);
        let kinematic_part_bodies = assembly.kinematic_part_bodies;
        let next_source_generation = assembly
            .source_generation
            .checked_add(1)
            .ok_or(ArticulatedError::WorkBudget)?;
        if kinematic_part_bodies {
            body = part.body;
        }
        let collider_pose = if kinematic_part_bodies {
            Pose::from_translation(Vector::from(origin_m))
        } else {
            Pose::new(
                Vector::new(translation[0], translation[1], translation[2]),
                rotation.to_scaled_axis(),
            )
        };
        let mut collider_builder = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(0.0)
            .position(collider_pose);
        if kinematic_part_bodies {
            collider_builder = collider_builder.active_collision_types(
                ActiveCollisionTypes::default() | ActiveCollisionTypes::KINEMATIC_FIXED,
            );
        }
        let mut collider = collider_builder.build();
        if query_only {
            collider.set_solver_groups(query_only_solver_groups());
        }
        collider.set_restitution(restitution);
        collider.set_restitution_combine_rule(CoefficientCombineRule::Max);

        let old_handle = part.collider;
        self.colliders
            .remove(old_handle, &mut self.islands, &mut self.bodies, true);
        let new_handle = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);
        self.tag_collider(new_handle, id);
        self.pending_removed.push(old_handle);
        self.pending_modified.push(new_handle);
        let assembly = self
            .articulated
            .get_mut(&id)
            .expect("assembly validated above");
        let part = &mut assembly.parts[part_index];
        part.grid = new_grid;
        part.source_revision = new_source_revision;
        part.part_surface_radius_m = part_surface_radius_m;
        part.root_surface_radius_m = root_surface_radius_m;
        part.collider = new_handle;
        part.primitives = built.primitives;
        assembly.topology_token = new_topology_token;
        assembly.source_generation = next_source_generation;
        if part_index == 0 {
            self.entries[entry_index(id, self.id_namespace)].collider = new_handle;
        }
        Ok(start.elapsed())
    }

    /// Removes source cells from exactly one live fracture component. The
    /// stable key is mandatory so a source part split across bodies cannot be
    /// edited by accidental vector order. Additions and material replacement
    /// remain unsupported until their incoming momentum policy is defined.
    pub fn rebuild_dynamic_corpse_component_part_grid(
        &mut self,
        id: ArticulatedBodyId,
        component_id: DerivedFractureComponentId,
        expected_topology_token: u64,
        new_topology_token: u64,
        part_id: u32,
        expected_source_revision: u64,
        new_source_revision: u64,
        new_grid: OccupancyGrid,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<CorpseEditOutcome, crate::articulated::ArticulatedError> {
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_SOLID_CELLS,
        };
        let started = Instant::now();
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(ArticulatedError::Hierarchy)?;
        if !assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if assembly.fracture_owner_key != Some(component_id.owner_key)
            || component_id.serial == 0
            || component_id.serial >= assembly.next_component_serial
            || assembly.topology_token != expected_topology_token
            || assembly.pending_pose.is_some()
            || assembly.pending_root_pose_token.is_some()
            || new_topology_token <= expected_topology_token
        {
            return Err(ArticulatedError::StaleTopology);
        }
        let component = assembly
            .parts
            .iter()
            .filter(|p| p.component_id == Some(component_id))
            .collect::<Vec<_>>();
        let target_index = assembly
            .parts
            .iter()
            .position(|p| p.component_id == Some(component_id) && p.part_id == part_id)
            .ok_or(ArticulatedError::StaleTopology)?;
        let target = assembly.parts[target_index].clone();
        if target.source_revision != expected_source_revision
            || new_source_revision <= expected_source_revision
        {
            return Err(ArticulatedError::StaleTopology);
        }
        if !articulated_grid_range_valid(&new_grid, target.cell_m) {
            return Err(ArticulatedError::GeometryRange);
        }
        let next_generation = assembly
            .source_generation
            .checked_add(1)
            .ok_or(ArticulatedError::WorkBudget)?;
        let manifest = assembly
            .density_manifest
            .as_ref()
            .ok_or(ArticulatedError::InvalidDensity)?;
        let mut callback_mismatch = false;
        for part in &component {
            part.grid.for_each_solid(|_, _, _, material| {
                callback_mismatch |= manifest
                    .get(&material)
                    .is_none_or(|bits| density(material).to_bits() != *bits);
            });
        }
        if callback_mismatch {
            return Err(ArticulatedError::InvalidDensity);
        }
        let stable_density = |material| manifest_density(manifest, material);
        // Admit the aggregate dense and solid workload before mass traversal,
        // shape generation, or cloning any replacement grids.
        let mut dense_preflight = 0_u64;
        let mut solid_preflight = 0_u64;
        for (index, part) in assembly.parts.iter().enumerate() {
            let grid = if index == target_index {
                &new_grid
            } else {
                &part.grid
            };
            if grid.solid_count() == 0 {
                continue;
            }
            let d = grid.dims();
            let dense = u64::from(d[0])
                .checked_mul(u64::from(d[1]))
                .and_then(|v| v.checked_mul(u64::from(d[2])))
                .ok_or(ArticulatedError::WorkBudget)?;
            dense_preflight = dense_preflight
                .checked_add(dense)
                .ok_or(ArticulatedError::WorkBudget)?;
            solid_preflight = solid_preflight
                .checked_add(grid.solid_count())
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        if dense_preflight > MAX_ARTICULATED_DENSE_CELLS
            || solid_preflight > MAX_ARTICULATED_SOLID_CELLS
            || assembly.parts.len() > MAX_ARTICULATED_PARTS
        {
            return Err(ArticulatedError::WorkBudget);
        }
        let old_origin = target.grid.origin();
        let new_origin = new_grid.origin();
        let new_dims = new_grid.dims();
        let mut removed_mass = 0.0_f64;
        let mut retained = 0_u64;
        let mut invalid_density = false;
        let mut unsupported = false;
        target.grid.for_each_solid(|x, y, z, material| {
            let Some(gx) = old_origin.x.checked_add(i64::from(x)) else {
                unsupported = true;
                return;
            };
            let Some(gy) = old_origin.y.checked_add(i64::from(y)) else {
                unsupported = true;
                return;
            };
            let Some(gz) = old_origin.z.checked_add(i64::from(z)) else {
                unsupported = true;
                return;
            };
            let (Some(lx), Some(ly), Some(lz)) = (
                gx.checked_sub(new_origin.x),
                gy.checked_sub(new_origin.y),
                gz.checked_sub(new_origin.z),
            ) else {
                unsupported = true;
                return;
            };
            let new_material = (lx >= 0
                && ly >= 0
                && lz >= 0
                && lx < i64::from(new_dims[0])
                && ly < i64::from(new_dims[1])
                && lz < i64::from(new_dims[2]))
            .then(|| new_grid.material(lx as u32, ly as u32, lz as u32))
            .flatten();
            match new_material {
                Some(current) if current == material => retained += 1,
                Some(_) => unsupported = true,
                None => {
                    let rho = stable_density(material);
                    if !rho.is_finite() || rho <= 0.0 {
                        invalid_density = true;
                    } else {
                        removed_mass += rho * f64::from(target.cell_m).powi(3);
                    }
                }
            }
        });
        if invalid_density {
            return Err(ArticulatedError::InvalidDensity);
        }
        if unsupported || new_grid.solid_count() != retained {
            return Err(ArticulatedError::UnsupportedCorpseEdit);
        }
        let old_sources = component
            .iter()
            .map(|p| SourcePart {
                part_id: p.part_id,
                parent_id: None,
                volume_id: p.volume_id,
                source_revision: p.source_revision,
                grid: p.grid.clone(),
                cell_m: p.cell_m,
                local_transform: p.assembly_pose,
            })
            .collect::<Vec<_>>();
        let old_poses = old_sources
            .iter()
            .map(|p| p.local_transform)
            .collect::<Vec<_>>();
        let old_mass =
            crate::mass::articulated_mass_properties(&old_sources, &old_poses, stable_density)
                .ok_or(ArticulatedError::InvalidDensity)?
                .to_body_properties();
        validate_body_mass_properties(old_mass)?;
        let mut new_sources = Vec::new();
        for part in &component {
            let grid = if part.part_id == part_id {
                &new_grid
            } else {
                &part.grid
            };
            if grid.solid_count() == 0 {
                continue;
            }
            new_sources.push(SourcePart {
                part_id: part.part_id,
                parent_id: None,
                volume_id: part.volume_id,
                source_revision: if part.part_id == part_id {
                    new_source_revision
                } else {
                    part.source_revision
                },
                grid: grid.clone(),
                cell_m: part.cell_m,
                local_transform: part.assembly_pose,
            });
        }
        let new_mass = if new_sources.is_empty() {
            None
        } else {
            let poses = new_sources
                .iter()
                .map(|p| p.local_transform)
                .collect::<Vec<_>>();
            let mass =
                crate::mass::articulated_mass_properties(&new_sources, &poses, stable_density)
                    .ok_or(ArticulatedError::InvalidDensity)?
                    .to_body_properties();
            validate_body_mass_properties(mass)?;
            Some(mass)
        };
        let mut primitive_total = 0_usize;
        for (index, part) in assembly.parts.iter().enumerate() {
            if index == target_index {
                continue;
            }
            primitive_total = primitive_total
                .checked_add(part.primitives)
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        let representation = self.entries[entry_index(id.0, self.id_namespace)].representation;
        let built = (new_grid.solid_count() > 0)
            .then(|| build_collider(&new_grid, target.cell_m, representation));
        if let Some(built) = &built {
            primitive_total = primitive_total
                .checked_add(built.primitives)
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        if primitive_total > 65_536
            || built
                .as_ref()
                .is_some_and(|b| b.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET)
        {
            return Err(ArticulatedError::WorkBudget);
        }
        let body = target.body;
        let pose = *self.bodies[body].position();
        let old_com = pose.transform_point(Vector::new(
            old_mass.local_com_m[0],
            old_mass.local_com_m[1],
            old_mass.local_com_m[2],
        ));
        let old_linear = self.bodies[body].linvel();
        let angular = self.bodies[body].angvel();
        let new_linear = if let Some(mass) = new_mass {
            let new_com = pose.transform_point(Vector::new(
                mass.local_com_m[0],
                mass.local_com_m[1],
                mass.local_com_m[2],
            ));
            old_linear + angular.cross(new_com - old_com)
        } else {
            old_linear
        };
        if ![new_linear.x, new_linear.y, new_linear.z]
            .iter()
            .all(|x| x.is_finite())
        {
            return Err(ArticulatedError::InvalidMassProperties);
        }
        let (new_handle, part_surface_radius_m, root_surface_radius_m) =
            if let Some(ref built) = built {
                let origin = new_grid.origin();
                let origin_m = [
                    origin.x as f32 * target.cell_m,
                    origin.y as f32 * target.cell_m,
                    origin.z as f32 * target.cell_m,
                ];
                let offset = rotate(target.assembly_pose.rotation_xyzw, origin_m);
                let local = Pose::new(
                    Vector::new(
                        target.assembly_pose.translation_m[0] + offset[0],
                        target.assembly_pose.translation_m[1] + offset[1],
                        target.assembly_pose.translation_m[2] + offset[2],
                    ),
                    Rotation::from_xyzw(
                        target.assembly_pose.rotation_xyzw[0],
                        target.assembly_pose.rotation_xyzw[1],
                        target.assembly_pose.rotation_xyzw[2],
                        target.assembly_pose.rotation_xyzw[3],
                    )
                    .to_scaled_axis(),
                );
                let old_collider = self
                    .colliders
                    .get(target.collider)
                    .ok_or(ArticulatedError::Hierarchy)?;
                let mut collider = ColliderBuilder::new(built.collider.shared_shape().clone())
                    .density(0.0)
                    .position(local)
                    .friction(old_collider.friction())
                    .active_collision_types(old_collider.active_collision_types())
                    .sensor(old_collider.is_sensor())
                    .build();
                collider.set_restitution(old_collider.restitution());
                collider.set_restitution_combine_rule(old_collider.restitution_combine_rule());
                collider.set_friction_combine_rule(old_collider.friction_combine_rule());
                collider.set_collision_groups(old_collider.collision_groups());
                collider.set_solver_groups(old_collider.solver_groups());
                let handle = self
                    .colliders
                    .insert_with_parent(collider, body, &mut self.bodies);
                self.tag_collider(handle, id.0);
                (
                    Some(handle),
                    source_surface_radius_m(&new_grid, target.cell_m, [0.0; 3], None),
                    source_surface_radius_m(
                        &new_grid,
                        target.cell_m,
                        [0.0; 3],
                        Some(target.assembly_pose),
                    ),
                )
            } else {
                (None, 0.0, 0.0)
            };
        let entry_idx = entry_index(id.0, self.id_namespace);
        let entry_primary_removed = self.entries[entry_idx].collider == target.collider;
        self.colliders
            .remove(target.collider, &mut self.islands, &mut self.bodies, true);
        self.pending_removed.push(target.collider);
        let assembly = self.articulated.get_mut(&id.0).expect("validated corpse");
        if let Some(handle) = new_handle {
            self.pending_modified.push(handle);
            let part = &mut assembly.parts[target_index];
            part.grid = new_grid;
            part.source_revision = new_source_revision;
            part.collider = handle;
            part.primitives = built.as_ref().map_or(0, |b| b.primitives);
            part.part_surface_radius_m = part_surface_radius_m;
            part.root_surface_radius_m = root_surface_radius_m;
        } else {
            assembly.parts.remove(target_index);
        }
        for part in &mut assembly.parts {
            if part.volume_id == target.volume_id {
                part.source_revision = new_source_revision;
            }
        }
        assembly.topology_token = new_topology_token;
        assembly.source_generation = next_generation;
        let component_remains = assembly
            .parts
            .iter()
            .any(|p| p.component_id == Some(component_id));
        let owner_remains = !assembly.parts.is_empty();
        if !owner_remains {
            self.retire_articulated_body(id);
            return Ok(CorpseEditOutcome::AggregateRetired {
                elapsed: started.elapsed(),
                removed_mass_kg: removed_mass,
            });
        }
        if !component_remains {
            self.bodies.remove(
                body,
                &mut self.islands,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                true,
            );
            let replacement = assembly.parts.first().expect("owner remains");
            let entry = &mut self.entries[entry_idx];
            if entry.body == body {
                entry.body = replacement.body;
                entry.collider = replacement.collider;
                entry.mass_properties = None;
            }
            self.ccd_solver = CCDSolver::new();
            return Ok(CorpseEditOutcome::PartRetired {
                elapsed: started.elapsed(),
                removed_mass_kg: removed_mass,
            });
        }
        let mass = new_mass.expect("remaining component has source cells");
        self.bodies[body]
            .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
        self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
        self.bodies[body].set_position(pose, true);
        self.bodies[body].set_linvel(new_linear, true);
        self.bodies[body].set_angvel(angular, true);
        let entry = &mut self.entries[entry_idx];
        if entry.body == body {
            if entry_primary_removed {
                entry.collider = new_handle.unwrap_or_else(|| {
                    assembly
                        .parts
                        .iter()
                        .find(|p| p.body == body)
                        .expect("surviving component shard")
                        .collider
                });
            }
            entry.mass_properties = Some(mass);
        }
        Ok(if new_handle.is_none() {
            CorpseEditOutcome::PartRetired {
                elapsed: started.elapsed(),
                removed_mass_kg: removed_mass,
            }
        } else {
            CorpseEditOutcome::Rebuilt {
                elapsed: started.elapsed(),
                removed_mass_kg: removed_mass,
            }
        })
    }

    /// Edits dynamic-corpse source geometry with an open-system removal rule:
    /// removed cells leave carrying their instantaneous rigid-body velocity
    /// field, while the surviving body's world pose and angular velocity stay
    /// fixed. Its COM linear velocity is adjusted to preserve the velocity
    /// field at every surviving point. Additions and material replacement are
    /// rejected until their incoming momentum policy is defined.
    pub fn rebuild_dynamic_corpse_part_grid(
        &mut self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        new_topology_token: u64,
        part_id: u32,
        expected_source_revision: u64,
        new_source_revision: u64,
        new_grid: OccupancyGrid,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<CorpseEditOutcome, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(ArticulatedError::Hierarchy)?;
        if !assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if assembly
            .parts
            .iter()
            .any(|part| part.component_id.is_some())
        {
            // The legacy operation addresses a whole source part, which may
            // span multiple independently moving fragments. It must not pick
            // one shard by vector order and silently leave siblings untouched.
            return Err(ArticulatedError::UnsupportedCorpseEdit);
        }
        if assembly.topology_token != expected_topology_token
            || assembly.pending_pose.is_some()
            || assembly.pending_root_pose_token.is_some()
            || new_topology_token <= expected_topology_token
        {
            return Err(ArticulatedError::StaleTopology);
        }
        let index_part = assembly
            .parts
            .iter()
            .position(|p| p.part_id == part_id)
            .ok_or(ArticulatedError::StaleTopology)?;
        let old = &assembly.parts[index_part];
        if old.source_revision != expected_source_revision
            || new_source_revision <= expected_source_revision
        {
            return Err(ArticulatedError::StaleTopology);
        }
        let original_parts = assembly.parts.clone();
        let manifest = assembly
            .density_manifest
            .clone()
            .ok_or(ArticulatedError::InvalidDensity)?;
        let mut density_mismatch = false;
        for part in &original_parts {
            part.grid.for_each_solid(|_, _, _, material| {
                density_mismatch |= manifest
                    .get(&material)
                    .is_none_or(|bits| density(material).to_bits() != *bits);
            });
        }
        if density_mismatch {
            return Err(ArticulatedError::InvalidDensity);
        }
        let stable_density = |material| manifest_density(&manifest, material);
        let mut removed_mass = 0.0;
        let mut invalid_density = false;
        let mut unsupported_edit = false;
        let mut retained_cells = 0_u64;
        let old_origin = old.grid.origin();
        let new_origin = new_grid.origin();
        let new_dims = new_grid.dims();
        old.grid.for_each_solid(|x, y, z, material| {
            let global = [
                old_origin.x + i64::from(x),
                old_origin.y + i64::from(y),
                old_origin.z + i64::from(z),
            ];
            let local = [
                global[0] - new_origin.x,
                global[1] - new_origin.y,
                global[2] - new_origin.z,
            ];
            let new_material = (0..3)
                .all(|axis| local[axis] >= 0 && local[axis] < i64::from(new_dims[axis]))
                .then(|| new_grid.material(local[0] as u32, local[1] as u32, local[2] as u32))
                .flatten();
            match new_material {
                Some(current) if current == material => retained_cells += 1,
                Some(_) => unsupported_edit = true,
                None => {
                    let rho = stable_density(material);
                    if !rho.is_finite() || rho <= 0.0 {
                        invalid_density = true;
                    } else {
                        removed_mass += rho * f64::from(old.cell_m).powi(3);
                    }
                }
            }
        });
        if invalid_density {
            return Err(ArticulatedError::InvalidDensity);
        }
        if unsupported_edit {
            return Err(ArticulatedError::UnsupportedCorpseEdit);
        }
        if new_grid.solid_count() != retained_cells {
            return Err(ArticulatedError::UnsupportedCorpseEdit);
        }
        let pose = *self.bodies[self.entries[entry_index(id.0, self.id_namespace)].body].position();
        if new_grid.solid_count() == 0 {
            if original_parts.len() == 1 {
                self.retire_articulated_body(id);
                return Ok(CorpseEditOutcome::AggregateRetired {
                    elapsed: Duration::ZERO,
                    removed_mass_kg: removed_mass,
                });
            }
            let remaining = original_parts
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != index_part)
                .map(|(_, p)| SourcePart {
                    part_id: p.part_id,
                    // Frozen corpse hierarchy is pose metadata only. Flatten
                    // every surviving part to its already-resolved assembly
                    // pose so removing a parent cannot orphan descendants.
                    parent_id: None,
                    volume_id: p.volume_id,
                    source_revision: p.source_revision,
                    grid: p.grid.clone(),
                    cell_m: p.cell_m,
                    local_transform: p.assembly_pose,
                })
                .collect::<Vec<_>>();
            let poses = remaining
                .iter()
                .map(|p| p.local_transform)
                .collect::<Vec<_>>();
            let mass = crate::mass::articulated_mass_properties(&remaining, &poses, stable_density)
                .ok_or(ArticulatedError::InvalidDensity)?
                .to_body_properties();
            validate_body_mass_properties(mass)?;
            let body = self.entries[entry_index(id.0, self.id_namespace)].body;
            let (linear, angular) = corpse_edit_velocity(&self.bodies[body], pose, mass)?;
            let started = Instant::now();
            let old_handle = original_parts[index_part].collider;
            self.colliders
                .remove(old_handle, &mut self.islands, &mut self.bodies, true);
            self.pending_removed.push(old_handle);
            self.bodies[body]
                .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
            self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
            self.bodies[body].set_position(pose, true);
            self.bodies[body].set_linvel(linear, true);
            self.bodies[body].set_angvel(angular, true);
            let a = self.articulated.get_mut(&id.0).unwrap();
            a.parts.remove(index_part);
            for part in &mut a.parts {
                part.parent_id = None;
                part.local_pose = part.assembly_pose;
            }
            a.topology_token = new_topology_token;
            self.entries[entry_index(id.0, self.id_namespace)].collider = a.parts[0].collider;
            self.entries[entry_index(id.0, self.id_namespace)].mass_properties = Some(mass);
            return Ok(CorpseEditOutcome::PartRetired {
                elapsed: started.elapsed(),
                removed_mass_kg: removed_mass,
            });
        }
        let sources = original_parts
            .iter()
            .enumerate()
            .map(|(i, p)| SourcePart {
                part_id: p.part_id,
                parent_id: p.parent_id,
                volume_id: p.volume_id,
                source_revision: if i == index_part {
                    new_source_revision
                } else {
                    p.source_revision
                },
                grid: if i == index_part {
                    new_grid.clone()
                } else {
                    p.grid.clone()
                },
                cell_m: p.cell_m,
                local_transform: p.local_pose,
            })
            .collect::<Vec<_>>();
        let poses = sources
            .iter()
            .map(|p| p.local_transform)
            .collect::<Vec<_>>();
        let mass = crate::mass::articulated_mass_properties(&sources, &poses, stable_density)
            .ok_or(ArticulatedError::InvalidDensity)?
            .to_body_properties();
        validate_body_mass_properties(mass)?;
        let index = entry_index(id.0, self.id_namespace);
        let body = self.entries[index].body;
        let (linear, angular) = corpse_edit_velocity(&self.bodies[body], pose, mass)?;
        let elapsed = self.rebuild_articulated_part_grid_impl(
            id,
            expected_topology_token,
            new_topology_token,
            part_id,
            expected_source_revision,
            new_source_revision,
            new_grid,
            true,
        )?;
        self.bodies[body]
            .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
        self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
        self.bodies[body].set_position(pose, true);
        self.bodies[body].set_linvel(linear, true);
        self.bodies[body].set_angvel(angular, true);
        self.entries[index].mass_properties = Some(mass);
        Ok(CorpseEditOutcome::Rebuilt {
            elapsed,
            removed_mass_kg: removed_mass,
        })
    }

    /// Moves the single kinematic assembly root after validating every
    /// transformed source-part collider pose. Part-local sampled poses stay
    /// unchanged and all attached colliders are refreshed together.
    pub fn set_articulated_root_pose(
        &mut self,
        id: ArticulatedBodyId,
        topology_token: u64,
        base_pose_token: u64,
        pose_token: u64,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
    ) -> Result<(), crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let body_id = id.0;
        let rotation = PartTransform {
            translation_m: [0.0; 3],
            rotation_xyzw,
        }
        .normalized()
        .ok_or(ArticulatedError::InvalidRootPose)?;
        if !articulated_translation_in_range(translation_m) {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if assembly.kinematic_part_bodies {
            return Err(ArticulatedError::InvalidRootPose);
        }
        if assembly.topology_token != topology_token
            || assembly.pose_token != base_pose_token
            || pose_token <= base_pose_token
        {
            return Err(ArticulatedError::StalePose);
        }
        if assembly.pending_root_pose_token.is_some() {
            return Err(ArticulatedError::PendingRootPose);
        }
        if assembly.pending_pose.is_some() {
            return Err(ArticulatedError::PendingRootPose);
        }
        for part in &assembly.parts {
            let origin = part.grid.origin();
            let origin_m = [
                origin.x as f32 * part.cell_m,
                origin.y as f32 * part.cell_m,
                origin.z as f32 * part.cell_m,
            ];
            let collider_offset = rotate(part.assembly_pose.rotation_xyzw, origin_m);
            let part_local = [
                part.assembly_pose.translation_m[0] + collider_offset[0],
                part.assembly_pose.translation_m[1] + collider_offset[1],
                part.assembly_pose.translation_m[2] + collider_offset[2],
            ];
            let rotated = rotate(rotation.rotation_xyzw, part_local);
            if (0..3).any(|axis| !(translation_m[axis] + rotated[axis]).is_finite()) {
                return Err(ArticulatedError::InvalidRootPose);
            }
        }
        let body = self.entries[entry_index(body_id, self.id_namespace)].body;
        self.bodies[body].set_translation(
            Vector::new(translation_m[0], translation_m[1], translation_m[2]),
            true,
        );
        self.bodies[body].set_rotation(
            Rotation::from_xyzw(
                rotation.rotation_xyzw[0],
                rotation.rotation_xyzw[1],
                rotation.rotation_xyzw[2],
                rotation.rotation_xyzw[3],
            ),
            true,
        );
        self.pending_modified
            .extend(assembly.parts.iter().map(|part| part.collider));
        self.articulated
            .get_mut(&body_id)
            .expect("validated assembly exists")
            .pose_token = pose_token;
        Ok(())
    }

    /// Schedules an authored root movement for the next Rapier fixed step. Its
    /// pose token becomes visible only after the solver installs that target.
    pub fn set_next_articulated_root_pose(
        &mut self,
        id: ArticulatedBodyId,
        topology_token: u64,
        base_pose_token: u64,
        pose_token: u64,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
    ) -> Result<(), crate::articulated::ArticulatedError> {
        self.set_next_articulated_root_pose_impl(
            id,
            topology_token,
            base_pose_token,
            pose_token,
            translation_m,
            rotation_xyzw,
            false,
        )
    }

    fn set_next_articulated_root_pose_impl(
        &mut self,
        id: ArticulatedBodyId,
        topology_token: u64,
        base_pose_token: u64,
        pose_token: u64,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        allow_kinematic_part_rig: bool,
    ) -> Result<(), crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let body_id = id.0;
        let rotation = PartTransform {
            translation_m: [0.0; 3],
            rotation_xyzw,
        }
        .normalized()
        .ok_or(ArticulatedError::InvalidRootPose)?;
        if !articulated_translation_in_range(translation_m) {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if assembly.kinematic_part_bodies && !allow_kinematic_part_rig {
            return Err(ArticulatedError::InvalidRootPose);
        }
        if assembly.topology_token != topology_token
            || assembly.pose_token != base_pose_token
            || pose_token <= base_pose_token
        {
            return Err(ArticulatedError::StalePose);
        }
        if assembly.pending_root_pose_token.is_some()
            || (assembly.pending_pose.is_some() && !allow_kinematic_part_rig)
        {
            return Err(ArticulatedError::PendingRootPose);
        }
        for part in &assembly.parts {
            let origin = part.grid.origin();
            let origin_m = [
                origin.x as f32 * part.cell_m,
                origin.y as f32 * part.cell_m,
                origin.z as f32 * part.cell_m,
            ];
            let collider_offset = rotate(part.assembly_pose.rotation_xyzw, origin_m);
            let part_local = [
                part.assembly_pose.translation_m[0] + collider_offset[0],
                part.assembly_pose.translation_m[1] + collider_offset[1],
                part.assembly_pose.translation_m[2] + collider_offset[2],
            ];
            let rotated = rotate(rotation.rotation_xyzw, part_local);
            if (0..3).any(|axis| !(translation_m[axis] + rotated[axis]).is_finite()) {
                return Err(ArticulatedError::InvalidRootPose);
            }
        }
        let root_target = Pose::new(
            Vector::new(translation_m[0], translation_m[1], translation_m[2]),
            Rotation::from_xyzw(
                rotation.rotation_xyzw[0],
                rotation.rotation_xyzw[1],
                rotation.rotation_xyzw[2],
                rotation.rotation_xyzw[3],
            )
            .to_scaled_axis(),
        );
        let part_targets = if assembly.kinematic_part_bodies {
            assembly
                .parts
                .iter()
                .enumerate()
                .map(|(index, part)| {
                    let part_pose = assembly
                        .pending_pose
                        .as_ref()
                        .map(|pending| pending.parts[index].2)
                        .unwrap_or(part.assembly_pose);
                    let local = Pose::new(
                        Vector::new(
                            part_pose.translation_m[0],
                            part_pose.translation_m[1],
                            part_pose.translation_m[2],
                        ),
                        Rotation::from_xyzw(
                            part_pose.rotation_xyzw[0],
                            part_pose.rotation_xyzw[1],
                            part_pose.rotation_xyzw[2],
                            part_pose.rotation_xyzw[3],
                        )
                        .to_scaled_axis(),
                    );
                    (part.body, root_target * local)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let body = self.entries[entry_index(body_id, self.id_namespace)].body;
        let mut all_targets = vec![(body, root_target)];
        all_targets.extend(part_targets.iter().copied());
        self.validate_kinematic_surface_travel(&all_targets)?;
        self.bodies[body].set_next_kinematic_position(root_target);
        for (part_body, target) in part_targets {
            self.bodies[part_body].set_next_kinematic_position(target);
        }
        self.articulated
            .get_mut(&body_id)
            .expect("validated assembly exists")
            .pending_root_pose_token = Some(pose_token);
        Ok(())
    }

    /// Conservatively sweeps a sphere enclosing the complete current assembly
    /// against live colliders, then schedules the clipped root and all part
    /// bodies for the same physics boundary. The sphere contract can block
    /// clear narrow passages; it never claims exact articulated clearance.
    pub fn sweep_articulated_root_translation(
        &mut self,
        id: ArticulatedBodyId,
        topology_token: u64,
        base_pose_token: u64,
        pose_token: u64,
        requested_translation_m: [f32; 3],
    ) -> Result<crate::articulated::ArticulatedMove, crate::articulated::ArticulatedError> {
        use crate::articulated::{ArticulatedError, ArticulatedMove};
        if !requested_translation_m
            .iter()
            .all(|value| value.is_finite())
        {
            return Err(ArticulatedError::InvalidRootPose);
        }
        // Broad-phase query casts must observe edits/pose changes made since
        // the last step, even when the caller has not explicitly synced.
        self.sync_queries();
        let body_id = id.0;
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if !assembly.kinematic_part_bodies {
            return Err(ArticulatedError::Hierarchy);
        }
        if assembly.topology_token != topology_token
            || assembly.pose_token != base_pose_token
            || pose_token <= base_pose_token
            || assembly.pending_root_pose_token.is_some()
        {
            return Err(ArticulatedError::StalePose);
        }
        let mut bounds_min = [f32::INFINITY; 3];
        let mut bounds_max = [f32::NEG_INFINITY; 3];
        for part in &assembly.parts {
            let origin = part.grid.origin();
            let dims = part.grid.dims();
            for corner in 0..8 {
                let source = [
                    (origin.x as f32 + if corner & 1 == 0 { 0.0 } else { dims[0] as f32 })
                        * part.cell_m,
                    (origin.y as f32 + if corner & 2 == 0 { 0.0 } else { dims[1] as f32 })
                        * part.cell_m,
                    (origin.z as f32 + if corner & 4 == 0 { 0.0 } else { dims[2] as f32 })
                        * part.cell_m,
                ];
                let rotated = rotate(part.assembly_pose.rotation_xyzw, source);
                for axis in 0..3 {
                    let value = part.assembly_pose.translation_m[axis] + rotated[axis];
                    bounds_min[axis] = bounds_min[axis].min(value);
                    bounds_max[axis] = bounds_max[axis].max(value);
                }
            }
        }
        let (local_center, radius) = if let Some(pending) = &assembly.pending_pose {
            let mut radius_squared = 0.0f32;
            for (index, part) in assembly.parts.iter().enumerate() {
                let origin = part.grid.origin();
                let dims = part.grid.dims();
                for corner in 0..8 {
                    let source = [
                        (origin.x as f32 + if corner & 1 == 0 { 0.0 } else { dims[0] as f32 })
                            * part.cell_m,
                        (origin.y as f32 + if corner & 2 == 0 { 0.0 } else { dims[1] as f32 })
                            * part.cell_m,
                        (origin.z as f32 + if corner & 4 == 0 { 0.0 } else { dims[2] as f32 })
                            * part.cell_m,
                    ];
                    for pose in [part.assembly_pose, pending.parts[index].2] {
                        let translation_len = pose
                            .translation_m
                            .iter()
                            .map(|value| value * value)
                            .sum::<f32>()
                            .sqrt();
                        let source_len =
                            source.iter().map(|value| value * value).sum::<f32>().sqrt();
                        // Translation interpolation stays in the endpoint
                        // convex hull; this sum bounds every source corner
                        // under any intermediate part rotation.
                        radius_squared = radius_squared.max((translation_len + source_len).powi(2));
                    }
                }
            }
            ([0.0; 3], radius_squared.sqrt().max(0.001))
        } else {
            let center =
                std::array::from_fn::<_, 3, _>(|axis| 0.5 * (bounds_min[axis] + bounds_max[axis]));
            let radius = (0..3)
                .map(|axis| (0.5 * (bounds_max[axis] - bounds_min[axis])).powi(2))
                .sum::<f32>()
                .sqrt()
                .max(0.001);
            (center, radius)
        };
        let root = assembly.root_pose;
        let rotated_center = rotate(root.rotation_xyzw, local_center);
        let start = Pose::from_translation(Vector::from([
            root.translation_m[0] + rotated_center[0],
            root.translation_m[1] + rotated_center[1],
            root.translation_m[2] + rotated_center[2],
        ]));
        let displacement = Vector::from(requested_translation_m);
        let displacement_length = displacement.length();
        let displacement_direction = if displacement_length > 0.0 {
            displacement / displacement_length
        } else {
            Vector::ZERO
        };
        // Parry's conservative advancement loses sub-10µm displacement at
        // world-coordinate float precision. Cast at a fixed minimum distance,
        // discard contacts beyond the actual request, and convert TOI back to
        // a fraction of that request.
        let query_distance = displacement_length.max(1.0e-5);
        let query_displacement = displacement_direction * query_distance;
        let excluded: Vec<_> = assembly.parts.iter().map(|part| part.collider).collect();
        // Root translation is explicitly a terrain sweep. Dynamic/kinematic
        // bodies are handled by the shared Rapier solver after scheduling;
        // sensors do not block locomotion.
        let terrain_colliders: Vec<_> = self
            .colliders
            .iter()
            .filter_map(|(handle, collider)| {
                let fixed = collider
                    .parent()
                    .and_then(|parent| self.bodies.get(parent))
                    .is_some_and(|body| body.is_fixed());
                (fixed && !collider.is_sensor()).then_some(handle)
            })
            .collect();
        let hit = if displacement.length_squared() == 0.0 {
            None
        } else {
            let shape = Ball::new(radius);
            let mut nearest: Option<(ColliderHandle, rapier3d::parry::query::ShapeCastHit)> = None;
            for &terrain_handle in &terrain_colliders {
                let only_terrain = |handle: ColliderHandle, _: &Collider| {
                    handle == terrain_handle && !excluded.contains(&handle)
                };
                let filter = QueryFilter::default().predicate(&only_terrain);
                let queries = self.broad_phase.as_query_pipeline(
                    self.narrow_phase.query_dispatcher(),
                    &self.bodies,
                    &self.colliders,
                    filter,
                );
                let Some(candidate) = queries.cast_shape(
                    &start,
                    query_displacement,
                    &shape,
                    rapier3d::parry::query::ShapeCastOptions {
                        max_time_of_impact: 1.0,
                        target_distance: 0.0,
                        stop_at_penetration: true,
                        compute_impact_geometry_on_penetration: true,
                    },
                ) else {
                    continue;
                };
                if candidate.1.time_of_impact * query_distance > displacement_length {
                    continue;
                }
                // At exact support tangency Rapier may report TOI=0 for a
                // tangent or separating request. Ignore only that contact;
                // retain zero-time hits whose normalized motion direction
                // points into terrain. The angular tolerance is independent of
                // request distance, so tiny inward moves are still blocked.
                if candidate.1.time_of_impact <= 1.0e-6
                    && candidate.1.normal1.dot(displacement_direction) >= -1.0e-6
                {
                    continue;
                }
                if nearest.is_none_or(|(_, hit): (ColliderHandle, _)| {
                    candidate.1.time_of_impact < hit.time_of_impact
                }) {
                    nearest = Some(candidate);
                }
            }
            nearest
        };
        let blocked = hit.is_some();
        let fraction = hit.map_or(1.0, |(_, hit)| {
            (hit.time_of_impact * query_distance / displacement_length).clamp(0.0, 1.0)
        });
        let applied_translation_m = requested_translation_m.map(|value| value * fraction);
        let target = [
            root.translation_m[0] + applied_translation_m[0],
            root.translation_m[1] + applied_translation_m[1],
            root.translation_m[2] + applied_translation_m[2],
        ];
        self.set_next_articulated_root_pose_impl(
            id,
            topology_token,
            base_pose_token,
            pose_token,
            target,
            root.rotation_xyzw,
            true,
        )?;
        Ok(ArticulatedMove {
            requested_translation_m,
            applied_translation_m,
            fraction,
            blocked,
        })
    }

    /// Atomically stages one complete sampled pose and a swept root
    /// translation for the same physics boundary. The sweep uses a root-
    /// centered sphere enclosing old and proposed source geometry under any
    /// interpolated part rotation/translation; this is conservative and may
    /// overblock. It does not claim exact limb motion or terrain response for
    /// arbitrary discontinuous authored pose changes.
    pub fn update_articulated_pose_with_swept_root_translation(
        &mut self,
        id: ArticulatedBodyId,
        update: ArticulatedPose,
        requested_translation_m: [f32; 3],
    ) -> Result<crate::articulated::ArticulatedMove, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        if !requested_translation_m
            .iter()
            .all(|value| value.is_finite())
        {
            return Err(ArticulatedError::InvalidRootPose);
        }
        if !articulated_translation_in_range(requested_translation_m) {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if !assembly.kinematic_part_bodies
            || assembly.topology_token != update.topology_token
            || assembly.pose_token != update.base_pose_token
            || update.pose_token <= update.base_pose_token
            || assembly.pending_root_pose_token.is_some()
            || assembly.pending_pose.is_some()
        {
            return Err(ArticulatedError::StalePose);
        }
        if update.parts.len() != assembly.parts.len()
            || update
                .parts
                .iter()
                .zip(&assembly.parts)
                .any(|((part_id, pose), part)| {
                    *part_id != part.part_id || pose.normalized().is_none()
                })
        {
            return Err(ArticulatedError::StalePose);
        }
        let locals = update
            .parts
            .iter()
            .map(|(part_id, pose)| {
                if !articulated_translation_in_range(pose.translation_m) {
                    return Err(ArticulatedError::GeometryRange);
                }
                Ok((*part_id, pose.normalized().expect("validated transform")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let flattened = flatten_local_poses(&assembly.parts, &locals)?;
        let root_target = std::array::from_fn::<_, 3, _>(|axis| {
            assembly.root_pose.translation_m[axis] + requested_translation_m[axis]
        });
        if !articulated_translation_in_range(root_target) {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let root_body = self.entries[entry_index(id.0, self.id_namespace)].body;
        let root_start = *self.bodies[root_body].position();
        let root_target_pose = Pose::new(
            Vector::from(root_target),
            root_start.rotation.to_scaled_axis(),
        );
        let mut scheduled_targets = vec![(root_body, root_target_pose)];
        for (part, world_pose) in assembly.parts.iter().zip(flattened) {
            if !articulated_translation_in_range(world_pose.translation_m) {
                return Err(ArticulatedError::GeometryRange);
            }
            let origin = part.grid.origin();
            let origin_m = [
                origin.x as f32 * part.cell_m,
                origin.y as f32 * part.cell_m,
                origin.z as f32 * part.cell_m,
            ];
            let local_offset = rotate(world_pose.rotation_xyzw, origin_m);
            let local_translation = std::array::from_fn::<_, 3, _>(|axis| {
                world_pose.translation_m[axis] + local_offset[axis]
            });
            let world_offset = rotate(assembly.root_pose.rotation_xyzw, local_translation);
            let world_translation =
                std::array::from_fn::<_, 3, _>(|axis| root_target[axis] + world_offset[axis]);
            if !articulated_translation_in_range(world_translation) {
                return Err(ArticulatedError::GeometryRange);
            }
            let local_pose = Pose::new(
                Vector::from(world_pose.translation_m),
                Rotation::from_xyzw(
                    world_pose.rotation_xyzw[0],
                    world_pose.rotation_xyzw[1],
                    world_pose.rotation_xyzw[2],
                    world_pose.rotation_xyzw[3],
                )
                .to_scaled_axis(),
            );
            scheduled_targets.push((part.body, root_target_pose * local_pose));
        }
        self.validate_kinematic_surface_travel(&scheduled_targets)?;
        // All pose, topology, and target coordinates have been validated
        // before either half is staged; the root sweep then cannot fail on
        // these inputs, and both targets commit at the same step boundary.
        self.update_articulated_pose(id, update.clone())?;
        self.sweep_articulated_root_translation(
            id,
            update.topology_token,
            update.base_pose_token,
            update.pose_token,
            requested_translation_m,
        )
    }

    /// Sweeps every exact occupied source cell in each source-part collider
    /// through its declared one-tick world-space interpolation (linear
    /// translation and shortest-arc rotation), including optional root motion,
    /// against current fixed terrain. Each source cell is swept as a Cuboid
    /// query shape so Parry's translating-only moving-Voxels nonlinear cast
    /// path is not used for rotating NativeVoxels parts. A terrain hit rejects
    /// the complete sample without staging any body or token. This contract
    /// does not claim joint-constrained intermediate poses or solver-driven
    /// terrain response.
    pub fn update_articulated_pose_with_swept_root_pose(
        &mut self,
        id: ArticulatedBodyId,
        update: ArticulatedPose,
        root_target: PartTransform,
    ) -> Result<crate::articulated::ArticulatedTransition, crate::articulated::ArticulatedError>
    {
        use crate::articulated::{ArticulatedError, ArticulatedTransition};
        if !root_target.is_valid() {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let root_target = root_target
            .normalized()
            .ok_or(ArticulatedError::InvalidRootPose)?;
        if !articulated_translation_in_range(root_target.translation_m) {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let body_id = id.0;
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if !assembly.kinematic_part_bodies
            || assembly.topology_token != update.topology_token
            || assembly.pose_token != update.base_pose_token
            || update.pose_token <= update.base_pose_token
            || assembly.pending_root_pose_token.is_some()
            || assembly.pending_pose.is_some()
        {
            return Err(ArticulatedError::StalePose);
        }
        if update.parts.len() != assembly.parts.len()
            || update
                .parts
                .iter()
                .zip(&assembly.parts)
                .any(|((part_id, pose), part)| {
                    *part_id != part.part_id || pose.normalized().is_none()
                })
        {
            return Err(ArticulatedError::StalePose);
        }
        let locals = update
            .parts
            .iter()
            .map(|(part_id, pose)| {
                if !articulated_translation_in_range(pose.translation_m) {
                    return Err(ArticulatedError::GeometryRange);
                }
                Ok((*part_id, pose.normalized().expect("validated transform")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let flattened = flatten_local_poses(&assembly.parts, &locals)?;
        let target_root = Pose::new(
            Vector::from(root_target.translation_m),
            Rotation::from_xyzw(
                root_target.rotation_xyzw[0],
                root_target.rotation_xyzw[1],
                root_target.rotation_xyzw[2],
                root_target.rotation_xyzw[3],
            )
            .to_scaled_axis(),
        );
        let mut targets = Vec::with_capacity(assembly.parts.len());
        for (part, world_pose) in assembly.parts.iter().zip(flattened.iter()) {
            let local = Pose::new(
                Vector::from(world_pose.translation_m),
                Rotation::from_xyzw(
                    world_pose.rotation_xyzw[0],
                    world_pose.rotation_xyzw[1],
                    world_pose.rotation_xyzw[2],
                    world_pose.rotation_xyzw[3],
                )
                .to_scaled_axis(),
            );
            let body_target = target_root * local;
            let collider_local = self.colliders[part.collider]
                .position_wrt_parent()
                .copied()
                .unwrap_or_else(Pose::identity);
            let collider_target = body_target * collider_local;
            if !articulated_translation_in_range([
                body_target.translation.x,
                body_target.translation.y,
                body_target.translation.z,
            ]) || !articulated_translation_in_range([
                collider_target.translation.x,
                collider_target.translation.y,
                collider_target.translation.z,
            ]) {
                return Err(ArticulatedError::GeometryRange);
            }
            targets.push((
                part.body,
                part.collider,
                *self.colliders[part.collider].position(),
                collider_target,
                body_target,
            ));
        }
        let root_body = self.entries[entry_index(body_id, self.id_namespace)].body;
        let mut scheduled_targets = vec![(root_body, target_root)];
        scheduled_targets.extend(
            targets
                .iter()
                .map(|(body, _, _, _, body_target)| (*body, *body_target)),
        );
        self.validate_kinematic_surface_travel(&scheduled_targets)?;
        let part_ids = assembly
            .parts
            .iter()
            .map(|part| part.part_id)
            .collect::<Vec<_>>();
        let source_parts = assembly
            .parts
            .iter()
            .map(|part| (part.body, part.grid.clone(), part.cell_m))
            .collect::<Vec<_>>();
        let total_cells = source_parts
            .iter()
            .map(|(_, grid, _)| grid.solid_count() as usize)
            .sum::<usize>();
        let candidate_limit = transition_candidate_limit(total_cells);

        // Query freshness is part of this gate: voxel edits/removals made since
        // the previous step must affect this same transition decision.
        let sync_started = Instant::now();
        self.sync_queries();
        let query_sync_time = sync_started.elapsed();
        let mut earliest: Option<(u32, f32)> = None;
        // Cull fixed terrain using a conservative bound for the complete
        // declared part-origin path. Every cell corner stays within `radius`
        // of its body origin: rotation preserves distance, while translation
        // follows the segment between the two validated poses. This includes
        // mid-arc orientations and off-center/negative-origin source grids.
        // The exact nonlinear source-cell casts below remain authoritative.
        let mut swept_bounds = Vec::with_capacity(targets.len());
        for ((part_body, collider_handle, _, _, body_target), (_, grid, cell_m)) in
            targets.iter().zip(&source_parts)
        {
            let body_start = *self.bodies[*part_body].position();
            let collider_local = self.colliders[*collider_handle]
                .position_wrt_parent()
                .copied()
                .unwrap_or_else(Pose::identity);
            let half_diagonal = (3.0_f32).sqrt() * *cell_m * 0.5;
            let radius = grid
                .solid_indices()
                .into_iter()
                .map(|[x, y, z]| {
                    let cell_center = Vector::new(
                        (x as f32 + 0.5) * *cell_m,
                        (y as f32 + 0.5) * *cell_m,
                        (z as f32 + 0.5) * *cell_m,
                    );
                    let local_center = collider_local * Pose::from_translation(cell_center);
                    local_center.translation.length_squared().sqrt() + half_diagonal
                })
                .fold(0.0_f32, f32::max);
            if !radius.is_finite() {
                return Err(ArticulatedError::GeometryRange);
            }
            let start = body_start.translation;
            let end = body_target.translation;
            // Outward rounding padding only adds candidates.
            let margin = 1.0e-4_f32;
            swept_bounds.push((
                Vector::new(
                    start.x.min(end.x) - radius - margin,
                    start.y.min(end.y) - radius - margin,
                    start.z.min(end.z) - radius - margin,
                ),
                Vector::new(
                    start.x.max(end.x) + radius + margin,
                    start.y.max(end.y) + radius + margin,
                    start.z.max(end.z) + radius + margin,
                ),
            ));
        }
        let fixed_nonsensor = |_: ColliderHandle, collider: &Collider| {
            !collider.is_sensor()
                && collider
                    .parent()
                    .and_then(|parent| self.bodies.get(parent))
                    .is_some_and(|body| body.is_fixed())
        };
        let candidate_started = Instant::now();
        let query = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            QueryFilter::default().predicate(&fixed_nonsensor),
        );
        let mut terrain_colliders = Vec::with_capacity(candidate_limit.min(4096));
        let mut seen_candidates = HashSet::with_capacity(candidate_limit.min(4096));
        for (min, max) in &swept_bounds {
            let bounds = rapier3d::parry::bounding_volume::Aabb::new(*min, *max);
            for (handle, _) in query.intersect_aabb_conservative(bounds) {
                if !seen_candidates.contains(&handle) {
                    if seen_candidates.len() == candidate_limit {
                        return Err(ArticulatedError::TerrainTransitionWorkBudget {
                            source_cells: total_cells,
                            terrain_candidates_at_least: candidate_limit.saturating_add(1),
                        });
                    }
                    seen_candidates.insert(handle);
                    terrain_colliders.push(handle);
                }
            }
        }
        let candidate_query_time = candidate_started.elapsed();
        let terrain_candidate_count = terrain_colliders.len();
        // Parry 0.30.2 explicitly treats a moving `Voxels` shape as translation
        // only during nonlinear casts and builds an end AABB from endpoint
        // orientations. A rotating off-center source grid therefore is not a
        // sound broadphase/cast primitive for this operation. Cast each exact
        // occupied source cell instead. This is still one source-part body and
        // one live collider; cell cuboids exist only as bounded query shapes.
        // Their center follows the same linear part-origin / shortest-arc
        // rotation path as the kinematic body, so their union is the source
        // grid at every time (up to Parry's documented iterative TOI tolerance).
        let mut cell_casts = 0usize;
        let casts_started = Instant::now();
        for ((part_id, (_, _, collider_start, _, body_target)), (body, grid, cell_m)) in
            part_ids.iter().zip(&targets).zip(&source_parts)
        {
            let body_start = *self.bodies[*body].position();
            let body_delta = body_target.translation - body_start.translation;
            let delta_rotation = body_target.rotation * body_start.rotation.inverse();
            let angular_delta = delta_rotation.to_scaled_axis();
            let origin = grid.origin();
            let grid_origin = Vector::new(
                origin.x as f32 * *cell_m,
                origin.y as f32 * *cell_m,
                origin.z as f32 * *cell_m,
            );
            let half = Vector::splat(*cell_m * 0.5);
            let cell_shape = rapier3d::prelude::Cuboid::new(half);
            for [x, y, z] in grid.solid_indices() {
                let cell_center = Vector::new(
                    (x as f32 + 0.5) * *cell_m,
                    (y as f32 + 0.5) * *cell_m,
                    (z as f32 + 0.5) * *cell_m,
                );
                let cell_start = *collider_start * Pose::from_translation(cell_center);
                let motion = rapier3d::parry::query::NonlinearRigidMotion::new(
                    cell_start,
                    -(grid_origin + cell_center),
                    body_delta,
                    angular_delta,
                );
                for &terrain_handle in &terrain_colliders {
                    cell_casts += 1;
                    // The terrain candidate list came from the synchronized live
                    // broad phase above. Querying one candidate by constructing a
                    // filtered QueryPipeline here would rebuild a view of every
                    // collider for every source cell. Cast directly against that
                    // candidate's exact live shape and world pose instead.
                    let terrain = &self.colliders[terrain_handle];
                    let stationary = rapier3d::parry::query::NonlinearRigidMotion::new(
                        *terrain.position(),
                        Vector::ZERO,
                        Vector::ZERO,
                        Vector::ZERO,
                    );
                    let cast_result = rapier3d::parry::query::cast_shapes_nonlinear(
                        &motion,
                        &cell_shape,
                        &stationary,
                        terrain.shape(),
                        0.0,
                        1.0,
                        true,
                    );
                    let Some(hit) = require_supported_transition_cast(*part_id, cast_result)?
                    else {
                        continue;
                    };
                    if earliest.is_none_or(|(_, toi)| hit.time_of_impact < toi) {
                        earliest = Some((*part_id, hit.time_of_impact));
                    }
                }
            }
        }
        if let Some((part_id, time_of_impact)) = earliest {
            return Ok(ArticulatedTransition {
                blocked: true,
                part_id: Some(part_id),
                time_of_impact: Some(time_of_impact),
                terrain_candidates: terrain_candidate_count,
                cell_casts,
                query_sync_time,
                candidate_query_time,
                exact_cast_time: casts_started.elapsed(),
            });
        }

        // All input validation and continuous terrain casts succeeded. Stage
        // root and part body targets together; both version tokens publish at
        // the same following physics boundary.
        self.bodies[root_body].set_next_kinematic_position(target_root);
        for (body, _, _, _, body_target) in &targets {
            self.bodies[*body].set_next_kinematic_position(*body_target);
        }
        let pending = locals
            .into_iter()
            .zip(flattened)
            .map(|((part_id, local), world)| (part_id, local, world))
            .collect();
        let assembly = self
            .articulated
            .get_mut(&body_id)
            .expect("validated assembly exists");
        assembly.pending_root_pose_token = Some(update.pose_token);
        assembly.pending_pose = Some(PendingArticulatedPose {
            pose_token: update.pose_token,
            parts: pending,
        });
        Ok(ArticulatedTransition {
            blocked: false,
            part_id: None,
            time_of_impact: None,
            terrain_candidates: terrain_candidate_count,
            cell_casts,
            query_sync_time,
            candidate_query_time,
            exact_cast_time: casts_started.elapsed(),
        })
    }

    /// Retires the kinematic root and all of its attached source-part colliders.
    pub fn retire_articulated_body(&mut self, id: ArticulatedBodyId) {
        let entry_body = self
            .entries
            .get(entry_index(id.0, self.id_namespace))
            .map(|entry| entry.body);
        let part_bodies = self
            .articulated
            .get(&id.0)
            .filter(|assembly| {
                assembly.kinematic_part_bodies
                    || assembly
                        .parts
                        .iter()
                        .any(|part| part.component_id.is_some())
            })
            .map(|assembly| {
                assembly
                    .parts
                    .iter()
                    .map(|part| part.body)
                    .filter(|body| Some(*body) != entry_body)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let had_part_bodies = !part_bodies.is_empty();
        for body in part_bodies {
            self.bodies.remove(
                body,
                &mut self.islands,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                true,
            );
        }
        if had_part_bodies {
            self.ccd_solver = CCDSolver::new();
        }
        self.retire_body(id.0);
    }

    /// Number of exact source-part colliders installed for an assembly.
    pub fn articulated_collider_count(&self, id: ArticulatedBodyId) -> usize {
        self.articulated
            .get(&id.0)
            .map_or(0, |assembly| assembly.parts.len())
    }

    /// Number of physical Rapier bodies used by an articulated candidate,
    /// including its empty conceptual root bookkeeping body when present.
    pub fn articulated_rigid_body_count(&self, id: ArticulatedBodyId) -> usize {
        self.articulated.get(&id.0).map_or(0, |assembly| {
            if assembly.kinematic_part_bodies {
                assembly.parts.len() + 1
            } else if assembly
                .parts
                .iter()
                .any(|part| part.component_id.is_some())
            {
                assembly
                    .parts
                    .iter()
                    .map(|part| part.body)
                    .collect::<HashSet<_>>()
                    .len()
            } else {
                1
            }
        })
    }

    /// Reads the assembly's primary rigid body state for bounded feasibility
    /// diagnostics. Kinematic rigs expose their stationary root; a corpse
    /// aggregate exposes its dynamic owner body.
    pub fn articulated_body_state(&self, id: ArticulatedBodyId) -> BodyState {
        self.body_state(id.0)
    }

    /// Returns current owner tokens and root motion without exposing handles.
    pub fn articulated_owner_snapshot(
        &self,
        id: ArticulatedBodyId,
    ) -> Option<ArticulatedOwnerSnapshot> {
        let assembly = self.articulated.get(&id.0)?;
        Some(ArticulatedOwnerSnapshot {
            topology_token: assembly.topology_token,
            pose_token: assembly.pose_token,
            pending_pose: assembly.pending_root_pose_token.is_some()
                || assembly.pending_pose.is_some(),
            root: self.body_state(id.0),
        })
    }

    /// Returns the applied authored local pose for each source part in stable
    /// part-ID order. Pending samples are not reported as applied state.
    pub fn articulated_pose_snapshot(
        &self,
        id: ArticulatedBodyId,
    ) -> Option<crate::articulated::ArticulatedPoseSnapshot> {
        let assembly = self.articulated.get(&id.0)?;
        let mut parts = assembly
            .parts
            .iter()
            .map(|part| (part.part_id, part.local_pose))
            .collect::<Vec<_>>();
        parts.sort_by_key(|(part_id, _)| *part_id);
        Some(crate::articulated::ArticulatedPoseSnapshot {
            topology_token: assembly.topology_token,
            pose_token: assembly.pose_token,
            parts,
        })
    }

    /// Returns live state for a currently owned stable fracture component.
    /// Retired, foreign-owner, and never-allocated keys fail closed.
    pub fn fracture_component_state(
        &self,
        id: ArticulatedBodyId,
        component_id: DerivedFractureComponentId,
    ) -> Result<BodyState, FractureError> {
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(FractureError::StaleOwner)?;
        if !assembly.dynamic_corpse {
            return Err(FractureError::StaleOwner);
        }
        if assembly.fracture_owner_key != Some(component_id.owner_key)
            || component_id.serial == 0
            || component_id.serial >= assembly.next_component_serial
        {
            return Err(FractureError::StaleOwner);
        }
        let mut bodies = assembly
            .parts
            .iter()
            .filter(|part| part.component_id == Some(component_id))
            .map(|part| part.body);
        let body = bodies.next().ok_or(FractureError::StaleOwner)?;
        if bodies.any(|other| other != body) {
            return Err(FractureError::InvalidPlan);
        }
        let rb = self.bodies.get(body).ok_or(FractureError::StaleOwner)?;
        let t = rb.translation();
        let q = rb.rotation();
        let lv = rb.linvel();
        let av = rb.angvel();
        Ok(BodyState {
            translation_m: [t.x, t.y, t.z],
            rotation: [q.x, q.y, q.z, q.w],
            linvel_m_s: [lv.x, lv.y, lv.z],
            angvel_rad_s: [av.x, av.y, av.z],
            sleeping: rb.is_sleeping(),
            mass_kg: rb.mass(),
        })
    }

    /// Sets a feasibility-test velocity on the unsplit corpse (`None`) or one
    /// explicitly keyed live component. This never exposes Rapier handles.
    pub fn set_dynamic_corpse_component_velocity(
        &mut self,
        id: ArticulatedBodyId,
        component_key: Option<DerivedFractureComponentId>,
        linear_m_s: [f32; 3],
        angular_rad_s: [f32; 3],
    ) -> Result<(), FractureError> {
        if !linear_m_s
            .iter()
            .chain(&angular_rad_s)
            .all(|v| v.is_finite())
        {
            return Err(FractureError::InvalidPlan);
        }
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(FractureError::StaleOwner)?;
        if !assembly.dynamic_corpse {
            return Err(FractureError::StaleOwner);
        }
        let body = if let Some(key) = component_key {
            if assembly.fracture_owner_key != Some(key.owner_key)
                || key.serial == 0
                || key.serial >= assembly.next_component_serial
            {
                return Err(FractureError::StaleOwner);
            }
            assembly
                .parts
                .iter()
                .find(|p| p.component_id == Some(key))
                .map(|part| part.body)
                .ok_or(FractureError::StaleOwner)?
        } else {
            if assembly.parts.iter().any(|p| p.component_id.is_some()) {
                return Err(FractureError::StaleOwner);
            }
            self.entries[entry_index(id.0, self.id_namespace)].body
        };
        let rb = self.bodies.get_mut(body).ok_or(FractureError::StaleOwner)?;
        rb.set_linvel(
            Vector::new(linear_m_s[0], linear_m_s[1], linear_m_s[2]),
            true,
        );
        rb.set_angvel(
            Vector::new(angular_rad_s[0], angular_rad_s[1], angular_rad_s[2]),
            true,
        );
        Ok(())
    }

    /// Casts against the live Rapier shape of every source part and maps the
    /// selected TOI back to the exact occupied source cell. TOIs within 10 µm
    /// tie by the smallest stable part ID.
    pub fn raycast_articulated(
        &mut self,
        id: ArticulatedBodyId,
        origin_m: [f32; 3],
        direction: [f32; 3],
        max_toi_m: f32,
    ) -> Result<SourceCellHit, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let id = id.0;
        let norm = direction.iter().map(|v| v * v).sum::<f32>().sqrt();
        if !origin_m.iter().all(|v| v.is_finite())
            || !norm.is_finite()
            || norm <= 1.0e-6
            || !max_toi_m.is_finite()
            || max_toi_m <= 0.0
        {
            return Err(ArticulatedError::Miss);
        }
        if !self.articulated.contains_key(&id) {
            return Err(ArticulatedError::Miss);
        }
        self.sync_queries();
        let direction = Vector::new(
            direction[0] / norm,
            direction[1] / norm,
            direction[2] / norm,
        );
        let ray = rapier3d::parry::query::Ray::new(
            Vector::new(origin_m[0], origin_m[1], origin_m[2]),
            direction,
        );
        let hit_tois: HashMap<_, _> = {
            let queries = self.broad_phase.as_query_pipeline(
                self.narrow_phase.query_dispatcher(),
                &self.bodies,
                &self.colliders,
                QueryFilter::default(),
            );
            queries
                .intersect_ray(ray, max_toi_m, true)
                .filter(|(handle, _, _)| {
                    self.articulated[&id]
                        .parts
                        .iter()
                        .any(|part| part.collider == *handle)
                })
                .map(|(handle, _, intersection)| (handle, intersection.time_of_impact))
                .collect()
        };
        let assembly = self.articulated.get(&id).ok_or(ArticulatedError::Miss)?;
        let root_pose =
            *self.bodies[self.entries[entry_index(id, self.id_namespace)].body].position();
        let mut best: Option<(f32, u32, [i32; 3], u64, SourceCellHit)> = None;
        for part in &assembly.parts {
            let Some(toi) = hit_tois.get(&part.collider).copied() else {
                continue;
            };
            let collider = self
                .colliders
                .get(part.collider)
                .ok_or(ArticulatedError::Miss)?;
            let collider_pose = if assembly.kinematic_part_bodies || part.component_id.is_some() {
                *collider.position()
            } else {
                let local_pose = *collider
                    .position_wrt_parent()
                    .ok_or(ArticulatedError::Miss)?;
                root_pose * local_pose
            };
            let world_hit = ray.point_at(toi);
            let local = collider_pose.inverse_transform_point(world_hit);
            let cell_coord = [
                local.x / part.cell_m,
                local.y / part.cell_m,
                local.z / part.cell_m,
            ];
            let dims = part.grid.dims();
            let mut candidates: [Vec<i64>; 3] = std::array::from_fn(|axis| {
                let value = cell_coord[axis];
                let nearest = value.round();
                let tolerance =
                    (1.0e-5 / part.cell_m).max(4.0 * f32::EPSILON * value.abs().max(1.0));
                if (value - nearest).abs() <= tolerance {
                    vec![nearest as i64 - 1, nearest as i64]
                } else {
                    vec![value.floor() as i64]
                }
            });
            for (axis, values) in candidates.iter_mut().enumerate() {
                values.retain(|value| *value >= 0 && *value < i64::from(dims[axis]));
            }
            let grid_origin = part.grid.origin();
            let mut cells = Vec::new();
            for &x in &candidates[0] {
                for &y in &candidates[1] {
                    for &z in &candidates[2] {
                        if part.grid.is_solid(x as u32, y as u32, z as u32) {
                            cells.push([x, y, z]);
                        }
                    }
                }
            }
            cells.sort_unstable();
            let Some(idx) = cells.first().copied() else {
                continue;
            };
            let source = [
                grid_origin.x + idx[0],
                grid_origin.y + idx[1],
                grid_origin.z + idx[2],
            ];
            if source
                .iter()
                .any(|v| *v < i32::MIN as i64 || *v > i32::MAX as i64)
            {
                continue;
            }
            let hit = SourceCellHit {
                part_id: part.part_id,
                volume_id: part.volume_id,
                source_revision: part.source_revision,
                cell: [source[0] as i32, source[1] as i32, source[2] as i32],
                toi_m: toi,
                topology_token: assembly.topology_token,
                pose_token: assembly.pose_token,
            };
            let component_serial = part.component_id.map_or(0, |key| key.serial);
            if best.is_none_or(|(old_toi, old_part, old_cell, old_component, _)| {
                toi < old_toi - 1.0e-5
                    || ((toi - old_toi).abs() <= 1.0e-5
                        && (part.part_id, hit.cell, component_serial)
                            < (old_part, old_cell, old_component))
            }) {
                best = Some((toi, part.part_id, hit.cell, component_serial, hit));
            }
        }
        best.map(|(_, _, _, _, hit)| hit)
            .ok_or(ArticulatedError::Miss)
    }

    /// Builds a Rapier rigid body + attached collider from `spec` and inserts
    /// both, returning their handles and the grid-origin collider offset. Shared
    /// by [`Self::add_body`] and [`Self::reactivate_body`].
    fn insert_rapier_body(
        &mut self,
        spec: &BodySpec,
    ) -> (RigidBodyHandle, ColliderHandle, [f32; 3]) {
        let prepared = PreparedCollider::new(&spec.grid, spec.cell_m, spec.representation);
        self.insert_prepared_rapier_body(
            spec.kind,
            spec.mass_properties,
            spec.density_kg_m3,
            spec.translation_m,
            spec.linvel_m_s,
            &prepared,
        )
    }

    fn insert_prepared_rapier_body(
        &mut self,
        kind: BodyKind,
        mass_properties: Option<BodyMassProperties>,
        density_kg_m3: f32,
        translation_m: [f32; 3],
        linvel_m_s: [f32; 3],
        prepared: &PreparedCollider,
    ) -> (RigidBodyHandle, ColliderHandle, [f32; 3]) {
        let rb = match kind {
            BodyKind::Fixed => RigidBodyBuilder::fixed(),
            BodyKind::Dynamic { ccd } => RigidBodyBuilder::dynamic()
                .linvel(Vector::new(linvel_m_s[0], linvel_m_s[1], linvel_m_s[2]))
                .ccd_enabled(ccd),
        }
        .translation(Vector::new(
            translation_m[0],
            translation_m[1],
            translation_m[2],
        ))
        .build();
        let body = self.bodies.insert(rb);

        let built = &prepared.build;
        let offset = prepared.offset_m;
        // With explicit mass properties the collision shape must not contribute
        // mass: the body carries the fine-grid mass / COM / inertia directly.
        let collider_density = if mass_properties.is_some() {
            0.0
        } else {
            density_kg_m3
        };
        let collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(collider_density)
            .translation(Vector::new(offset[0], offset[1], offset[2]))
            .build();
        let collider = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);

        if let Some(props) = mass_properties {
            let rb = &mut self.bodies[body];
            rb.set_additional_mass_properties(rapier_mass_properties(props, offset), false);
            rb.recompute_mass_properties_from_colliders(&self.colliders);
        }
        self.pending_modified.push(collider);
        (body, collider, offset)
    }

    /// Adds a body whose collider was built ahead of time with [`PreparedCollider::new`] from
    /// the same grid, cell size and representation. Identical to [`Self::add_body`] with that
    /// grid, but the expensive shape construction has already happened, possibly on another
    /// thread. Ids and handles are assigned in call order, so adding prepared bodies in a fixed
    /// order gives the same world however they were built.
    pub fn add_prepared_body(
        &mut self,
        spec: PreparedBodySpec,
        prepared: PreparedCollider,
    ) -> BodyId {
        let (body, collider, offset) = self.insert_prepared_rapier_body(
            spec.kind,
            spec.mass_properties,
            spec.density_kg_m3,
            spec.translation_m,
            spec.linvel_m_s,
            &prepared,
        );
        let id = BodyId((u64::from(self.id_namespace) << 32) | self.entries.len() as u64);
        self.entries.push(Entry {
            body,
            collider,
            cell_m: spec.cell_m,
            representation: spec.representation,
            density: spec.density_kg_m3,
            collider_offset_m: offset,
            mass_properties: spec.mass_properties,
            retired: false,
            dormant: false,
            query_only: false,
            restitution: 0.0,
        });
        self.tag_collider(collider, id);
        id
    }

    /// Adds a body and returns its stable id.
    pub fn add_body(&mut self, spec: BodySpec) -> BodyId {
        let (body, collider, offset) = self.insert_rapier_body(&spec);

        let id = BodyId((u64::from(self.id_namespace) << 32) | self.entries.len() as u64);
        self.entries.push(Entry {
            body,
            collider,
            cell_m: spec.cell_m,
            representation: spec.representation,
            density: spec.density_kg_m3,
            collider_offset_m: offset,
            mass_properties: spec.mass_properties,
            retired: false,
            dormant: false,
            query_only: false,
            restitution: 0.0,
        });
        self.tag_collider(collider, id);
        id
    }

    /// Swaps a body's collider for one rebuilt from `grid` in `rep` (an edit).
    /// Returns the **complete** rebuild cost: removing the old collider, the full
    /// occupancy → shape construction (native index extraction or greedy
    /// decomposition plus all shape allocation), the Rapier wrapping, and
    /// re-inserting the new collider under the same body.
    pub fn rebuild_collider(
        &mut self,
        id: BodyId,
        grid: &OccupancyGrid,
        rep: Representation,
    ) -> Duration {
        // An articulated body owns several source colliders. Rebuilding only
        // its compatibility/primary handle would leave stale child shapes.
        assert!(
            !self.articulated.contains_key(&id),
            "single-grid rebuild is invalid for ArticulatedBodyId; use rebuild_articulated_part_grid"
        );
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        debug_assert!(!entry.retired, "rebuild_collider on a retired body");
        if entry.retired {
            return Duration::ZERO;
        }
        let (cell_m, density, body, old_collider, mass_properties, query_only, restitution) = (
            entry.cell_m,
            entry.density,
            entry.body,
            entry.collider,
            entry.mass_properties,
            entry.query_only,
            entry.restitution,
        );

        let start = Instant::now();
        self.colliders
            .remove(old_collider, &mut self.islands, &mut self.bodies, true);
        let built = build_collider(grid, cell_m, rep);
        let offset = grid_origin_offset_m(grid, cell_m);
        // The installed mass properties live on the rigid body and survive the
        // shape swap; the replacement collider stays massless so it cannot
        // perturb them. A geometry edit that changes the fine grid must follow
        // this with `set_mass_properties`.
        let collider_density = if mass_properties.is_some() {
            0.0
        } else {
            density
        };
        let mut collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(collider_density)
            .translation(Vector::new(offset[0], offset[1], offset[2]))
            .build();
        if query_only {
            collider.set_solver_groups(query_only_solver_groups());
        }
        collider.set_restitution(restitution);
        collider.set_restitution_combine_rule(CoefficientCombineRule::Max);
        let handle = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);
        if mass_properties.is_some() {
            self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
        }
        let total = start.elapsed();

        self.tag_collider(handle, id);
        self.entries[entry_index(id, self.id_namespace)].collider = handle;
        self.entries[entry_index(id, self.id_namespace)].representation = rep;
        self.entries[entry_index(id, self.id_namespace)].collider_offset_m = offset;
        self.pending_removed.push(old_collider);
        self.pending_modified.push(handle);
        total
    }

    /// Installs `props` — mass / COM / full inertia derived from a body's fine
    /// material grid — into the body, replacing whatever it carried. The
    /// collision shape is untouched and contributes no mass. Call this after a
    /// geometry edit has rebuilt the collider so the solver's mass properties
    /// track the new fine grid. Wakes the body.
    ///
    /// The body must have been added with [`BodySpec::mass_properties`] set (so
    /// its collider is already massless); otherwise the shape's own mass would
    /// be added on top of `props`.
    pub fn set_mass_properties(&mut self, id: BodyId, props: BodyMassProperties) {
        assert!(
            !self.articulated.contains_key(&id),
            "mass properties are not supported by kinematic articulated assemblies"
        );
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        debug_assert!(!entry.retired, "set_mass_properties on a retired body");
        if entry.retired {
            return;
        }
        debug_assert!(
            entry.mass_properties.is_some() || entry.density == 0.0,
            "set_mass_properties requires a body added with BodySpec::mass_properties = Some(_)"
        );
        entry.mass_properties = Some(props);
        let body = entry.body;
        let offset = entry.collider_offset_m;
        let rb = &mut self.bodies[body];
        rb.set_additional_mass_properties(rapier_mass_properties(props, offset), true);
        rb.recompute_mass_properties_from_colliders(&self.colliders);
    }

    /// Sets contact restitution and retains it across geometry rebuilds and
    /// dormancy. `Max` combination lets a deliberately bouncy body rebound
    /// from ordinary zero-restitution terrain.
    pub fn set_restitution(&mut self, id: BodyId, restitution: f32) {
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        if entry.retired {
            return;
        }
        entry.restitution = restitution.clamp(0.0, 1.0);
        if !entry.dormant {
            if let Some(assembly) = self.articulated.get(&id) {
                for part in &assembly.parts {
                    let collider = &mut self.colliders[part.collider];
                    collider.set_restitution(entry.restitution);
                    collider.set_restitution_combine_rule(CoefficientCombineRule::Max);
                }
            } else {
                let collider = &mut self.colliders[entry.collider];
                collider.set_restitution(entry.restitution);
                collider.set_restitution_combine_rule(CoefficientCombineRule::Max);
            }
        }
    }

    /// Retires a body whose authoritative volume became empty (`ENG-56`): its
    /// Rapier rigid body and attached collider are removed from the simulation
    /// so it can no longer collide with, rest on, or be swept against by any
    /// other body, nor be stepped or queried. The [`BodyId`] slot is kept (ids
    /// are indices — removing one would shift every later id) and marked
    /// retired; callers must drop the handle. Idempotent.
    pub fn retire_body(&mut self, id: BodyId) {
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        if entry.retired {
            return;
        }
        entry.retired = true;
        let body = entry.body;
        self.articulated.remove(&id);
        self.bodies.remove(
            body,
            &mut self.islands,
            &mut self.colliders,
            &mut self.impulse_joints,
            &mut self.multibody_joints,
            true,
        );
        // Same hazard as `rebuild_collider`: the CCD solver caches fixed-target
        // collider handles and only refreshes that cache on a step where a body
        // is CCD-active, so a collider removed on a quiet step can be
        // dereferenced later ("No element at index"). Clearing the solver (it
        // holds nothing else) forces a rescan.
        self.ccd_solver = CCDSolver::new();
    }

    /// Whether `id` has been retired by [`Self::retire_body`].
    pub fn is_retired(&self, id: BodyId) -> bool {
        self.entries[entry_index(id, self.id_namespace)].retired
    }

    /// Deactivates a **dormant** body (T21): removes its Rapier rigid body and
    /// collider so it costs nothing to step and cannot be contacted or swept,
    /// but keeps its [`BodyId`] slot and every rebuild parameter
    /// (`cell_m` / representation / density / mass properties) so
    /// [`Self::reactivate_body`] can restore it. The caller owns the frozen
    /// pose / velocity and passes them back on reactivation. Idempotent; a no-op
    /// on a retired body.
    pub fn deactivate_body(&mut self, id: BodyId) {
        // Dormancy snapshots currently own one grid/collider and cannot restore
        // a per-part assembly without losing its stable source IDs.
        assert!(
            !self.articulated.contains_key(&id),
            "articulated assemblies do not support single-grid dormancy; use retire_articulated_body"
        );
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        if entry.retired || entry.dormant {
            return;
        }
        entry.dormant = true;
        let body = entry.body;
        self.bodies.remove(
            body,
            &mut self.islands,
            &mut self.colliders,
            &mut self.impulse_joints,
            &mut self.multibody_joints,
            true,
        );
        // See `retire_body`: drop the CCD fixed-target cache so it cannot keep a
        // dangling handle to the collider just removed.
        self.ccd_solver = CCDSolver::new();
    }

    /// Restores a body deactivated by [`Self::deactivate_body`] into its
    /// original slot: rebuilds the Rapier rigid body + collider from `grid` and
    /// the stored rebuild parameters, at `translation_m` / `rotation` (xyzw)
    /// with `linvel_m_s` / `angvel_rad_s`. The body starts awake; the solver
    /// re-sleeps it on the next quiet step. A no-op on a retired body or one
    /// that is not dormant.
    #[allow(clippy::too_many_arguments)]
    pub fn reactivate_body(
        &mut self,
        id: BodyId,
        grid: &OccupancyGrid,
        translation_m: [f32; 3],
        rotation: [f32; 4],
        linvel_m_s: [f32; 3],
        angvel_rad_s: [f32; 3],
    ) {
        assert!(
            !self.articulated.contains_key(&id),
            "single-grid reactivation is invalid for ArticulatedBodyId; use an articulated lifecycle operation"
        );
        let entry = &self.entries[entry_index(id, self.id_namespace)];
        if entry.retired || !entry.dormant {
            return;
        }
        let restitution = entry.restitution;
        let spec = BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: entry.representation,
            grid: grid.clone(),
            cell_m: entry.cell_m,
            density_kg_m3: entry.density,
            mass_properties: entry.mass_properties,
            translation_m,
            linvel_m_s,
        };
        let (body, collider, offset) = self.insert_rapier_body(&spec);
        self.tag_collider(collider, id);
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        entry.body = body;
        entry.collider = collider;
        entry.collider_offset_m = offset;
        entry.dormant = false;
        self.set_body_pose(id, translation_m, rotation);
        self.set_body_velocity(id, linvel_m_s, angvel_rad_s);
        self.set_restitution(id, restitution);
    }

    /// Whether `id` is currently dormant (deactivated by [`Self::deactivate_body`]).
    pub fn is_dormant(&self, id: BodyId) -> bool {
        self.entries[entry_index(id, self.id_namespace)].dormant
    }

    /// Bodies that are neither retired nor dormant — the set the solver actually
    /// steps.
    pub fn active_body_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| !e.retired && !e.dormant)
            .count()
    }

    /// Removes just a body's attached collider, keeping the rigid body itself.
    /// Used when a *terrain* ownership's volume becomes empty (`ENG-56`): the
    /// fixed body stays so a later refill can rebuild a collider on it, but
    /// nothing collides with the obsolete solid shape in the meantime. A no-op
    /// on a retired body.
    pub fn remove_collider(&mut self, id: BodyId) {
        assert!(
            !self.articulated.contains_key(&id),
            "single-grid collider removal is invalid for ArticulatedBodyId; use retire_articulated_body"
        );
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        if entry.retired {
            return;
        }
        let collider = entry.collider;
        self.colliders
            .remove(collider, &mut self.islands, &mut self.bodies, true);
        // See `retire_body`: drop the CCD fixed-target cache so it cannot keep a
        // dangling handle to the collider just removed.
        self.ccd_solver = CCDSolver::new();
        self.pending_removed.push(collider);
    }

    /// Whether `id` currently has an attached collider in Rapier.
    pub fn has_collider(&self, id: BodyId) -> bool {
        self.entries
            .get(entry_index(id, self.id_namespace))
            .is_some_and(|entry| !entry.retired && self.colliders.contains(entry.collider))
    }

    /// Excludes `id`'s collider from ever producing a rigid-body **solver**
    /// response — no dynamic body colliding with it is pushed, slowed, or
    /// stopped by it, ever, regardless of either side's own collision groups
    /// (`solver_groups` zeroed on both membership and filter, so Rapier's
    /// interaction test — `And` or `Or` — always fails). It stays fully
    /// visible to ordinary **queries** (`sweep_character`/`cast_shape`/...),
    /// which only ever consult `collision_groups`, a separate field this
    /// leaves untouched.
    ///
    /// For [`CharacterQueryCache`](crate::query_cache::CharacterQueryCache)'s
    /// windows: a window collider must block/redirect the *character's own*
    /// query-based sweep exactly like real terrain would, while never once
    /// acting as a real physical obstacle for any dynamic body (debris, other
    /// players' own dynamics) that happens to pass through the same space —
    /// it is a query-time convenience, not a real object in the world.
    ///
    /// The setting is sticky: every later [`Self::rebuild_collider`] call on
    /// `id` reapplies it to the fresh collider automatically. A no-op on a
    /// retired body.
    pub fn set_query_only(&mut self, id: BodyId) {
        let entry = &mut self.entries[entry_index(id, self.id_namespace)];
        if entry.retired {
            return;
        }
        entry.query_only = true;
        if let Some(assembly) = self.articulated.get(&id) {
            for part in &assembly.parts {
                if let Some(collider) = self.colliders.get_mut(part.collider) {
                    collider.set_solver_groups(query_only_solver_groups());
                }
            }
        } else if let Some(collider) = self.colliders.get_mut(entry.collider) {
            collider.set_solver_groups(query_only_solver_groups());
        }
    }

    /// Refreshes the broad-phase spatial index for every collider added or
    /// rebuilt since the last call to this or [`Self::step`], for
    /// **queries only** (`sweep_character`/`cast_shape`/...) — without
    /// stepping the dynamics pipeline: no gravity or velocity integration, no
    /// contact resolution, for *any* body in this world. [`Self::step`]
    /// already refreshes the broad-phase as part of its own full pipeline
    /// step, so a caller that steps this world's simulation every tick
    /// regardless (the normal case) never needs this. It exists for a caller
    /// that adds/rebuilds colliders on a world it does **not** own the tick
    /// loop for — [`CharacterQueryCache`](crate::query_cache::CharacterQueryCache),
    /// which must never advance a simulation another caller is driving.
    ///
    /// Uses [`BroadPhaseBvh::set_aabb`] per modified collider, **not**
    /// [`BroadPhaseBvh::update`]: `update` is the same call [`Self::step`]
    /// uses to detect newly-overlapping pairs and emit `AddPair` — and
    /// Rapier permanently remembers, per pair, that it already announced one
    /// ("no need to re-send an `AddPair` event... if no `RemovePair`
    /// happened since", `BroadPhaseBvh::update`'s own doc). Calling `update`
    /// from here — as this used to — consumes that one-time announcement
    /// for any dynamic body's pair that happens to be pending, with no
    /// narrow-phase registration ever created for it: `step`'s *own* later
    /// `update` call sees the pair as already-announced and never re-emits
    /// it, so that pair's contacts are never generated again for the rest of
    /// the simulation, however many times `step` runs afterwards. Confirmed
    /// with an isolated repro (`.local/analysis/pr109-probe`): a dynamic
    /// cube resting on a fixed one falls straight through it if a single
    /// `sync_queries` call happens before the first `step`. `set_aabb`
    /// refits the tree immediately (so queries see the new shape/position
    /// right away) but explicitly defers pair evaluation "to the next
    /// broad-phase update" (its own doc) — i.e. to `step`'s real one, which
    /// still runs the full detect-and-announce logic exactly once, correctly.
    /// A collider in `pending_removed` needs no action here: `remove_collider`/
    /// `retire_body` already removed it from `self.colliders` before pushing
    /// it there, so a query pipeline built from `self.colliders` naturally
    /// filters it out via the lookup miss regardless of the tree's own leaf
    /// cleanup timing, and `step`'s removal handling runs on Rapier's native
    /// dirty flags next time regardless of what this does.
    pub fn sync_queries(&mut self) {
        if self.pending_modified.is_empty() && self.pending_removed.is_empty() {
            return;
        }
        // `set_body_pose` updates the rigid body immediately, but attached
        // collider world poses are normally propagated by `step`. Query-only
        // mirrors do not step, so do that narrow propagation here before
        // computing their new AABBs.
        self.bodies
            .propagate_modified_body_positions_to_colliders(&mut self.colliders);
        for &handle in &self.pending_modified {
            if let Some(collider) = self.colliders.get(handle) {
                let aabb = collider.compute_aabb();
                self.broad_phase.set_aabb(&self.params, handle, aabb);
            }
        }
        self.pending_modified.clear();
        self.pending_removed.clear();
    }

    fn validate_kinematic_surface_travel(
        &self,
        targets: &[(RigidBodyHandle, Pose)],
    ) -> Result<(), crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        if self.solver_substeps == 1 {
            return Ok(());
        }
        for (body_handle, target) in targets {
            let body = &self.bodies[*body_handle];
            let start = *body.position();
            let translation = (target.translation - start.translation).length();
            let angle = (target.rotation * start.rotation.inverse())
                .to_scaled_axis()
                .length();
            let radius = self
                .articulated
                .values()
                .flat_map(|assembly| {
                    assembly.parts.iter().filter_map(move |part| {
                        (part.body == *body_handle).then_some(if assembly.kinematic_part_bodies {
                            part.part_surface_radius_m
                        } else {
                            part.root_surface_radius_m
                        })
                    })
                })
                .fold(0.0_f32, f32::max);
            let maximum_surface_travel = translation + angle * radius;
            let admitted = self.solver_substeps as f32 * MAX_KINEMATIC_SURFACE_TRAVEL_PER_SUBSTEP_M;
            if maximum_surface_travel > admitted + 1.0e-6 {
                return Err(ArticulatedError::WorkBudget);
            }
        }
        Ok(())
    }

    /// Advances the world by one fixed step.
    pub fn step(&mut self) -> StepTiming {
        // The pipeline's own broad-phase pass reads Rapier's native
        // per-collider dirty flags directly (`ColliderChanges`), not this
        // list — so whatever's pending is already covered by the step below,
        // and needs clearing here or it grows without bound for a caller
        // that always steps (never calling `sync_queries`) and never
        // otherwise touches these — the normal case, and the only reason
        // `sync_queries` exists at all is the caller that doesn't.
        self.pending_modified.clear();
        self.pending_removed.clear();
        let outer_dt = self.params.dt;
        let substeps = self.solver_substeps;
        let kinematic_targets = self
            .bodies
            .iter()
            .filter_map(|(handle, body)| {
                let start = *body.position();
                let target = *body.next_position();
                (body.is_kinematic() && start != target).then_some((handle, start, target))
            })
            .collect::<Vec<_>>();
        let mut pipeline = Duration::ZERO;
        let mut contact_witness_capture = Duration::ZERO;
        let mut normal_impulse_n_s = 0.0_f32;
        let mut impulse_pair_substeps = 0;
        self.articulated_contact_witnesses.clear();
        self.contact_witness_overflow = 0;
        self.params.dt = outer_dt / substeps as f32;
        for substep in 0..substeps {
            let fraction = (substep + 1) as f32 / substeps as f32;
            for (handle, start, target) in &kinematic_targets {
                let substep_target = if substep + 1 == substeps {
                    *target
                } else {
                    let translation =
                        start.translation + (target.translation - start.translation) * fraction;
                    let angular_delta = target.rotation * start.rotation.inverse();
                    let rotation =
                        Rotation::from_scaled_axis(angular_delta.to_scaled_axis() * fraction)
                            * start.rotation;
                    Pose::new(translation, rotation.to_scaled_axis())
                };
                self.bodies[*handle].set_next_kinematic_position(substep_target);
            }
            let start = Instant::now();
            self.pipeline.step(
                self.gravity,
                &self.params,
                &mut self.islands,
                &mut self.broad_phase,
                &mut self.narrow_phase,
                &mut self.bodies,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                &mut self.ccd_solver,
                &(),
                &(),
            );
            pipeline += start.elapsed();
            let capture_start = Instant::now();
            self.capture_articulated_contact_witnesses(substep, substeps);
            contact_witness_capture += capture_start.elapsed();
            for pair in self.narrow_phase.contact_pairs() {
                let impulse = pair.total_impulse_magnitude();
                if pair.has_any_active_contact() && impulse.is_finite() && impulse > 0.0 {
                    normal_impulse_n_s += impulse;
                    impulse_pair_substeps += 1;
                }
            }
        }
        self.params.dt = outer_dt;
        // Rapier advances position-based kinematics after its broad-phase
        // update. Refresh articulated shape AABBs now so queries immediately
        // after this same published physics boundary see the committed pose,
        // including large root rotations that leave the previous AABB.
        let transitioned_colliders = self
            .articulated
            .values()
            .filter(|assembly| {
                assembly.pending_root_pose_token.is_some() || assembly.pending_pose.is_some()
            })
            .flat_map(|assembly| assembly.parts.iter().map(|part| part.collider))
            .collect::<Vec<_>>();
        if !transitioned_colliders.is_empty() {
            self.bodies
                .propagate_modified_body_positions_to_colliders(&mut self.colliders);
            for handle in transitioned_colliders {
                if let Some(collider) = self.colliders.get(handle) {
                    self.broad_phase
                        .set_aabb(&self.params, handle, collider.compute_aabb());
                }
            }
        }
        self.step_count += 1;
        for (id, assembly) in &mut self.articulated {
            if let Some(pose_token) = assembly.pending_root_pose_token.take() {
                assembly.pose_token = pose_token;
                if assembly.kinematic_part_bodies {
                    let body = self.entries[entry_index(*id, self.id_namespace)].body;
                    let pose = self.bodies[body].position();
                    assembly.root_pose = PartTransform {
                        translation_m: [pose.translation.x, pose.translation.y, pose.translation.z],
                        rotation_xyzw: [
                            pose.rotation.x,
                            pose.rotation.y,
                            pose.rotation.z,
                            pose.rotation.w,
                        ],
                    };
                }
            }
            if let Some(pending) = assembly.pending_pose.take() {
                for (part, (part_id, local, world)) in assembly.parts.iter_mut().zip(pending.parts)
                {
                    debug_assert_eq!(part.part_id, part_id);
                    part.local_pose = local;
                    part.assembly_pose = world;
                }
                assembly.pose_token = pending.pose_token;
            }
        }
        StepTiming {
            step_index: self.step_count,
            pipeline,
            contact_witness_capture,
            substeps,
            normal_impulse_n_s,
            impulse_pair_substeps,
        }
    }

    /// Sweeps a capsule character through the current collider world one tick
    /// (T19). `position_m` and the returned translation are the capsule's
    /// **feet**; collision resolution (wall slide, autostep, slope limit, ground
    /// snap) is Rapier's `KinematicCharacterController` with the frozen
    /// [`crate::character::tuning`] constants.
    ///
    /// The query runs against the broad-phase BVH as last refreshed by
    /// [`Self::step`], so callers advance characters *after* stepping physics on
    /// a tick that rebuilt any collider.
    pub fn sweep_character(
        &self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
    ) -> crate::character::CharacterMove {
        self.sweep_character_with(params, position_m, desired_translation_m, dt_s, |_| {})
    }

    /// Same as [`Self::sweep_character`], but `on_collision` is called for
    /// every [`CharacterCollision`] Rapier's controller reports along the way
    /// — normally discarded (`sweep_character` passes an empty closure).
    /// ENG-69 round 15: added to directly confirm (not just infer from
    /// endpoint position diffs) that a representation-divergence event is a
    /// real contact against real geometry, not a coincidental integration
    /// difference — see `spall_physics::character::tests::
    /// strafing_the_g1_tower_wall_diverges_between_representations`'s
    /// per-tick trace, which found the divergence lands entirely within one
    /// tick.
    pub fn sweep_character_with(
        &self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        on_collision: impl FnMut(&CharacterCollision),
    ) -> crate::character::CharacterMove {
        self.sweep_character_impl(
            params,
            position_m,
            desired_translation_m,
            dt_s,
            &[],
            on_collision,
        )
    }

    /// Same as [`Self::sweep_character`], but every collider in `exclude`
    /// (by [`BodyId`] — a retired id is silently skipped) is invisible to
    /// this one sweep's query, as if it were not in the world at all. ENG-69
    /// round 18: a character's own [`CharacterQueryCache`](crate::query_cache::CharacterQueryCache)
    /// window must be the *only* terrain-like collider it sees for its own
    /// movement — not the real whole-terrain collider (the window replaces
    /// it, exactly), and not another character's own window (each
    /// character's window is sized and centred for *that* character alone;
    /// nothing about it is meaningful to anyone else's sweep). Both need
    /// excluding explicitly, by identity, not by a collision-group category —
    /// see [`Self::set_query_only`]'s doc for why a coarse per-category
    /// exclusion isn't the right tool here (it would also have to reject the
    /// caller's *own* window, which a predicate lets back in individually).
    pub fn sweep_character_excluding(
        &self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        exclude: &[BodyId],
    ) -> crate::character::CharacterMove {
        let mut handles = Vec::new();
        for id in exclude {
            let entry = &self.entries[entry_index(*id, self.id_namespace)];
            if entry.retired || entry.dormant {
                continue;
            }
            if let Some(assembly) = self.articulated.get(id) {
                handles.extend(assembly.parts.iter().map(|part| part.collider));
            } else {
                handles.push(entry.collider);
            }
        }
        self.sweep_character_impl(
            params,
            position_m,
            desired_translation_m,
            dt_s,
            &handles,
            |_| {},
        )
    }

    /// Authoritative character sweep that also transfers momentum into any
    /// dynamic rigid bodies the capsule contacts. The capsule itself remains
    /// kinematic, but Rapier's character impulse approximation treats it as a
    /// body of `params.mass_kg`, so a many-voxel body accelerates less than a
    /// small one under the same player movement.
    ///
    /// This is intentionally separate from [`Self::sweep_character_excluding`]:
    /// prediction clients use query-only replica colliders and must never
    /// become an authority for rigid-body impulses.
    pub fn sweep_character_pushing_excluding(
        &mut self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        exclude: &[BodyId],
    ) -> crate::character::CharacterMove {
        let base = self.exclusion_for(exclude.iter().copied());
        self.sweep_character_pushing_excluding_set(
            params,
            position_m,
            desired_translation_m,
            dt_s,
            &base,
            &[],
        )
    }

    /// The bodies `ids` as a reusable [`ColliderExclusion`].
    pub fn exclusion_for(&self, ids: impl IntoIterator<Item = BodyId>) -> ColliderExclusion {
        ColliderExclusion {
            bodies: ids.into_iter().collect(),
        }
    }

    /// Records which body owns `collider`, so a query can tell whose it is without the handle.
    fn tag_collider(&mut self, collider: ColliderHandle, id: BodyId) {
        if let Some(c) = self.colliders.get_mut(collider) {
            c.user_data = u128::from(id.0) + 1;
        }
    }

    /// [`Self::sweep_character_pushing_excluding`] with the bulk of the exclusion prebuilt.
    /// Every terrain brick has its own collider, so a full-size world excludes hundreds of
    /// thousands of them from each character query; as a list that was tens of milliseconds per
    /// tick, as a hash set built once it is a lookup. `extra` is the few per-call additions.
    #[allow(clippy::too_many_arguments)]
    pub fn sweep_character_pushing_excluding_set(
        &mut self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        base: &ColliderExclusion,
        extra: &[BodyId],
    ) -> crate::character::CharacterMove {
        let extra_bodies = self.exclusion_for(extra.iter().copied());
        let excluded = CombinedExclusion {
            base,
            extra: &extra_bodies,
        };
        let controller = crate::character::controller();
        let shape = crate::character::capsule(params);
        let centre = params.centre_offset_m();
        let feet = Vector::new(
            position_m[0] as f32,
            position_m[1] as f32,
            position_m[2] as f32,
        );
        let pos = Pose::from_translation(feet + Vector::new(0.0, centre, 0.0));
        let excluded_predicate =
            |_handle: ColliderHandle, collider: &Collider| !excluded.excludes(collider);
        let filter = if excluded.is_empty() {
            QueryFilter::default()
        } else {
            QueryFilter::default().predicate(&excluded_predicate)
        };
        let desired = Vector::new(
            desired_translation_m[0],
            desired_translation_m[1],
            desired_translation_m[2],
        );
        let mut collisions = Vec::new();
        let moved = {
            let queries = self.broad_phase.as_query_pipeline(
                self.narrow_phase.query_dispatcher(),
                &self.bodies,
                &self.colliders,
                filter,
            );
            controller.move_shape(dt_s, &queries, &shape, &pos, desired, |collision| {
                collisions.push(collision)
            })
        };
        // Rapier's impulse solver keeps one manifold buffer across every
        // dynamic collider the capsule overlaps, but parry's
        // `contact_manifolds` may clear that buffer, after which Rapier
        // slices it from a stale index and panics ("range start index N out
        // of range for slice of length 0") as soon as the capsule touches a
        // second dynamic collider (e.g. two plinko balls at once). Solve one
        // dynamic collider per call so the buffer never spans colliders.
        let mut touched: Vec<ColliderHandle> = Vec::new();
        {
            let queries = self.broad_phase.as_query_pipeline(
                self.narrow_phase.query_dispatcher(),
                &self.bodies,
                &self.colliders,
                QueryFilter::default(),
            );
            for collision in &collisions {
                let aabb = shape.compute_aabb(&collision.character_pos).loosened(0.5);
                for (handle, collider) in queries.intersect_aabb_conservative(aabb) {
                    let dynamic = collider
                        .parent()
                        .and_then(|parent| self.bodies.get(parent))
                        .is_some_and(|body| body.is_dynamic());
                    if dynamic && !excluded.excludes(collider) && !touched.contains(&handle) {
                        touched.push(handle);
                    }
                }
            }
        }
        // The impulse is sized from the character's mass, so a heavy
        // character launches a body faster than it is walking; the body then
        // separates, friction slows it, and the character catches up and
        // kicks it again -- visible as a body jumping in increments. Cap the
        // horizontal speed a push can add at the character's own.
        let push_velocity = Vector::new(desired.x / dt_s, 0.0, desired.z / dt_s);
        let push_speed = push_velocity.length();
        for handle in touched {
            let body_handle = self.colliders.get(handle).and_then(|c| c.parent());
            let along_before = body_handle
                .and_then(|b| self.bodies.get(b))
                .map(|rb| rb.linvel().dot(push_velocity) / push_speed.max(f32::MIN_POSITIVE));
            let only = move |candidate: ColliderHandle, _collider: &Collider| candidate == handle;
            let mut queries = self.broad_phase.as_query_pipeline_mut(
                self.narrow_phase.query_dispatcher(),
                &mut self.bodies,
                &mut self.colliders,
                QueryFilter::default().predicate(&only),
            );
            controller.solve_character_collision_impulses(
                dt_s,
                &mut queries,
                &shape,
                params.mass_kg.max(f32::MIN_POSITIVE),
                &collisions,
            );
            if push_speed > 1.0e-3
                && let (Some(b), Some(before)) = (body_handle, along_before)
                && let Some(rb) = self.bodies.get_mut(b)
            {
                let dir = push_velocity / push_speed;
                let velocity = rb.linvel();
                let along = velocity.dot(dir);
                let limit = push_speed.max(before);
                if along > limit {
                    rb.set_linvel(velocity - dir * (along - limit), true);
                }
            }
        }
        // The controller stops the character a skin's width short of a body,
        // so on some ticks it reports no collision at all: no impulse, the
        // body slows against friction, and the next tick kicks it again --
        // the push alternates on and off. Carry every dynamic body that is
        // near and ahead of the character at the character's own speed
        // whether or not this tick registered a contact.
        if push_speed > CARRY_MIN_SPEED_M_S {
            let end = Pose::from_translation(pos.translation + moved.translation);
            let dir = push_velocity / push_speed;
            let candidates: Vec<(ColliderHandle, RigidBodyHandle)> = {
                let queries = self.broad_phase.as_query_pipeline(
                    self.narrow_phase.query_dispatcher(),
                    &self.bodies,
                    &self.colliders,
                    QueryFilter::default(),
                );
                let aabb = shape.compute_aabb(&end).loosened(CARRY_MARGIN_M);
                queries
                    .intersect_aabb_conservative(aabb)
                    .filter(|(_, collider)| !excluded.excludes(collider))
                    .filter_map(|(handle, collider)| {
                        let body = collider.parent()?;
                        self.bodies
                            .get(body)
                            .is_some_and(|rb| rb.is_dynamic())
                            .then_some((handle, body))
                    })
                    .collect()
            };
            for (handle, body) in candidates {
                let collider = &self.colliders[handle];
                let ahead = (collider.position().translation - end.translation).dot(dir) > 0.0;
                let near = rapier3d::parry::query::distance(
                    &end,
                    &shape,
                    collider.position(),
                    collider.shape(),
                )
                .is_ok_and(|gap| gap <= CARRY_MARGIN_M);
                if !(ahead && near) {
                    continue;
                }
                let rb = &mut self.bodies[body];
                let velocity = rb.linvel();
                let along = velocity.dot(dir);
                if along < push_speed {
                    rb.set_linvel(velocity + dir * (push_speed - along), true);
                }
            }
        }
        crate::character::CharacterMove {
            translation_m: [
                moved.translation.x,
                moved.translation.y,
                moved.translation.z,
            ],
            grounded: moved.grounded,
        }
    }

    fn sweep_character_impl(
        &self,
        params: crate::character::CharacterParams,
        position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        exclude: &[ColliderHandle],
        mut on_collision: impl FnMut(&CharacterCollision),
    ) -> crate::character::CharacterMove {
        let controller = crate::character::controller();
        let shape = crate::character::capsule(params);
        let centre = params.centre_offset_m();
        let feet = Vector::new(
            position_m[0] as f32,
            position_m[1] as f32,
            position_m[2] as f32,
        );
        let pos = Pose::from_translation(feet + Vector::new(0.0, centre, 0.0));
        let excluded_predicate =
            move |handle: ColliderHandle, _collider: &Collider| !exclude.contains(&handle);
        let filter = if exclude.is_empty() {
            QueryFilter::default()
        } else {
            QueryFilter::default().predicate(&excluded_predicate)
        };
        let queries = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            filter,
        );
        let desired = Vector::new(
            desired_translation_m[0],
            desired_translation_m[1],
            desired_translation_m[2],
        );
        let moved = controller.move_shape(dt_s, &queries, &shape, &pos, desired, |c| {
            on_collision(&c);
        });
        crate::character::CharacterMove {
            translation_m: [
                moved.translation.x,
                moved.translation.y,
                moved.translation.z,
            ],
            grounded: moved.grounded,
        }
    }

    /// Number of bodies.
    pub fn body_count(&self) -> usize {
        self.entries.len()
    }

    /// Number of steps run so far.
    pub fn steps_run(&self) -> u64 {
        self.step_count
    }

    /// Current representation of a body.
    pub fn representation(&self, id: BodyId) -> Representation {
        self.entries[entry_index(id, self.id_namespace)].representation
    }

    /// Captures the creating world's identity, step/slot counts and selected
    /// body/collider state for a same-step commit fence. A fixed body slot may
    /// have no collider after an empty terrain-brick publication; that absence
    /// is captured and a collider appearing or disappearing makes the fence
    /// stale.
    pub fn snapshot_body_commit_fence(
        &self,
        ids: &[BodyId],
    ) -> Result<BodyCommitFence, crate::articulated::ArticulatedError> {
        let mut seen = std::collections::BTreeSet::new();
        let mut bodies = Vec::with_capacity(ids.len());
        for id in ids {
            if !seen.insert(id.0) || (id.0 >> 32) as u32 != self.id_namespace {
                return Err(crate::articulated::ArticulatedError::Hierarchy);
            }
            let index = (id.0 as u32) as usize;
            let entry = self
                .entries
                .get(index)
                .ok_or(crate::articulated::ArticulatedError::Hierarchy)?;
            let body = self
                .bodies
                .get(entry.body)
                .ok_or(crate::articulated::ArticulatedError::Hierarchy)?;
            let collider =
                self.colliders
                    .get(entry.collider)
                    .map(|collider| ColliderCommitFenceState {
                        handle: entry.collider,
                        shape: collider.shared_shape().clone(),
                        position: *collider.position(),
                        enabled: collider.is_enabled(),
                        sensor: collider.is_sensor(),
                        active_collision_types: collider.active_collision_types(),
                        collision_groups: collider.collision_groups(),
                        solver_groups: collider.solver_groups(),
                        friction: collider.friction(),
                        restitution: collider.restitution(),
                    });
            bodies.push(BodyCommitFenceEntry {
                id: *id,
                body: entry.body,
                body_type: body.body_type(),
                position: *body.position(),
                linvel: body.linvel(),
                angvel: body.angvel(),
                mass: body.mass(),
                center_of_mass: body.center_of_mass(),
                retired: entry.retired,
                dormant: entry.dormant,
                collider,
                representation: entry.representation,
                query_only: entry.query_only,
                density: entry.density,
                mass_properties: entry.mass_properties,
            });
        }
        Ok(BodyCommitFence {
            world_identity: self.preparation_identity.clone(),
            step_count: self.step_count,
            entry_count: self.entries.len(),
            body_count: self.bodies.len(),
            bodies,
        })
    }

    /// Revalidates a selected-body fence. Same-step pose/twist, collider
    /// rebuild/shape/config, or entry changes invalidate it.
    pub fn validate_body_commit_fence(&self, fence: &BodyCommitFence) -> bool {
        if !std::sync::Arc::ptr_eq(&fence.world_identity, &self.preparation_identity)
            || fence.step_count != self.step_count
            || fence.entry_count != self.entries.len()
            || fence.body_count != self.bodies.len()
        {
            return false;
        }
        fence.bodies.iter().all(|saved| {
            if (saved.id.0 >> 32) as u32 != self.id_namespace {
                return false;
            }
            let index = (saved.id.0 as u32) as usize;
            let Some(entry) = self.entries.get(index) else {
                return false;
            };
            if entry.body != saved.body
                || entry.retired != saved.retired
                || entry.dormant != saved.dormant
                || entry.representation != saved.representation
                || entry.query_only != saved.query_only
                || entry.density.to_bits() != saved.density.to_bits()
                || entry.mass_properties != saved.mass_properties
            {
                return false;
            }
            let Some(body) = self.bodies.get(saved.body) else {
                return false;
            };
            let collider_matches = match (&saved.collider, self.colliders.get(entry.collider)) {
                (None, None) => true,
                (Some(saved), Some(collider)) => {
                    saved.handle == entry.collider
                        && std::sync::Arc::ptr_eq(&saved.shape.0, &collider.shared_shape().0)
                        && *collider.position() == saved.position
                        && collider.is_enabled() == saved.enabled
                        && collider.is_sensor() == saved.sensor
                        && collider.active_collision_types() == saved.active_collision_types
                        && collider.collision_groups() == saved.collision_groups
                        && collider.solver_groups() == saved.solver_groups
                        && collider.friction().to_bits() == saved.friction.to_bits()
                        && collider.restitution().to_bits() == saved.restitution.to_bits()
                }
                _ => false,
            };
            collider_matches
                && body.body_type() == saved.body_type
                && *body.position() == saved.position
                && body.linvel() == saved.linvel
                && body.angvel() == saved.angvel
                && body.mass().to_bits() == saved.mass.to_bits()
                && body.center_of_mass() == saved.center_of_mass
        })
    }

    /// Kinematic snapshot of a body. A retired body (its volume became empty)
    /// or a dormant one (T21, deactivated pending reactivation) has no Rapier
    /// body left; it reports an all-zero, non-sleeping state and the caller is
    /// expected to hold the authoritative pose itself.
    pub fn body_state(&self, id: BodyId) -> BodyState {
        let entry = &self.entries[entry_index(id, self.id_namespace)];
        debug_assert!(!entry.retired, "body_state on a retired body");
        debug_assert!(!entry.dormant, "body_state on a dormant body");
        if entry.retired || entry.dormant {
            return BodyState {
                translation_m: [0.0; 3],
                rotation: [0.0, 0.0, 0.0, 1.0],
                linvel_m_s: [0.0; 3],
                angvel_rad_s: [0.0; 3],
                sleeping: false,
                mass_kg: 0.0,
            };
        }
        let rb = &self.bodies[entry.body];
        let t = rb.translation();
        let q = rb.rotation();
        let lv = rb.linvel();
        let av = rb.angvel();
        BodyState {
            translation_m: [t.x, t.y, t.z],
            rotation: [q.x, q.y, q.z, q.w],
            linvel_m_s: [lv.x, lv.y, lv.z],
            angvel_rad_s: [av.x, av.y, av.z],
            sleeping: rb.is_sleeping(),
            mass_kg: entry
                .mass_properties
                .map(|p| p.mass_kg)
                .unwrap_or_else(|| rb.mass()),
        }
    }

    /// Diagnostic (wake-burst attribution): for each id in `ids`, the *other dynamic*
    /// bodies it currently has an active contact manifold with, as
    /// `(queried, partner)` pairs. Read-only; builds
    /// a handle -> id map per call, so call it only for bursts.
    pub fn touching_dynamic_bodies(&self, ids: &[BodyId]) -> Vec<(BodyId, BodyId)> {
        let by_handle: std::collections::HashMap<RigidBodyHandle, BodyId> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.retired && !e.dormant)
            .map(|(i, e)| {
                (
                    e.body,
                    BodyId((u64::from(self.id_namespace) << 32) | i as u64),
                )
            })
            .collect();
        let mut out = Vec::new();
        for &id in ids {
            let entry = &self.entries[entry_index(id, self.id_namespace)];
            if entry.retired || entry.dormant {
                continue;
            }
            for &c in self.bodies[entry.body].colliders() {
                for pair in self.narrow_phase.contact_pairs_with(c) {
                    if !pair.has_any_active_contact() {
                        continue;
                    }
                    let other = if pair.collider1 == c {
                        pair.collider2
                    } else {
                        pair.collider1
                    };
                    let Some(parent) = self.colliders.get(other).and_then(|o| o.parent()) else {
                        continue;
                    };
                    if !self.bodies[parent].is_dynamic() {
                        continue;
                    }
                    if let Some(&pid) = by_handle.get(&parent) {
                        out.push((id, pid));
                    }
                }
            }
        }
        out
    }

    /// Number of narrow-phase contact pairs currently tracked.
    pub fn contact_pair_count(&self) -> usize {
        self.narrow_phase.contact_pairs().count()
    }

    /// The configured gravity vector, m/s². Callers converting contact impulses
    /// into damage (T21) use its magnitude for the `m·g·dt` resting-load
    /// reference.
    pub fn gravity_m_s2(&self) -> [f32; 3] {
        [self.gravity.x, self.gravity.y, self.gravity.z]
    }

    /// Deepest current contact penetration across all pairs, metres (0 if none).
    pub fn max_penetration_m(&self) -> f32 {
        let mut worst = 0.0_f32;
        for pair in self.narrow_phase.contact_pairs() {
            for manifold in &pair.manifolds {
                for point in &manifold.points {
                    if point.dist < 0.0 {
                        worst = worst.max(-point.dist);
                    }
                }
            }
        }
        worst
    }

    /// The [`RigidBodyHandle`] for `id`, mapped back to a [`BodyId`], if `id` is
    /// a live (non-retired) body this world owns.
    fn body_id_of(&self, handle: RigidBodyHandle) -> Option<BodyId> {
        self.entries
            .iter()
            .position(|e| !e.retired && e.body == handle)
            .map(|i| BodyId((u64::from(self.id_namespace) << 32) | i as u64))
            .or_else(|| {
                self.articulated.iter().find_map(|(id, assembly)| {
                    (assembly.kinematic_part_bodies
                        || assembly
                            .parts
                            .iter()
                            .any(|part| part.component_id.is_some()))
                    .then(|| assembly.parts.iter().any(|part| part.body == handle))
                    .filter(|owned| *owned)
                    .map(|_| *id)
                })
            })
    }

    fn source_part_of(&self, body: BodyId, collider: ColliderHandle) -> Option<u32> {
        self.articulated.get(&body).and_then(|assembly| {
            assembly
                .parts
                .iter()
                .find(|part| part.collider == collider)
                .map(|part| part.part_id)
        })
    }

    /// Copies solver-point contact data before the next global solver substep
    /// can recycle or discard it. The returned records contain no Rapier
    /// handles; published query pose tokens are updated only after the complete
    /// outer step.
    fn capture_articulated_contact_witnesses(&mut self, substep: usize, substeps: usize) {
        let mut captured = Vec::new();
        let room =
            MAX_CONTACT_WITNESSES_PER_STEP.saturating_sub(self.articulated_contact_witnesses.len());
        let mut overflow = 0_u64;
        let fraction = (substep + 1) as f32 / substeps as f32;
        for pair in self.narrow_phase.contact_pairs() {
            if !pair.has_any_active_contact() {
                continue;
            }
            for manifold in pair.solver_manifolds() {
                let (Some(rb1), Some(rb2)) = (manifold.data.rigid_body1, manifold.data.rigid_body2)
                else {
                    continue;
                };
                let (Some(body1), Some(body2)) = (self.body_id_of(rb1), self.body_id_of(rb2))
                else {
                    continue;
                };
                let Some(collider1) = self.colliders.get(pair.collider1) else {
                    continue;
                };
                let Some(collider2) = self.colliders.get(pair.collider2) else {
                    continue;
                };
                let normal = manifold.data.normal;
                let normal_len = normal.length();
                if !normal_len.is_finite() || normal_len <= f32::EPSILON {
                    continue;
                }
                let normal = normal / normal_len;
                let pose1 = collider1.position();
                let pose2 = collider2.position();
                for contact in &manifold.data.solver_contacts {
                    let [contact_index] = contact.contact_indices();
                    let Some(point) = manifold.points.get(contact_index as usize) else {
                        continue;
                    };
                    // Solver anchors are COM-local and Rapier exposes the
                    // matching world witness transform. This remains correct
                    // for collider offsets and clustered compound subshapes.
                    let (p1, p2) = manifold
                        .data
                        .solver_contact_world_points(contact, &self.bodies);
                    let source_local1 = pose1.rotation.inverse() * (p1 - pose1.translation);
                    let source_local2 = pose2.rotation.inverse() * (p2 - pose2.translation);
                    if ![p1.x, p1.y, p1.z, p2.x, p2.y, p2.z, point.dist]
                        .iter()
                        .all(|value| value.is_finite())
                    {
                        continue;
                    }
                    let impulse = point.data.impulse;
                    let impulse = (impulse.is_finite() && impulse >= 0.0).then_some(impulse);
                    let source_sides = [
                        self.source_contact_witness(
                            body1,
                            pair.collider1,
                            source_local1,
                            p1,
                            normal,
                            point.dist,
                            impulse,
                        ),
                        self.source_contact_witness(
                            body2,
                            pair.collider2,
                            source_local2,
                            p2,
                            -normal,
                            point.dist,
                            impulse,
                        ),
                    ];
                    if source_sides.iter().all(Option::is_none) {
                        continue;
                    }
                    let record = ArticulatedContactWitness {
                        version: CONTACT_WITNESS_VERSION,
                        outer_step: self.step_count + 1,
                        solver_substep: substep as u16,
                        substep_fraction: fraction,
                        bodies: [body1, body2],
                        world_contact_point_m: [
                            0.5 * (p1.x + p2.x),
                            0.5 * (p1.y + p2.y),
                            0.5 * (p1.z + p2.z),
                        ],
                        source_sides,
                    };
                    if captured.len() < room {
                        captured.push(record);
                    } else {
                        overflow += 1;
                    }
                }
            }
        }
        self.contact_witness_overflow += overflow;
        self.articulated_contact_witnesses.extend(captured);
    }

    fn source_contact_witness(
        &self,
        body: BodyId,
        collider_handle: ColliderHandle,
        local_point: Vector,
        world_point: Vector,
        outward_world: Vector,
        separation_m: f32,
        impulse: Option<f32>,
    ) -> Option<SourceContactWitness> {
        let assembly = self.articulated.get(&body)?;
        let part = assembly
            .parts
            .iter()
            .find(|part| part.collider == collider_handle)?;
        let collider = self.colliders.get(collider_handle)?;
        let collider_pose = collider.position();
        let local_normal = collider_pose.rotation.inverse() * outward_world;
        let attribution = resolve_source_cell_witness(
            &part.grid,
            part.cell_m,
            local_point,
            local_normal,
            separation_m,
        );
        Some(SourceContactWitness {
            part_id: part.part_id,
            volume_id: part.volume_id,
            source_revision: part.source_revision,
            topology_token: assembly.topology_token,
            pose_token: assembly
                .pending_pose
                .as_ref()
                .map(|pending| pending.pose_token)
                .or(assembly.pending_root_pose_token)
                .unwrap_or(assembly.pose_token),
            source_witness_world_m: [world_point.x, world_point.y, world_point.z],
            source_outward_normal_world: [outward_world.x, outward_world.y, outward_world.z],
            separation_m,
            normal_impulse_n_s: impulse,
            attribution,
        })
    }

    /// Geometric witness records retained from the last completed outer step.
    /// Their authored pose token and substep fraction identify the source pose
    /// that generated them; the records do not mutate or replace ray-query
    /// provenance.
    pub fn articulated_contact_witnesses(&self) -> &[ArticulatedContactWitness] {
        &self.articulated_contact_witnesses
    }

    /// Number of source-bearing solver points omitted when the per-step
    /// witness cap was reached.
    pub fn contact_witness_overflow(&self) -> u64 {
        self.contact_witness_overflow
    }

    /// Every contact pair with a non-zero solved normal impulse this step, as
    /// [`ContactImpulse`] records in a deterministic order (by body-id pair).
    /// This is the T21 input surface: the caller thresholds these, applies
    /// per-region cooldowns, and converts survivors into bounded edit intents
    /// for a *later* tick — nothing here mutates world state.
    ///
    /// Call after [`Self::step`]. Pairs with no active solver contact, pairs
    /// touching a retired or world-detached body, and pairs whose accumulated
    /// normal impulse is not positive are omitted.
    pub fn contact_impulses(&self) -> Vec<ContactImpulse> {
        let mut out: Vec<ContactImpulse> = Vec::new();
        for pair in self.narrow_phase.contact_pairs() {
            if !pair.has_any_active_contact() {
                continue;
            }
            let impulse = pair.total_impulse_magnitude();
            if impulse <= 0.0 || !impulse.is_finite() {
                continue;
            }

            // Body handles + world contact normal live on the manifold data.
            let Some(manifold) = pair.manifolds.first() else {
                continue;
            };
            let (Some(rb1), Some(rb2)) = (manifold.data.rigid_body1, manifold.data.rigid_body2)
            else {
                continue;
            };
            let (Some(b1), Some(b2)) = (self.body_id_of(rb1), self.body_id_of(rb2)) else {
                continue;
            };
            let n = manifold.data.normal;
            let nlen = (n.x * n.x + n.y * n.y + n.z * n.z).sqrt();
            let normal = if nlen > f32::EPSILON {
                [n.x / nlen, n.y / nlen, n.z / nlen]
            } else {
                [0.0, 1.0, 0.0]
            };

            // Mean of every manifold contact point. In the pipeline's manifolds
            // `local_p1` / `local_p2` are the touch points on each body's surface
            // expressed relative to that body's centre of mass, in its local
            // frame; lift both to world space (world COM + body rotation · point)
            // and average — a stable brush centre for the hit.
            // A pair can outlive a body removed since the last step (a retired debris body):
            // skip it instead of indexing a stale handle.
            let (Some(rb1), Some(rb2)) = (self.bodies.get(rb1), self.bodies.get(rb2)) else {
                continue;
            };
            let (com1, rot1) = (rb1.center_of_mass(), *rb1.rotation());
            let (com2, rot2) = (rb2.center_of_mass(), *rb2.rotation());
            let mut point_sum = [0.0_f64; 3];
            let mut point_n = 0.0_f64;
            for m in &pair.manifolds {
                for p in &m.points {
                    let w1 = com1 + rot1 * p.local_p1;
                    let w2 = com2 + rot2 * p.local_p2;
                    point_sum[0] += 0.5 * f64::from(w1.x + w2.x);
                    point_sum[1] += 0.5 * f64::from(w1.y + w2.y);
                    point_sum[2] += 0.5 * f64::from(w1.z + w2.z);
                    point_n += 1.0;
                }
            }
            if point_n == 0.0 {
                continue;
            }
            let inv = 1.0 / point_n;
            out.push(ContactImpulse {
                bodies: [b1, b2],
                source_parts: [
                    self.source_part_of(b1, pair.collider1),
                    self.source_part_of(b2, pair.collider2),
                ],
                dynamic: [
                    self.bodies[self.entries[entry_index(b1, self.id_namespace)].body].is_dynamic(),
                    self.bodies[self.entries[entry_index(b2, self.id_namespace)].body].is_dynamic(),
                ],
                point_m: [
                    (point_sum[0] * inv) as f32,
                    (point_sum[1] * inv) as f32,
                    (point_sum[2] * inv) as f32,
                ],
                normal,
                normal_impulse_n_s: impulse,
                source_cells: [None, None],
            });
        }
        out.sort_by_key(|c| {
            (
                c.bodies[0].0.min(c.bodies[1].0),
                c.bodies[0].0.max(c.bodies[1].0),
            )
        });
        out
    }

    /// Sets a body's world pose. `rotation` is a quaternion `[x, y, z, w]`
    /// (renormalised by Rapier). Used by the authoritative sim (T08) to spawn a
    /// split child at its parent's transform so world geometry is unchanged at
    /// the split instant.
    pub fn set_body_pose(&mut self, id: BodyId, translation_m: [f32; 3], rotation: [f32; 4]) {
        assert!(
            !self.articulated.contains_key(&id),
            "use set_articulated_root_pose for an articulated body"
        );
        let (body, primary) = {
            let entry = &self.entries[entry_index(id, self.id_namespace)];
            (entry.body, entry.collider)
        };
        let rb = &mut self.bodies[body];
        rb.set_translation(
            Vector::new(translation_m[0], translation_m[1], translation_m[2]),
            true,
        );
        rb.set_rotation(
            Rotation::from_xyzw(rotation[0], rotation[1], rotation[2], rotation[3]),
            true,
        );
        // A query-only mirror (for example, client-predicted collision against
        // replicated debris) may move without ever stepping this PhysicsWorld.
        // Mark its collider so `sync_queries` refits the broad-phase AABB.
        if let Some(assembly) = self.articulated.get(&id) {
            self.pending_modified
                .extend(assembly.parts.iter().map(|part| part.collider));
        } else {
            self.pending_modified.push(primary);
        }
    }

    /// Applies a linear impulse (N·s) to a body at its centre of mass and wakes
    /// it if it was asleep. Models a documented "nearby interaction" — a blast
    /// impulse or an impact from an adjacent edit — for the sleep/wake
    /// feasibility scenario. No-op for `Fixed` bodies.
    pub fn apply_impulse(&mut self, id: BodyId, impulse_n_s: [f32; 3]) {
        assert!(
            !self.articulated.contains_key(&id),
            "kinematic articulated roots do not accept impulses"
        );
        let rb = &mut self.bodies[self.entries[entry_index(id, self.id_namespace)].body];
        rb.apply_impulse(
            Vector::new(impulse_n_s[0], impulse_n_s[1], impulse_n_s[2]),
            true,
        );
    }

    /// Applies an angular impulse (N·m·s) about a body's centre of mass and wakes
    /// it. The resulting change in angular velocity is `I⁻¹ · impulse` for the
    /// body's installed inertia tensor — the direct motion check for
    /// mixed-material inertia. No-op for `Fixed` bodies.
    pub fn apply_torque_impulse(&mut self, id: BodyId, torque_impulse_n_m_s: [f32; 3]) {
        assert!(
            !self.articulated.contains_key(&id),
            "kinematic articulated roots do not accept impulses"
        );
        let rb = &mut self.bodies[self.entries[entry_index(id, self.id_namespace)].body];
        rb.apply_torque_impulse(
            Vector::new(
                torque_impulse_n_m_s[0],
                torque_impulse_n_m_s[1],
                torque_impulse_n_m_s[2],
            ),
            true,
        );
    }

    /// Applies a linear impulse (N·s) at a world-space point, producing both a
    /// linear response (`impulse / mass`) and an angular response about the
    /// body's centre of mass (`I⁻¹ · ((point − com) × impulse)`) — so it
    /// exercises the installed COM and inertia together. Wakes the body. No-op
    /// for `Fixed` bodies.
    pub fn apply_impulse_at_point(
        &mut self,
        id: BodyId,
        impulse_n_s: [f32; 3],
        point_world_m: [f32; 3],
    ) {
        assert!(
            !self.articulated.contains_key(&id),
            "kinematic articulated roots do not accept impulses"
        );
        let rb = &mut self.bodies[self.entries[entry_index(id, self.id_namespace)].body];
        rb.apply_impulse_at_point(
            Vector::new(impulse_n_s[0], impulse_n_s[1], impulse_n_s[2]),
            Vector::new(point_world_m[0], point_world_m[1], point_world_m[2]),
            true,
        );
    }

    /// Sets a body's linear and angular velocity, m/s and rad/s. Used to hand a
    /// split child its inherited velocity.
    pub fn set_body_velocity(&mut self, id: BodyId, linvel_m_s: [f32; 3], angvel_rad_s: [f32; 3]) {
        assert!(
            !self.articulated.contains_key(&id),
            "use set_articulated_root_pose to move a kinematic articulated root"
        );
        let rb = &mut self.bodies[self.entries[entry_index(id, self.id_namespace)].body];
        rb.set_linvel(
            Vector::new(linvel_m_s[0], linvel_m_s[1], linvel_m_s[2]),
            true,
        );
        rb.set_angvel(
            Vector::new(angvel_rad_s[0], angvel_rad_s[1], angvel_rad_s[2]),
            true,
        );
    }

    /// The body-local collider offset applied for this body's occupancy-grid
    /// origin (`grid.origin() * cell_m`). Add it to
    /// [`Self::derived_mass_properties`]'s grid-local centre of mass to get the
    /// body-local centre of mass (`ENG-55`).
    pub fn collider_offset_m(&self, id: BodyId) -> [f32; 3] {
        self.entries[entry_index(id, self.id_namespace)].collider_offset_m
    }

    /// A body's authoritative mass properties: `(mass_kg, local centre of mass in
    /// metres, principal inertia diagonal)`. When fine-grid
    /// [`BodySpec::mass_properties`] were installed these are exactly those
    /// values (independent of the collision shape); otherwise they are what
    /// Rapier derived from the collider at its uniform density. Either way the
    /// centre of mass is in the collider shape's grid-local frame (cell
    /// `(0, 0, 0)` corner at the origin); [`Self::collider_offset_m`] shifts it
    /// to the body frame.
    pub fn derived_mass_properties(&self, id: BodyId) -> (f32, [f32; 3], [f32; 3]) {
        let entry = &self.entries[entry_index(id, self.id_namespace)];
        if let Some(p) = entry.mass_properties {
            return (
                p.mass_kg,
                p.local_com_m,
                principal_inertia(p.inertia_com_kg_m2),
            );
        }
        let shape = self.colliders[entry.collider].shared_shape().clone();
        let mp = shape.mass_properties(entry.density);
        let com = mp.local_com;
        let inertia = mp.principal_inertia();
        (
            mp.mass(),
            [com.x, com.y, com.z],
            [inertia.x, inertia.y, inertia.z],
        )
    }

    /// Mass properties read straight off the live Rapier rigid body: `(mass_kg,
    /// local centre of mass in metres, principal inertia diagonal)`. This is the
    /// value the solver integrates — after the collider contribution and any
    /// installed override are folded together — so tests can prove the fine-grid
    /// properties reached the solver, not just this adapter's record of them.
    pub fn live_body_mass_properties(&self, id: BodyId) -> (f32, [f32; 3], [f32; 3]) {
        let rb = &self.bodies[self.entries[entry_index(id, self.id_namespace)].body];
        let mp = &rb.mass_properties().local_mprops;
        let com = mp.local_com;
        let pi = mp.principal_inertia();
        (mp.mass(), [com.x, com.y, com.z], [pi.x, pi.y, pi.z])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mass::analytic_mass_properties;
    use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
    use spall_voxel::{EditPlan, Volume, fixtures};

    const CELL_M: f64 = 0.25;
    const STONE_RHO: f64 = 1000.0;
    const DIRT_RHO: f64 = 3000.0;

    #[test]
    fn transition_cast_errors_reject_but_supported_clear_casts_remain_clear() {
        assert_eq!(
            require_supported_transition_cast(
                7,
                Ok::<Option<usize>, rapier3d::parry::query::Unsupported>(None),
            ),
            Ok(None)
        );
        assert_eq!(
            require_supported_transition_cast(
                7,
                Err::<Option<usize>, _>(rapier3d::parry::query::Unsupported,)
            ),
            Err(
                crate::articulated::ArticulatedError::UnsupportedTerrainTransitionCast {
                    part_id: 7,
                }
            )
        );
    }

    #[test]
    fn transition_candidate_budget_is_derived_from_the_exact_cast_cap() {
        assert_eq!(transition_candidate_limit(0), MAX_TRANSITION_CELL_CASTS);
        assert_eq!(transition_candidate_limit(1), 1_000_000);
        assert_eq!(transition_candidate_limit(217), 4_608);
        assert_eq!(transition_candidate_limit(285), 3_508);
        assert_eq!(transition_candidate_limit(1_000_001), 0);
    }

    fn mix_density(m: MaterialId) -> f64 {
        if m == fixtures::STONE {
            STONE_RHO
        } else {
            DIRT_RHO
        }
    }

    /// Two adjacent 4³ blocks on x: left stone, right 3× denser dirt — the
    /// mixed-material fixture from `mass.rs`. Analytic COM sits at x = 1.25 m,
    /// the geometric centre is x = 1.0 m.
    fn mixed_density_grid() -> OccupancyGrid {
        let vid = VolumeId::new(9).unwrap();
        let mut v = Volume::new(vid, CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            vid,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(3, 3, 3),
            fixtures::STONE,
        ))
        .unwrap();
        v.apply_edit(&EditPlan::filled_box(
            vid,
            GlobalCell::new(4, 0, 0),
            GlobalCell::new(7, 3, 3),
            fixtures::DIRT,
        ))
        .unwrap();
        OccupancyGrid::from_region(&v, GlobalCell::new(0, 0, 0), GlobalCell::new(7, 3, 3)).unwrap()
    }

    fn one_cell_grid(origin: GlobalCell) -> OccupancyGrid {
        OccupancyGrid::from_solid_mask(origin, [1, 1, 1], vec![true], vec![fixtures::STONE])
            .unwrap()
    }

    fn one_part(grid: OccupancyGrid, volume: u64, revision: u64) -> crate::articulated::SourcePart {
        crate::articulated::SourcePart {
            part_id: 7,
            parent_id: None,
            volume_id: VolumeId::new(volume).unwrap(),
            source_revision: revision,
            grid,
            cell_m: 0.25,
            local_transform: crate::articulated::PartTransform::IDENTITY,
        }
    }

    fn source_cell_refs(part: &crate::articulated::SourcePart) -> Vec<SourceCellRef> {
        let origin = part.grid.origin();
        let mut cells = Vec::new();
        part.grid.for_each_solid(|x, y, z, material| {
            cells.push(SourceCellRef {
                volume_id: part.volume_id,
                part_id: part.part_id,
                cell: [
                    (origin.x + i64::from(x)) as i32,
                    (origin.y + i64::from(y)) as i32,
                    (origin.z + i64::from(z)) as i32,
                ],
                material,
                source_revision: part.source_revision,
            });
        });
        cells.sort();
        cells
    }

    fn grid_from_x_cells(xs: &[usize], width: usize) -> OccupancyGrid {
        let mut mask = vec![false; width];
        for &x in xs {
            mask[x] = true;
        }
        OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [width as u32, 1, 1],
            mask,
            vec![fixtures::STONE; width],
        )
        .unwrap()
    }

    fn restore_component(
        id: Option<DerivedFractureComponentId>,
        state: BodyState,
        cells: Vec<SourceCellRef>,
    ) -> crate::articulated::DynamicCorpseComponentRestore {
        crate::articulated::DynamicCorpseComponentRestore {
            component_id: id,
            translation_m: state.translation_m,
            rotation_xyzw: state.rotation,
            linear_velocity_m_s: state.linvel_m_s,
            angular_velocity_rad_s: state.angvel_rad_s,
            cells,
        }
    }

    #[test]
    fn articulated_batch_publishes_alive_birth_and_checked_transfer_together() {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let alive = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                7,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 501, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let before = world.body_count();
        let prepared = world
            .prepare_articulated_batch(
                vec![
                    ArticulatedBatchOperation::AliveBirth {
                        operation_tag: 11,
                        translation_m: [2.0, 0.0, 0.0],
                        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                        topology_token: 9,
                        parts: vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 502, 1)],
                        representation: Representation::MergedCuboids,
                    },
                    ArticulatedBatchOperation::CheckedDeathTransfer {
                        operation_tag: 12,
                        id: alive,
                        expected_topology_token: 7,
                        expected_pose_token: 0,
                    },
                ],
                mix_density,
            )
            .unwrap();
        assert_eq!(
            world.body_count(),
            before,
            "prepare must not publish bodies"
        );
        let validated = world.validate_articulated_batch(prepared).unwrap();
        assert_eq!(
            world.body_count(),
            before,
            "validation must not publish bodies"
        );
        let outcomes = world.publish_validated_articulated_batch(validated);
        assert_eq!(
            outcomes.iter().map(|o| o.operation_tag).collect::<Vec<_>>(),
            vec![11, 12]
        );
        assert!(outcomes[1].transfer.is_some());
        let born = world.articulated.get(&outcomes[0].owner_id.0).unwrap();
        assert!(born.kinematic_part_bodies && !born.dynamic_corpse);
        let converted = world.articulated.get(&alive.0).unwrap();
        assert!(!converted.kinematic_part_bodies && converted.dynamic_corpse);
        assert!(
            converted
                .source_volume_ids
                .contains(&VolumeId::new(501).unwrap())
        );
    }

    #[test]
    fn articulated_batch_prepares_two_deaths_birth_and_cold_restore_atomically() {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let death_a = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                31,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 531, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let death_b = world
            .add_articulated_kinematic_parts(
                [1.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                32,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 532, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let restored_part = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 533, 4);
        let restored = crate::articulated::DynamicCorpseRestore {
            owner_key: 0x5eed,
            source_generation: 4,
            topology_revision: 3,
            pose_epoch: 8,
            next_component_serial: 1,
            source_parts: vec![restored_part.clone()],
            components: vec![restore_component(
                None,
                BodyState {
                    translation_m: [4.0, 0.0, 0.0],
                    rotation: [0.0, 0.0, 0.0, 1.0],
                    linvel_m_s: [0.0; 3],
                    angvel_rad_s: [0.0; 3],
                    mass_kg: 0.0,
                    sleeping: false,
                },
                source_cell_refs(&restored_part),
            )],
        };
        let before = world.body_count();
        let prepared = world
            .prepare_articulated_batch(
                vec![
                    ArticulatedBatchOperation::AliveBirth {
                        operation_tag: 41,
                        translation_m: [2.0, 0.0, 0.0],
                        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                        topology_token: 40,
                        parts: vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 534, 1)],
                        representation: Representation::MergedCuboids,
                    },
                    ArticulatedBatchOperation::CheckedDeathTransfer {
                        operation_tag: 42,
                        id: death_a,
                        expected_topology_token: 31,
                        expected_pose_token: 0,
                    },
                    ArticulatedBatchOperation::ColdRestore {
                        operation_tag: 43,
                        restore: restored,
                        representation: Representation::MergedCuboids,
                    },
                    ArticulatedBatchOperation::CheckedDeathTransfer {
                        operation_tag: 44,
                        id: death_b,
                        expected_topology_token: 32,
                        expected_pose_token: 0,
                    },
                ],
                mix_density,
            )
            .unwrap();
        assert_eq!(
            world.body_count(),
            before,
            "prepare must not publish members"
        );
        let outcomes = world.commit_articulated_batch(prepared).unwrap();
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.operation_tag)
                .collect::<Vec<_>>(),
            vec![41, 42, 43, 44]
        );
        assert!(outcomes[1].transfer.is_some());
        assert!(outcomes[3].transfer.is_some());
        assert!(world.articulated.get(&death_a.0).unwrap().dynamic_corpse);
        assert!(world.articulated.get(&death_b.0).unwrap().dynamic_corpse);
        assert!(
            world
                .articulated
                .get(&outcomes[0].owner_id.0)
                .unwrap()
                .kinematic_part_bodies
        );
        assert_eq!(
            world
                .articulated_owner_snapshot(outcomes[2].owner_id)
                .unwrap()
                .pose_token,
            8
        );
        assert_eq!(world.body_count(), before + 2);
    }

    #[test]
    fn articulated_batch_rejects_same_step_transfer_drift_before_birth_publication() {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let alive = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                17,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 511, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let before = world.body_count();
        let prepared = world
            .prepare_articulated_batch(
                vec![
                    ArticulatedBatchOperation::AliveBirth {
                        operation_tag: 21,
                        translation_m: [2.0, 0.0, 0.0],
                        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                        topology_token: 19,
                        parts: vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 512, 1)],
                        representation: Representation::MergedCuboids,
                    },
                    ArticulatedBatchOperation::CheckedDeathTransfer {
                        operation_tag: 22,
                        id: alive,
                        expected_topology_token: 17,
                        expected_pose_token: 0,
                    },
                ],
                mix_density,
            )
            .unwrap();
        let live_body = world.entries[entry_index(alive.0, world.id_namespace)].body;
        world.bodies[live_body]
            .set_position(Pose::from_translation(Vector::new(0.001, 0.0, 0.0)), true);
        assert!(matches!(
            world.commit_articulated_batch(prepared),
            Err(crate::articulated::ArticulatedError::StalePreparedBatch)
        ));
        assert_eq!(
            world.body_count(),
            before,
            "stale last member must leave the first birth unpublished"
        );
        assert!(
            world
                .articulated
                .get(&alive.0)
                .unwrap()
                .kinematic_part_bodies
        );
        assert!(!world.articulated.values().any(|owner| {
            owner
                .source_volume_ids
                .contains(&VolumeId::new(512).unwrap())
        }));
    }

    #[test]
    fn body_commit_fence_rejects_same_step_twist_changes() {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let id = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                27,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 521, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let fence = world.snapshot_body_commit_fence(&[id.0]).unwrap();
        assert!(world.validate_body_commit_fence(&fence));
        let live_body = world.entries[entry_index(id.0, world.id_namespace)].body;
        world.bodies[live_body]
            .set_position(Pose::from_translation(Vector::new(0.001, 0.0, 0.0)), true);
        assert!(!world.validate_body_commit_fence(&fence));
    }

    #[test]
    fn body_commit_fence_accepts_and_tracks_absent_terrain_collider() {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let id = world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
            cell_m: 0.25,
            density_kg_m3: 1.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        world.remove_collider(id);
        let fence = world.snapshot_body_commit_fence(&[id]).unwrap();
        assert!(world.validate_body_commit_fence(&fence));
        world.rebuild_collider(
            id,
            &one_cell_grid(GlobalCell::new(1, 0, 0)),
            Representation::MergedCuboids,
        );
        assert!(!world.validate_body_commit_fence(&fence));
    }

    #[test]
    fn cold_restore_preserves_split_motion_mass_query_and_serial_highwater() {
        let cfg = PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            solver_substeps: 4,
            ..Default::default()
        };
        let mut live = PhysicsWorld::new(cfg);
        let part = one_part(grid_from_x_cells(&[0, 1, 2, 4], 5), 90, 7);
        let owner = live
            .add_dynamic_corpse_aggregate(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![part.clone()],
                Representation::MergedCuboids,
                mix_density,
            )
            .unwrap();
        let snapshot = live
            .capture_dynamic_corpse_fracture_snapshot(owner, 0xabc, None)
            .unwrap();
        let plan = prepare_fracture(&snapshot).unwrap();
        assert_eq!(plan.components.len(), 2);
        let committed = live
            .commit_dynamic_corpse_fracture(owner, &snapshot, &plan, mix_density)
            .unwrap();
        let ids: Vec<_> = committed.components.iter().map(|entry| entry.0).collect();
        assert_eq!(
            ids,
            vec![
                DerivedFractureComponentId {
                    owner_key: 0xabc,
                    serial: 1
                },
                DerivedFractureComponentId {
                    owner_key: 0xabc,
                    serial: 2
                },
            ]
        );
        live.set_dynamic_corpse_component_velocity(owner, Some(ids[0]), [1.0, 0.0, 0.0], [0.0; 3])
            .unwrap();
        live.set_dynamic_corpse_component_velocity(owner, Some(ids[1]), [-1.0, 0.0, 0.0], [0.0; 3])
            .unwrap();
        live.step();

        let components = ids
            .iter()
            .zip(&plan.components)
            .map(|(component_id, planned)| {
                restore_component(
                    Some(*component_id),
                    live.fracture_component_state(owner, *component_id).unwrap(),
                    planned.cells.clone(),
                )
            })
            .collect();
        let mut cold = PhysicsWorld::new(cfg);
        let restored = cold
            .restore_dynamic_corpse(
                crate::articulated::DynamicCorpseRestore {
                    owner_key: 0xabc,
                    source_generation: 2,
                    topology_revision: 2,
                    pose_epoch: 77,
                    next_component_serial: 3,
                    source_parts: vec![part],
                    components,
                },
                Representation::MergedCuboids,
                mix_density,
            )
            .unwrap();
        assert_eq!(cold.articulated_collider_count(restored), 2);
        assert_eq!(cold.articulated_rigid_body_count(restored), 2);
        assert_eq!(
            cold.articulated_owner_snapshot(restored)
                .unwrap()
                .pose_token,
            77
        );
        for &id in &ids {
            let expected = live.fracture_component_state(owner, id).unwrap();
            let actual = cold.fracture_component_state(restored, id).unwrap();
            assert!((expected.translation_m[0] - actual.translation_m[0]).abs() < 1.0e-5);
            assert!((expected.linvel_m_s[0] - actual.linvel_m_s[0]).abs() < 1.0e-5);
            assert!((expected.mass_kg - actual.mass_kg).abs() < 1.0e-3);
        }
        // The adapter restored the portable pose epoch without modifying the
        // world-global freshness counter used by fracture snapshots.
        assert_eq!(
            cold.capture_dynamic_corpse_fracture_snapshot(restored, 0xabc, Some(ids[0]))
                .unwrap()
                .token
                .pose_epoch,
            0
        );
        for x in [0.14_f32, 1.11_f32] {
            let hit = cold
                .raycast_articulated(restored, [x, 1.0, 0.125], [0.0, -1.0, 0.0], 2.0)
                .unwrap();
            assert_eq!(hit.volume_id, VolumeId::new(90).unwrap());
        }
        let fixed = cold.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
            cell_m: 0.25,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [0.75, 0.0, 0.0],
            linvel_m_s: [0.0; 3],
        });
        cold.step();
        assert!(cold.articulated_contact_witnesses().iter().any(|witness| {
            witness.bodies.contains(&restored.0)
                && witness.bodies.contains(&fixed)
                && witness.source_sides.iter().flatten().any(|side| {
                    side.volume_id == VolumeId::new(90).unwrap()
                        && side.normal_impulse_n_s.is_some()
                })
        }));

        // Removing the bridge from the restored first component creates two
        // new components; serials continue above the persisted high-water.
        let edited = grid_from_x_cells(&[0, 2], 3);
        cold.rebuild_dynamic_corpse_component_part_grid(
            restored,
            ids[0],
            2,
            3,
            7,
            7,
            8,
            edited,
            mix_density,
        )
        .unwrap();
        let edited_snapshot = cold
            .capture_dynamic_corpse_fracture_snapshot(restored, 0xabc, Some(ids[0]))
            .unwrap();
        let edited_plan = prepare_fracture(&edited_snapshot).unwrap();
        assert_eq!(edited_plan.components.len(), 2);
        let resplit = cold
            .commit_dynamic_corpse_fracture(restored, &edited_snapshot, &edited_plan, mix_density)
            .unwrap();
        assert_eq!(
            resplit
                .components
                .iter()
                .map(|entry| entry.0.serial)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert!(cold.fracture_component_state(restored, ids[0]).is_err());
    }

    #[test]
    fn cold_restore_accepts_mixed_source_cell_sizes_and_empty_bindings() {
        let mut root = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 101, 1);
        let mut child = crate::articulated::SourcePart {
            part_id: 8,
            parent_id: Some(root.part_id),
            volume_id: VolumeId::new(102).unwrap(),
            source_revision: 3,
            grid: one_cell_grid(GlobalCell::new(4, 0, 0)),
            cell_m: 0.5,
            local_transform: crate::articulated::PartTransform {
                translation_m: [1.0, 0.0, 0.0],
                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            },
        };
        let mut empty = one_part(
            OccupancyGrid::from_solid_mask(
                GlobalCell::new(10, 0, 0),
                [1, 1, 1],
                vec![false],
                vec![MaterialId::AIR],
            )
            .unwrap(),
            103,
            1,
        );
        empty.part_id = 9;
        empty.parent_id = Some(root.part_id);
        root.local_transform.translation_m = [0.0, 0.0, 0.0];
        child.local_transform.translation_m = [1.0, 0.0, 0.0];
        let mut cells = source_cell_refs(&root);
        cells.extend(source_cell_refs(&child));
        cells.sort();
        let component = crate::articulated::DynamicCorpseComponentRestore {
            component_id: None,
            translation_m: [2.0, 3.0, 4.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [0.5, 0.0, 0.0],
            angular_velocity_rad_s: [0.0; 3],
            cells,
        };
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let mut incomplete = component.clone();
        incomplete.cells.pop();
        assert_eq!(
            world.restore_dynamic_corpse(
                crate::articulated::DynamicCorpseRestore {
                    owner_key: 0xdef,
                    source_generation: 5,
                    topology_revision: 4,
                    pose_epoch: 8,
                    next_component_serial: 1,
                    source_parts: vec![root.clone(), child.clone(), empty.clone()],
                    components: vec![incomplete],
                },
                Representation::MergedCuboids,
                mix_density,
            ),
            Err(crate::articulated::ArticulatedError::InvalidRestore)
        );
        assert!(world.entries.is_empty());
        let id = world
            .restore_dynamic_corpse(
                crate::articulated::DynamicCorpseRestore {
                    owner_key: 0xdef,
                    source_generation: 5,
                    topology_revision: 4,
                    pose_epoch: 8,
                    next_component_serial: 1,
                    source_parts: vec![root, child, empty],
                    components: vec![component],
                },
                Representation::MergedCuboids,
                mix_density,
            )
            .unwrap();
        assert_eq!(world.articulated_collider_count(id), 2);
        assert_eq!(world.articulated_rigid_body_count(id), 1);
        assert_eq!(world.articulated_owner_snapshot(id).unwrap().pose_token, 8);
        assert!((world.articulated_body_state(id).mass_kg - 15.625 - 125.0).abs() < 1.0e-2);
        let reused_empty_binding = one_part(one_cell_grid(GlobalCell::new(20, 0, 0)), 103, 2);
        assert_eq!(
            world.restore_dynamic_corpse(
                crate::articulated::DynamicCorpseRestore {
                    owner_key: 0x123,
                    source_generation: 1,
                    topology_revision: 1,
                    pose_epoch: 1,
                    next_component_serial: 1,
                    components: vec![crate::articulated::DynamicCorpseComponentRestore {
                        component_id: None,
                        translation_m: [0.0; 3],
                        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                        linear_velocity_m_s: [0.0; 3],
                        angular_velocity_rad_s: [0.0; 3],
                        cells: source_cell_refs(&reused_empty_binding),
                    }],
                    source_parts: vec![reused_empty_binding.clone()],
                },
                Representation::MergedCuboids,
                mix_density,
            ),
            Err(crate::articulated::ArticulatedError::DuplicateVolume)
        );
        assert_eq!(
            world.add_articulated_body(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![reused_empty_binding.clone()],
                Representation::MergedCuboids,
            ),
            Err(crate::articulated::ArticulatedError::DuplicateVolume)
        );
        world.retire_articulated_body(id);
        assert_eq!(world.fracture_owner_keys.get(&0xdef), Some(&id.0));
        assert!(
            world
                .add_articulated_body(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![reused_empty_binding],
                    Representation::MergedCuboids,
                )
                .is_ok()
        );
    }

    #[test]
    fn validated_articulated_batch_rejects_foreign_world_before_mutation() {
        let source = PhysicsWorld::new(PhysicsConfig::default());
        let prepared = source
            .prepare_articulated_batch(
                vec![ArticulatedBatchOperation::AliveBirth {
                    operation_tag: 25,
                    translation_m: [0.0; 3],
                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    topology_token: 25,
                    parts: vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 515, 1)],
                    representation: Representation::MergedCuboids,
                }],
                mix_density,
            )
            .unwrap();
        let validated = source.validate_articulated_batch(prepared).unwrap();
        let mut other = PhysicsWorld::new(PhysicsConfig::default());
        let before_source = source.body_count();
        let before_other = other.body_count();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            other.publish_validated_articulated_batch(validated)
        }));
        assert!(result.is_err());
        assert_eq!(source.body_count(), before_source);
        assert_eq!(other.body_count(), before_other);
    }

    #[test]
    fn restore_rejects_bad_identity_partition_and_density_before_publication() {
        let part = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 111, 4);
        let cells = source_cell_refs(&part);
        let make_intact = || crate::articulated::DynamicCorpseRestore {
            owner_key: 0x456,
            source_generation: 3,
            topology_revision: 2,
            pose_epoch: 1,
            next_component_serial: 1,
            source_parts: vec![part.clone()],
            components: vec![crate::articulated::DynamicCorpseComponentRestore {
                component_id: None,
                translation_m: [0.0; 3],
                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                linear_velocity_m_s: [0.0; 3],
                angular_velocity_rad_s: [0.0; 3],
                cells: cells.clone(),
            }],
        };
        let mut invalid = Vec::new();
        let mut bad = make_intact();
        bad.owner_key = 0;
        invalid.push(bad);
        let mut bad = make_intact();
        bad.source_generation = 0;
        invalid.push(bad);
        let mut bad = make_intact();
        bad.topology_revision = 0;
        invalid.push(bad);
        let mut bad = make_intact();
        bad.pose_epoch = 0;
        invalid.push(bad);
        let mut bad = make_intact();
        bad.next_component_serial = 2;
        invalid.push(bad);
        let mut bad = make_intact();
        bad.source_parts[0].source_revision = 0;
        invalid.push(bad);
        let mut bad = make_intact();
        bad.components[0].component_id = Some(DerivedFractureComponentId {
            owner_key: 0x999,
            serial: 1,
        });
        bad.next_component_serial = 2;
        invalid.push(bad);
        let mut bad = make_intact();
        bad.components[0].component_id = Some(DerivedFractureComponentId {
            owner_key: 0x456,
            serial: 1,
        });
        bad.next_component_serial = 1;
        invalid.push(bad);

        let derived_component = |serial| crate::articulated::DynamicCorpseComponentRestore {
            component_id: Some(DerivedFractureComponentId {
                owner_key: 0x456,
                serial,
            }),
            translation_m: [0.0; 3],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            cells: cells.clone(),
        };
        let mut overlapping = make_intact();
        overlapping.next_component_serial = 3;
        overlapping.components = vec![derived_component(1), derived_component(2)];
        invalid.push(overlapping);

        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let live_owner = world
            .add_articulated_body(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                19,
                vec![one_part(one_cell_grid(GlobalCell::new(30, 0, 0)), 110, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        for bundle in invalid {
            assert!(
                world
                    .restore_dynamic_corpse(bundle, Representation::MergedCuboids, mix_density)
                    .is_err()
            );
            assert_eq!(world.entries.len(), 1);
            assert!(world.articulated_owner_snapshot(live_owner).is_some());
            assert!(world.fracture_owner_keys.is_empty());
        }

        assert_eq!(
            world.restore_dynamic_corpse(make_intact(), Representation::MergedCuboids, |_| 0.0,),
            Err(crate::articulated::ArticulatedError::InvalidDensity)
        );
        assert_eq!(world.entries.len(), 1);
        assert!(world.articulated_owner_snapshot(live_owner).is_some());
        assert!(world.fracture_owner_keys.is_empty());
    }

    #[test]
    fn restore_keeps_single_derived_component_highwater_for_next_split() {
        let part = one_part(grid_from_x_cells(&[0, 1, 2], 3), 112, 7);
        let component_id = DerivedFractureComponentId {
            owner_key: 0x789,
            serial: 4,
        };
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let owner = world
            .restore_dynamic_corpse(
                crate::articulated::DynamicCorpseRestore {
                    owner_key: component_id.owner_key,
                    source_generation: 9,
                    topology_revision: 8,
                    pose_epoch: 5,
                    next_component_serial: 9,
                    source_parts: vec![part.clone()],
                    components: vec![crate::articulated::DynamicCorpseComponentRestore {
                        component_id: Some(component_id),
                        translation_m: [1.0, 2.0, 3.0],
                        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                        linear_velocity_m_s: [0.5, 0.0, 0.0],
                        angular_velocity_rad_s: [0.0; 3],
                        cells: source_cell_refs(&part),
                    }],
                },
                Representation::MergedCuboids,
                mix_density,
            )
            .unwrap();
        assert_eq!(world.articulated_rigid_body_count(owner), 1);
        assert_eq!(
            world
                .fracture_component_state(owner, component_id)
                .unwrap()
                .translation_m,
            [1.0, 2.0, 3.0]
        );
        world
            .rebuild_dynamic_corpse_component_part_grid(
                owner,
                component_id,
                8,
                9,
                7,
                7,
                8,
                grid_from_x_cells(&[0, 2], 3),
                mix_density,
            )
            .unwrap();
        let snapshot = world
            .capture_dynamic_corpse_fracture_snapshot(owner, 0x789, Some(component_id))
            .unwrap();
        let plan = prepare_fracture(&snapshot).unwrap();
        let split = world
            .commit_dynamic_corpse_fracture(owner, &snapshot, &plan, mix_density)
            .unwrap();
        assert_eq!(
            split
                .components
                .iter()
                .map(|entry| entry.0.serial)
                .collect::<Vec<_>>(),
            vec![9, 10]
        );
    }

    #[test]
    fn source_cell_witness_requires_a_unique_exterior_face_in_source_coordinates() {
        let one = one_cell_grid(GlobalCell::new(-3, 2, 1));
        for (point, normal) in [
            (Vector::new(1.0, 0.4, 0.6), Vector::X),
            (Vector::new(0.0, 0.4, 0.6), -Vector::X),
        ] {
            assert_eq!(
                resolve_source_cell_witness(&one, 1.0, point, normal, 0.0),
                ContactCellAttribution::Resolved {
                    cell: [-3, 2, 1],
                    material: fixtures::STONE,
                }
            );
        }

        let shared = OccupancyGrid::from_solid_mask(
            GlobalCell::new(-2, 0, 0),
            [2, 1, 1],
            vec![true, true],
            vec![fixtures::STONE; 2],
        )
        .unwrap();
        assert_eq!(
            resolve_source_cell_witness(&shared, 1.0, Vector::new(1.0, 0.5, 0.5), Vector::X, 0.0),
            ContactCellAttribution::Unattributed(ContactWitnessUnattributed::InternalSharedFace)
        );
        let holed = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [2, 1, 1],
            vec![true, false],
            vec![fixtures::STONE, fixtures::STONE],
        )
        .unwrap();
        assert!(matches!(
            resolve_source_cell_witness(&holed, 1.0, Vector::new(1.0, 0.5, 0.5), Vector::X, 0.0),
            ContactCellAttribution::Resolved {
                cell: [0, 0, 0],
                ..
            }
        ));
        for (point, normal, separation, reason) in [
            (
                Vector::new(1.0, 0.0, 0.5),
                Vector::X,
                0.0,
                ContactWitnessUnattributed::EdgeOrCorner,
            ),
            (
                Vector::new(1.0, 0.00005, 0.5),
                Vector::X,
                0.0,
                ContactWitnessUnattributed::EdgeOrCorner,
            ),
            (
                Vector::new(1.0, 0.5, 0.5),
                Vector::new(1.0, 1.0, 0.0),
                0.0,
                ContactWitnessUnattributed::NotAxisAlignedFace,
            ),
            (
                Vector::new(1.0, 0.5, 0.5),
                Vector::X,
                -0.01,
                ContactWitnessUnattributed::Penetrating,
            ),
        ] {
            assert_eq!(
                resolve_source_cell_witness(&one, 1.0, point, normal, separation),
                ContactCellAttribution::Unattributed(reason)
            );
        }
    }

    #[test]
    fn articulated_topology_rebuild_installs_new_source_occupancy_atomically() {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0, -9.81, 0.0],
            ..PhysicsConfig::default()
        });
        let body = world
            .add_articulated_body(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                3,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 70, 1)],
                Representation::NativeVoxels,
            )
            .unwrap();
        let hit = world
            .raycast_articulated(body, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 2.0)
            .unwrap();
        assert_eq!(hit.source_revision, 1);
        let old_radius = world.articulated[&body.0].parts[0].root_surface_radius_m;
        assert!((old_radius - (3.0_f32.sqrt() * 0.25)).abs() < 1.0e-6);
        assert!(
            world
                .rebuild_articulated_part_grid(
                    body,
                    3,
                    4,
                    7,
                    1,
                    2,
                    one_cell_grid(GlobalCell::new(4, 0, 0)),
                )
                .is_ok()
        );
        assert!(matches!(
            world.raycast_articulated(body, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 2.0),
            Err(crate::articulated::ArticulatedError::Miss)
        ));
        let moved = world
            .raycast_articulated(body, [0.5, 0.125, 0.125], [1.0, 0.0, 0.0], 2.0)
            .unwrap();
        assert_eq!(moved.source_revision, 2);
        assert_eq!(moved.topology_token, 4);
        let rebuilt_radius = world.articulated[&body.0].parts[0].root_surface_radius_m;
        assert!(
            (rebuilt_radius - (1.25_f32.powi(2) + 2.0 * 0.25_f32.powi(2)).sqrt()).abs() < 1.0e-6
        );

        let stale = world.rebuild_articulated_part_grid(
            body,
            3,
            5,
            7,
            2,
            3,
            one_cell_grid(GlobalCell::new(8, 0, 0)),
        );
        assert_eq!(
            stale.unwrap_err(),
            crate::articulated::ArticulatedError::StaleTopology
        );
        let still_current = world
            .raycast_articulated(body, [0.5, 0.125, 0.125], [1.0, 0.0, 0.0], 2.0)
            .unwrap();
        assert_eq!(still_current.source_revision, 2);
        assert_eq!(still_current.topology_token, 4);
    }

    #[test]
    fn articulated_source_rebuild_rejects_pose_staged_for_prior_topology() {
        use crate::articulated::{ArticulatedError, ArticulatedPose, PartTransform};

        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                solver_substeps: 4,
                ..PhysicsConfig::default()
            });
            let body = world
                .add_articulated_kinematic_parts(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    10,
                    vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 71, 3)],
                    representation,
                )
                .unwrap();
            let initial_hit = world
                .raycast_articulated(body, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 3.0)
                .unwrap();
            assert_eq!(
                (initial_hit.topology_token, initial_hit.pose_token),
                (10, 0)
            );
            let initial_colliders = world.articulated_collider_count(body);
            let initial_bodies = world.articulated_rigid_body_count(body);

            world
                .update_articulated_pose(
                    body,
                    ArticulatedPose {
                        topology_token: 10,
                        base_pose_token: 0,
                        pose_token: 1,
                        parts: vec![(
                            7,
                            PartTransform {
                                translation_m: [0.01, 0.0, 0.0],
                                ..PartTransform::IDENTITY
                            },
                        )],
                    },
                )
                .unwrap();
            assert_eq!(world.articulated[&body.0].topology_token, 10);
            assert_eq!(world.articulated[&body.0].pose_token, 0);
            assert_eq!(
                world.articulated[&body.0]
                    .pending_pose
                    .as_ref()
                    .map(|pending| pending.pose_token),
                Some(1)
            );

            let rejected = world.rebuild_articulated_part_grid(
                body,
                10,
                11,
                7,
                3,
                4,
                one_cell_grid(GlobalCell::new(4, 0, 0)),
            );
            assert_eq!(rejected.unwrap_err(), ArticulatedError::StaleTopology);
            assert_eq!(world.articulated_collider_count(body), initial_colliders);
            assert_eq!(world.articulated_rigid_body_count(body), initial_bodies);
            let assembly = &world.articulated[&body.0];
            assert_eq!(assembly.topology_token, 10);
            assert_eq!(assembly.pose_token, 0);
            assert_eq!(assembly.parts[0].source_revision, 3);
            assert_eq!(assembly.parts[0].grid.origin(), GlobalCell::new(0, 0, 0));
            assert_eq!(
                assembly
                    .pending_pose
                    .as_ref()
                    .map(|pending| pending.pose_token),
                Some(1)
            );
            let still_old = world
                .raycast_articulated(body, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 3.0)
                .unwrap();
            assert_eq!((still_old.topology_token, still_old.pose_token), (10, 0));

            world.step();
            let applied = world
                .raycast_articulated(body, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 3.0)
                .unwrap();
            assert_eq!((applied.topology_token, applied.pose_token), (10, 1));
            assert!((applied.toi_m - 1.01).abs() < 1.0e-4);

            world
                .rebuild_articulated_part_grid(
                    body,
                    10,
                    11,
                    7,
                    3,
                    4,
                    one_cell_grid(GlobalCell::new(4, 0, 0)),
                )
                .unwrap();
            let rebuilt = world
                .raycast_articulated(body, [0.0, 0.125, 0.125], [1.0, 0.0, 0.0], 3.0)
                .unwrap();
            assert_eq!(rebuilt.source_revision, 4);
            assert_eq!((rebuilt.topology_token, rebuilt.pose_token), (11, 1));
            assert_eq!(rebuilt.cell, [4, 0, 0]);
        }
    }

    #[test]
    fn occupied_source_corner_radius_handles_negative_off_center_nested_geometry() {
        let grid = OccupancyGrid::from_solid_mask(
            GlobalCell::new(-2, 1, -1),
            [2, 1, 1],
            vec![false, true],
            vec![fixtures::STONE; 2],
        )
        .unwrap();
        let local_radius = source_surface_radius_m(&grid, 0.25, [0.0; 3], None);
        assert!((local_radius - 0.375_f32.sqrt()).abs() < 1.0e-6);
        let parent = PartTransform {
            translation_m: [1.0, 0.0, 0.0],
            rotation_xyzw: [
                0.0,
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
            ],
        };
        let child = PartTransform {
            translation_m: [0.0, 1.0, 0.0],
            rotation_xyzw: PartTransform::IDENTITY.rotation_xyzw,
        };
        let flattened = compose(parent, child);
        let root_radius = source_surface_radius_m(&grid, 0.25, [0.0; 3], Some(flattened));
        assert!(
            (root_radius - 0.375_f32.sqrt()).abs() < 1.0e-6,
            "radius={root_radius}"
        );
    }

    #[test]
    fn articulated_rejects_unrepresentable_source_and_shape_bounds_without_mutation() {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let body = world
            .add_articulated_body(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 72, 1)],
                Representation::NativeVoxels,
            )
            .unwrap();
        let out_of_source_range = one_cell_grid(GlobalCell::new(i32::MAX as i64 + 1, 0, 0));
        assert_eq!(
            world
                .rebuild_articulated_part_grid(body, 1, 2, 7, 1, 2, out_of_source_range)
                .unwrap_err(),
            crate::articulated::ArticulatedError::GeometryRange
        );
        let preserved = world
            .raycast_articulated(body, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 2.0)
            .unwrap();
        assert_eq!(preserved.source_revision, 1);
        assert_eq!(preserved.topology_token, 1);

        let mut impossible_scale = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 73, 1);
        impossible_scale.cell_m = f32::MAX;
        assert_eq!(
            world
                .add_articulated_body(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![impossible_scale],
                    Representation::NativeVoxels,
                )
                .unwrap_err(),
            crate::articulated::ArticulatedError::GeometryRange
        );
    }

    #[test]
    fn articulated_rebuild_enforces_aggregate_dense_budget_before_mutation() {
        fn sparse_grid(dims: [u32; 3]) -> OccupancyGrid {
            let count = (u64::from(dims[0]) * u64::from(dims[1]) * u64::from(dims[2])) as usize;
            let mut solid = vec![false; count];
            let mut material = vec![MaterialId::AIR; count];
            solid[0] = true;
            material[0] = fixtures::STONE;
            OccupancyGrid::from_solid_mask(GlobalCell::new(0, 0, 0), dims, solid, material).unwrap()
        }
        let mut root_part = one_part(sparse_grid([128, 128, 116]), 77, 1);
        root_part.part_id = 1;
        let mut child = one_part(sparse_grid([128, 128, 116]), 78, 1);
        child.part_id = 2;
        child.parent_id = Some(1);
        child.local_transform.translation_m[0] = 0.5;
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let body = world
            .add_articulated_body(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![root_part, child],
                Representation::MergedCuboids,
            )
            .unwrap();
        let hit_before = world
            .raycast_articulated(body, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 2.0)
            .unwrap();
        let result =
            world.rebuild_articulated_part_grid(body, 1, 2, 1, 1, 2, sparse_grid([128, 128, 135]));
        assert_eq!(
            result.unwrap_err(),
            crate::articulated::ArticulatedError::WorkBudget
        );
        let hit_after = world
            .raycast_articulated(body, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 2.0)
            .unwrap();
        assert_eq!(hit_after.source_revision, hit_before.source_revision);
        assert_eq!(hit_after.topology_token, hit_before.topology_token);
        assert_eq!(world.articulated_collider_count(body), 2);
    }

    #[test]
    fn articulated_source_volume_has_one_live_assembly_owner() {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let first = world
            .add_articulated_body(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 79, 1)],
                Representation::NativeVoxels,
            )
            .unwrap();
        let duplicate = world.add_articulated_body(
            [2.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 79, 1)],
            Representation::NativeVoxels,
        );
        assert_eq!(
            duplicate.unwrap_err(),
            crate::articulated::ArticulatedError::DuplicateVolume
        );
        assert_eq!(world.articulated_collider_count(first), 1);
        world.retire_articulated_body(first);
        assert!(
            world
                .add_articulated_body(
                    [2.0, 0.0, 0.0],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 79, 2)],
                    Representation::NativeVoxels,
                )
                .is_ok()
        );
    }

    #[test]
    fn direct_character_sweep_exclusion_covers_every_source_part_collider() {
        let dims = [1, 4, 1];
        let child_grid = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            dims,
            vec![true; 4],
            vec![fixtures::STONE; 4],
        )
        .unwrap();
        let mut root_part = one_part(child_grid.clone(), 82, 1);
        root_part.part_id = 1;
        root_part.local_transform.translation_m[0] = -5.0;
        let mut child_part = one_part(child_grid, 83, 1);
        child_part.part_id = 2;
        child_part.parent_id = Some(1);
        child_part.local_transform.translation_m[0] = 7.0;
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let assembly = world
            .add_articulated_body(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![root_part, child_part],
                Representation::MergedCuboids,
            )
            .unwrap();
        world.sync_queries();
        let movement = world.sweep_character_excluding(
            crate::character::CharacterParams::DEFAULT,
            [0.0, 0.0, 0.125],
            [4.0, 0.0, 0.0],
            1.0 / 60.0,
            &[assembly.0],
        );
        assert!(
            movement.translation_m[0] > 3.99,
            "excluded assembly's non-primary child collider blocked the direct sweep: {movement:?}"
        );
    }

    #[test]
    fn legacy_single_collider_lifecycle_calls_explicitly_reject_articulated_owners() {
        let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let assembly = world
            .add_articulated_body(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(grid.clone(), 84, 1)],
                Representation::NativeVoxels,
            )
            .unwrap();
        let owner = assembly.0;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| world.remove_collider(owner)))
                .is_err()
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| world.rebuild_collider(
                owner,
                &grid,
                Representation::NativeVoxels
            )))
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| world.deactivate_body(owner)))
                .is_err()
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| world.reactivate_body(
                owner,
                &grid,
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                [0.0; 3],
                [0.0; 3]
            )))
            .is_err()
        );
        assert_eq!(world.articulated_collider_count(assembly), 1);
        assert!(!world.is_retired(owner));
    }

    #[test]
    fn diagnostic_bounded_root_motion_vs_part_local_motion() {
        fn run(root_motion: bool, representation: Representation) -> (f32, f32, usize) {
            let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                ..PhysicsConfig::default()
            });
            let mut part = one_part(grid.clone(), if root_motion { 74 } else { 75 }, 1);
            if !root_motion {
                part.local_transform.translation_m[0] = -0.75;
            }
            let animal = world
                .add_articulated_body(
                    if root_motion {
                        [-0.75, 0.0, 0.0]
                    } else {
                        [0.0; 3]
                    },
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![part],
                    representation,
                )
                .unwrap();
            let probe = world.add_body(BodySpec {
                kind: BodyKind::Dynamic { ccd: false },
                representation,
                grid,
                cell_m: 0.25,
                density_kg_m3: 1_000.0,
                mass_properties: None,
                translation_m: [0.0; 3],
                linvel_m_s: [0.0; 3],
            });
            let mut impulse_sum = 0.0f32;
            let mut contact_steps = 0usize;
            for tick in 1..=60u64 {
                let x = -0.75 + tick as f32 * 0.01;
                if root_motion {
                    world
                        .set_next_articulated_root_pose(
                            animal,
                            1,
                            tick - 1,
                            tick,
                            [x, 0.0, 0.0],
                            [0.0, 0.0, 0.0, 1.0],
                        )
                        .unwrap();
                } else {
                    world
                        .update_articulated_pose(
                            animal,
                            ArticulatedPose {
                                topology_token: 1,
                                base_pose_token: tick - 1,
                                pose_token: tick,
                                parts: vec![(
                                    7,
                                    PartTransform {
                                        translation_m: [x, 0.0, 0.0],
                                        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                                    },
                                )],
                            },
                        )
                        .unwrap();
                }
                world.step();
                for contact in world
                    .contact_impulses()
                    .into_iter()
                    .filter(|c| c.bodies.contains(&probe))
                {
                    impulse_sum += contact.normal_impulse_n_s;
                    contact_steps += 1;
                }
            }
            let state = world.body_state(probe);
            (state.translation_m[0], impulse_sum, contact_steps)
        }

        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let root = run(true, representation);
            let part = run(false, representation);
            println!(
                "bounded 60-step root motion {representation:?}: probe_x={}, impulse_sum={}, contact_steps={}; part-local: probe_x={}, impulse_sum={}, contact_steps={}",
                root.0, root.1, root.2, part.0, part.1, part.2
            );
            assert!(
                root.1 > 0.0,
                "root-driven motion should produce a real Rapier response for {representation:?}"
            );
        }
    }

    #[test]
    fn articulated_kinematic_part_body_motion_has_solver_surface_velocity() {
        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                ..PhysicsConfig::default()
            });
            let mut part = one_part(grid.clone(), 174, 1);
            part.local_transform.translation_m[0] = -0.75;
            let animal = world
                .add_articulated_kinematic_parts(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![part],
                    representation,
                )
                .unwrap();
            let probe = world.add_body(BodySpec {
                kind: BodyKind::Dynamic { ccd: false },
                representation,
                grid,
                cell_m: 0.25,
                density_kg_m3: 1_000.0,
                mass_properties: None,
                translation_m: [0.0; 3],
                linvel_m_s: [0.0; 3],
            });
            let mut impulse_sum = 0.0f32;
            let mut contact_steps = 0usize;
            for tick in 1..=60u64 {
                let x = -0.75 + tick as f32 * 0.01;
                world
                    .update_articulated_pose(
                        animal,
                        ArticulatedPose {
                            topology_token: 1,
                            base_pose_token: tick - 1,
                            pose_token: tick,
                            parts: vec![(
                                7,
                                PartTransform {
                                    translation_m: [x, 0.0, 0.0],
                                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                                },
                            )],
                        },
                    )
                    .unwrap();
                let before_step = world
                    .raycast_articulated(animal, [-2.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                    .unwrap();
                assert_eq!(before_step.pose_token, tick - 1);
                world.step();
                for contact in world
                    .contact_impulses()
                    .into_iter()
                    .filter(|contact| contact.bodies.contains(&probe))
                {
                    assert_eq!(contact.source_cells, [None, None]);
                    if contact.bodies.contains(&animal.0) {
                        assert!(contact.source_parts.contains(&Some(7)));
                    }
                    impulse_sum += contact.normal_impulse_n_s;
                    contact_steps += 1;
                }
            }
            let state = world.body_state(probe);
            println!(
                "candidate 60-step part-body motion {representation:?}: probe_x={}, impulse_sum={}, contact_steps={}, body_count={}, collider_count={}",
                state.translation_m[0],
                impulse_sum,
                contact_steps,
                world.articulated_rigid_body_count(animal),
                world.articulated_collider_count(animal)
            );
            assert!(impulse_sum > 0.0);
            assert!(contact_steps > 0);
            assert_eq!(world.articulated_rigid_body_count(animal), 2);
            assert_eq!(world.articulated_collider_count(animal), 1);
        }
    }

    #[test]
    fn articulated_sweep_allows_grounded_horizontal_and_upward_recovery() {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: OccupancyGrid::from_solid_mask(
                GlobalCell::new(-4, 0, -4),
                [8, 1, 8],
                vec![true; 64],
                vec![fixtures::STONE; 64],
            )
            .unwrap(),
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        // Wall begins at x=.75, directly adjacent to the floor surface.
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: OccupancyGrid::from_solid_mask(
                GlobalCell::new(3, 1, -4),
                [1, 4, 8],
                vec![true; 32],
                vec![fixtures::STONE; 32],
            )
            .unwrap(),
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        // The conservative sphere is tangent to the floor top at y=.25.
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0, 0.34150636, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 194, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let lateral = world
            .sweep_articulated_root_translation(animal, 1, 0, 1, [0.25, 0.0, 0.0])
            .unwrap();
        assert!(lateral.fraction > 0.99, "grounded lateral={lateral:?}");
        world.step();
        let upward = world
            .sweep_articulated_root_translation(animal, 1, 1, 2, [0.0, 0.2, 0.0])
            .unwrap();
        assert!(upward.fraction > 0.99, "grounded upward={upward:?}");
        world.step();
        let wall_adjacent_to_floor = world
            .sweep_articulated_root_translation(animal, 1, 2, 3, [0.25, 0.0, 0.0])
            .unwrap();
        assert!(
            wall_adjacent_to_floor.blocked,
            "wall/floor={wall_adjacent_to_floor:?}"
        );
        world.step();
        let downward = world
            .sweep_articulated_root_translation(animal, 1, 3, 4, [0.0, -0.3, 0.0])
            .unwrap();
        assert!(downward.blocked, "movement into floor={downward:?}");
        assert!(downward.fraction < 0.99, "movement into floor={downward:?}");
    }

    #[test]
    fn articulated_sweep_does_not_skip_later_wall_in_shared_l_terrain_collider() {
        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let mut solid = vec![false; 4 * 4 * 4];
            for z in 0..4 {
                for x in 0..4 {
                    solid[(z * 4) + x] = true;
                }
            }
            for y in 1..4 {
                for z in 0..4 {
                    solid[(y * 4 * 4) + (z * 4) + 2] = true;
                }
            }
            let terrain = OccupancyGrid::from_solid_mask(
                GlobalCell::new(0, 0, 0),
                [4, 4, 4],
                solid,
                vec![fixtures::STONE; 4 * 4 * 4],
            )
            .unwrap();
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                ..PhysicsConfig::default()
            });
            world.add_body(BodySpec {
                kind: BodyKind::Fixed,
                representation,
                grid: terrain,
                cell_m: 0.25,
                density_kg_m3: 0.0,
                mass_properties: None,
                translation_m: [0.0; 3],
                linvel_m_s: [0.0; 3],
            });
            assert_eq!(
                world.colliders.len(),
                1,
                "expected one shared terrain collider"
            );
            let animal = world
                .add_articulated_kinematic_parts(
                    [0.0, 0.34150636, 0.0],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 197, 1)],
                    Representation::MergedCuboids,
                )
                .unwrap();
            let movement = world
                .sweep_articulated_root_translation(animal, 1, 0, 1, [1.0, 0.0, 0.0])
                .unwrap();
            assert!(movement.blocked, "{representation:?}: {movement:?}");
            assert!(movement.fraction < 0.5, "{representation:?}: {movement:?}");
        }
    }

    #[test]
    fn articulated_sweep_encloses_rotated_off_center_source_volume() {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: one_cell_grid(GlobalCell::new(0, 0, -12)),
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [
                    0.0,
                    std::f32::consts::FRAC_1_SQRT_2,
                    0.0,
                    std::f32::consts::FRAC_1_SQRT_2,
                ],
                1,
                vec![one_part(one_cell_grid(GlobalCell::new(8, 0, 0)), 195, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let movement = world
            .sweep_articulated_root_translation(animal, 1, 0, 1, [0.0, 0.0, -1.0])
            .unwrap();
        assert!(movement.blocked, "rotated off-center sweep={movement:?}");
        assert!(movement.fraction < 0.8, "sweep={movement:?}");
    }

    #[test]
    fn articulated_sweep_rejects_initial_penetration_and_allows_small_ledge_departure() {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0, 0.3, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 196, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let into_overlap = world
            .sweep_articulated_root_translation(animal, 1, 0, 1, [0.0, -0.1, 0.0])
            .unwrap();
        assert!(into_overlap.blocked, "initial overlap={into_overlap:?}");
        assert_eq!(into_overlap.fraction, 0.0);
        world.step();
        let tiny_into_surface = world
            .sweep_articulated_root_translation(animal, 1, 1, 2, [0.0, -1.0e-7, 0.0])
            .unwrap();
        assert!(
            tiny_into_surface.blocked,
            "tiny inward move={tiny_into_surface:?}"
        );
        assert_eq!(tiny_into_surface.fraction, 0.0);
        world.step();
        let ledge_departure = world
            .sweep_articulated_root_translation(animal, 1, 2, 3, [0.5, 0.0, 0.0])
            .unwrap();
        assert!(
            !ledge_departure.blocked,
            "ledge departure={ledge_departure:?}"
        );
        assert_eq!(ledge_departure.fraction, 1.0);
    }

    #[test]
    fn articulated_combined_range_rejection_does_not_leave_part_targets_pending() {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let mut source = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 198, 1);
        source.local_transform.translation_m[0] = 9.6;
        let animal = world
            .add_articulated_kinematic_parts(
                [990.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![source],
                Representation::MergedCuboids,
            )
            .unwrap();
        let before = world.body_state(animal.0);
        let hit_before = world
            .raycast_articulated(animal, [990.0, 0.125, 0.125], [1.0, 0.0, 0.0], 20.0)
            .unwrap();
        let near_limit_update = ArticulatedPose {
            topology_token: 1,
            base_pose_token: 0,
            pose_token: 1,
            parts: vec![(
                7,
                PartTransform {
                    translation_m: [9.6, 0.0, 0.0],
                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                },
            )],
        };
        assert!(
            world
                .update_articulated_pose_with_swept_root_translation(
                    animal,
                    near_limit_update.clone(),
                    [0.5, 0.0, 0.0],
                )
                .is_err()
        );
        assert!(
            world
                .update_articulated_pose_with_swept_root_pose(
                    animal,
                    near_limit_update,
                    PartTransform {
                        translation_m: [990.5, 0.0, 0.0],
                        ..PartTransform::IDENTITY
                    },
                )
                .is_err()
        );
        let after = world.body_state(animal.0);
        assert_eq!(after.translation_m, before.translation_m);
        let hit_after = world
            .raycast_articulated(animal, [990.0, 0.125, 0.125], [1.0, 0.0, 0.0], 20.0)
            .unwrap();
        assert_eq!(hit_after, hit_before);
        world.step();
        assert_eq!(
            world.body_state(animal.0).translation_m,
            before.translation_m
        );
    }

    #[test]
    fn articulated_conservative_sweep_blocks_wall_and_rejects_stale_updates() {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let wall_grid = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [1, 4, 4],
            vec![true; 16],
            vec![fixtures::STONE; 16],
        )
        .unwrap();
        let wall = world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: wall_grid,
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [0.5, 0.0, 0.0],
            linvel_m_s: [0.0; 3],
        });
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 181, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let turn = [
            0.0,
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
        ];
        assert!(
            world
                .set_next_articulated_root_pose(animal, 1, 0, 1, [0.0; 3], turn)
                .is_err()
        );
        assert!(
            world
                .set_articulated_root_pose(animal, 1, 0, 1, [0.0; 3], turn)
                .is_err()
        );
        let movement = world
            .sweep_articulated_root_translation(animal, 1, 0, 1, [2.0, 0.0, 0.0])
            .unwrap();
        println!("candidate conservative 2m fixed-wall sweep: {movement:?}");
        assert!(movement.blocked);
        assert!(movement.fraction < 0.2, "movement={movement:?}");
        assert!(movement.applied_translation_m[0] < 0.5);
        assert!(
            world
                .set_next_articulated_root_pose(
                    animal,
                    1,
                    0,
                    2,
                    [1.0, 0.0, 0.0],
                    [0.0, 0.0, 0.0, 1.0],
                )
                .is_err()
        );
        assert!(
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 1,
                        base_pose_token: 0,
                        pose_token: 2,
                        parts: vec![(7, PartTransform::IDENTITY)],
                    },
                )
                .is_err()
        );
        let before = world
            .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
            .unwrap();
        assert_eq!(before.pose_token, 0);
        world.step();
        let state = world.body_state(animal.0);
        assert!(state.translation_m[0] < 0.5, "state={state:?}");
        let after = world
            .raycast_articulated(
                animal,
                [state.translation_m[0] - 1.0, 0.125, 0.125],
                [1.0, 0.0, 0.0],
                4.0,
            )
            .unwrap();
        assert_eq!(after.pose_token, 1);
        world.remove_collider(wall);
        let after_removal = world
            .sweep_articulated_root_translation(animal, 1, 1, 2, [0.5, 0.0, 0.0])
            .unwrap();
        assert!(
            !after_removal.blocked,
            "after terrain removal={after_removal:?}"
        );
        assert_eq!(after_removal.fraction, 1.0);
        world.step();
        let unchanged_body = world.body_state(animal.0);
        let unchanged_hit = world
            .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
            .unwrap();
        assert!(
            world
                .update_articulated_pose_with_swept_root_translation(
                    animal,
                    ArticulatedPose {
                        topology_token: 1,
                        base_pose_token: 2,
                        pose_token: 3,
                        parts: vec![(999, PartTransform::IDENTITY)],
                    },
                    [0.2, 0.0, 0.0],
                )
                .is_err()
        );
        let after_rejection_body = world.body_state(animal.0);
        assert_eq!(
            after_rejection_body.translation_m,
            unchanged_body.translation_m
        );
        assert_eq!(after_rejection_body.rotation, unchanged_body.rotation);
        assert_eq!(
            world
                .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                .unwrap(),
            unchanged_hit
        );
        let combined = world
            .update_articulated_pose_with_swept_root_translation(
                animal,
                ArticulatedPose {
                    topology_token: 1,
                    base_pose_token: 2,
                    pose_token: 3,
                    parts: vec![(7, PartTransform::IDENTITY)],
                },
                [0.1, 0.0, 0.0],
            )
            .unwrap();
        assert!(!combined.blocked);
        let before_combined_step = world
            .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
            .unwrap();
        assert_eq!(before_combined_step.pose_token, 2);
        world.step();
        let after_combined_step = world
            .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
            .unwrap();
        assert_eq!(after_combined_step.pose_token, 3);
    }

    #[test]
    fn articulated_part_body_fast_single_step_contact_is_measured_without_sweep() {
        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            for ccd in [false, true] {
                let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
                let mut world = PhysicsWorld::new(PhysicsConfig {
                    gravity_m_s2: [0.0; 3],
                    ..Default::default()
                });
                let mut source = one_part(grid.clone(), 183, 1);
                source.local_transform.translation_m[0] = -0.5;
                let animal = world
                    .add_articulated_kinematic_parts(
                        [0.0; 3],
                        [0.0, 0.0, 0.0, 1.0],
                        1,
                        vec![source],
                        representation,
                    )
                    .unwrap();
                let probe = world.add_body(BodySpec {
                    kind: BodyKind::Dynamic { ccd },
                    representation,
                    grid,
                    cell_m: 0.25,
                    density_kg_m3: 1_000.0,
                    mass_properties: None,
                    translation_m: [0.0; 3],
                    linvel_m_s: [0.0; 3],
                });
                world
                    .update_articulated_pose(
                        animal,
                        ArticulatedPose {
                            topology_token: 1,
                            base_pose_token: 0,
                            pose_token: 1,
                            parts: vec![(
                                7,
                                PartTransform {
                                    translation_m: [0.0, 0.0, 0.0],
                                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                                },
                            )],
                        },
                    )
                    .unwrap();
                world.step();
                let broad_contact_pairs = world.contact_pair_count();
                let contacts = world
                    .contact_impulses()
                    .into_iter()
                    .filter(|contact| contact.bodies.contains(&probe))
                    .collect::<Vec<_>>();
                let pair_count = contacts.len();
                let impulse = contacts
                    .iter()
                    .map(|contact| contact.normal_impulse_n_s)
                    .sum::<f32>();
                println!(
                    "original unswept 0.5m one-step part target {representation:?} dynamic_ccd={ccd}: contact_pairs={}, positive_impulse_pairs={}, impulse_sum={impulse}",
                    broad_contact_pairs, pair_count
                );
            }
            // Keep the original no-CCD result and measure the same exact
            // workload with candidate dynamic CCD enabled; neither is assumed
            // to provide a kinematic-part impact response.
        }
    }

    #[test]
    fn articulated_contact_witness_resolves_only_unique_source_face_contacts() {
        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                solver_substeps: 4,
                ..Default::default()
            });
            let mut root_part = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 186, 3);
            root_part.local_transform.translation_m[0] = 3.0;
            let nested_source = crate::articulated::SourcePart {
                part_id: 8,
                parent_id: Some(7),
                volume_id: VolumeId::new(188).unwrap(),
                source_revision: 10,
                grid: one_cell_grid(GlobalCell::new(-2, 0, 0)),
                cell_m: 0.25,
                local_transform: PartTransform {
                    translation_m: [-3.0, 0.0, 0.0],
                    ..PartTransform::IDENTITY
                },
            };
            let animal = world
                .add_articulated_kinematic_parts(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    23,
                    vec![root_part, nested_source],
                    representation,
                )
                .unwrap();
            let probe_grid = one_cell_grid(GlobalCell::new(0, 0, 0));
            let probe = world.add_body(BodySpec {
                kind: BodyKind::Dynamic { ccd: false },
                representation,
                grid: probe_grid,
                cell_m: 0.0625,
                density_kg_m3: 1_000.0,
                mass_properties: None,
                translation_m: [-0.25, 0.09375, 0.09375],
                linvel_m_s: [0.0; 3],
            });
            // Source cell at grid-local x=[0,.25] has its authored global cell
            // at x=-2, and therefore world x=[-.5,-.25]. A small probe touches
            // only the center of its +X face.
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 23,
                        base_pose_token: 0,
                        pose_token: 1,
                        parts: vec![
                            (
                                7,
                                PartTransform {
                                    translation_m: [3.0, 0.0, 0.0],
                                    ..PartTransform::IDENTITY
                                },
                            ),
                            (
                                8,
                                PartTransform {
                                    translation_m: [-2.995, 0.0, 0.0],
                                    ..PartTransform::IDENTITY
                                },
                            ),
                        ],
                    },
                )
                .unwrap();
            world.step();
            let contacts = world
                .articulated_contact_witnesses()
                .iter()
                .filter(|contact| contact.bodies.contains(&probe))
                .collect::<Vec<_>>();
            assert!(
                !contacts.is_empty(),
                "{representation:?} produced no per-substep witnesses"
            );
            assert!(
                contacts
                    .iter()
                    .all(|contact| contact.version == CONTACT_WITNESS_VERSION)
            );
            assert!(contacts.iter().all(|contact| contact.outer_step == 1));
            assert!(contacts.iter().all(|contact| contact.solver_substep < 4));
            let resolved = contacts
                .iter()
                .flat_map(|contact| contact.source_sides.iter().flatten())
                .filter(|side| matches!(side.attribution, ContactCellAttribution::Resolved { .. }))
                .collect::<Vec<_>>();
            let positive_point_impulses = resolved
                .iter()
                .filter(|side| side.normal_impulse_n_s.is_some_and(|impulse| impulse > 0.0))
                .count();
            println!(
                "source witness candidate {representation:?}: point_records={} resolved={} positive_point_impulses={} overflow={} final_pair_records={}",
                contacts.len(),
                resolved.len(),
                positive_point_impulses,
                world.contact_witness_overflow(),
                world.contact_impulses().len()
            );
            assert!(
                !resolved.is_empty(),
                "centered face contact should resolve without rounding: {:?}",
                contacts
            );
            for side in resolved {
                assert_eq!(side.part_id, 8);
                assert_eq!(side.volume_id, VolumeId::new(188).unwrap());
                assert_eq!(side.source_revision, 10);
                assert_eq!(side.topology_token, 23);
                assert_eq!(side.pose_token, 1);
                assert_eq!(
                    side.attribution,
                    ContactCellAttribution::Resolved {
                        cell: [-2, 0, 0],
                        material: fixtures::STONE,
                    }
                );
                assert!(
                    side.normal_impulse_n_s
                        .is_some_and(|impulse| impulse.is_finite())
                );
            }
            assert!(positive_point_impulses > 0);
            assert!(
                world.contact_impulses().is_empty(),
                "this fixture's point witnesses are transient, not current-final pair records"
            );
            assert!(
                world
                    .contact_impulses()
                    .iter()
                    .all(|pair| pair.source_cells == [None, None])
            );

            // Stale and pending authored updates cannot mutate the prior
            // published witness snapshot or its query provenance.
            let original_hit = world
                .raycast_articulated(animal, [-0.6, 0.125, 0.125], [1.0, 0.0, 0.0], 1.0)
                .unwrap();
            let snapshot = world.articulated_contact_witnesses().to_vec();
            assert!(
                world
                    .update_articulated_pose(
                        animal,
                        ArticulatedPose {
                            topology_token: 23,
                            base_pose_token: 0,
                            pose_token: 2,
                            parts: vec![
                                (
                                    7,
                                    PartTransform {
                                        translation_m: [3.0, 0.0, 0.0],
                                        ..PartTransform::IDENTITY
                                    }
                                ),
                                (
                                    8,
                                    PartTransform {
                                        translation_m: [-3.0, 0.0, 0.0],
                                        ..PartTransform::IDENTITY
                                    }
                                ),
                            ],
                        }
                    )
                    .is_err()
            );
            assert_eq!(world.articulated_contact_witnesses(), snapshot.as_slice());
            assert_eq!(
                world
                    .raycast_articulated(animal, [-0.6, 0.125, 0.125], [1.0, 0.0, 0.0], 1.0)
                    .unwrap(),
                original_hit
            );
            world
                .rebuild_articulated_part_grid(
                    animal,
                    23,
                    24,
                    8,
                    10,
                    11,
                    one_cell_grid(GlobalCell::new(-2, 0, 0)),
                )
                .unwrap();
            assert_eq!(world.articulated_contact_witnesses(), snapshot.as_slice());
            assert!(
                world
                    .update_articulated_pose(
                        animal,
                        ArticulatedPose {
                            topology_token: 23,
                            base_pose_token: 1,
                            pose_token: 2,
                            parts: vec![
                                (
                                    7,
                                    PartTransform {
                                        translation_m: [3.0, 0.0, 0.0],
                                        ..PartTransform::IDENTITY
                                    }
                                ),
                                (
                                    8,
                                    PartTransform {
                                        translation_m: [-2.995, 0.0, 0.0],
                                        ..PartTransform::IDENTITY
                                    }
                                ),
                            ],
                        }
                    )
                    .is_err()
            );
            let replacement_hit = world
                .raycast_articulated(animal, [-0.6, 0.125, 0.125], [1.0, 0.0, 0.0], 1.0)
                .unwrap();
            assert_eq!(replacement_hit.topology_token, 24);
            assert_eq!(replacement_hit.source_revision, 11);
            assert_eq!(replacement_hit.cell, [-2, 0, 0]);
        }
    }

    #[test]
    fn articulated_contact_witness_resolves_source_when_it_is_pair_side_two() {
        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                solver_substeps: 4,
                ..Default::default()
            });
            let probe = world.add_body(BodySpec {
                kind: BodyKind::Dynamic { ccd: false },
                representation,
                grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
                cell_m: 0.0625,
                density_kg_m3: 1_000.0,
                mass_properties: None,
                translation_m: [0.25, 0.09375, 0.09375],
                linvel_m_s: [0.0; 3],
            });
            let animal = world
                .add_articulated_kinematic_parts(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    31,
                    vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 190, 2)],
                    representation,
                )
                .unwrap();
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 31,
                        base_pose_token: 0,
                        pose_token: 1,
                        parts: vec![(
                            7,
                            PartTransform {
                                translation_m: [0.005, 0.0, 0.0],
                                ..PartTransform::IDENTITY
                            },
                        )],
                    },
                )
                .unwrap();
            world.step();
            let source_sides = world
                .articulated_contact_witnesses()
                .iter()
                .filter(|contact| contact.bodies.contains(&probe))
                .filter_map(|contact| contact.source_sides[1])
                .collect::<Vec<_>>();
            assert!(
                !source_sides.is_empty(),
                "{representation:?} did not put source collider second"
            );
            assert!(source_sides.iter().all(|side| {
                side.part_id == 7
                    && side.volume_id == VolumeId::new(190).unwrap()
                    && matches!(side.attribution, ContactCellAttribution::Resolved { cell: [0, 0, 0], material } if material == fixtures::STONE)
            }));
        }
    }

    #[test]
    fn articulated_substeps_preserve_fast_target_time_and_produce_contact_response() {
        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            for ccd in [false, true] {
                let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
                let mut world = PhysicsWorld::new(PhysicsConfig {
                    gravity_m_s2: [0.0; 3],
                    solver_substeps: 20,
                    ..Default::default()
                });
                let mut source = one_part(grid.clone(), 183, 1);
                source.local_transform.translation_m[0] = -0.5;
                let animal = world
                    .add_articulated_kinematic_parts(
                        [0.0; 3],
                        [0.0, 0.0, 0.0, 1.0],
                        1,
                        vec![source],
                        representation,
                    )
                    .unwrap();
                let probe = world.add_body(BodySpec {
                    kind: BodyKind::Dynamic { ccd },
                    representation,
                    grid,
                    cell_m: 0.25,
                    density_kg_m3: 1_000.0,
                    mass_properties: None,
                    translation_m: [0.0; 3],
                    linvel_m_s: [0.0; 3],
                });
                world
                    .update_articulated_pose(
                        animal,
                        ArticulatedPose {
                            topology_token: 1,
                            base_pose_token: 0,
                            pose_token: 1,
                            parts: vec![(7, PartTransform::IDENTITY)],
                        },
                    )
                    .unwrap();
                let start = Instant::now();
                let timing = world.step();
                let elapsed = start.elapsed();
                let contacts = world
                    .contact_impulses()
                    .into_iter()
                    .filter(|contact| contact.bodies.contains(&probe))
                    .collect::<Vec<_>>();
                let impulse_sum = contacts
                    .iter()
                    .map(|contact| contact.normal_impulse_n_s)
                    .sum::<f32>();
                let state = world.body_state(probe);
                println!(
                    "substep candidate {representation:?} dynamic_ccd={ccd} dt={} elapsed_us={} pipeline_us={} outer_steps={} substeps={} last_contacts={} last_impulse={} accumulated_impulse={} impulse_pair_substeps={} probe_x={} probe_vx={} mass_kg={} momentum_x={} kinetic_energy_j={}",
                    1.0 / 60.0,
                    elapsed.as_secs_f64() * 1.0e6,
                    timing.pipeline.as_secs_f64() * 1.0e6,
                    world.steps_run(),
                    timing.substeps,
                    contacts.len(),
                    impulse_sum,
                    timing.normal_impulse_n_s,
                    timing.impulse_pair_substeps,
                    state.translation_m[0],
                    state.linvel_m_s[0],
                    state.mass_kg,
                    state.mass_kg * state.linvel_m_s[0],
                    0.5 * state.mass_kg * state.linvel_m_s.iter().map(|v| v * v).sum::<f32>(),
                );
                assert_eq!(world.steps_run(), 1, "substeps publish one outer step");
                assert!(state.is_finite());
                assert!(timing.normal_impulse_n_s.is_finite() && timing.normal_impulse_n_s > 0.0);
                assert!(timing.impulse_pair_substeps > 0);
                assert!(state.linvel_m_s[0] > 0.0);
                let animal_hit = world
                    .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                    .unwrap();
                assert_eq!(animal_hit.pose_token, 1);
                assert!(animal_hit.toi_m > 0.0);
            }
        }
    }

    #[test]
    fn articulated_substeps_produce_contact_response_for_fast_angular_target() {
        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                solver_substeps: 64,
                ..Default::default()
            });
            let animal = world
                .add_articulated_kinematic_parts(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![one_part(grid.clone(), 185, 1)],
                    representation,
                )
                .unwrap();
            let probe_lo = [-0.15625_f32, 0.21875, 0.09375];
            let probe_hi = [
                probe_lo[0] + 0.0625,
                probe_lo[1] + 0.0625,
                probe_lo[2] + 0.0625,
            ];
            let intervals_overlap = |a0: f32, a1: f32, b0: f32, b1: f32| a0 < b1 && b0 < a1;
            // Independent source-cell boxes: initially x-disjoint; after the
            // exact 90-degree rigid rotation, source and probe overlap on all
            // three axes. This verifies the fixture intersects geometrically.
            assert!(!intervals_overlap(probe_lo[0], probe_hi[0], 0.0, 0.25));
            assert!(intervals_overlap(probe_lo[0], probe_hi[0], -0.25, 0.0));
            assert!(intervals_overlap(probe_lo[1], probe_hi[1], 0.0, 0.25));
            assert!(intervals_overlap(probe_lo[2], probe_hi[2], 0.0, 0.25));
            // The probe begins wholly outside the source cell at x < 0. A
            // 90-degree part-local turn carries the source cell across it.
            let probe = world.add_body(BodySpec {
                kind: BodyKind::Dynamic { ccd: false },
                representation,
                grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
                cell_m: 0.0625,
                density_kg_m3: 1_000.0,
                mass_properties: None,
                translation_m: [-0.15625, 0.21875, 0.09375],
                linvel_m_s: [0.0; 3],
            });
            let half_angle = std::f32::consts::FRAC_PI_4;
            let before = world
                .raycast_articulated(animal, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 1.0)
                .unwrap();
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 1,
                        base_pose_token: 0,
                        pose_token: 1,
                        parts: vec![(
                            7,
                            PartTransform {
                                rotation_xyzw: [0.0, 0.0, half_angle.sin(), half_angle.cos()],
                                ..PartTransform::IDENTITY
                            },
                        )],
                    },
                )
                .unwrap();
            let staged = world
                .raycast_articulated(animal, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 1.0)
                .unwrap();
            assert_eq!(staged, before, "pending angular pose stays unpublished");
            let timing = world.step();
            let state = world.body_state(probe);
            println!(
                "substep angular candidate {representation:?} angle_deg=90 dt={} substeps={} impulse_n_s={} impulse_pair_substeps={} probe_v={:?} mass_kg={} energy_j={}",
                1.0 / 60.0,
                timing.substeps,
                timing.normal_impulse_n_s,
                timing.impulse_pair_substeps,
                state.linvel_m_s,
                state.mass_kg,
                0.5 * state.mass_kg * state.linvel_m_s.iter().map(|v| v * v).sum::<f32>(),
            );
            assert_eq!(timing.substeps, 64);
            assert!(timing.normal_impulse_n_s.is_finite() && timing.normal_impulse_n_s > 0.0);
            assert!(state.is_finite());
            assert!(
                state
                    .linvel_m_s
                    .iter()
                    .any(|component| component.abs() > 1.0e-3)
            );
            assert_eq!(world.steps_run(), 1);
            let hit = world
                .raycast_articulated(animal, [-0.2, 0.125, 0.125], [1.0, 0.0, 0.0], 1.0)
                .unwrap();
            assert_eq!(hit.pose_token, 1);
        }
    }

    #[test]
    fn articulated_substeps_advance_unrelated_world_dynamics_by_one_outer_dt() {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0, -9.81, 0.0],
            solver_substeps: 20,
            ..Default::default()
        });
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 186, 1)],
                Representation::NativeVoxels,
            )
            .unwrap();
        let unrelated = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::NativeVoxels,
            grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
            cell_m: 0.25,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [10.0, 10.0, 10.0],
            linvel_m_s: [0.0; 3],
        });
        world
            .update_articulated_pose(
                animal,
                ArticulatedPose {
                    topology_token: 1,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(
                        7,
                        PartTransform {
                            translation_m: [0.1, 0.0, 0.0],
                            ..PartTransform::IDENTITY
                        },
                    )],
                },
            )
            .unwrap();
        let timing = world.step();
        let state = world.body_state(unrelated);
        let expected_vy = -9.81 / 60.0;
        println!(
            "unrelated dynamic gravity substeps={} outer_dt={} vy={} expected_vy={} y={} steps={}",
            timing.substeps,
            1.0 / 60.0,
            state.linvel_m_s[1],
            expected_vy,
            state.translation_m[1],
            world.steps_run(),
        );
        assert_eq!(timing.substeps, 20);
        assert_eq!(world.steps_run(), 1);
        assert!((state.linvel_m_s[1] - expected_vy).abs() < 1.0e-5);
        assert!(state.translation_m[1] < 10.0);
        let hit = world
            .raycast_articulated(animal, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 1.0)
            .unwrap();
        assert_eq!(hit.pose_token, 1);
    }

    #[test]
    fn articulated_substeps_admit_slow_motion_and_reject_over_budget_atomically() {
        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                solver_substeps: 20,
                ..Default::default()
            });
            let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
            let mut source = one_part(grid.clone(), 193, 1);
            source.local_transform.translation_m[0] = -0.5;
            let animal = world
                .add_articulated_kinematic_parts(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![source],
                    representation,
                )
                .unwrap();
            let probe = world.add_body(BodySpec {
                kind: BodyKind::Dynamic { ccd: false },
                representation,
                grid,
                cell_m: 0.25,
                density_kg_m3: 1_000.0,
                mass_properties: None,
                translation_m: [0.0; 3],
                linvel_m_s: [0.0; 3],
            });
            let mut accumulated_impulse = 0.0_f32;
            for pose_token in 1..=60 {
                let target_x = -0.5 + pose_token as f32 * 0.01;
                world
                    .update_articulated_pose(
                        animal,
                        ArticulatedPose {
                            topology_token: 1,
                            base_pose_token: pose_token - 1,
                            pose_token,
                            parts: vec![(
                                7,
                                PartTransform {
                                    translation_m: [target_x, 0.0, 0.0],
                                    ..PartTransform::IDENTITY
                                },
                            )],
                        },
                    )
                    .unwrap();
                let timing = world.step();
                assert_eq!(timing.substeps, 20);
                accumulated_impulse += timing.normal_impulse_n_s;
            }
            let state = world.body_state(probe);
            let hit = world
                .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                .unwrap();
            println!(
                "substep slow candidate {representation:?} displacement=0.6m outer_ticks=60 substeps=1200 elapsed_s={} impulse_n_s={} probe_x={} probe_vx={} probe_mass_kg={} dynamic_momentum_x={} kinetic_energy_j={} published_pose_token={}",
                60.0 / 60.0,
                accumulated_impulse,
                state.translation_m[0],
                state.linvel_m_s[0],
                state.mass_kg,
                state.mass_kg * state.linvel_m_s[0],
                0.5 * state.mass_kg * state.speed_m_s().powi(2),
                hit.pose_token,
            );
            assert!(accumulated_impulse.is_finite() && accumulated_impulse > 0.0);
            assert!(state.is_finite());
            assert_eq!(hit.pose_token, 60);
            assert_eq!(world.steps_run(), 60);
        }

        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                solver_substeps: 20,
                ..Default::default()
            });
            let animal = world
                .add_articulated_kinematic_parts(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![one_part(grid, 194, 1)],
                    representation,
                )
                .unwrap();
            let before = world
                .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                .unwrap();
            let rejected_translation = world.update_articulated_pose(
                animal,
                ArticulatedPose {
                    topology_token: 1,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(
                        7,
                        PartTransform {
                            translation_m: [0.8, 0.0, 0.0],
                            ..PartTransform::IDENTITY
                        },
                    )],
                },
            );
            assert!(matches!(
                rejected_translation,
                Err(crate::articulated::ArticulatedError::WorkBudget)
            ));
            let after_translation = world
                .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                .unwrap();
            assert_eq!(after_translation, before);

            let rejected_rotation = world.update_articulated_pose(
                animal,
                ArticulatedPose {
                    topology_token: 1,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(
                        7,
                        PartTransform {
                            rotation_xyzw: [0.0, 0.0, 1.0, 0.0],
                            ..PartTransform::IDENTITY
                        },
                    )],
                },
            );
            assert!(matches!(
                rejected_rotation,
                Err(crate::articulated::ArticulatedError::WorkBudget)
            ));
            let after_rotation = world
                .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                .unwrap();
            assert_eq!(after_rotation, before);
            let timing = world.step();
            assert_eq!(timing.substeps, 20);
            assert_eq!(world.steps_run(), 1);
            assert_eq!(
                world
                    .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                    .unwrap(),
                before
            );
        }

        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                solver_substeps: 20,
                ..Default::default()
            });
            let animal = world
                .add_articulated_kinematic_parts(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![one_part(grid.clone(), 195, 1)],
                    representation,
                )
                .unwrap();
            let probe = world.add_body(BodySpec {
                kind: BodyKind::Dynamic { ccd: false },
                representation,
                grid,
                cell_m: 0.25,
                density_kg_m3: 1_000.0,
                mass_properties: None,
                translation_m: [10.0; 3],
                linvel_m_s: [0.0; 3],
            });
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 1,
                        base_pose_token: 0,
                        pose_token: 1,
                        parts: vec![(7, PartTransform::IDENTITY)],
                    },
                )
                .unwrap();
            let timing = world.step();
            let state = world.body_state(probe);
            assert_eq!(timing.substeps, 20);
            assert_eq!(timing.normal_impulse_n_s, 0.0);
            assert_eq!(state.linvel_m_s, [0.0; 3]);
            assert_eq!(world.steps_run(), 1);
        }
    }

    #[test]
    fn nonlinear_pose_and_root_rotation_sweep_blocks_wall_then_accepts_after_removal() {
        let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..Default::default()
        });
        let wall = world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: grid.clone(),
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [0.0, -0.125, 0.0],
            linvel_m_s: [0.0; 3],
        });
        let mut part = one_part(grid.clone(), 189, 1);
        part.local_transform.translation_m[0] = -0.5;
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                PartTransform::IDENTITY.rotation_xyzw,
                1,
                vec![part],
                Representation::MergedCuboids,
            )
            .unwrap();
        let update = ArticulatedPose {
            topology_token: 1,
            base_pose_token: 0,
            pose_token: 1,
            parts: vec![(
                7,
                PartTransform {
                    translation_m: [-0.5, 0.0, 0.0],
                    ..PartTransform::IDENTITY
                },
            )],
        };
        let rotated_root = PartTransform {
            translation_m: [0.0; 3],
            rotation_xyzw: [0.0, 0.0, 1.0, 0.0],
        };
        let blocked = world
            .update_articulated_pose_with_swept_root_pose(animal, update.clone(), rotated_root)
            .unwrap();
        assert!(blocked.blocked);
        assert_eq!(blocked.part_id, Some(7));
        assert!(blocked.time_of_impact.unwrap() < 1.0);
        assert_eq!(world.body_state(animal.0).translation_m, [0.0; 3]);
        let before = world
            .raycast_articulated(animal, [-2.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
            .unwrap();
        assert_eq!(before.pose_token, 0);

        world.retire_body(wall);
        let accepted = world
            .update_articulated_pose_with_swept_root_pose(animal, update, rotated_root)
            .unwrap();
        assert!(!accepted.blocked);
        world.step();
        let after = world
            .raycast_articulated(animal, [-2.0, 0.0, 0.0], [1.0, 0.0, 0.0], 4.0)
            .unwrap();
        assert_eq!(after.pose_token, 1);
        assert_eq!(before.cell, after.cell);

        // A replacement obstacle at the same world location must re-enter
        // the current candidate set; an edit that moves its solid cell away
        // must remove it from the next sweep query.
        let replacement_wall = world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: grid.clone(),
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [0.0, -0.125, 0.0],
            linvel_m_s: [0.0; 3],
        });
        let reverse_update = ArticulatedPose {
            topology_token: 1,
            base_pose_token: 1,
            pose_token: 2,
            parts: vec![(
                7,
                PartTransform {
                    translation_m: [-0.5, 0.0, 0.0],
                    ..PartTransform::IDENTITY
                },
            )],
        };
        let replacement_hit = world
            .update_articulated_pose_with_swept_root_pose(
                animal,
                reverse_update.clone(),
                PartTransform::IDENTITY,
            )
            .unwrap();
        assert!(
            replacement_hit.blocked,
            "replacement obstacle={replacement_hit:?}"
        );
        let moved_cell = OccupancyGrid::from_solid_mask(
            GlobalCell::new(8, 0, 0),
            [1, 1, 1],
            vec![true],
            vec![fixtures::STONE],
        )
        .unwrap();
        world.rebuild_collider(replacement_wall, &moved_cell, Representation::MergedCuboids);
        let after_edit = world
            .update_articulated_pose_with_swept_root_pose(
                animal,
                reverse_update,
                PartTransform::IDENTITY,
            )
            .unwrap();
        assert!(!after_edit.blocked, "edited obstacle={after_edit:?}");
    }

    #[test]
    fn articulated_sweep_localizes_full_world_fixed_colliders_before_work_budget() {
        // 285 exact source cells represent the authored deer workload. The
        // legacy Cartesian loop exceeded its one-million cast budget above
        // 3,508 fixed colliders, even when every collider was hundreds of
        // metres from the declared transition.
        let source_grid = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [285, 1, 1],
            vec![true; 285],
            vec![fixtures::STONE; 285],
        )
        .unwrap();
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        for index in 0..3_509 {
            world.add_body(BodySpec {
                kind: BodyKind::Fixed,
                representation: Representation::MergedCuboids,
                grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
                cell_m: 0.25,
                density_kg_m3: 0.0,
                mass_properties: None,
                translation_m: [20.0 + index as f32 * 0.5, 100.0, 100.0],
                linvel_m_s: [0.0; 3],
            });
        }
        assert_eq!(world.colliders.len(), 3_509);
        let animal = world
            .add_articulated_kinematic_parts(
                [250.0, 0.0, 0.0],
                PartTransform::IDENTITY.rotation_xyzw,
                1,
                vec![one_part(source_grid, 213, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let transition = world
            .update_articulated_pose_with_swept_root_pose(
                animal,
                ArticulatedPose {
                    topology_token: 1,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(7, PartTransform::IDENTITY)],
                },
                PartTransform {
                    translation_m: [250.25, 0.0, 0.0],
                    ..PartTransform::IDENTITY
                },
            )
            .unwrap();
        assert!(!transition.blocked);
        world.step();
        assert_eq!(world.body_state(animal.0).translation_m, [250.25, 0.0, 0.0]);
        assert_eq!(transition.terrain_candidates, 0);
        assert_eq!(transition.cell_casts, 0);
    }

    #[test]
    fn articulated_sweep_loses_ground_support_and_falls_after_voxel_removal() {
        let dt = 1.0 / 60.0;
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let floor_grid = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [8, 1, 8],
            vec![true; 64],
            vec![fixtures::STONE; 64],
        )
        .unwrap();
        let floor = world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: floor_grid,
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [-1.0, 0.0, -1.0],
            linvel_m_s: [0.0; 3],
        });
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0, 2.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 182, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        world.sync_queries();
        let mut token = 0;
        let mut vertical_speed = 0.0f32;
        let mut supported = false;
        for _ in 0..240 {
            vertical_speed -= 9.81 * dt;
            let movement = world
                .sweep_articulated_root_translation(
                    animal,
                    1,
                    token,
                    token + 1,
                    [0.0, vertical_speed * dt, 0.0],
                )
                .unwrap();
            token += 1;
            if movement.blocked {
                supported = true;
                vertical_speed = 0.0;
            }
            world.step();
            if supported {
                break;
            }
        }
        assert!(supported, "conservative rig never found floor support");
        let supported_y = world.body_state(animal.0).translation_m[1];
        world.remove_collider(floor);
        world.sync_queries();
        let fall_start = world.body_state(animal.0).translation_m[1];
        for _ in 0..30 {
            vertical_speed -= 9.81 * dt;
            let movement = world
                .sweep_articulated_root_translation(
                    animal,
                    1,
                    token,
                    token + 1,
                    [0.0, vertical_speed * dt, 0.0],
                )
                .unwrap();
            assert!(!movement.blocked);
            token += 1;
            world.step();
        }
        let fallen_y = world.body_state(animal.0).translation_m[1];
        println!(
            "conservative ground support: root_y={supported_y}, after support removal/fall root_y={fallen_y}"
        );
        assert!(fallen_y < fall_start - 0.5);
    }

    #[test]
    fn diagnostic_fast_one_tick_root_target_contacts_without_probe_response() {
        let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let animal = world
            .add_articulated_body(
                [-0.5, 0.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(grid.clone(), 76, 1)],
                Representation::NativeVoxels,
            )
            .unwrap();
        let probe = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::NativeVoxels,
            grid,
            cell_m: 0.25,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        world
            .set_next_articulated_root_pose(animal, 1, 0, 1, [0.0; 3], [0.0, 0.0, 0.0, 1.0])
            .unwrap();
        world.step();
        let state = world.body_state(probe);
        let impulse: f32 = world
            .contact_impulses()
            .into_iter()
            .filter(|contact| contact.bodies.contains(&probe))
            .map(|contact| contact.normal_impulse_n_s)
            .sum();
        println!(
            "fast one-tick 0.5m root target over 16.67ms: contact_pairs={}, probe_vx={}, probe_x={}, impulse_sum={impulse}",
            world.contact_pair_count(),
            state.linvel_m_s[0],
            state.translation_m[0]
        );
        assert!(world.contact_pair_count() > 0);
        assert!(state.linvel_m_s[0].abs() < 1.0e-6);
        assert_eq!(impulse, 0.0);
    }

    #[test]
    fn diagnostic_kinematic_root_crosses_fixed_terrain_but_character_sweep_blocks() {
        let assembly_grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let terrain_grid = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [1, 8, 1],
            vec![true; 8],
            vec![fixtures::STONE; 8],
        )
        .unwrap();
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: terrain_grid,
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        let animal = world
            .add_articulated_body(
                [-1.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(assembly_grid, 85, 1)],
                Representation::NativeVoxels,
            )
            .unwrap();
        for tick in 1..=120u64 {
            world
                .set_next_articulated_root_pose(
                    animal,
                    1,
                    tick - 1,
                    tick,
                    [-1.0 + (tick as f32 / 60.0), 0.0, 0.0],
                    [0.0, 0.0, 0.0, 1.0],
                )
                .unwrap();
            world.step();
        }
        let root_x = world.body_state(animal.0).translation_m[0];
        println!(
            "2m root path through 0.25m fixed terrain over 2s: root_x={root_x}, contact_pairs={}, max_penetration={}m",
            world.contact_pair_count(),
            world.max_penetration_m()
        );
        assert!(
            (root_x - 1.0).abs() < 0.001,
            "kinematic target teleports through the fixed obstacle"
        );
        assert_eq!(
            world.contact_pair_count(),
            0,
            "default Rapier active collision types omit kinematic-fixed pairs"
        );

        world.retire_articulated_body(animal);
        world.sync_queries();
        let movement = world.sweep_character_excluding(
            crate::character::CharacterParams::DEFAULT,
            [-1.0, 0.0, 0.125],
            [2.0, 0.0, 0.0],
            1.0 / 60.0,
            &[],
        );
        println!(
            "character query sweep through the same fixed terrain: moved_x={}",
            movement.translation_m[0]
        );
        assert!(
            movement.translation_m[0] < 1.5,
            "query-based locomotion sweep should see and stop at terrain"
        );
    }

    fn pushed_cube_speed(density_kg_m3: f64) -> (f32, f32) {
        let id = VolumeId::new(10).unwrap();
        let mut volume = Volume::new(id, CellSizeCode::Quarter);
        volume
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(3, 3, 3),
                fixtures::STONE,
            ))
            .unwrap();
        let grid = OccupancyGrid::from_volume(&volume).unwrap().unwrap();
        let props = analytic_mass_properties(&grid, CELL_M, |_| density_kg_m3).to_body_properties();
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let body = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::NativeVoxels,
            grid,
            cell_m: CELL_M as f32,
            density_kg_m3: density_kg_m3 as f32,
            mass_properties: Some(props),
            translation_m: [0.5, 0.0, 0.0],
            linvel_m_s: [0.0; 3],
        });
        world.step();
        world.sweep_character_pushing_excluding(
            crate::character::CharacterParams::DEFAULT,
            [0.0, 0.0, 0.5],
            [0.75, 0.0, 0.0],
            1.0 / 60.0,
            &[],
        );
        let state = world.body_state(body);
        (state.linvel_m_s[0], state.mass_kg)
    }

    /// Speed given to a zero-gravity 1 m cube at `cube_x` (its origin; the cube
    /// spans `+1 m` in x) by one 0.075 m walking step along +x from x = 0.
    /// Returns `(speed_along_x, speed_total)`.
    fn carried_cube_speed(cube_x: f32, cube_z: f32) -> (f32, f32) {
        let id = VolumeId::new(12).unwrap();
        let mut volume = Volume::new(id, CellSizeCode::Quarter);
        volume
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(3, 3, 3),
                fixtures::STONE,
            ))
            .unwrap();
        let grid = OccupancyGrid::from_volume(&volume).unwrap().unwrap();
        let props = analytic_mass_properties(&grid, CELL_M, |_| 2_000.0).to_body_properties();
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let body = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::MergedCuboids,
            grid,
            cell_m: CELL_M as f32,
            density_kg_m3: 2_000.0,
            mass_properties: Some(props),
            translation_m: [cube_x, 0.0, cube_z],
            linvel_m_s: [0.0; 3],
        });
        world.step();
        world.sweep_character_pushing_excluding(
            crate::character::CharacterParams::DEFAULT,
            [0.0, 0.0, 0.5],
            [0.075, 0.0, 0.0],
            1.0 / 60.0,
            &[],
        );
        let v = world.body_state(body).linvel_m_s;
        (v[0], (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt())
    }

    /// Audit of the "carry" helper (bodies near and ahead of a walking
    /// character are lifted to its speed even without a registered contact):
    /// it must never launch a body that is out of reach, behind, or faster
    /// than the character itself walks.
    #[test]
    fn carry_helper_never_launches_out_of_reach_or_faster_than_the_walker() {
        const WALK: f32 = crate::character::tuning::WALK_SPEED_M_S;
        // Capsule radius 0.3 at x = 0.075: front at 0.375. Cube x-extent 1 m.
        // Well ahead (gap > margin): untouched.
        let (far, _) = carried_cube_speed(0.6, 0.0);
        assert!(far.abs() < 1e-4, "out-of-reach cube launched: {far}");
        // Beside the walker (1 m off to the side, far beyond any margin).
        let (side, total) = carried_cube_speed(0.0, 1.5);
        assert!(total < 1e-4, "cube beside the walker launched: {side}");
        // Behind the walker.
        let (behind, total) = carried_cube_speed(-1.6, 0.0);
        assert!(total < 1e-4, "cube behind the walker launched: {behind}");
        // Touching / within the margin: carried, but never past walking speed.
        let (near, _) = carried_cube_speed(0.4, 0.0);
        assert!(near > 0.0, "the near cube should be pushed along");
        assert!(
            near <= WALK + 1e-3,
            "carried faster than the walker: {near}"
        );
    }

    /// A prebuilt exclusion names bodies, not collider handles, so it keeps excluding a body whose
    /// collider is rebuilt (an edit gives the collider a new handle).
    #[test]
    fn an_exclusion_still_hides_a_body_after_its_collider_is_rebuilt() {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let floor_volume = crate::fixtures::floor_slab(VolumeId::new(11).unwrap(), 2, 2, 4);
        let grid = OccupancyGrid::from_volume(&floor_volume).unwrap().unwrap();
        let floor = world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: grid.clone(),
            cell_m: CELL_M as f32,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        world.step();
        let params = crate::character::CharacterParams::DEFAULT;
        // Standing above the floor and asked to move straight down by a metre.
        let sweep = |world: &mut PhysicsWorld, exclusion: &ColliderExclusion| {
            world
                .sweep_character_pushing_excluding_set(
                    params,
                    [0.5, 1.5, 0.5],
                    [0.0, -1.0, 0.0],
                    1.0 / 60.0,
                    exclusion,
                    &[],
                )
                .translation_m[1]
        };
        let none = ColliderExclusion::default();
        let hidden = world.exclusion_for([floor]);
        let blocked = sweep(&mut world, &none);
        let through = sweep(&mut world, &hidden);
        assert!(blocked > -1.0, "the floor stops the character: {blocked}");
        assert!(
            through < blocked - 0.05,
            "excluded, it falls through: {through} vs {blocked}"
        );

        world.rebuild_collider(floor, &grid, Representation::MergedCuboids);
        world.step();
        let after_blocked = sweep(&mut world, &none);
        let after_through = sweep(&mut world, &hidden);
        assert!(
            (after_blocked - blocked).abs() < 1.0e-3,
            "the rebuilt floor still blocks"
        );
        assert!(
            (after_through - through).abs() < 1.0e-3,
            "the same exclusion still hides the rebuilt collider: {after_through} vs {through}"
        );
    }

    #[test]
    fn character_push_uses_fine_voxel_mass() {
        let (small_speed, small_mass) = pushed_cube_speed(20.0);
        let (large_speed, large_mass) = pushed_cube_speed(2_000.0);
        assert!(
            (small_mass - 20.0).abs() < 1.0e-3,
            "small mass={small_mass}"
        );
        assert!(
            (large_mass - 2_000.0).abs() < 1.0e-2,
            "large mass={large_mass}"
        );
        assert!(small_speed > 0.0, "the player did not push the small cube");
        assert!(
            small_speed > large_speed * 5.0,
            "voxel mass should resist the same push: small={small_speed}, large={large_speed}"
        );
    }

    #[test]
    fn high_restitution_body_visibly_rebounds_from_zero_restitution_terrain() {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let floor_volume = crate::fixtures::floor_slab(VolumeId::new(11).unwrap(), 2, 2, 4);
        let floor_grid = OccupancyGrid::from_volume(&floor_volume).unwrap().unwrap();
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: floor_grid,
            cell_m: CELL_M as f32,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });

        let id = VolumeId::new(12).unwrap();
        let mut cube = Volume::new(id, CellSizeCode::Quarter);
        cube.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(1, 1, 1),
            fixtures::STONE,
        ))
        .unwrap();
        let grid = OccupancyGrid::from_volume(&cube).unwrap().unwrap();
        let props = analytic_mass_properties(&grid, CELL_M, |_| 2_000.0).to_body_properties();
        let body = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::NativeVoxels,
            grid,
            cell_m: CELL_M as f32,
            density_kg_m3: 2_000.0,
            mass_properties: Some(props),
            translation_m: [8.0, 5.0, 8.0],
            linvel_m_s: [0.0; 3],
        });
        world.set_restitution(body, 0.72);

        let mut fell = false;
        let mut rebound_speed = 0.0_f32;
        for _ in 0..180 {
            world.step();
            let vy = world.body_state(body).linvel_m_s[1];
            fell |= vy < -1.0;
            if fell {
                rebound_speed = rebound_speed.max(vy);
            }
        }
        assert!(
            rebound_speed > 1.0,
            "expected a visible rebound, max upward speed was {rebound_speed} m/s"
        );
    }

    fn gravity_free() -> PhysicsWorld {
        PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        })
    }

    fn add_mixed_body(world: &mut PhysicsWorld, props: Option<BodyMassProperties>) -> BodyId {
        world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::MergedCuboids,
            grid: mixed_density_grid(),
            cell_m: CELL_M as f32,
            density_kg_m3: 2000.0,
            mass_properties: props,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        })
    }

    /// ENG-61: the authoritative serve loop rebuilds the *terrain* collider on
    /// every committed cut. A body that is settling onto that terrain must still
    /// come to rest on it — a rebuild that drops the broad-phase proxy and the
    /// contact manifold each tick must not let a faller tunnel the floor or jitter
    /// forever (CCD is disabled for the networked destruction scenes).
    #[test]
    fn a_body_settles_on_terrain_whose_collider_is_rebuilt_every_step() {
        use crate::fixtures as phys_fx;

        let mut world = PhysicsWorld::new(PhysicsConfig {
            disable_ccd: true,
            ..PhysicsConfig::default()
        });

        // Fixed floor: 4 cells (1.0 m) thick, top surface at y = 1.0 m.
        let floor = phys_fx::floor_slab(VolumeId::new(1).unwrap(), 2, 2, 4);
        let floor_grid = crate::occupancy::OccupancyGrid::from_volume(&floor)
            .unwrap()
            .unwrap();
        let floor_id = world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: floor_grid.clone(),
            cell_m: phys_fx::CELL_M,
            density_kg_m3: phys_fx::STONE_DENSITY,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });

        // A small cube released ~4 m up, so it reaches a real fall speed before
        // it meets the floor whose collider is churning under it.
        let piece = phys_fx::debris_pieces(50, 1, 2).pop().unwrap().1;
        let grid = crate::occupancy::OccupancyGrid::from_volume(&piece)
            .unwrap()
            .unwrap();
        let body = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::MergedCuboids,
            grid,
            cell_m: phys_fx::CELL_M,
            density_kg_m3: phys_fx::STONE_DENSITY,
            mass_properties: None,
            translation_m: [4.0, 5.0, 4.0],
            linvel_m_s: [0.0; 3],
        });

        for _ in 0..600 {
            world.rebuild_collider(floor_id, &floor_grid, Representation::MergedCuboids);
            world.step();
            assert!(world.body_state(body).is_finite());
        }

        let st = world.body_state(body);
        assert!(
            st.translation_m[1] > 0.9,
            "body tunnelled the rebuilt-every-step floor (top at y = 1.0 m, body at y = {})",
            st.translation_m[1]
        );
        assert!(
            st.speed_m_s() < 0.05,
            "body never came to rest on the churning collider (speed = {} m/s)",
            st.speed_m_s()
        );
        assert!(
            world.max_penetration_m() < 0.1,
            "resting body did not sink into the floor ({} m)",
            world.max_penetration_m()
        );
    }

    /// PR #109 review finding: [`PhysicsWorld::sync_queries`] used to call
    /// [`rapier3d::geometry::BroadPhaseBvh::update`] directly, which Rapier
    /// permanently remembers as "this pair was announced" — so a single
    /// `sync_queries` call before a pair's very first real [`PhysicsWorld::step`]
    /// silently and permanently starved that pair of contacts, for the rest
    /// of the simulation, however many times `step` ran afterwards. This is
    /// exactly [`CharacterQueryCache`](crate::query_cache::CharacterQueryCache)'s
    /// real call pattern: it shares this `PhysicsWorld` with a simulation it
    /// does not own the tick loop for, and calls `sync_queries` (never
    /// `step`) after every window rebuild.
    #[test]
    fn sync_queries_before_the_first_step_does_not_starve_a_dynamic_body_of_contacts() {
        fn unit_cube_body(world: &mut PhysicsWorld, kind: BodyKind, y: f32) -> BodyId {
            world.add_body(BodySpec {
                kind,
                representation: Representation::MergedCuboids,
                grid: OccupancyGrid::from_solid_mask(
                    GlobalCell::new(0, 0, 0),
                    [1, 1, 1],
                    vec![true],
                    vec![MaterialId(1)],
                )
                .unwrap(),
                cell_m: 1.0,
                density_kg_m3: 1.0,
                mass_properties: None,
                translation_m: [0.0, y, 0.0],
                linvel_m_s: [0.0; 3],
            })
        }

        let mut without_sync = PhysicsWorld::new(PhysicsConfig::default());
        unit_cube_body(&mut without_sync, BodyKind::Fixed, 0.0);
        let dynamic_no_sync =
            unit_cube_body(&mut without_sync, BodyKind::Dynamic { ccd: false }, 1.0);
        for _ in 0..60 {
            without_sync.step();
        }
        let baseline = without_sync.body_state(dynamic_no_sync);
        assert!(
            baseline.translation_m[1] > 0.9,
            "baseline (no sync_queries) should rest on the floor, got y = {}",
            baseline.translation_m[1]
        );

        let mut with_sync = PhysicsWorld::new(PhysicsConfig::default());
        unit_cube_body(&mut with_sync, BodyKind::Fixed, 0.0);
        let dynamic = unit_cube_body(&mut with_sync, BodyKind::Dynamic { ccd: false }, 1.0);
        with_sync.sync_queries();
        for _ in 0..60 {
            with_sync.step();
        }
        let st = with_sync.body_state(dynamic);
        assert!(
            st.translation_m[1] > 0.9,
            "a sync_queries call before the first step must not stop the pair from ever \
             contacting — expected the body to rest at y > 0.9 like the no-sync baseline \
             ({}), got y = {}",
            baseline.translation_m[1],
            st.translation_m[1]
        );
        assert!(
            with_sync.contact_pair_count() >= 1,
            "expected at least one live contact pair after settling, found none"
        );
    }

    /// Same regression, but exercising [`CharacterQueryCache`]'s actual usage
    /// pattern: a collider *replacement* ([`PhysicsWorld::rebuild_collider`])
    /// followed by `sync_queries`, on a `PhysicsWorld` a real dynamic body is
    /// also resting in — not just a freshly-added collider.
    #[test]
    fn sync_queries_after_collider_replacement_does_not_starve_a_dynamic_body_of_contacts() {
        let mut world = PhysicsWorld::new(PhysicsConfig::default());
        let grid = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [1, 1, 1],
            vec![true],
            vec![MaterialId(1)],
        )
        .unwrap();
        let floor = world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: grid.clone(),
            cell_m: 1.0,
            density_kg_m3: 1.0,
            mass_properties: None,
            translation_m: [0.0, 0.0, 0.0],
            linvel_m_s: [0.0; 3],
        });
        let dynamic = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::MergedCuboids,
            grid: grid.clone(),
            cell_m: 1.0,
            density_kg_m3: 1.0,
            mass_properties: None,
            translation_m: [0.0, 1.0, 0.0],
            linvel_m_s: [0.0; 3],
        });

        // Rebuild the floor's collider (a no-op geometry change) and sync
        // queries against it, exactly like `CharacterQueryCache::rebuild`
        // does for its window collider — before the dynamic body's own pair
        // has ever gone through a real `step`.
        world.rebuild_collider(floor, &grid, Representation::MergedCuboids);
        world.sync_queries();

        for _ in 0..60 {
            world.step();
        }
        let st = world.body_state(dynamic);
        assert!(
            st.translation_m[1] > 0.9,
            "a collider-replacement + sync_queries call must not stop the dynamic body's \
             pair from ever contacting — got y = {}",
            st.translation_m[1]
        );
        assert!(
            world.contact_pair_count() >= 1,
            "expected at least one live contact pair after settling, found none"
        );
    }

    /// T21 input surface: [`PhysicsWorld::contact_impulses`] must spike on a real
    /// impact and then fall back to roughly the resting weight-support impulse
    /// (`m·g·dt`) once the body settles — the separation the contact-damage rules
    /// threshold on so a resting body never keeps fracturing the floor.
    #[test]
    fn contact_impulses_spike_on_impact_then_decay_to_the_resting_load() {
        use crate::fixtures as phys_fx;

        let mut world = PhysicsWorld::new(PhysicsConfig {
            disable_ccd: true,
            ..PhysicsConfig::default()
        });

        let floor = phys_fx::floor_slab(VolumeId::new(1).unwrap(), 2, 2, 4);
        let floor_grid = crate::occupancy::OccupancyGrid::from_volume(&floor)
            .unwrap()
            .unwrap();
        let floor_id = world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: floor_grid.clone(),
            cell_m: phys_fx::CELL_M,
            density_kg_m3: phys_fx::STONE_DENSITY,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });

        let piece = phys_fx::debris_pieces(50, 1, 3).pop().unwrap().1;
        let grid = crate::occupancy::OccupancyGrid::from_volume(&piece)
            .unwrap()
            .unwrap();
        let body = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::MergedCuboids,
            grid,
            cell_m: phys_fx::CELL_M,
            density_kg_m3: phys_fx::STONE_DENSITY,
            mass_properties: None,
            translation_m: [4.0, 4.0, 4.0],
            linvel_m_s: [0.0; 3],
        });
        let mass = world.body_state(body).mass_kg;

        let mut peak_impact = 0.0_f32;
        let mut resting = 0.0_f32;
        for step in 0..500 {
            world.step();
            let contacts = world.contact_impulses();
            let pair_impulse: f32 = contacts
                .iter()
                .filter(|c| c.bodies.contains(&body))
                .map(|c| c.normal_impulse_n_s)
                .sum();
            if step < 200 {
                peak_impact = peak_impact.max(pair_impulse);
            } else {
                resting = pair_impulse;
            }
        }

        let st = world.body_state(body);
        assert!(
            st.speed_m_s() < 0.1,
            "body settled (speed {})",
            st.speed_m_s()
        );

        // Resting support impulse is on the order of m·g·dt; the impact was a
        // multiple of it (a real fall from ~2.5 m onto a rigid floor).
        let weight_impulse = mass * 9.81 * world.params.dt;
        assert!(
            resting > 0.0 && resting < 4.0 * weight_impulse,
            "settled contact impulse ({resting} N·s) is near the resting load \
             (m·g·dt = {weight_impulse} N·s)"
        );
        assert!(
            peak_impact > 8.0 * resting.max(weight_impulse),
            "impact impulse ({peak_impact} N·s) spikes well above the resting \
             load ({resting} N·s)"
        );

        // The settled pair is body-vs-terrain: exactly one side is dynamic.
        let contact = world
            .contact_impulses()
            .into_iter()
            .find(|c| c.bodies.contains(&body))
            .expect("a resting body keeps a tracked contact with the floor");
        assert_ne!(
            contact.dynamic[0], contact.dynamic[1],
            "a falling body resting on fixed terrain: one dynamic, one fixed"
        );
        assert!(
            contact.bodies.contains(&floor_id),
            "the other side of the contact is the floor"
        );
        assert!(
            (contact.point_m[1] - 1.0).abs() < 0.2,
            "contact point sits on the floor top (y = 1.0 m), got {}",
            contact.point_m[1]
        );
        assert!(
            (contact.point_m[0] - 4.375).abs() < 0.6 && (contact.point_m[2] - 4.375).abs() < 0.6,
            "contact point is under the body (~x/z 4.375 m), got {:?}",
            contact.point_m
        );
    }

    /// T21 dormancy: deactivating a settled body drops it out of the stepped
    /// set, and reactivating it into the same slot restores its pose so it
    /// resumes physics from where it was.
    #[test]
    fn a_dormant_body_leaves_the_step_set_and_reactivates_at_its_pose() {
        use crate::fixtures as phys_fx;

        let mut world = PhysicsWorld::new(PhysicsConfig {
            disable_ccd: true,
            ..PhysicsConfig::default()
        });
        let floor = phys_fx::floor_slab(VolumeId::new(1).unwrap(), 2, 2, 4);
        let floor_grid = crate::occupancy::OccupancyGrid::from_volume(&floor)
            .unwrap()
            .unwrap();
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: floor_grid,
            cell_m: phys_fx::CELL_M,
            density_kg_m3: phys_fx::STONE_DENSITY,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        let piece = phys_fx::debris_pieces(50, 1, 3).pop().unwrap().1;
        let grid = crate::occupancy::OccupancyGrid::from_volume(&piece)
            .unwrap()
            .unwrap();
        let body = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::MergedCuboids,
            grid: grid.clone(),
            cell_m: phys_fx::CELL_M,
            density_kg_m3: phys_fx::STONE_DENSITY,
            mass_properties: None,
            translation_m: [4.0, 2.0, 4.0],
            linvel_m_s: [0.0; 3],
        });

        for _ in 0..400 {
            world.step();
        }
        let settled = world.body_state(body);
        assert!(settled.sleeping, "body settled to sleep");
        assert_eq!(world.active_body_count(), 2);

        world.deactivate_body(body);
        assert!(world.is_dormant(body));
        assert_eq!(world.active_body_count(), 1, "dormant body is not stepped");
        // Stepping the world for a while does nothing to the dormant body.
        for _ in 0..120 {
            world.step();
        }

        world.reactivate_body(
            body,
            &grid,
            settled.translation_m,
            settled.rotation,
            [0.0; 3],
            [0.0; 3],
        );
        assert!(!world.is_dormant(body));
        assert_eq!(world.active_body_count(), 2);
        let back = world.body_state(body);
        for axis in 0..3 {
            assert!(
                (back.translation_m[axis] - settled.translation_m[axis]).abs() < 1e-4,
                "reactivated at the frozen pose (axis {axis}: {} vs {})",
                back.translation_m[axis],
                settled.translation_m[axis]
            );
        }
        // It stays put on the floor after reactivation — no fall, no explosion.
        for _ in 0..200 {
            world.step();
        }
        let after = world.body_state(body);
        assert!(after.is_finite() && after.speed_m_s() < 0.1);
        assert!((after.translation_m[1] - settled.translation_m[1]).abs() < 0.05);
    }

    #[test]
    fn installed_fine_grid_properties_reach_the_solver() {
        let grid = mixed_density_grid();
        let analytic = analytic_mass_properties(&grid, CELL_M, mix_density);
        let mut world = gravity_free();
        let id = add_mixed_body(&mut world, Some(analytic.to_body_properties()));
        world.step();

        for (label, (mass, com, inertia)) in [
            ("adapter", world.derived_mass_properties(id)),
            ("live rapier body", world.live_body_mass_properties(id)),
        ] {
            let mass_rel = ((analytic.mass_kg - f64::from(mass)) / analytic.mass_kg).abs();
            assert!(
                mass_rel < 1e-4,
                "{label}: mass {mass} vs {}",
                analytic.mass_kg
            );

            // COM is pulled off the geometric centre (1.0 m) to x = 1.25 m.
            assert!(
                (f64::from(com[0]) - analytic.com_m[0]).abs() < 2e-4,
                "{label}: com_x {} vs {}",
                com[0],
                analytic.com_m[0]
            );
            assert!(
                com[0] > 1.10 && com[0] < 1.40,
                "{label}: com_x {} not shifted toward the dense block",
                com[0]
            );
            for (axis, &c) in com.iter().enumerate().skip(1) {
                assert!(
                    (f64::from(c) - analytic.com_m[axis]).abs() < 2e-4,
                    "{label}: com[{axis}] {c} vs {}",
                    analytic.com_m[axis]
                );
            }

            let mut got = [
                f64::from(inertia[0]),
                f64::from(inertia[1]),
                f64::from(inertia[2]),
            ];
            let mut want = analytic.principal_diagonal();
            got.sort_by(|a, b| a.partial_cmp(b).unwrap());
            want.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let scale = want.iter().cloned().fold(1e-9_f64, f64::max);
            for i in 0..3 {
                assert!(
                    (got[i] - want[i]).abs() / scale < 2e-3,
                    "{label}: principal inertia {got:?} vs {want:?}"
                );
            }
        }
    }

    #[test]
    fn uniform_density_body_keeps_the_geometric_centre() {
        // The pre-fix behaviour: no explicit properties -> Rapier's COM is the
        // geometric centre for any uniform density, so the mixed-material shift
        // is lost.
        let mut world = gravity_free();
        let id = add_mixed_body(&mut world, None);
        world.step();
        let (_, com, _) = world.derived_mass_properties(id);
        assert!(
            (com[0] - 1.0).abs() < 1e-4,
            "uniform density COM should stay at the geometric centre, got {}",
            com[0]
        );
    }

    #[test]
    fn torque_impulse_response_matches_the_analytic_inertia() {
        let grid = mixed_density_grid();
        let analytic = analytic_mass_properties(&grid, CELL_M, mix_density);
        let diag = analytic.principal_diagonal(); // tensor is diagonal for this fixture
        let mut world = gravity_free();
        let id = add_mixed_body(&mut world, Some(analytic.to_body_properties()));
        world.step();

        // A small torque impulse about each axis; free space, so Δω = I⁻¹·L.
        let l = [0.30_f32, 0.20, 0.25];
        world.apply_torque_impulse(id, l);
        world.step();
        let w = world.body_state(id).angvel_rad_s;
        for axis in 0..3 {
            let expected = f64::from(l[axis]) / diag[axis];
            let rel = ((f64::from(w[axis]) - expected) / expected).abs();
            assert!(
                rel < 5e-3,
                "axis {axis}: ω {} vs analytic I⁻¹·L {expected}",
                w[axis]
            );
        }
    }

    #[test]
    fn off_centre_impulse_uses_the_shifted_centre_of_mass() {
        let grid = mixed_density_grid();
        let analytic = analytic_mass_properties(&grid, CELL_M, mix_density);
        let diag = analytic.principal_diagonal();
        let mut world = gravity_free();
        let id = add_mixed_body(&mut world, Some(analytic.to_body_properties()));
        world.step();

        // Impulse along +z applied on the x = 0 face at the COM height/depth.
        // Torque about y is (r × J)_y with r = point − com = (−com_x, 0, 0):
        //   (r × J)_y = −( r_z·J_x − r_x·J_z ) = r_x·J_z  →  −com_x·J_z ... in
        // Rapier's left-to-right cross this resolves to ω_y ≈ com_x·J_z / I_yy.
        let jz = 0.4_f32;
        let point = [0.0_f32, analytic.com_m[1] as f32, analytic.com_m[2] as f32];
        world.apply_impulse_at_point(id, [0.0, 0.0, jz], point);
        world.step();
        let w = world.body_state(id).angvel_rad_s;

        let with_true_com = analytic.com_m[0] * f64::from(jz) / diag[1];
        let with_geom_com = 1.0 * f64::from(jz) / diag[1];
        let err_true = (f64::from(w[1]).abs() - with_true_com.abs()).abs();
        let err_geom = (f64::from(w[1]).abs() - with_geom_com.abs()).abs();
        assert!(
            err_true < 0.1 * with_true_com.abs(),
            "ω_y {} should track the shifted COM prediction {with_true_com}",
            w[1]
        );
        assert!(
            err_true < err_geom,
            "ω_y {} fits the true COM (1.25 m) better than the geometric centre (1.0 m)",
            w[1]
        );
    }
}

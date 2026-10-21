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

mod articulated;
mod articulated_body;
mod articulated_grid;
mod articulated_pose;
mod articulated_query;
mod character_sweep;
mod contacts;
mod corpse;
mod corpse_restore;
mod rigid;
#[cfg(test)]
mod tests;

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
    pub owner_key: Option<u128>,
    pub source_generation: u64,
    pub next_component_serial: u64,
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
    /// Reconstruct one live owner with applied poses and measured root/part motion.
    AliveRestore {
        operation_tag: u64,
        restore: crate::articulated::AliveArticulatedRestore,
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

impl PreparedArticulatedBatch {
    /// Returns the exact prepared motion and mass-conservation witness for a
    /// checked death transfer, before publication. Tags are unique within the
    /// batch; absent or non-transfer tags return `None`.
    pub fn transfer_outcome(&self, operation_tag: u64) -> Option<&CorpseTransferOutcome> {
        self.changes.iter().find_map(|change| match change {
            PreparedArticulatedChange::CheckedDeathTransfer {
                operation_tag: tag,
                transfer,
            } if *tag == operation_tag => Some(&transfer.outcome),
            _ => None,
        })
    }

    /// Number of new logical `BodyId` slots the batch will append. Each birth
    /// or cold restore creates one owner entry; checked death transfers reuse
    /// their existing owner slot.
    pub fn additional_body_slots(&self) -> usize {
        self.changes
            .iter()
            .filter(|change| {
                !matches!(
                    change,
                    PreparedArticulatedChange::CheckedDeathTransfer { .. }
                )
            })
            .count()
    }
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
    AliveRestore {
        operation_tag: u64,
        birth: PreparedArticulatedBirth,
        restore: PreparedAliveArticulatedMetadata,
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

struct PreparedAliveArticulatedMetadata {
    pose_token: u64,
    source_generation: u64,
    next_component_serial: u64,
    root_linear_velocity_m_s: [f32; 3],
    root_angular_velocity_rad_s: [f32; 3],
    part_motion: Vec<crate::articulated::AliveArticulatedPartMotion>,
    owner_key: u128,
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
    velocity_based_restore: bool,
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
#[cfg(any(test, feature = "contact-witnesses"))]
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
    /// Restored live owners temporarily use velocity-based kinematics so a
    /// checkpoint's measured part twists are observable before the next tick.
    /// They are returned to ordinary sampled-position control before their
    /// first physics integration; entries are bounded by recent restores.
    restored_velocity_based_owners: Vec<ArticulatedBodyId>,
    /// Logical fracture keys are never reusable during this world lifetime,
    /// including after their owner is retired.
    fracture_owner_keys: HashMap<u128, BodyId>,
    step_count: u64,
    /// Source-cell attribution of articulated contacts, for tests and
    /// evidence only: no game system reads it, and capturing it walks every
    /// contact manifold each substep (about 10 ms a tick with 40 animals).
    #[cfg(any(test, feature = "contact-witnesses"))]
    articulated_contact_witnesses: Vec<ArticulatedContactWitness>,
    #[cfg(any(test, feature = "contact-witnesses"))]
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
            restored_velocity_based_owners: Vec::new(),
            fracture_owner_keys: HashMap::new(),
            step_count: 0,
            #[cfg(any(test, feature = "contact-witnesses"))]
            articulated_contact_witnesses: Vec::new(),
            #[cfg(any(test, feature = "contact-witnesses"))]
            contact_witness_overflow: 0,
            pending_modified: Vec::new(),
            pending_removed: Vec::new(),
        }
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
        for id in std::mem::take(&mut self.restored_velocity_based_owners) {
            let Some(assembly) = self.articulated.get(&id.0) else {
                continue;
            };
            if !assembly.velocity_based_restore {
                continue;
            }
            let root_body = self.entries[entry_index(id.0, self.id_namespace)].body;
            self.bodies[root_body].set_body_type(RigidBodyType::KinematicPositionBased, true);
            for part in &assembly.parts {
                self.bodies[part.body].set_body_type(RigidBodyType::KinematicPositionBased, true);
            }
            self.articulated
                .get_mut(&id.0)
                .expect("restore owner remained present before its first step")
                .velocity_based_restore = false;
        }
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
        #[allow(unused_mut)]
        let mut contact_witness_capture = Duration::ZERO;
        let mut normal_impulse_n_s = 0.0_f32;
        let mut impulse_pair_substeps = 0;
        #[cfg(any(test, feature = "contact-witnesses"))]
        {
            self.articulated_contact_witnesses.clear();
            self.contact_witness_overflow = 0;
        }
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
            #[cfg(any(test, feature = "contact-witnesses"))]
            {
                let capture_start = Instant::now();
                self.capture_articulated_contact_witnesses(substep, substeps);
                contact_witness_capture += capture_start.elapsed();
            }
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

    /// Number of bodies.
    pub fn body_count(&self) -> usize {
        self.entries.len()
    }

    /// Number of steps run so far.
    pub fn steps_run(&self) -> u64 {
        self.step_count
    }

    /// The configured gravity vector, m/s². Callers converting contact impulses
    /// into damage (T21) use its magnitude for the `m·g·dt` resting-load
    /// reference.
    pub fn gravity_m_s2(&self) -> [f32; 3] {
        [self.gravity.x, self.gravity.y, self.gravity.z]
    }
}

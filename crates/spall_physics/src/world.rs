//! The narrow Rapier adapter: a fixed-step physics world that speaks in engine
//! terms only. Rapier handles never leave this module; callers address bodies
//! by an opaque [`BodyId`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rapier3d::control::CharacterCollision;
use rapier3d::prelude::*;

use crate::articulated::{
    ArticulatedBodyId, ArticulatedPose, MAX_ARTICULATED_PARTS, PartTransform, SourceCellHit,
    SourcePart, compose, normalize, rotate,
};
use crate::collider::{Representation, build_collider};
use crate::mass::BodyMassProperties;
use crate::occupancy::OccupancyGrid;

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

/// Per-step timing.
#[derive(Debug, Clone, Copy)]
pub struct StepTiming {
    /// 1-based index of the step just run.
    pub step_index: u64,
    /// Wall-clock time inside `PhysicsPipeline::step`.
    pub pipeline: Duration,
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

struct ArticulatedPartEntry {
    part_id: u32,
    parent_id: Option<u32>,
    volume_id: spall_core::VolumeId,
    source_revision: u64,
    grid: OccupancyGrid,
    cell_m: f32,
    local_pose: PartTransform,
    assembly_pose: PartTransform,
    collider: ColliderHandle,
    primitives: usize,
}

struct ArticulatedEntry {
    topology_token: u64,
    pose_token: u64,
    pending_root_pose_token: Option<u64>,
    parts: Vec<ArticulatedPartEntry>,
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
    id_namespace: u32,
    gravity: Vector,
    params: IntegrationParameters,
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
    step_count: u64,
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
        let mut params = IntegrationParameters {
            dt: cfg.dt_s,
            ..Default::default()
        };
        params.num_solver_iterations = cfg.solver_iterations.max(1);
        if cfg.disable_ccd {
            params.max_ccd_substeps = 0;
        }
        Self {
            id_namespace,
            gravity: Vector::new(
                cfg.gravity_m_s2[0],
                cfg.gravity_m_s2[1],
                cfg.gravity_m_s2[2],
            ),
            params,
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
            step_count: 0,
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
            .flat_map(|assembly| assembly.parts.iter().map(|part| part.volume_id))
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
        let root = self.bodies.insert(
            RigidBodyBuilder::kinematic_position_based()
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
            let local_rotation = Rotation::from_xyzw(
                local_rotation_xyzw[0],
                local_rotation_xyzw[1],
                local_rotation_xyzw[2],
                local_rotation_xyzw[3],
            );
            let collider = ColliderBuilder::new(build.collider.shared_shape().clone())
                .density(0.0)
                .position(Pose::new(
                    Vector::new(
                        local_translation[0],
                        local_translation[1],
                        local_translation[2],
                    ),
                    local_rotation.to_scaled_axis(),
                ))
                .build();
            let handle = self
                .colliders
                .insert_with_parent(collider, root, &mut self.bodies);
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
                collider: handle,
                primitives: build.primitives,
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
            mass_properties: None,
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
                pending_root_pose_token: None,
                parts: built_parts,
            },
        );
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
        if update.topology_token != current.topology_token
            || current.pending_root_pose_token.is_some()
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
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_SOLID_CELLS,
        };
        let id = id.0;
        let assembly = self
            .articulated
            .get(&id)
            .ok_or(ArticulatedError::Hierarchy)?;
        let part_index = assembly
            .parts
            .iter()
            .position(|part| part.part_id == part_id)
            .ok_or(ArticulatedError::StaleTopology)?;
        let part = &assembly.parts[part_index];
        if assembly.topology_token != expected_topology_token
            || assembly.pending_root_pose_token.is_some()
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
        let (body, query_only, restitution) = (entry.body, entry.query_only, entry.restitution);
        let mut collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(0.0)
            .position(Pose::new(
                Vector::new(translation[0], translation[1], translation[2]),
                rotation.to_scaled_axis(),
            ))
            .build();
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
        part.collider = new_handle;
        part.primitives = built.primitives;
        assembly.topology_token = new_topology_token;
        if part_index == 0 {
            self.entries[entry_index(id, self.id_namespace)].collider = new_handle;
        }
        Ok(start.elapsed())
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
        if assembly.topology_token != topology_token
            || assembly.pose_token != base_pose_token
            || pose_token <= base_pose_token
        {
            return Err(ArticulatedError::StalePose);
        }
        if assembly.pending_root_pose_token.is_some() {
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
        if assembly.topology_token != topology_token
            || assembly.pose_token != base_pose_token
            || pose_token <= base_pose_token
        {
            return Err(ArticulatedError::StalePose);
        }
        if assembly.pending_root_pose_token.is_some() {
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
        self.bodies[body].set_next_kinematic_position(Pose::new(
            Vector::new(translation_m[0], translation_m[1], translation_m[2]),
            Rotation::from_xyzw(
                rotation.rotation_xyzw[0],
                rotation.rotation_xyzw[1],
                rotation.rotation_xyzw[2],
                rotation.rotation_xyzw[3],
            )
            .to_scaled_axis(),
        ));
        self.articulated
            .get_mut(&body_id)
            .expect("validated assembly exists")
            .pending_root_pose_token = Some(pose_token);
        Ok(())
    }

    /// Retires the kinematic root and all of its attached source-part colliders.
    pub fn retire_articulated_body(&mut self, id: ArticulatedBodyId) {
        self.retire_body(id.0);
    }

    /// Number of exact source-part colliders installed for an assembly.
    pub fn articulated_collider_count(&self, id: ArticulatedBodyId) -> usize {
        self.articulated
            .get(&id.0)
            .map_or(0, |assembly| assembly.parts.len())
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
        let mut best: Option<(f32, u32, SourceCellHit)> = None;
        for part in &assembly.parts {
            let Some(toi) = hit_tois.get(&part.collider).copied() else {
                continue;
            };
            let collider = self
                .colliders
                .get(part.collider)
                .ok_or(ArticulatedError::Miss)?;
            let local_pose = *collider
                .position_wrt_parent()
                .ok_or(ArticulatedError::Miss)?;
            let collider_pose = root_pose * local_pose;
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
            if best.is_none_or(|(old_toi, old_part, _)| {
                toi < old_toi - 1.0e-5
                    || ((toi - old_toi).abs() <= 1.0e-5 && part.part_id < old_part)
            }) {
                best = Some((toi, part.part_id, hit));
            }
        }
        best.map(|(_, _, hit)| hit).ok_or(ArticulatedError::Miss)
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
        let pipeline = start.elapsed();
        self.step_count += 1;
        for assembly in self.articulated.values_mut() {
            if let Some(pose_token) = assembly.pending_root_pose_token.take() {
                assembly.pose_token = pose_token;
            }
        }
        StepTiming {
            step_index: self.step_count,
            pipeline,
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

    /// Kinematic snapshot of a body. A retired body (its volume became empty) or
    /// a dormant one (T21, deactivated pending reactivation) has no Rapier body
    /// left; it reports an all-zero, non-sleeping state and the caller is
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

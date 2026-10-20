//! The narrow Rapier adapter: a fixed-step physics world that speaks in engine
//! terms only. Rapier handles never leave this module; callers address bodies
//! by an opaque [`BodyId`].

use std::time::{Duration, Instant};

use rapier3d::control::CharacterCollision;
use rapier3d::prelude::*;

use crate::collider::{Representation, build_collider};
use crate::mass::BodyMassProperties;
use crate::occupancy::OccupancyGrid;

mod character_sweep;
mod contacts;
mod rigid;
#[cfg(test)]
mod tests;

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
    /// kinematics are interpolated across these substeps.
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

/// A body fence named a body this world does not own, or named one twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("body fence names an unknown or duplicated body")]
pub struct BodyFenceError;

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

/// Per-step timing.
#[derive(Debug, Clone, Copy)]
pub struct StepTiming {
    /// 1-based index of the step just run.
    pub step_index: u64,
    /// Wall-clock time inside `PhysicsPipeline::step`.
    pub pipeline: Duration,
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
            step_count: 0,
            pending_modified: Vec::new(),
            pending_removed: Vec::new(),
        }
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
        let mut normal_impulse_n_s = 0.0_f32;
        let mut impulse_pair_substeps = 0;
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
            for pair in self.narrow_phase.contact_pairs() {
                let impulse = pair.total_impulse_magnitude();
                if pair.has_any_active_contact() && impulse.is_finite() && impulse > 0.0 {
                    normal_impulse_n_s += impulse;
                    impulse_pair_substeps += 1;
                }
            }
        }
        self.params.dt = outer_dt;
        self.step_count += 1;
        StepTiming {
            step_index: self.step_count,
            pipeline,
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
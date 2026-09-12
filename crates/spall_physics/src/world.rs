//! The narrow Rapier adapter: a fixed-step physics world that speaks in engine
//! terms only. Rapier handles never leave this module; callers address bodies
//! by an opaque [`BodyId`].

use std::time::{Duration, Instant};

use rapier3d::control::CharacterCollision;
use rapier3d::prelude::*;

use crate::collider::{Representation, build_collider};
use crate::mass::BodyMassProperties;
use crate::occupancy::OccupancyGrid;

/// Opaque, stable identifier for a body in a [`PhysicsWorld`]. Never a Rapier
/// handle; safe to store outside the adapter for the life of the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BodyId(u32);

impl BodyId {
    /// The raw index, for report keys only.
    pub fn index(self) -> u32 {
        self.0
    }
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
    step_count: u64,
    /// Collider handles added/rebuilt/removed since the last [`Self::step`]
    /// or [`Self::sync_queries`] — see [`Self::sync_queries`]'s doc for why
    /// this exists alongside `step`, not instead of it.
    pending_modified: Vec<ColliderHandle>,
    pending_removed: Vec<ColliderHandle>,
}

impl PhysicsWorld {
    /// Creates an empty world.
    pub fn new(cfg: PhysicsConfig) -> Self {
        let mut params = IntegrationParameters {
            dt: cfg.dt_s,
            ..Default::default()
        };
        params.num_solver_iterations = cfg.solver_iterations.max(1);
        if cfg.disable_ccd {
            params.max_ccd_substeps = 0;
        }
        Self {
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
            step_count: 0,
            pending_modified: Vec::new(),
            pending_removed: Vec::new(),
        }
    }

    /// Builds a Rapier rigid body + attached collider from `spec` and inserts
    /// both, returning their handles and the grid-origin collider offset. Shared
    /// by [`Self::add_body`] and [`Self::reactivate_body`].
    fn insert_rapier_body(
        &mut self,
        spec: &BodySpec,
    ) -> (RigidBodyHandle, ColliderHandle, [f32; 3]) {
        let rb = match spec.kind {
            BodyKind::Fixed => RigidBodyBuilder::fixed(),
            BodyKind::Dynamic { ccd } => RigidBodyBuilder::dynamic()
                .linvel(Vector::new(
                    spec.linvel_m_s[0],
                    spec.linvel_m_s[1],
                    spec.linvel_m_s[2],
                ))
                .ccd_enabled(ccd),
        }
        .translation(Vector::new(
            spec.translation_m[0],
            spec.translation_m[1],
            spec.translation_m[2],
        ))
        .build();
        let body = self.bodies.insert(rb);

        let built = build_collider(&spec.grid, spec.cell_m, spec.representation);
        let offset = grid_origin_offset_m(&spec.grid, spec.cell_m);
        // With explicit mass properties the collision shape must not contribute
        // mass: the body carries the fine-grid mass / COM / inertia directly.
        let collider_density = if spec.mass_properties.is_some() {
            0.0
        } else {
            spec.density_kg_m3
        };
        let collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(collider_density)
            .translation(Vector::new(offset[0], offset[1], offset[2]))
            .build();
        let collider = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);

        if let Some(props) = spec.mass_properties {
            let rb = &mut self.bodies[body];
            rb.set_additional_mass_properties(rapier_mass_properties(props, offset), false);
            rb.recompute_mass_properties_from_colliders(&self.colliders);
        }
        self.pending_modified.push(collider);
        (body, collider, offset)
    }

    /// Adds a body and returns its stable id.
    pub fn add_body(&mut self, spec: BodySpec) -> BodyId {
        let (body, collider, offset) = self.insert_rapier_body(&spec);

        let id = BodyId(self.entries.len() as u32);
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
        });
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
        let entry = &mut self.entries[id.0 as usize];
        debug_assert!(!entry.retired, "rebuild_collider on a retired body");
        if entry.retired {
            return Duration::ZERO;
        }
        let (cell_m, density, body, old_collider, mass_properties, query_only) = (
            entry.cell_m,
            entry.density,
            entry.body,
            entry.collider,
            entry.mass_properties,
            entry.query_only,
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
        let handle = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);
        if mass_properties.is_some() {
            self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
        }
        let total = start.elapsed();

        self.entries[id.0 as usize].collider = handle;
        self.entries[id.0 as usize].representation = rep;
        self.entries[id.0 as usize].collider_offset_m = offset;
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
        let entry = &mut self.entries[id.0 as usize];
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

    /// Retires a body whose authoritative volume became empty (`ENG-56`): its
    /// Rapier rigid body and attached collider are removed from the simulation
    /// so it can no longer collide with, rest on, or be swept against by any
    /// other body, nor be stepped or queried. The [`BodyId`] slot is kept (ids
    /// are indices — removing one would shift every later id) and marked
    /// retired; callers must drop the handle. Idempotent.
    pub fn retire_body(&mut self, id: BodyId) {
        let entry = &mut self.entries[id.0 as usize];
        if entry.retired {
            return;
        }
        entry.retired = true;
        let body = entry.body;
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
        self.entries[id.0 as usize].retired
    }

    /// Deactivates a **dormant** body (T21): removes its Rapier rigid body and
    /// collider so it costs nothing to step and cannot be contacted or swept,
    /// but keeps its [`BodyId`] slot and every rebuild parameter
    /// (`cell_m` / representation / density / mass properties) so
    /// [`Self::reactivate_body`] can restore it. The caller owns the frozen
    /// pose / velocity and passes them back on reactivation. Idempotent; a no-op
    /// on a retired body.
    pub fn deactivate_body(&mut self, id: BodyId) {
        let entry = &mut self.entries[id.0 as usize];
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
        let entry = &self.entries[id.0 as usize];
        if entry.retired || !entry.dormant {
            return;
        }
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
        let entry = &mut self.entries[id.0 as usize];
        entry.body = body;
        entry.collider = collider;
        entry.collider_offset_m = offset;
        entry.dormant = false;
        self.set_body_pose(id, translation_m, rotation);
        self.set_body_velocity(id, linvel_m_s, angvel_rad_s);
    }

    /// Whether `id` is currently dormant (deactivated by [`Self::deactivate_body`]).
    pub fn is_dormant(&self, id: BodyId) -> bool {
        self.entries[id.0 as usize].dormant
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
        let entry = &mut self.entries[id.0 as usize];
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
        let entry = &mut self.entries[id.0 as usize];
        if entry.retired {
            return;
        }
        entry.query_only = true;
        let collider = entry.collider;
        if let Some(c) = self.colliders.get_mut(collider) {
            c.set_solver_groups(query_only_solver_groups());
        }
    }

    /// Refreshes the broad-phase for every collider added, rebuilt, or
    /// removed since the last call to this or [`Self::step`] — **without**
    /// stepping the dynamics pipeline: no gravity or velocity integration, no
    /// contact resolution, for *any* body in this world, including ones
    /// wholly unrelated to what changed. [`Self::step`] already refreshes the
    /// broad-phase as part of its own full pipeline step, so a caller that
    /// steps this world's simulation every tick regardless (the normal case)
    /// never needs this. It exists for a caller that adds/rebuilds/removes
    /// colliders on a world it does **not** own the tick loop for —
    /// [`CharacterQueryCache`](crate::query_cache::CharacterQueryCache),
    /// which must never advance a simulation another caller is driving.
    pub fn sync_queries(&mut self) {
        if self.pending_modified.is_empty() && self.pending_removed.is_empty() {
            return;
        }
        let mut events = Vec::new();
        self.broad_phase.update(
            &self.params,
            &self.colliders,
            &self.bodies,
            &self.pending_modified,
            &self.pending_removed,
            &mut events,
        );
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
        let handles: Vec<ColliderHandle> = exclude
            .iter()
            .filter(|id| !self.entries[id.0 as usize].retired)
            .map(|id| self.entries[id.0 as usize].collider)
            .collect();
        self.sweep_character_impl(
            params,
            position_m,
            desired_translation_m,
            dt_s,
            &handles,
            |_| {},
        )
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
        self.entries[id.0 as usize].representation
    }

    /// Kinematic snapshot of a body. A retired body (its volume became empty) or
    /// a dormant one (T21, deactivated pending reactivation) has no Rapier body
    /// left; it reports an all-zero, non-sleeping state and the caller is
    /// expected to hold the authoritative pose itself.
    pub fn body_state(&self, id: BodyId) -> BodyState {
        let entry = &self.entries[id.0 as usize];
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
            .map(|i| BodyId(i as u32))
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
            let rb1 = &self.bodies[rb1];
            let rb2 = &self.bodies[rb2];
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
                    self.bodies[self.entries[b1.0 as usize].body].is_dynamic(),
                    self.bodies[self.entries[b2.0 as usize].body].is_dynamic(),
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
        let rb = &mut self.bodies[self.entries[id.0 as usize].body];
        rb.set_translation(
            Vector::new(translation_m[0], translation_m[1], translation_m[2]),
            true,
        );
        rb.set_rotation(
            Rotation::from_xyzw(rotation[0], rotation[1], rotation[2], rotation[3]),
            true,
        );
    }

    /// Applies a linear impulse (N·s) to a body at its centre of mass and wakes
    /// it if it was asleep. Models a documented "nearby interaction" — a blast
    /// impulse or an impact from an adjacent edit — for the sleep/wake
    /// feasibility scenario. No-op for `Fixed` bodies.
    pub fn apply_impulse(&mut self, id: BodyId, impulse_n_s: [f32; 3]) {
        let rb = &mut self.bodies[self.entries[id.0 as usize].body];
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
        let rb = &mut self.bodies[self.entries[id.0 as usize].body];
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
        let rb = &mut self.bodies[self.entries[id.0 as usize].body];
        rb.apply_impulse_at_point(
            Vector::new(impulse_n_s[0], impulse_n_s[1], impulse_n_s[2]),
            Vector::new(point_world_m[0], point_world_m[1], point_world_m[2]),
            true,
        );
    }

    /// Sets a body's linear and angular velocity, m/s and rad/s. Used to hand a
    /// split child its inherited velocity.
    pub fn set_body_velocity(&mut self, id: BodyId, linvel_m_s: [f32; 3], angvel_rad_s: [f32; 3]) {
        let rb = &mut self.bodies[self.entries[id.0 as usize].body];
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
        self.entries[id.0 as usize].collider_offset_m
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
        let entry = &self.entries[id.0 as usize];
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
        let rb = &self.bodies[self.entries[id.0 as usize].body];
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

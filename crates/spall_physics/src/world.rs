//! The narrow Rapier adapter: a fixed-step physics world that speaks in engine
//! terms only. Rapier handles never leave this module; callers address bodies
//! by an opaque [`BodyId`].

use std::time::{Duration, Instant};

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
}

impl Default for PhysicsConfig {
    fn default() -> Self {
        Self {
            gravity_m_s2: [0.0, -9.81, 0.0],
            dt_s: 1.0 / 60.0,
            solver_iterations: 4,
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
}

impl PhysicsWorld {
    /// Creates an empty world.
    pub fn new(cfg: PhysicsConfig) -> Self {
        let mut params = IntegrationParameters {
            dt: cfg.dt_s,
            ..Default::default()
        };
        params.num_solver_iterations = cfg.solver_iterations.max(1);
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
        }
    }

    /// Adds a body and returns its stable id.
    pub fn add_body(&mut self, spec: BodySpec) -> BodyId {
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

        let id = BodyId(self.entries.len() as u32);
        self.entries.push(Entry {
            body,
            collider,
            cell_m: spec.cell_m,
            representation: spec.representation,
            density: spec.density_kg_m3,
            collider_offset_m: offset,
            mass_properties: spec.mass_properties,
        });
        id
    }

    /// Swaps a body's collider for one rebuilt from `grid` in `rep` (an edit).
    /// Returns the total rebuild cost: shape construction plus reinsertion.
    pub fn rebuild_collider(
        &mut self,
        id: BodyId,
        grid: &OccupancyGrid,
        rep: Representation,
    ) -> Duration {
        let entry = &mut self.entries[id.0 as usize];
        let (cell_m, density, body, mass_properties) = (
            entry.cell_m,
            entry.density,
            entry.body,
            entry.mass_properties,
        );
        self.colliders
            .remove(entry.collider, &mut self.islands, &mut self.bodies, true);

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
        let start = Instant::now();
        let collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(collider_density)
            .translation(Vector::new(offset[0], offset[1], offset[2]))
            .build();
        let handle = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);
        if mass_properties.is_some() {
            self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
        }
        let insert = start.elapsed();

        self.entries[id.0 as usize].collider = handle;
        self.entries[id.0 as usize].representation = rep;
        self.entries[id.0 as usize].collider_offset_m = offset;
        built.build + insert
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

    /// Advances the world by one fixed step.
    pub fn step(&mut self) -> StepTiming {
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

    /// Kinematic snapshot of a body.
    pub fn body_state(&self, id: BodyId) -> BodyState {
        let entry = &self.entries[id.0 as usize];
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

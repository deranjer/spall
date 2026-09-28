//! The narrow Rapier adapter: a fixed-step physics world that speaks in engine
//! terms only. Rapier handles never leave this module; callers address bodies
//! by an opaque [`BodyId`].

use std::time::{Duration, Instant};

use rapier3d::prelude::*;

use crate::collider::{Representation, build_collider};
use crate::mass::MassProperties as FineMassProps;
use crate::occupancy::OccupancyGrid;

/// Converts the analytic fine-grid [`FineMassProps`] (grid-local metre frame,
/// full inertia tensor about the COM) into a Rapier [`MassProperties`]. The
/// grid-local frame is the body-local frame (the collider shape is placed with
/// grid cell `(0,0,0)`'s corner at the body origin), so COM and inertia carry
/// over unchanged. Rapier diagonalises the tensor internally to recover the
/// principal moments and their frame, so a mixed-density body with a genuinely
/// rotated principal frame is represented faithfully.
fn to_rapier_mass_props(mp: &FineMassProps) -> MassProperties {
    let com = Vector::new(
        mp.com_m[0] as f32,
        mp.com_m[1] as f32,
        mp.com_m[2] as f32,
    );
    let t = &mp.inertia_com;
    let inertia = Matrix::from_cols_array_2d(&[
        [t[0][0] as f32, t[0][1] as f32, t[0][2] as f32],
        [t[1][0] as f32, t[1][1] as f32, t[1][2] as f32],
        [t[2][0] as f32, t[2][1] as f32, t[2][2] as f32],
    ]);
    MassProperties::with_inertia_matrix(com, mp.mass_kg as f32, inertia)
}

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
    /// Uniform collider density, kg/m³. Used only when `mass_properties` is
    /// `None`: mass, centre of mass and inertia are then derived from the
    /// collision shape at this density.
    pub density_kg_m3: f32,
    /// Rigid mass, centre of mass and full inertia derived from the **fine
    /// material grid**, in the body-local (grid) metre frame. When `Some`, these
    /// are installed on the rigid body verbatim and the collision shape
    /// contributes no mass — so a mixed-density or coarsened-collider body has
    /// solver mass/COM/inertia that match the authoritative material calc rather
    /// than the collision geometry. When `None`, `density_kg_m3` and the shape
    /// are used (single-material bodies, fixed terrain).
    pub mass_properties: Option<FineMassProps>,
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
    /// Fine-grid mass properties installed on this body, if any. While `Some`,
    /// the collider is always built at zero density and these values are the
    /// body's authoritative mass/COM/inertia — preserved across every collider
    /// rebuild and re-derived by the caller after a split.
    mass_props: Option<FineMassProps>,
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

        // With fine-grid mass properties the shape must not contribute mass:
        // build the collider at zero density and install the analytic values on
        // the rigid body directly.
        let collider_density = if spec.mass_properties.is_some() {
            0.0
        } else {
            spec.density_kg_m3
        };
        let built = build_collider(&spec.grid, spec.cell_m, spec.representation);
        let collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(collider_density)
            .build();
        let collider = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);

        if let Some(mp) = &spec.mass_properties {
            let rapier_mp = to_rapier_mass_props(mp);
            let rb = &mut self.bodies[body];
            rb.set_additional_mass_properties(rapier_mp, false);
            rb.recompute_mass_properties_from_colliders(&self.colliders);
        }

        let id = BodyId(self.entries.len() as u32);
        self.entries.push(Entry {
            body,
            collider,
            cell_m: spec.cell_m,
            representation: spec.representation,
            density: spec.density_kg_m3,
            mass_props: spec.mass_properties,
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
        let entry = &self.entries[id.0 as usize];
        let (cell_m, body, old_collider) = (entry.cell_m, entry.body, entry.collider);
        // Preserve installed fine-grid mass properties across the rebuild: the
        // new collider is built at the same (zero) density and the analytic
        // values are re-asserted on the body afterwards.
        let (collider_density, mass_props) = match &entry.mass_props {
            Some(mp) => (0.0, Some(to_rapier_mass_props(mp))),
            None => (entry.density, None),
        };

        let start = Instant::now();
        self.colliders
            .remove(old_collider, &mut self.islands, &mut self.bodies, true);
        let built = build_collider(grid, cell_m, rep);
        let collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(collider_density)
            .build();
        let handle = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);
        if let Some(rapier_mp) = mass_props {
            let rb = &mut self.bodies[body];
            rb.set_additional_mass_properties(rapier_mp, false);
            rb.recompute_mass_properties_from_colliders(&self.colliders);
        }
        let total = start.elapsed();

        self.entries[id.0 as usize].collider = handle;
        self.entries[id.0 as usize].representation = rep;
        total
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
        let rb = &self.bodies[self.entries[id.0 as usize].body];
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
            mass_kg: rb.mass(),
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

    /// Replaces the fine-grid mass properties installed on a body — used after a
    /// split or an edit changes a body's geometry, so the solver's mass, COM and
    /// inertia keep matching the authoritative material calc. The body must have
    /// been created with `BodySpec::mass_properties: Some(..)`; on a body without
    /// installed properties this is a no-op.
    pub fn set_mass_properties(&mut self, id: BodyId, mp: FineMassProps) {
        let entry = &mut self.entries[id.0 as usize];
        if entry.mass_props.is_none() {
            return;
        }
        entry.mass_props = Some(mp);
        let body = entry.body;
        let rapier_mp = to_rapier_mass_props(&mp);
        let rb = &mut self.bodies[body];
        rb.set_additional_mass_properties(rapier_mp, false);
        rb.recompute_mass_properties_from_colliders(&self.colliders);
    }

    /// The body's effective mass properties: `(mass_kg, local centre of mass in
    /// metres, principal inertia)`. When fine-grid properties are installed
    /// (`BodySpec::mass_properties`), these are the analytic values actually
    /// driving the solver; otherwise they are derived from the collision shape
    /// at the body's uniform density.
    pub fn derived_mass_properties(&self, id: BodyId) -> (f32, [f32; 3], [f32; 3]) {
        let entry = &self.entries[id.0 as usize];
        let mp = match &entry.mass_props {
            Some(fine) => to_rapier_mass_props(fine),
            None => {
                let shape = self.colliders[entry.collider].shared_shape().clone();
                shape.mass_properties(entry.density)
            }
        };
        let com = mp.local_com;
        let inertia = mp.principal_inertia();
        (
            mp.mass(),
            [com.x, com.y, com.z],
            [inertia.x, inertia.y, inertia.z],
        )
    }
}

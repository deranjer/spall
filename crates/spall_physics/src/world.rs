//! The narrow Rapier adapter: a fixed-step physics world that speaks in engine
//! terms only. Rapier handles never leave this module; callers address bodies
//! by an opaque [`BodyId`].

use std::time::{Duration, Instant};

use rapier3d::prelude::*;

use crate::collider::{Representation, build_collider};
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
    /// Uniform collider density, kg/m³ (mass is derived from the shape).
    pub density_kg_m3: f32,
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
    /// Body-local translation applied to the collider shape so its grid cell
    /// `(0, 0, 0)` sits at `OccupancyGrid::origin() * cell_m` instead of at the
    /// body-local origin (`ENG-55`). Refreshed on every `rebuild_collider`.
    collider_offset_m: [f32; 3],
}

/// The body-local offset that places a tight [`OccupancyGrid`]'s cell `(0, 0, 0)`
/// corner at `origin * cell_m` — so the collider shape lands on the volume's
/// authoritative cells, not near the body-local origin.
fn collider_grid_offset_m(grid: &OccupancyGrid, cell_m: f32) -> [f32; 3] {
    let o = grid.origin();
    [
        o.x as f32 * cell_m,
        o.y as f32 * cell_m,
        o.z as f32 * cell_m,
    ]
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

        let offset = collider_grid_offset_m(&spec.grid, spec.cell_m);
        let built = build_collider(&spec.grid, spec.cell_m, spec.representation);
        let collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(spec.density_kg_m3)
            .translation(Vector::new(offset[0], offset[1], offset[2]))
            .build();
        let collider = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);

        let id = BodyId(self.entries.len() as u32);
        self.entries.push(Entry {
            body,
            collider,
            cell_m: spec.cell_m,
            representation: spec.representation,
            density: spec.density_kg_m3,
            collider_offset_m: offset,
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
        let (cell_m, density, body) = (entry.cell_m, entry.density, entry.body);
        self.colliders
            .remove(entry.collider, &mut self.islands, &mut self.bodies, true);

        // A rebuild after an edit can shift the tight grid's origin, so the
        // body-local collider offset is recomputed here too (`ENG-55`).
        let offset = collider_grid_offset_m(grid, cell_m);
        let built = build_collider(grid, cell_m, rep);
        let start = Instant::now();
        let collider = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(density)
            .translation(Vector::new(offset[0], offset[1], offset[2]))
            .build();
        let handle = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);
        let insert = start.elapsed();

        self.entries[id.0 as usize].collider = handle;
        self.entries[id.0 as usize].representation = rep;
        self.entries[id.0 as usize].collider_offset_m = offset;
        built.build + insert
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

    /// Mass properties Rapier derived for a body's collider, in the collider
    /// shape's own frame (grid cell `(0, 0, 0)` corner at the origin):
    /// `(mass_kg, centre of mass in metres, principal inertia diagonal)`. This
    /// is the frame [`crate::analytic_mass_properties`] uses, so the two are
    /// directly comparable. Use [`Self::body_local_com`] for the centre of mass
    /// in the *body* frame (with the grid-origin collider offset applied).
    pub fn derived_mass_properties(&self, id: BodyId) -> (f32, [f32; 3], [f32; 3]) {
        let entry = &self.entries[id.0 as usize];
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

    /// Centre of mass in the body-local frame, metres: the collider shape's own
    /// centre of mass plus the grid-origin collider offset (`ENG-55`). This is
    /// the quantity the authoritative sim needs to place a body's world COM.
    pub fn body_local_com(&self, id: BodyId) -> [f32; 3] {
        let entry = &self.entries[id.0 as usize];
        let shape = self.colliders[entry.collider].shared_shape().clone();
        let com = shape.mass_properties(entry.density).local_com;
        [
            com.x + entry.collider_offset_m[0],
            com.y + entry.collider_offset_m[1],
            com.z + entry.collider_offset_m[2],
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
    use spall_voxel::{EditPlan, Volume};

    fn vid(n: u64) -> VolumeId {
        VolumeId::new(n).unwrap()
    }

    fn box_grid(min: GlobalCell, max: GlobalCell) -> OccupancyGrid {
        let mut v = Volume::new(vid(1), CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(v.id(), min, max, MaterialId(1)))
            .unwrap();
        OccupancyGrid::from_volume(&v).unwrap().unwrap()
    }

    /// `ENG-55`: a body whose tight occupancy starts at `(8, 8, 1)` — not local
    /// cell zero — must collide on those cells, not near the body-local origin.
    /// A floor placed under the body's *authoritative* cells catches it; without
    /// the grid-origin collider offset the shape sits ~2 m lower and tunnels
    /// straight through.
    #[test]
    fn a_collider_lands_on_its_grid_origin_cells_not_the_body_local_origin() {
        let cell_m = 0.25_f32;
        let mut world = PhysicsWorld::new(PhysicsConfig::default());

        // Floor: cells x 6..13, y 0..1, z 0..3 -> top surface at y = 0.5 m,
        // directly beneath the beam's authoritative x/z range.
        let floor = box_grid(GlobalCell::new(6, 0, 0), GlobalCell::new(13, 1, 3));
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: floor,
            cell_m,
            density_kg_m3: 2600.0,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });

        // Beam: cells x 8..11, y 8..9, z 1..2 -> grid origin (8, 8, 1); bottom
        // face at y = 2.0 m, 1.5 m above the floor.
        let beam = box_grid(GlobalCell::new(8, 8, 1), GlobalCell::new(11, 9, 2));
        let id = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::MergedCuboids,
            grid: beam,
            cell_m,
            density_kg_m3: 2600.0,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });

        // Body-local COM sits on the authoritative cells: cell-centre means
        // x 10.0, y 9.0, z 2.0 -> (2.5, 2.25, 0.5) m.
        let com = world.body_local_com(id);
        assert!(
            (com[0] - 2.5).abs() < 0.05
                && (com[1] - 2.25).abs() < 0.05
                && (com[2] - 0.5).abs() < 0.05,
            "body-local COM {com:?} should be on the grid-origin cells"
        );

        for _ in 0..240 {
            world.step();
        }
        let st = world.body_state(id);
        assert!(st.is_finite());
        // Rests on the floor (frame origin ends near y = 0.5 - 2.0 = -1.5 m),
        // nowhere near the < -5 m free-fall the displaced collider produced.
        assert!(
            st.translation_m[1] > -2.5,
            "beam settled on the floor under its authoritative cells (y = {})",
            st.translation_m[1]
        );
    }

    /// A rebuild after an edit that deletes the minimum cells shifts the tight
    /// grid origin; the collider offset must follow so the remaining cells stay
    /// put in world space.
    #[test]
    fn rebuild_tracks_a_shifted_grid_origin() {
        let cell_m = 0.25_f32;
        let mut world = PhysicsWorld::new(PhysicsConfig::default());

        let full = box_grid(GlobalCell::new(4, 4, 4), GlobalCell::new(11, 11, 11));
        let id = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: Representation::MergedCuboids,
            grid: full,
            cell_m,
            density_kg_m3: 2600.0,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        // Cells 4..11 on each axis: cell-centre mean 8.0 -> 2.0 m.
        let com0 = world.body_local_com(id);
        assert!((com0[0] - 2.0).abs() < 0.05 && (com0[1] - 2.0).abs() < 0.05);

        // Drop the x = 4..5, y = 4..5 and z = 4..5 slabs: tight origin moves to
        // (6, 6, 4).
        let shrunk = box_grid(GlobalCell::new(6, 6, 4), GlobalCell::new(11, 11, 11));
        world.rebuild_collider(id, &shrunk, Representation::MergedCuboids);
        // New cell-centre means: x/y 9.0 -> 2.25 m, z 8.0 -> 2.0 m.
        let com1 = world.body_local_com(id);
        assert!(
            (com1[0] - 2.25).abs() < 0.05
                && (com1[1] - 2.25).abs() < 0.05
                && (com1[2] - 2.0).abs() < 0.05,
            "rebuilt collider COM {com1:?} tracks the shifted grid origin"
        );
    }
}

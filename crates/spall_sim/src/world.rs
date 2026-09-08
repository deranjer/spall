//! The authoritative world: terrain plus every detached body, the physics
//! world their colliders live in, and the monotonic identity / invalidation
//! counters every job token is checked against.
//!
//! [`SimWorld`] implements [`spall_jobs::WorldView`], so a staged edit's
//! [`spall_jobs::JobToken`] is validated against the *live* world at commit
//! time exactly like any other off-tick job result.

use std::collections::BTreeMap;

use glam::DQuat;
use spall_core::{
    BrickCoord, CellSizeCode, EntityId, GlobalCell, IdError, LocalCell, MaterialId, Revision,
    VolumeId,
};
use spall_jobs::{BrickRef, BrickStatus, Generation, TopologyEpoch, WorldView};
use spall_physics::{
    BodyKind as PhysBodyKind, BodySpec, OccupancyGrid, PhysicsConfig, PhysicsWorld,
};
use spall_protocol::{
    CanonicalBrick, CanonicalLayer, CanonicalOwner, CanonicalVolume, Hash32, MotionSnapshot,
    canonical_topology_hash,
};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, BrickBounds, BrickState, EditPlan, Volume};

use crate::body::{Body, BodyKind, BodyPose};
use crate::collider::plan_collider;
use crate::registry::IdRegistry;

/// The stable numeric layer code for the material layer in a canonical brick.
const MATERIAL_LAYER_KIND: u16 = 0;

/// Why a world operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorldError {
    #[error("body {0} does not exist")]
    UnknownBody(EntityId),
    #[error("volume {0} does not exist in this world")]
    UnknownVolume(VolumeId),
    #[error("terrain fixture has no solid cell to build a collider from")]
    EmptyTerrain,
    #[error("spawned body has no solid cell")]
    EmptyBody,
    #[error("id space exhausted: {0}")]
    Ids(#[from] spall_core::IdError),
    #[error("occupancy extraction failed: {0}")]
    Occupancy(#[from] spall_physics::ExtractError),
    #[error("edit during replay failed: {0}")]
    Edit(#[from] spall_voxel::EditError),
    #[error("journal replay precondition failed: {0}")]
    ReplayPrecondition(String),
    #[error("journal replay result hash mismatch: {0}")]
    ReplayResultHash(String),
}

/// A detached body being reinstated from a persisted checkpoint record.
#[derive(Debug, Clone)]
pub struct RestoredBody {
    pub entity: EntityId,
    /// The body's volume, already carrying its saved [`VolumeId`].
    pub volume: Volume,
    pub pose: BodyPose,
    pub linvel_m_s: [f64; 3],
    pub angvel_rad_s: [f64; 3],
    pub sleeping: bool,
    pub collider_revision: u64,
    /// Inclusive global-cell box `[min, max]` the collider covers.
    pub collider_region: (GlobalCell, GlobalCell),
    /// Bulk density input, kg/m³.
    pub density_kg_m3: f32,
}

/// A [`BodyPose`] from a wire [`MotionSnapshot`] (rotation is the decoded i16
/// quaternion; a zero-magnitude quaternion falls back to identity).
fn pose_from_snapshot(snap: &MotionSnapshot) -> BodyPose {
    let q = snap.pose.rotation.to_unit().unwrap_or([0.0, 0.0, 0.0, 1.0]);
    BodyPose::new(
        DQuat::from_xyzw(
            f64::from(q[0]),
            f64::from(q[1]),
            f64::from(q[2]),
            f64::from(q[3]),
        ),
        snap.pose.translation_m,
    )
}

/// Everything needed to stand up a world.
pub struct WorldSetup {
    /// The terrain volume. Its solid cells become the fixed terrain collider.
    pub terrain: Volume,
    /// Inclusive global-cell box the terrain collider covers. Terrain edits
    /// inside it trigger a bounded rebuild; edits outside are rejected as
    /// out-of-region for T08.
    pub terrain_collider_region: (GlobalCell, GlobalCell),
    /// Material properties (density feeds mass, friction/restitution feed the
    /// solver later).
    pub materials: spall_core::MaterialManifest,
    /// The declared lower support plane (global `y`).
    pub anchor: AnchorPlane,
    pub physics: PhysicsConfig,
}

/// The authoritative simulation world.
pub struct SimWorld {
    registry: IdRegistry,
    materials: spall_core::MaterialManifest,
    anchor: AnchorPlane,
    generation: Generation,
    topology_epoch: TopologyEpoch,
    terrain: Body,
    /// Detached bodies keyed by entity id.
    bodies: BTreeMap<u64, Body>,
    /// `volume id -> entity id` for detached bodies.
    volume_owner: BTreeMap<u64, u64>,
    physics: PhysicsWorld,
}

impl SimWorld {
    /// Builds a world from `setup`, installing the terrain collider.
    pub fn new(setup: WorldSetup) -> Result<Self, WorldError> {
        let mut registry = IdRegistry::new();
        let terrain_volume_id = registry.allocate_volume()?; // volume 1 == terrain

        let grid = OccupancyGrid::from_volume(&setup.terrain)?.ok_or(WorldError::EmptyTerrain)?;
        let plan = plan_collider(&grid);
        let cell_m = setup.terrain.cell_size().metres() as f32;

        let mut physics = PhysicsWorld::new(setup.physics);
        let phys = physics.add_body(BodySpec {
            kind: PhysBodyKind::Fixed,
            representation: plan.representation,
            grid: plan.grid,
            cell_m,
            density_kg_m3: 1.0,
            translation_m: grid_origin_translation(&grid, cell_m),
            linvel_m_s: [0.0; 3],
        });

        let terrain = Body {
            entity: None,
            volume_id: terrain_volume_id,
            volume: setup.terrain,
            kind: BodyKind::Terrain,
            pose: BodyPose::identity(),
            linvel_m_s: [0.0; 3],
            angvel_rad_s: [0.0; 3],
            sleeping: true,
            collider_revision: 1,
            coarsen_k: plan.coarsen_k,
            phys,
            collider_region: setup.terrain_collider_region,
        };

        Ok(Self {
            registry,
            materials: setup.materials,
            anchor: setup.anchor,
            generation: Generation::START,
            topology_epoch: TopologyEpoch::START,
            terrain,
            bodies: BTreeMap::new(),
            volume_owner: BTreeMap::new(),
            physics,
        })
    }

    pub fn anchor(&self) -> AnchorPlane {
        self.anchor
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn topology_epoch(&self) -> TopologyEpoch {
        self.topology_epoch
    }

    /// Advances the topology epoch — call after a commit that moved cells
    /// between components (a split), which can invalidate derived work beyond
    /// the exact bricks a job listed.
    pub fn bump_topology_epoch(&mut self) {
        self.topology_epoch = self
            .topology_epoch
            .checked_next()
            .expect("topology epoch exhausted");
    }

    pub fn registry_mut(&mut self) -> &mut IdRegistry {
        &mut self.registry
    }

    pub fn registry(&self) -> &IdRegistry {
        &self.registry
    }

    pub fn materials(&self) -> &spall_core::MaterialManifest {
        &self.materials
    }

    pub fn physics_mut(&mut self) -> &mut PhysicsWorld {
        &mut self.physics
    }

    pub fn physics(&self) -> &PhysicsWorld {
        &self.physics
    }

    /// Advances physics one fixed step and refreshes every dynamic body's
    /// extracted pose / velocity / sleep state from the solver.
    pub fn step_physics(&mut self) {
        self.physics.step();
        let physics = &self.physics;
        for body in self.bodies.values_mut() {
            let st = physics.body_state(body.phys);
            body.pose = BodyPose::new(
                DQuat::from_xyzw(
                    f64::from(st.rotation[0]),
                    f64::from(st.rotation[1]),
                    f64::from(st.rotation[2]),
                    f64::from(st.rotation[3]),
                ),
                [
                    f64::from(st.translation_m[0]),
                    f64::from(st.translation_m[1]),
                    f64::from(st.translation_m[2]),
                ],
            );
            body.linvel_m_s = st.linvel_m_s.map(f64::from);
            body.angvel_rad_s = st.angvel_rad_s.map(f64::from);
            body.sleeping = st.sleeping;
        }
    }

    /// Density of a material, kg/m³. Air is `0`; an unknown id is `0` (callers
    /// only ask about solid cells that came from a validated plan).
    pub fn density(&self, id: MaterialId) -> f64 {
        if id.is_air() {
            return 0.0;
        }
        self.materials
            .get(id)
            .map(|d| f64::from(d.sim.density_kg_m3))
            .unwrap_or(0.0)
    }

    pub fn terrain(&self) -> &Body {
        &self.terrain
    }

    pub fn terrain_volume_id(&self) -> VolumeId {
        self.terrain.volume_id
    }

    /// The body owning `volume`, if any (never terrain).
    pub fn body_by_volume(&self, volume: VolumeId) -> Option<&Body> {
        let entity = self.volume_owner.get(&volume.get())?;
        self.bodies.get(entity)
    }

    pub fn body(&self, entity: EntityId) -> Option<&Body> {
        self.bodies.get(&entity.get())
    }

    /// The body owning `volume`, terrain or detached.
    pub fn volume_body(&self, volume: VolumeId) -> Option<&Body> {
        if volume == self.terrain.volume_id {
            return Some(&self.terrain);
        }
        self.body_by_volume(volume)
    }

    pub fn bodies(&self) -> impl Iterator<Item = &Body> {
        self.bodies.values()
    }

    pub fn body_count(&self) -> usize {
        self.bodies.len()
    }

    /// A mutable reference to whichever body owns `volume` (terrain or detached).
    pub fn volume_body_mut(&mut self, volume: VolumeId) -> Option<&mut Body> {
        if volume == self.terrain.volume_id {
            return Some(&mut self.terrain);
        }
        let entity = *self.volume_owner.get(&volume.get())?;
        self.bodies.get_mut(&entity)
    }

    /// The volume owned by `volume` id (terrain or detached).
    pub fn volume_ref(&self, volume: VolumeId) -> Option<&Volume> {
        if volume == self.terrain.volume_id {
            return Some(&self.terrain.volume);
        }
        self.body_by_volume(volume).map(|b| &b.volume)
    }

    /// Spawns a detached dynamic body. `build` receives the freshly-allocated
    /// volume id so the body's [`Volume`] carries a consistent identity.
    /// Primarily for standing up test scenarios and, later, a game's initial
    /// scene.
    pub fn spawn_body(
        &mut self,
        build: impl FnOnce(VolumeId) -> Volume,
        pose: BodyPose,
        linvel_m_s: [f64; 3],
        angvel_rad_s: [f64; 3],
        density_kg_m3: f32,
        collider_pad_cells: i64,
    ) -> Result<EntityId, WorldError> {
        let entity = self.registry.allocate_entity()?;
        let volume_id = self.registry.allocate_volume()?;
        let volume = build(volume_id);

        let grid = OccupancyGrid::from_volume(&volume)?.ok_or(WorldError::EmptyBody)?;
        let plan = plan_collider(&grid);
        let cell_m = volume.cell_size().metres() as f32;
        let trans = [
            pose.translation_m[0] as f32,
            pose.translation_m[1] as f32,
            pose.translation_m[2] as f32,
        ];
        let rot = pose.rotation;
        let rot_xyzw = [rot.x as f32, rot.y as f32, rot.z as f32, rot.w as f32];
        let linvel = linvel_m_s.map(|v| v as f32);
        let angvel = angvel_rad_s.map(|v| v as f32);

        let phys = self.physics.add_body(BodySpec {
            kind: PhysBodyKind::Dynamic { ccd: false },
            representation: plan.representation,
            grid: plan.grid.clone(),
            cell_m,
            density_kg_m3: density_kg_m3.max(f32::MIN_POSITIVE),
            translation_m: trans,
            linvel_m_s: linvel,
        });
        self.physics.set_body_pose(phys, trans, rot_xyzw);
        self.physics.set_body_velocity(phys, linvel, angvel);

        let o = grid.origin();
        let d = grid.dims();
        let region = (
            GlobalCell::new(
                o.x - collider_pad_cells,
                o.y - collider_pad_cells,
                o.z - collider_pad_cells,
            ),
            GlobalCell::new(
                o.x + d[0] as i64 - 1 + collider_pad_cells,
                o.y + d[1] as i64 - 1 + collider_pad_cells,
                o.z + d[2] as i64 - 1 + collider_pad_cells,
            ),
        );

        let body = Body {
            entity: Some(entity),
            volume_id,
            volume,
            kind: BodyKind::Dynamic,
            pose,
            linvel_m_s,
            angvel_rad_s,
            sleeping: false,
            collider_revision: 1,
            coarsen_k: plan.coarsen_k,
            phys,
            collider_region: region,
        };
        self.volume_owner.insert(volume_id.get(), entity.get());
        self.bodies.insert(entity.get(), body);
        Ok(entity)
    }

    /// Inserts a freshly-created detached body and its physics collider. Returns
    /// the body's entity id.
    pub fn insert_body(&mut self, mut body: Body) -> EntityId {
        let entity = body
            .entity
            .expect("a detached body must carry an entity id");
        body.kind = BodyKind::Dynamic;
        self.volume_owner.insert(body.volume_id.get(), entity.get());
        self.bodies.insert(entity.get(), body);
        entity
    }

    // --- save recovery (T16) ------------------------------------------------
    //
    // These reinstate authoritative state from persisted records without
    // replaying an edit. `spall_store` holds the bytes; the save-record ↔
    // `SimWorld` mapping lives in the integrator (`spall_server::persist`).

    /// Replaces the id allocators with resumed counters (`docs/protocol.md`:
    /// "Persist next-ID counters" so a restart cannot re-hand a live id).
    pub fn resume_registry(
        &mut self,
        next_entity: u64,
        next_volume: u64,
        next_transaction: u64,
        next_journal_seq: u64,
    ) -> Result<(), IdError> {
        self.registry =
            IdRegistry::resume(next_entity, next_volume, next_transaction, next_journal_seq)?;
        Ok(())
    }

    /// Reinstates one detached body from a checkpoint record: it keeps its saved
    /// entity/volume id, transform, velocity, sleep flag, and collider revision.
    /// A body saved asleep resumes with zero velocity (the solver re-sleeps it
    /// on the next step); the saved `sleeping` flag stays authoritative for
    /// replication/journalling until then.
    pub fn insert_restored_body(&mut self, spec: RestoredBody) -> Result<EntityId, WorldError> {
        let grid = OccupancyGrid::from_volume(&spec.volume)?.ok_or(WorldError::EmptyBody)?;
        let plan = plan_collider(&grid);
        let cell_m = spec.volume.cell_size().metres() as f32;
        let trans = [
            spec.pose.translation_m[0] as f32,
            spec.pose.translation_m[1] as f32,
            spec.pose.translation_m[2] as f32,
        ];
        let rot = spec.pose.rotation;
        let rot_xyzw = [rot.x as f32, rot.y as f32, rot.z as f32, rot.w as f32];
        let (linvel, angvel) = if spec.sleeping {
            ([0.0f32; 3], [0.0f32; 3])
        } else {
            (
                spec.linvel_m_s.map(|v| v as f32),
                spec.angvel_rad_s.map(|v| v as f32),
            )
        };

        let phys = self.physics.add_body(BodySpec {
            kind: PhysBodyKind::Dynamic { ccd: false },
            representation: plan.representation,
            grid: plan.grid.clone(),
            cell_m,
            density_kg_m3: spec.density_kg_m3.max(f32::MIN_POSITIVE),
            translation_m: trans,
            linvel_m_s: linvel,
        });
        self.physics.set_body_pose(phys, trans, rot_xyzw);
        self.physics.set_body_velocity(phys, linvel, angvel);

        let entity = spec.entity;
        let body = Body {
            entity: Some(entity),
            volume_id: spec.volume.id(),
            volume: spec.volume,
            kind: BodyKind::Dynamic,
            pose: spec.pose,
            linvel_m_s: spec.linvel_m_s,
            angvel_rad_s: spec.angvel_rad_s,
            sleeping: spec.sleeping,
            collider_revision: spec.collider_revision,
            coarsen_k: plan.coarsen_k,
            phys,
            collider_region: spec.collider_region,
        };
        self.volume_owner.insert(body.volume_id.get(), entity.get());
        self.bodies.insert(entity.get(), body);
        Ok(entity)
    }

    /// Applies the ops of one journalled [`spall_protocol::TopologyTransaction`]
    /// to the live world during recovery: brush / cell-run writes go to their
    /// named volume, and each `SplitOff` child is built from its canonical fill
    /// runs and installed as a body using the matching participant snapshot for
    /// its transform. Colliders of every touched volume are rebuilt. The caller
    /// bumps the id counters past the replayed suffix afterwards.
    ///
    /// The transaction's `before` brick revisions are checked against the live
    /// world *before* any op is applied, and its `after` brick revisions and
    /// `result_hashes` are checked against the resulting geometry — a decodable
    /// but semantically wrong record (or a replay defect) is rejected rather
    /// than silently accepted as authoritative state.
    pub fn replay_transaction(
        &mut self,
        tx: &spall_protocol::TopologyTransaction,
        participants: &[MotionSnapshot],
    ) -> Result<(), WorldError> {
        use spall_protocol::TopologyOp;

        self.check_replay_preconditions(tx)?;

        let terrain_cell_size = self.terrain.volume.cell_size();
        let mut touched: Vec<VolumeId> = Vec::new();
        let mut split = false;

        // Group consecutive same-volume cell writes so each volume is edited
        // once. A `SplitOff` opens a new child group; its following `CellRun`s
        // (same child volume) fill it.
        struct Group {
            volume: VolumeId,
            new_child: Option<EntityId>,
            writes: Vec<(GlobalCell, MaterialId)>,
        }
        let mut group: Option<Group> = None;
        let flush = |world: &mut SimWorld,
                     group: Option<Group>,
                     touched: &mut Vec<VolumeId>|
         -> Result<(), WorldError> {
            let Some(g) = group else { return Ok(()) };
            if let Some(child_entity) = g.new_child {
                if g.writes.is_empty() {
                    return Err(WorldError::EmptyBody);
                }
                let mut min = BrickCoord::new(i64::MAX, i64::MAX, i64::MAX);
                let mut max = BrickCoord::new(i64::MIN, i64::MIN, i64::MIN);
                for (cell, _) in &g.writes {
                    let b = cell.split().0;
                    min = BrickCoord::new(min.x.min(b.x), min.y.min(b.y), min.z.min(b.z));
                    max = BrickCoord::new(max.x.max(b.x), max.y.max(b.y), max.z.max(b.z));
                }
                let bounds = BrickBounds::new(min, max).expect("min <= max by construction");
                let mut child = Volume::bounded(g.volume, terrain_cell_size, bounds);
                for bz in min.z..=max.z {
                    for by in min.y..=max.y {
                        for bx in min.x..=max.x {
                            child
                                .insert_brick(
                                    BrickCoord::new(bx, by, bz),
                                    Brick::uniform(MaterialId::AIR, Revision(1)),
                                )
                                .expect("brick within the bounds just set");
                        }
                    }
                }
                let mut plan = EditPlan::new(g.volume);
                for (cell, material) in &g.writes {
                    plan.set(*cell, *material);
                }
                child.apply_edit(&plan)?;

                let snap = participants.iter().find(|s| s.body == child_entity);
                let pose = snap
                    .map(pose_from_snapshot)
                    .unwrap_or_else(BodyPose::identity);
                let (linvel, angvel, sleeping) = snap
                    .map(|s| {
                        (
                            s.linear_velocity.map(f64::from),
                            s.angular_velocity.map(f64::from),
                            s.sleeping,
                        )
                    })
                    .unwrap_or(([0.0; 3], [0.0; 3], false));
                let grid = OccupancyGrid::from_volume(&child)?.ok_or(WorldError::EmptyBody)?;
                let region = {
                    let o = grid.origin();
                    let d = grid.dims();
                    (
                        GlobalCell::new(o.x, o.y, o.z),
                        GlobalCell::new(
                            o.x + d[0] as i64 - 1,
                            o.y + d[1] as i64 - 1,
                            o.z + d[2] as i64 - 1,
                        ),
                    )
                };
                world.insert_restored_body(RestoredBody {
                    entity: child_entity,
                    volume: child,
                    pose,
                    linvel_m_s: linvel,
                    angvel_rad_s: angvel,
                    sleeping,
                    collider_revision: 1,
                    collider_region: region,
                    density_kg_m3: 1.0,
                })?;
                touched.push(g.volume);
            } else {
                let vol = world
                    .volume_body_mut(g.volume)
                    .ok_or(WorldError::UnknownVolume(g.volume))?;
                let mut plan = EditPlan::new(g.volume);
                for (cell, material) in &g.writes {
                    plan.set(*cell, *material);
                }
                vol.volume.apply_edit(&plan)?;
                if !touched.contains(&g.volume) {
                    touched.push(g.volume);
                }
            }
            Ok(())
        };

        for op in &tx.ops {
            match op {
                TopologyOp::IntegerBrush {
                    volume,
                    brush,
                    material,
                } => {
                    flush(self, group.take(), &mut touched)?;
                    let vol = self
                        .volume_body_mut(*volume)
                        .ok_or(WorldError::UnknownVolume(*volume))?;
                    vol.volume
                        .apply_edit(&EditPlan::sphere(*volume, *brush, *material))?;
                    if !touched.contains(volume) {
                        touched.push(*volume);
                    }
                }
                TopologyOp::SplitOff {
                    child,
                    child_entity,
                    ..
                } => {
                    flush(self, group.take(), &mut touched)?;
                    split = true;
                    group = Some(Group {
                        volume: *child,
                        new_child: Some(*child_entity),
                        writes: Vec::new(),
                    });
                }
                TopologyOp::CellRun {
                    volume,
                    start,
                    len,
                    material,
                } => {
                    if group.as_ref().map(|g| g.volume) != Some(*volume) {
                        flush(self, group.take(), &mut touched)?;
                        group = Some(Group {
                            volume: *volume,
                            new_child: None,
                            writes: Vec::new(),
                        });
                    }
                    let g = group.as_mut().expect("just set");
                    let last_x = start
                        .x
                        .checked_add(i64::from(*len).saturating_sub(1))
                        .ok_or(WorldError::UnknownVolume(*volume))?;
                    for x in start.x..=last_x {
                        g.writes
                            .push((GlobalCell::new(x, start.y, start.z), *material));
                    }
                }
            }
        }
        flush(self, group.take(), &mut touched)?;

        for vid in touched {
            self.rebuild_volume_collider(vid)?;
        }
        if split {
            self.bump_topology_epoch();
        }

        self.check_replay_results(tx)?;
        Ok(())
    }

    /// Checks a journalled transaction's `before` brick revisions against the
    /// live world before any op is replayed. A non-resident brick reads as
    /// [`Revision::ZERO`] — the implicit "before" of an untouched cell.
    fn check_replay_preconditions(
        &self,
        tx: &spall_protocol::TopologyTransaction,
    ) -> Result<(), WorldError> {
        for br in &tx.before {
            let found = self
                .volume_ref(br.volume)
                .ok_or_else(|| {
                    WorldError::ReplayPrecondition(format!(
                        "transaction {} references unknown volume {}",
                        tx.transaction_id.get(),
                        br.volume
                    ))
                })?
                .brick_revision(br.coord)
                .map_err(|e| {
                    WorldError::ReplayPrecondition(format!(
                        "transaction {} brick {:?} in volume {}: {e}",
                        tx.transaction_id.get(),
                        br.coord,
                        br.volume
                    ))
                })?
                .unwrap_or(Revision::ZERO);
            if found != br.revision {
                return Err(WorldError::ReplayPrecondition(format!(
                    "transaction {} expected volume {} brick {:?} at revision {}, live world has {}",
                    tx.transaction_id.get(),
                    br.volume,
                    br.coord,
                    br.revision.get(),
                    found.get()
                )));
            }
        }
        Ok(())
    }

    /// Checks a journalled transaction's `after` brick revisions and
    /// `result_hashes` against the geometry the replay just produced.
    fn check_replay_results(
        &self,
        tx: &spall_protocol::TopologyTransaction,
    ) -> Result<(), WorldError> {
        for br in &tx.after {
            let found = self
                .volume_ref(br.volume)
                .and_then(|v| v.brick_revision(br.coord).ok().flatten())
                .unwrap_or(Revision::ZERO);
            if found != br.revision {
                return Err(WorldError::ReplayResultHash(format!(
                    "transaction {} recorded volume {} brick {:?} at revision {} after replay, got {}",
                    tx.transaction_id.get(),
                    br.volume,
                    br.coord,
                    br.revision.get(),
                    found.get()
                )));
            }
        }
        for vh in &tx.result_hashes {
            let got = self.volume_hash(vh.volume);
            if got != Some(vh.hash) {
                return Err(WorldError::ReplayResultHash(format!(
                    "transaction {} recorded a result hash for volume {} that the replayed geometry does not reproduce",
                    tx.transaction_id.get(),
                    vh.volume
                )));
            }
        }
        Ok(())
    }

    /// Rebuilds one volume's collider from its current geometry (recovery and
    /// post-replay). No-op if the volume has no solid cell left.
    pub fn rebuild_volume_collider(&mut self, volume: VolumeId) -> Result<(), WorldError> {
        let Some(body) = self.volume_body(volume) else {
            return Err(WorldError::UnknownVolume(volume));
        };
        let phys = body.phys;
        let Some(grid) = OccupancyGrid::from_volume(&body.volume)? else {
            return Ok(());
        };
        let plan = plan_collider(&grid);
        self.physics
            .rebuild_collider(phys, &plan.grid, plan.representation);
        if let Some(body) = self.volume_body_mut(volume) {
            body.collider_revision += 1;
            body.coarsen_k = plan.coarsen_k;
        }
        Ok(())
    }

    /// Applies a durable 20 Hz pose batch during recovery: each body's
    /// transform, velocity, and sleep flag are set to the snapshot's (rotation
    /// is the i16-quantized wire value — the accepted motion-rewind loss from
    /// `docs/protocol.md`).
    pub fn apply_pose_batch(&mut self, snapshots: &[MotionSnapshot]) {
        for snap in snapshots {
            let Some(body) = self.bodies.get_mut(&snap.body.get()) else {
                continue;
            };
            let pose = pose_from_snapshot(snap);
            let trans = [
                pose.translation_m[0] as f32,
                pose.translation_m[1] as f32,
                pose.translation_m[2] as f32,
            ];
            let rot = pose.rotation;
            let rot_xyzw = [rot.x as f32, rot.y as f32, rot.z as f32, rot.w as f32];
            let linvel = snap.linear_velocity;
            let angvel = snap.angular_velocity;
            body.pose = pose;
            body.linvel_m_s = linvel.map(f64::from);
            body.angvel_rad_s = angvel.map(f64::from);
            body.sleeping = snap.sleeping;
            let phys = body.phys;
            self.physics.set_body_pose(phys, trans, rot_xyzw);
            self.physics.set_body_velocity(phys, linvel, angvel);
        }
    }

    /// Canonical representation of one volume for hashing / result hashes. The
    /// material-layer payload is the brick's representation-independent content
    /// hash (a deterministic digest of exactly the authoritative material layer).
    pub fn canonical_volume(&self, volume: VolumeId) -> Option<CanonicalVolume> {
        let (v, owner) = if volume == self.terrain.volume_id {
            (&self.terrain.volume, CanonicalOwner::Terrain)
        } else {
            let body = self.body_by_volume(volume)?;
            (
                &body.volume,
                CanonicalOwner::Body(body.entity.expect("detached body has an entity")),
            )
        };
        Some(canonical_volume_for(v, owner))
    }

    /// Canonical topology hash of just `volume`.
    pub fn volume_hash(&self, volume: VolumeId) -> Option<Hash32> {
        self.canonical_volume(volume)
            .map(|cv| canonical_topology_hash(&[cv]))
    }

    /// Canonical topology hash of the whole world (terrain + every body).
    pub fn world_hash(&self) -> Hash32 {
        let mut volumes = vec![
            self.canonical_volume(self.terrain.volume_id)
                .expect("terrain volume always exists"),
        ];
        for body in self.bodies.values() {
            volumes.push(
                self.canonical_volume(body.volume_id)
                    .expect("body volume exists"),
            );
        }
        canonical_topology_hash(&volumes)
    }

    /// Total solid cells across terrain and every body — the conservation
    /// invariant's left-hand side over the whole world.
    pub fn total_solid_cells(&self) -> u64 {
        let mut total = solid_cells(&self.terrain.volume);
        for body in self.bodies.values() {
            total += solid_cells(&body.volume);
        }
        total
    }
}

impl WorldView for SimWorld {
    fn generation(&self) -> Generation {
        self.generation
    }

    fn topology_epoch(&self) -> TopologyEpoch {
        self.topology_epoch
    }

    fn brick_status(&self, brick: BrickRef) -> BrickStatus {
        let volume = if brick.volume == self.terrain.volume_id {
            Some(&self.terrain.volume)
        } else {
            self.body_by_volume(brick.volume).map(|b| &b.volume)
        };
        match volume {
            None => BrickStatus::Absent,
            Some(v) => match v.brick_state(brick.brick) {
                Ok(BrickState::Resident { revision, .. }) => BrickStatus::Resident(revision),
                Ok(BrickState::Failed) => BrickStatus::Failed,
                Ok(BrickState::Absent) | Err(_) => BrickStatus::Absent,
            },
        }
    }
}

/// Canonical single-volume representation of `volume` under `owner`, for a
/// volume that may not be installed in the world yet (a commit candidate).
/// [`SimWorld::canonical_volume`] is this plus the live-world owner lookup.
pub fn canonical_volume_for(volume: &Volume, owner: CanonicalOwner) -> CanonicalVolume {
    let mut bricks = Vec::new();
    for coord in volume.resident_brick_coords() {
        let snap = volume
            .snapshot_brick(coord)
            .ok()
            .flatten()
            .expect("coord came from the resident set");
        bricks.push(CanonicalBrick {
            coord,
            revision: snap.revision(),
            layers: vec![CanonicalLayer {
                kind: MATERIAL_LAYER_KIND,
                bytes: spall_voxel::BrickHash::to_bytes(snap.content_hash()).to_vec(),
            }],
        });
    }
    CanonicalVolume {
        volume_id: volume.id(),
        cell_size: volume.cell_size(),
        owner,
        bricks,
    }
}

/// Canonical topology hash of just `volume` under `owner` — the same value
/// [`SimWorld::volume_hash`] returns once the volume is installed, so a commit
/// candidate can compute its result hashes before publishing.
pub fn volume_topology_hash_for(volume: &Volume, owner: CanonicalOwner) -> Hash32 {
    canonical_topology_hash(&[canonical_volume_for(volume, owner)])
}

/// The world translation that places a body-local grid whose cell `(0,0,0)`
/// corner is at global cell `grid.origin()` and whose transform is identity.
pub fn grid_origin_translation(grid: &OccupancyGrid, cell_m: f32) -> [f32; 3] {
    let o = grid.origin();
    [
        o.x as f32 * cell_m,
        o.y as f32 * cell_m,
        o.z as f32 * cell_m,
    ]
}

/// Count of solid cells in a volume (walks every resident brick).
pub fn solid_cells(volume: &Volume) -> u64 {
    let mut total = 0u64;
    for coord in volume.resident_brick_coords() {
        let Some(snap) = volume.snapshot_brick(coord).ok().flatten() else {
            continue;
        };
        for index in 0..spall_core::CELLS_PER_BRICK as u16 {
            let local = LocalCell::from_linear_index(index).expect("index < 32768");
            if !snap.get(local).is_air() {
                total += 1;
            }
        }
    }
    total
}

/// Convenience: the brick coordinate that owns `cell`.
pub fn brick_of(cell: GlobalCell) -> BrickCoord {
    cell.split().0
}

/// The cell size code of a volume, restated for callers that only hold an id.
pub fn cell_size_of(volume: &Volume) -> CellSizeCode {
    volume.cell_size()
}

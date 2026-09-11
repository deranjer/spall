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
    BrickCoord, CellSizeCode, EntityId, GlobalCell, IdError, LocalCell, MaterialId, PlayerInput,
    Revision, VolumeId,
};
use spall_jobs::{BrickRef, BrickStatus, Generation, TopologyEpoch, WorldView};
use spall_physics::{
    BodyKind as PhysBodyKind, BodySpec, CharacterParams, OccupancyGrid, PhysicsConfig,
    PhysicsWorld, analytic_mass_properties, step_character,
};
use spall_protocol::{
    CanonicalBrick, CanonicalLayer, CanonicalOwner, CanonicalVolume, Hash32, MotionSnapshot,
    canonical_topology_hash,
};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, BrickBounds, BrickState, EditPlan, EvictedBricks, Volume};

use crate::body::{Body, BodyKind, BodyPose};
use crate::collider::plan_collider;
use crate::player::Player;
use crate::registry::IdRegistry;
use spall_protocol::InputSeq;

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
    #[error("no exact active collider for the body: {0}")]
    Collider(#[from] crate::collider::ColliderInfeasible),
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
    /// Authoritative player capsules keyed by their reserved-band entity id
    /// (T19). Not bodies: no volume, never split, never in the dynamic set.
    players: BTreeMap<u64, Player>,
    physics: PhysicsWorld,
    /// T23 / G3 row 7, slice B: per-volume digests of bricks that have been
    /// evicted from the live cache. Empty unless the residency pass (slice D)
    /// populates it, so every logical path is byte-identical to today by
    /// default. Keyed by raw volume id.
    evicted: BTreeMap<u64, EvictedBricks>,
    /// T23 / G3 row 7, slice C: durable source used to reload an evicted brick's
    /// cells when an edit needs them. `None` unless the residency pass installs
    /// one; with `None`, an edit that needs evicted geometry is rejected rather
    /// than reloaded.
    backing: Option<std::sync::Arc<dyn crate::backing::BrickBacking>>,
}

/// A shared empty digest set, so [`SimWorld::evicted`] can return a reference
/// for a volume that has nothing evicted without allocating.
fn empty_evicted() -> &'static EvictedBricks {
    static EMPTY: std::sync::OnceLock<EvictedBricks> = std::sync::OnceLock::new();
    EMPTY.get_or_init(EvictedBricks::new)
}

impl SimWorld {
    /// Builds a world from `setup`, installing the terrain collider.
    pub fn new(setup: WorldSetup) -> Result<Self, WorldError> {
        let mut registry = IdRegistry::new();
        let terrain_volume_id = registry.allocate_volume()?; // volume 1 == terrain

        let grid = OccupancyGrid::from_volume(&setup.terrain)?.ok_or(WorldError::EmptyTerrain)?;
        let plan = plan_collider(&grid)?;
        let cell_m = setup.terrain.cell_size().metres() as f32;

        let mut physics = PhysicsWorld::new(setup.physics);
        let phys = physics.add_body(BodySpec {
            kind: PhysBodyKind::Fixed,
            representation: plan.representation,
            grid: plan.grid,
            cell_m,
            density_kg_m3: 1.0,
            // Terrain is immovable: mass properties never enter the solver.
            mass_properties: None,
            // Identity pose: `PhysicsWorld` carries the tight grid's origin as a
            // body-local collider offset, so the terrain body stays at the world
            // origin like its `BodyPose::identity()` (`ENG-55`).
            translation_m: [0.0; 3],
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
            dormant: false,
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
            players: BTreeMap::new(),
            physics,
            evicted: BTreeMap::new(),
            backing: None,
        })
    }

    /// The retained evicted-brick digests for `volume` (empty by default —
    /// residency is off until slice D). See `docs/reports/G3-residency-hash.md`.
    pub fn evicted(&self, volume: VolumeId) -> &EvictedBricks {
        self.evicted
            .get(&volume.get())
            .unwrap_or_else(|| empty_evicted())
    }

    /// Mutable digest set for `volume`, created empty if absent. The residency
    /// pass keeps this consistent with the live volume under the digest
    /// lifecycle contract.
    pub fn evicted_mut(&mut self, volume: VolumeId) -> &mut EvictedBricks {
        self.evicted.entry(volume.get()).or_default()
    }

    /// Replaces `volume`'s digest set wholesale (a full baseline / recovery
    /// transition, or a test fixture).
    pub fn set_evicted(&mut self, volume: VolumeId, digests: EvictedBricks) {
        if digests.is_empty() {
            self.evicted.remove(&volume.get());
        } else {
            self.evicted.insert(volume.get(), digests);
        }
    }

    /// `true` if any volume has retained evicted digests.
    pub fn has_evicted(&self) -> bool {
        self.evicted.values().any(|e| !e.is_empty())
    }

    /// Evicts one brick of `volume` from the live cache, retaining its exact
    /// `(revision, content_hash, solid_cells)` digest so `world_hash`,
    /// conservation, and `result_hashes` still see it (the digest-lifecycle
    /// "evict" transition). `Ok(false)` if the brick is not resident. The
    /// residency pass (slice D) is the real caller; also used by tests.
    pub fn evict_brick(
        &mut self,
        volume: VolumeId,
        coord: BrickCoord,
    ) -> Result<bool, spall_voxel::DigestError> {
        let Some(vol) = self.volume_ref(volume) else {
            return Ok(false);
        };
        let digest = match spall_voxel::BrickDigest::capture(vol, coord) {
            Ok(d) => d,
            Err(spall_voxel::DigestError::NotResident(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        self.evicted_mut(volume).record(coord, digest)?;
        self.volume_body_mut(volume)
            .expect("volume_ref matched")
            .volume
            .evict_brick(coord);
        Ok(true)
    }

    /// Reload lifecycle: after geometry has been reinstalled at `coord`, verify
    /// it matches the retained digest and drop the digest. `Err` leaves the
    /// digest in place (the reload stays pending/failed).
    pub fn clear_evicted_after_reload(
        &mut self,
        volume: VolumeId,
        coord: BrickCoord,
    ) -> Result<(), spall_voxel::DigestError> {
        let vol = self
            .volume_ref(volume)
            .ok_or(spall_voxel::DigestError::NoRetained(coord))?;
        self.evicted(volume).verify_reload(vol, coord)?;
        self.evicted_mut(volume).clear(coord)?;
        Ok(())
    }

    /// Installs the durable brick source used by [`Self::reload_brick`].
    pub fn set_backing(&mut self, backing: std::sync::Arc<dyn crate::backing::BrickBacking>) {
        self.backing = Some(backing);
    }

    /// `true` once a durable brick source is installed.
    pub fn has_backing(&self) -> bool {
        self.backing.is_some()
    }

    /// Reloads one evicted brick's cells from the backing, reinstalls it,
    /// verifies it against the retained digest, and drops the digest. `Ok(true)`
    /// if the brick is now resident (including "was never evicted"); `Ok(false)`
    /// if no backing is installed or the durable record is unavailable — the
    /// caller then rejects the edit with a bounded explicit failure.
    pub fn reload_brick(
        &mut self,
        volume: VolumeId,
        coord: BrickCoord,
    ) -> Result<bool, spall_voxel::DigestError> {
        if !self.evicted(volume).contains(coord) {
            return Ok(true);
        }
        let Some(backing) = self.backing.clone() else {
            return Ok(false);
        };
        // Validate the candidate before publication. A wrong backing record
        // must leave the brick nonresident and the retained digest intact;
        // otherwise the logical view contains the same key twice and later
        // hash/conservation queries fail.
        let brick = match backing.load(volume, coord) {
            crate::backing::BackingBrick::Loaded(brick) => brick,
            crate::backing::BackingBrick::KnownEmpty { revision, edited } => {
                spall_voxel::Brick::restored(
                    &[spall_core::MaterialId::AIR; spall_core::CELLS_PER_BRICK],
                    revision,
                    edited,
                )
            }
            crate::backing::BackingBrick::Unavailable => return Ok(false),
        };
        self.evicted(volume).verify_candidate(coord, &brick)?;
        self.volume_body_mut(volume)
            .ok_or(spall_voxel::DigestError::NoRetained(coord))?
            .volume
            .insert_brick(coord, brick)?;
        self.evicted_mut(volume).clear(coord)?;
        Ok(true)
    }

    /// Reloads every coord in `coords`. Returns `Ok(true)` only if every one is
    /// now resident.
    pub fn reload_bricks(
        &mut self,
        volume: VolumeId,
        coords: impl IntoIterator<Item = BrickCoord>,
    ) -> Result<bool, spall_voxel::DigestError> {
        let mut all = true;
        for coord in coords {
            all &= self.reload_brick(volume, coord)?;
        }
        Ok(all)
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
    /// extracted pose / velocity / sleep state from the solver. Dormant bodies
    /// (T21) have no physics body and are skipped — their stored pose stays
    /// authoritative until [`Self::reactivate_body`].
    pub fn step_physics(&mut self) {
        self.physics.step();
        let physics = &self.physics;
        for body in self.bodies.values_mut() {
            if body.dormant {
                continue;
            }
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

    /// The detached body owning physics handle `phys`, if any (never terrain).
    /// Used by T21 contact-damage conversion to map a solver contact back to an
    /// authoritative entity.
    pub fn body_by_phys(&self, phys: spall_physics::BodyId) -> Option<&Body> {
        self.bodies.values().find(|b| b.phys == phys)
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

    // --- players (T19) ----------------------------------------------------

    /// Registers an authoritative player capsule standing at `feet_m`. `entity`
    /// is the caller's reserved-band id ([`spall_core::player_entity_for`]).
    /// Replaces any existing player with the same id.
    pub fn add_player(
        &mut self,
        entity: EntityId,
        feet_m: [f64; 3],
        params: CharacterParams,
    ) -> EntityId {
        self.players
            .insert(entity.get(), Player::new(entity, params, feet_m));
        entity
    }

    /// Removes a player capsule (its connection dropped).
    pub fn remove_player(&mut self, entity: EntityId) {
        self.players.remove(&entity.get());
    }

    pub fn player(&self, entity: EntityId) -> Option<&Player> {
        self.players.get(&entity.get())
    }

    pub fn players(&self) -> impl Iterator<Item = &Player> {
        self.players.values()
    }

    pub fn player_count(&self) -> usize {
        self.players.len()
    }

    /// Accepts one validated input frame for a player. Returns `false` for an
    /// unknown player or a stale / duplicate / non-finite frame.
    pub fn set_player_input(
        &mut self,
        entity: EntityId,
        input: PlayerInput,
        seq: InputSeq,
    ) -> bool {
        match self.players.get_mut(&entity.get()) {
            Some(player) => player.accept_input(input, seq),
            None => false,
        }
    }

    /// Advances every player capsule one fixed step against the current collider
    /// world with the deterministic [`step_character`] kernel.
    ///
    /// `invalidation_boxes` are the world-space AABBs (metres) of the cells
    /// touched by transactions that committed this tick
    /// ([`crate::player::transaction_world_box`]). A player within
    /// [`crate::player::INVALIDATION_MARGIN_M`] of one has its `movement_epoch`
    /// bumped and is depenetrated before the normal step, so the authoritative
    /// capsule is never left inside new solid or hovering over removed floor and
    /// the client knows to rebuild prediction from the next snapshot.
    pub fn advance_players(&mut self, dt_s: f32, invalidation_boxes: &[([f64; 3], [f64; 3])]) {
        let physics = &self.physics;
        for player in self.players.values_mut() {
            if invalidation_boxes
                .iter()
                .any(|(lo, hi)| player.near_world_box(*lo, *hi))
            {
                player.movement_epoch = player.movement_epoch.wrapping_add(1);
                let mv =
                    physics.sweep_character(player.params, player.state.position_m, [0.0; 3], dt_s);
                player.state.position_m = [
                    player.state.position_m[0] + f64::from(mv.translation_m[0]),
                    player.state.position_m[1] + f64::from(mv.translation_m[1]),
                    player.state.position_m[2] + f64::from(mv.translation_m[2]),
                ];
                player.state.grounded = mv.grounded;
                player.state.velocity_m_s = [0.0; 3];
            }

            let input = player.effective_input();
            let params = player.params;
            player.state = step_character(player.state, input, dt_s, |pos, desired| {
                physics.sweep_character(params, pos, desired, dt_s)
            });

            // Bounded-fixture safety net: a capsule that leaves the world (bad
            // input math, a floor pulled out from under it with nothing below)
            // is reset to its spawn rather than integrated to infinity.
            if !player.state.is_finite() || player.state.position_m[1] < -100.0 {
                player.state = player.spawn;
                player.movement_epoch = player.movement_epoch.wrapping_add(1);
            }

            player.age_input();
        }
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
        let plan = plan_collider(&grid)?;
        let cell_size_m = volume.cell_size().metres();
        let cell_m = cell_size_m as f32;
        // Mass / COM / inertia from the exact fine grid at the requested bulk
        // density, so a coarsened collision shape cannot inflate the mass.
        let mass_properties =
            analytic_mass_properties(&grid, cell_size_m, |_| f64::from(density_kg_m3))
                .to_body_properties();
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
            mass_properties: Some(mass_properties),
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
            dormant: false,
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

    /// Retires the ownership of `volume` because its authoritative geometry
    /// became empty (`ENG-56`), atomically with the edit that emptied it:
    ///
    /// * a **detached body** is dropped from the world — its physics rigid body
    ///   and collider are removed, and it is no longer enumerable, targetable by
    ///   a raycast, or published in a motion batch;
    /// * **terrain** keeps its (now empty) record but loses its physical
    ///   collider, so nothing rests on or tunnels the obsolete solid shape.
    ///
    /// Idempotent; a no-op for an unknown volume.
    pub fn retire_empty_volume(&mut self, volume: VolumeId) {
        if volume == self.terrain.volume_id {
            self.physics.remove_collider(self.terrain.phys);
            self.terrain.collider_revision += 1;
            return;
        }
        let Some(&entity) = self.volume_owner.get(&volume.get()) else {
            return;
        };
        if let Some(body) = self.bodies.remove(&entity) {
            self.physics.retire_body(body.phys);
        }
        self.volume_owner.remove(&volume.get());
    }

    // --- dormancy (T21) ---------------------------------------------------
    //
    // Deactivating a settled body drops its physics rigid body / collider to
    // save step cost while keeping the authoritative `Body` record intact
    // (volume, pose, damage, identity) — so `world_hash`, conservation, and the
    // checkpoint set are all unchanged. It must be reactivated before any edit
    // targeting it or any nearby interaction.

    /// Number of bodies currently dormant.
    pub fn dormant_body_count(&self) -> usize {
        self.bodies.values().filter(|b| b.dormant).count()
    }

    /// Whether the detached body `entity` is dormant.
    pub fn body_is_dormant(&self, entity: EntityId) -> bool {
        self.bodies.get(&entity.get()).is_some_and(|b| b.dormant)
    }

    /// Deactivates a settled detached body: its physics rigid body and collider
    /// are removed, its record is frozen (`sleeping = true`, zero velocity), and
    /// its pose stays authoritative. Returns `false` for terrain, an unknown
    /// body, or one already dormant.
    pub fn deactivate_body(&mut self, entity: EntityId) -> bool {
        let Some(body) = self.bodies.get_mut(&entity.get()) else {
            return false;
        };
        if body.kind != BodyKind::Dynamic || body.dormant {
            return false;
        }
        let phys = body.phys;
        body.dormant = true;
        body.sleeping = true;
        body.linvel_m_s = [0.0; 3];
        body.angvel_rad_s = [0.0; 3];
        self.physics.deactivate_body(phys);
        true
    }

    /// Restores a dormant body to the physics world at its stored pose (awake;
    /// the solver re-sleeps it on the next quiet step). Returns `false` for an
    /// unknown or non-dormant body, or if its volume could not be gridded.
    pub fn reactivate_body(&mut self, entity: EntityId) -> bool {
        let Some(body) = self.bodies.get(&entity.get()) else {
            return false;
        };
        if !body.dormant {
            return false;
        }
        let phys = body.phys;
        let grid = match OccupancyGrid::from_volume(&body.volume) {
            Ok(Some(grid)) => grid,
            _ => return false,
        };
        let t = body.pose.translation_m;
        let trans = [t[0] as f32, t[1] as f32, t[2] as f32];
        let r = body.pose.rotation;
        let rot = [r.x as f32, r.y as f32, r.z as f32, r.w as f32];
        self.physics
            .reactivate_body(phys, &grid, trans, rot, [0.0; 3], [0.0; 3]);
        if let Some(body) = self.bodies.get_mut(&entity.get()) {
            body.dormant = false;
        }
        true
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
        let plan = plan_collider(&grid)?;
        let cell_size_m = spec.volume.cell_size().metres();
        let cell_m = cell_size_m as f32;
        // Re-derive the exact mass properties from the persisted fine material
        // grid, so a restored body carries the same mass / shifted COM / inertia
        // it had before the save — never the collision shape's.
        let mass_properties =
            analytic_mass_properties(&grid, cell_size_m, |m| self.density(m)).to_body_properties();
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
            mass_properties: Some(mass_properties),
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
            dormant: false,
            collider_revision: spec.collider_revision,
            coarsen_k: plan.coarsen_k,
            phys,
            collider_region: spec.collider_region,
        };
        self.volume_owner.insert(body.volume_id.get(), entity.get());
        self.bodies.insert(entity.get(), body);
        Ok(entity)
    }

    /// Installs a replay-reconstructed child body: the fine-grid material mass
    /// (so its Rapier mass / COM / inertia match the live split), a collider
    /// region from its occupancy grid, and the participant snapshot's pose /
    /// velocity / sleep. Shared by the `SplitOff` (inline `CellRun`) and
    /// `SplitOffBaseline` (compressed blob) replay paths.
    fn install_replayed_child(
        &mut self,
        child_entity: EntityId,
        child: Volume,
        src_cell_size: spall_core::CellSizeCode,
        participants: &[MotionSnapshot],
    ) -> Result<(), WorldError> {
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

        // Representative density from the fine voxel grid's material mass,
        // exactly as the live split does (`transfer::plan_child` + `commit`):
        // mass / (solid-cell count * cell_m^3).
        let cell_m = src_cell_size.metres();
        let mp = analytic_mass_properties(&grid, cell_m, |m| self.density(m));
        let cube_m3 = cell_m.powi(3);
        let cell_count = solid_cells(&child) as f64;
        let density_kg_m3 = if cell_count > 0.0 && cube_m3 > 0.0 {
            (mp.mass_kg / (cell_count * cube_m3)) as f32
        } else {
            1.0
        };

        self.insert_restored_body(RestoredBody {
            entity: child_entity,
            volume: child,
            pose,
            linvel_m_s: linvel,
            angvel_rad_s: angvel,
            sleeping,
            collider_revision: 1,
            collider_region: region,
            density_kg_m3,
        })?;
        Ok(())
    }

    /// Rebuild a split child `Volume` from a [`spall_protocol::baseline::BaselineVolume`]
    /// (inline blob, T17 increment 1, or the out-of-band bulk world, increment 2)
    /// and install it as a body. Shared by both replay paths.
    fn install_baseline_child(
        &mut self,
        child_entity: EntityId,
        child_volume_id: VolumeId,
        bv: &spall_protocol::baseline::BaselineVolume,
        participants: &[MotionSnapshot],
        touched: &mut Vec<VolumeId>,
    ) -> Result<(), WorldError> {
        let child = crate::replication::volume_from_baseline(bv)
            .map_err(|e| WorldError::ReplayResultHash(e.to_string()))?;
        let cs = child.cell_size();
        self.install_replayed_child(child_entity, child, cs, participants)?;
        touched.push(child_volume_id);
        Ok(())
    }

    /// Overwrite `source`'s named bricks with the post-cut authoritative state
    /// from a [`spall_protocol::baseline::BaselineVolume`]. Shared by the inline
    /// (`SourcePatchBaseline`) and bulk (`SourcePatchBulkBaseline`) replay paths.
    fn patch_source_from_baseline(
        &mut self,
        source: VolumeId,
        bv: &spall_protocol::baseline::BaselineVolume,
        touched: &mut Vec<VolumeId>,
    ) -> Result<(), WorldError> {
        use spall_protocol::baseline::BaselineCells;
        let vol = self
            .volume_body_mut(source)
            .ok_or(WorldError::UnknownVolume(source))?;
        for bb in &bv.bricks {
            let cells: Vec<MaterialId> = match &bb.cells {
                BaselineCells::Uniform(id) => vec![MaterialId(*id); spall_core::CELLS_PER_BRICK],
                BaselineCells::Dense(raw) => raw.iter().copied().map(MaterialId).collect(),
            };
            vol.volume
                .insert_brick(
                    BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]),
                    Brick::restored(&cells, Revision(bb.revision), bb.edited),
                )
                .map_err(|e| {
                    WorldError::ReplayResultHash(format!("source patch brick {:?}: {e}", bb.coord))
                })?;
        }
        if !touched.contains(&source) {
            touched.push(source);
        }
        Ok(())
    }

    /// Applies the ops of one journalled [`spall_protocol::TopologyTransaction`]
    /// to the live world during recovery: brush / cell-run writes go to their
    /// named volume, and each `SplitOff` child is built from its canonical fill
    /// runs and installed as a body. The child keeps the **source volume's**
    /// cell size, the fine-grid material mass (so its Rapier mass, centre of
    /// mass, and inertia match the live split), and the participant snapshot's
    /// pose / velocity / sleep. Existing participants — the cut parent of a
    /// body-to-body split — are advanced to the same transaction frame so
    /// recovery never pairs a checkpoint-frame parent with split-frame
    /// children. Colliders of every touched volume are rebuilt; the caller
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
        bulk: Option<&spall_protocol::baseline::BaselineWorld>,
    ) -> Result<(), WorldError> {
        use spall_protocol::TopologyOp;

        // T17 increment 2: a giant split's `SplitOffBulkBaseline` /
        // `SourcePatchBulkBaseline` markers get their geometry from this
        // out-of-band world, delivered as a `spall_store` `TopologyBulkSplit`
        // journal payload (or the bulk transfer, on a live replica).
        let bulk_volume =
            |id: VolumeId| -> Result<&spall_protocol::baseline::BaselineVolume, WorldError> {
                bulk.and_then(|w| w.volumes.iter().find(|v| v.volume_id == id))
                    .ok_or_else(|| {
                        WorldError::ReplayResultHash(format!(
                            "transaction {} bulk split baseline is missing volume {id}",
                            tx.transaction_id.get()
                        ))
                    })
            };

        self.check_replay_preconditions(tx)?;

        let terrain_cell_size = self.terrain.volume.cell_size();
        let mut touched: Vec<VolumeId> = Vec::new();
        let mut new_children: Vec<EntityId> = Vec::new();
        let mut split = false;

        // Group consecutive same-volume cell writes so each volume is edited
        // once. A `SplitOff` opens a new child group carrying its source volume;
        // its following `CellRun`s (same child volume) fill it.
        struct Group {
            volume: VolumeId,
            new_child: Option<EntityId>,
            source: Option<VolumeId>,
            writes: Vec<(GlobalCell, MaterialId)>,
        }
        let flush = |world: &mut SimWorld,
                     group: Option<Group>,
                     touched: &mut Vec<VolumeId>,
                     new_children: &mut Vec<EntityId>|
         -> Result<(), WorldError> {
            let Some(g) = group else { return Ok(()) };
            if let Some(child_entity) = g.new_child {
                if g.writes.is_empty() {
                    return Err(WorldError::EmptyBody);
                }
                // The child body lives in the source volume's cell frame, not
                // necessarily the terrain's (a detail-cell body cut is finer).
                let src_cell_size = g
                    .source
                    .and_then(|s| world.volume_ref(s).map(|v| v.cell_size()))
                    .unwrap_or(terrain_cell_size);

                let mut min = BrickCoord::new(i64::MAX, i64::MAX, i64::MAX);
                let mut max = BrickCoord::new(i64::MIN, i64::MIN, i64::MIN);
                for (cell, _) in &g.writes {
                    let b = cell.split().0;
                    min = BrickCoord::new(min.x.min(b.x), min.y.min(b.y), min.z.min(b.z));
                    max = BrickCoord::new(max.x.max(b.x), max.y.max(b.y), max.z.max(b.z));
                }
                let bounds = BrickBounds::new(min, max).expect("min <= max by construction");
                let mut child = Volume::bounded(g.volume, src_cell_size, bounds);
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

                world.install_replayed_child(child_entity, child, src_cell_size, participants)?;
                new_children.push(child_entity);
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

        let mut group: Option<Group> = None;
        for op in &tx.ops {
            match op {
                TopologyOp::IntegerBrush {
                    volume,
                    brush,
                    material,
                } => {
                    flush(self, group.take(), &mut touched, &mut new_children)?;
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
                    source,
                    child,
                    child_entity,
                } => {
                    flush(self, group.take(), &mut touched, &mut new_children)?;
                    split = true;
                    group = Some(Group {
                        volume: *child,
                        new_child: Some(*child_entity),
                        source: Some(*source),
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
                        flush(self, group.take(), &mut touched, &mut new_children)?;
                        group = Some(Group {
                            volume: *volume,
                            new_child: None,
                            source: None,
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
                // T17: an oversized split's child geometry, as a compressed
                // `BaselineVolume` inline (increment 1) or from the out-of-band
                // bulk world (increment 2). Rebuild + install exactly like the
                // inline `SplitOff` + `CellRun` path.
                TopologyOp::SplitOffBaseline {
                    child,
                    child_entity,
                    blob,
                    ..
                } => {
                    flush(self, group.take(), &mut touched, &mut new_children)?;
                    split = true;
                    let bv = spall_protocol::baseline::BaselineVolume::decode_compressed(blob)
                        .map_err(|e| {
                            WorldError::ReplayResultHash(format!(
                                "split baseline blob for volume {}: {e}",
                                child.get()
                            ))
                        })?;
                    self.install_baseline_child(
                        *child_entity,
                        *child,
                        &bv,
                        participants,
                        &mut touched,
                    )?;
                    new_children.push(*child_entity);
                }
                TopologyOp::SplitOffBulkBaseline {
                    child,
                    child_entity,
                    ..
                } => {
                    flush(self, group.take(), &mut touched, &mut new_children)?;
                    split = true;
                    let bv = bulk_volume(*child)?;
                    self.install_baseline_child(
                        *child_entity,
                        *child,
                        bv,
                        participants,
                        &mut touched,
                    )?;
                    new_children.push(*child_entity);
                }
                // T17: the source side of an oversized split — overwrite the
                // named source bricks with their post-cut authoritative state.
                TopologyOp::SourcePatchBaseline { source, blob } => {
                    flush(self, group.take(), &mut touched, &mut new_children)?;
                    let bv = spall_protocol::baseline::BaselineVolume::decode_compressed(blob)
                        .map_err(|e| {
                            WorldError::ReplayResultHash(format!(
                                "source patch baseline blob for volume {}: {e}",
                                source.get()
                            ))
                        })?;
                    self.patch_source_from_baseline(*source, &bv, &mut touched)?;
                }
                TopologyOp::SourcePatchBulkBaseline { source, .. } => {
                    flush(self, group.take(), &mut touched, &mut new_children)?;
                    let bv = bulk_volume(*source)?;
                    self.patch_source_from_baseline(*source, bv, &mut touched)?;
                }
            }
        }
        flush(self, group.take(), &mut touched, &mut new_children)?;

        for &vid in &touched {
            self.rebuild_volume_collider(vid)?;
        }

        // Advance every *existing* participant (notably the cut parent of a
        // body-to-body split) to the transaction frame. `apply_pose_batch`
        // skips ids it does not own, so terrain parents and the just-built
        // children (already posed from their own snapshot) are left alone.
        let carry: Vec<MotionSnapshot> = participants
            .iter()
            .filter(|s| !new_children.contains(&s.body))
            .cloned()
            .collect();
        if !carry.is_empty() {
            self.apply_pose_batch(&carry);
        }

        if split {
            self.bump_topology_epoch();
        }

        self.check_replay_results(tx)?;

        // Retire any volume this transaction cleared to empty, now that its
        // verified post-state has been checked against the record. This mirrors
        // the live commit path so a recovered world has the same set of live
        // bodies and colliders (`ENG-56`).
        for vid in touched {
            if self.volume_ref(vid).is_some_and(|v| solid_cells(v) == 0) {
                self.retire_empty_volume(vid);
            }
        }
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
    /// post-replay). No-op if the volume has no solid cell left — an emptied
    /// volume is retired by [`Self::retire_empty_volume`] once the replay's
    /// result checks have run (`ENG-56`).
    pub fn rebuild_volume_collider(&mut self, volume: VolumeId) -> Result<(), WorldError> {
        let Some(body) = self.volume_body(volume) else {
            return Err(WorldError::UnknownVolume(volume));
        };
        let phys = body.phys;
        let is_dynamic = body.kind == BodyKind::Dynamic;
        let cell_size_m = body.volume.cell_size().metres();
        let Some(grid) = OccupancyGrid::from_volume(&body.volume)? else {
            return Ok(());
        };
        let plan = plan_collider(&grid)?;
        self.physics
            .rebuild_collider(phys, &plan.grid, plan.representation);
        if is_dynamic {
            // The geometry changed: reinstall mass / COM / inertia from the new
            // fine material grid so the solver tracks it (and never the coarse
            // collider).
            let mass_properties = analytic_mass_properties(&grid, cell_size_m, |m| self.density(m))
                .to_body_properties();
            self.physics.set_mass_properties(phys, mass_properties);
        }
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
        // Over the *logical* brick set (resident ∪ retained evicted digests), so
        // the value never moves when cache contents differ. Identical to
        // `canonical_volume_for` when nothing is evicted.
        Some(
            canonical_logical_volume_for(v, self.evicted(volume), owner)
                .expect("logical volume: resident/evicted digest invariant holds"),
        )
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
    /// invariant's left-hand side over the whole world. Over the **logical**
    /// brick set (resident cells + retained evicted digests' `solid_cells`), so
    /// eviction never changes the conservation accounting. Identical to a
    /// resident-only walk when nothing is evicted.
    pub fn total_solid_cells(&self) -> u64 {
        let logical = |vol: &Volume, vid: VolumeId| {
            spall_voxel::logical_solid_cells(vol, self.evicted(vid))
                .expect("logical solid count: resident/evicted digest invariant holds")
        };
        let mut total = logical(&self.terrain.volume, self.terrain.volume_id);
        for body in self.bodies.values() {
            total += logical(&body.volume, body.volume_id);
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

/// [`canonical_volume_for`] over the **logical** brick set (T23 / G3 row 7):
/// resident bricks plus retained evicted digests, each key once, canonical
/// order. Byte-identical to `canonical_volume_for` when `evicted` is empty, and
/// to the *pre-eviction* full volume for any subset of clean bricks moved into
/// `evicted` — so cache placement never moves the topology hash.
pub fn canonical_logical_volume_for(
    volume: &Volume,
    evicted: &spall_voxel::EvictedBricks,
    owner: CanonicalOwner,
) -> Result<CanonicalVolume, spall_voxel::DigestError> {
    let bricks = spall_voxel::logical_bricks(volume, evicted)?
        .into_iter()
        .map(|b| CanonicalBrick {
            coord: b.coord,
            revision: b.revision,
            layers: vec![CanonicalLayer {
                kind: MATERIAL_LAYER_KIND,
                bytes: spall_voxel::BrickHash::to_bytes(b.content_hash).to_vec(),
            }],
        })
        .collect();
    Ok(CanonicalVolume {
        volume_id: volume.id(),
        cell_size: volume.cell_size(),
        owner,
        bricks,
    })
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

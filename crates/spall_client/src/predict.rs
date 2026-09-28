//! Client-side player prediction and reconciliation (T19).
//!
//! `docs/protocol.md`: "The local client keeps a bounded input/state history,
//! predicts capsule movement, and replays unacknowledged inputs after an
//! authoritative correction. Collision topology changes invalidate affected
//! history: restore the authoritative player state and rebuild prediction
//! against a known revision."
//!
//! [`PredictedPlayer`] runs the *same* [`spall_physics::step_character`] kernel
//! the server runs, against [`ClientPhysics`] — a physics world holding replica
//! terrain plus query-only mirrors of detached bodies. Because physics is not lockstep,
//! the predicted state drifts from the authoritative one; [`PredictedPlayer::reconcile`]
//! snaps to each snapshot and replays the still-unacknowledged inputs, and the
//! residual is reported as a bounded correction.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;
use spall_core::{BrickCoord, EntityId, GlobalCell, MaterialId, PlayerInput, Tick, units::Pose};
pub use spall_physics::WindowStats;
use spall_physics::{
    BodyId, BodyKind, BodySpec, CharacterMove, CharacterParams, CharacterQueryCache,
    CharacterState, OccupancyGrid, PhysicsConfig, PhysicsWorld, Representation,
    analytic_mass_properties, step_character,
};
use spall_protocol::InputSeq;
use spall_voxel::{Sample, Volume};

/// Bounded predicted-input history depth.
pub const PREDICTION_HISTORY: usize = 128;
/// Terrain cell size, metres (the 0.25 m world grid).
pub const CELL_M: f32 = 0.25;
/// Cells per brick edge (`docs/architecture.md`'s fixed brick size).
const BRICK_CELLS: i64 = 32;

/// Must equal `spall_sim::PLINKO_RESTITUTION` / `SHOWCASE_RESTITUTION` (the
/// server's values; this crate cannot depend on `spall_sim`) so client-
/// authoritative and server-authoritative playgrounds bounce identically.
const PLINKO_RESTITUTION: f32 = 0.45;
const SHOWCASE_RESTITUTION: f32 = 0.15;

/// One replicated detached body as seen by the prediction collision mirror.
/// Geometry is body-local; `pose` places it in the same world frame used by
/// rendering and authoritative motion snapshots.
#[derive(Clone)]
pub(crate) struct ClientBodyCollision {
    pub entity: EntityId,
    pub topology_version: u64,
    /// Present only when this body is new or its topology version changed.
    /// Ordinary motion ticks update just the lightweight pose.
    pub volume: Option<Volume>,
    /// The pose in the body's newest snapshot, taken at `motion.snapshot_tick`.
    pub pose: Pose,
    pub motion: BodyMotion,
}

/// What a body's newest snapshot says about how it is moving, so its pose can
/// be advanced to whichever server tick a prediction step is simulating.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct BodyMotion {
    /// Server tick `ClientBodyCollision::pose` was sampled at.
    pub snapshot_tick: u64,
    pub linear_velocity_m_s: [f32; 3],
    pub angular_velocity_rad_s: [f32; 3],
    pub sleeping: bool,
}

impl BodyMotion {
    /// A body that never moves (tests, sleeping bodies at a fixed pose).
    #[cfg(test)]
    pub const STATIC: Self = Self {
        snapshot_tick: 0,
        linear_velocity_m_s: [0.0; 3],
        angular_velocity_rad_s: [0.0; 3],
        sleeping: false,
    };
}

/// Farthest a snapshot pose is advanced (either direction) to reach a
/// simulated tick. A body the server last reported more than this long ago is
/// held at the extrapolation limit rather than flung along a stale velocity.
const MAX_BODY_EXTRAPOLATION_TICKS: f64 = 10.0;

impl ClientBodyCollision {
    /// This body's pose at server tick `tick`: the snapshot pose advanced along
    /// its reported linear velocity and, for orientation, rotated about its
    /// angular velocity. Sleeping bodies stay put. Deliberately the same
    /// constant-velocity model for translation and rotation, so a rolling or
    /// tumbling box's collision shape turns with it instead of translating with
    /// a frozen orientation.
    ///
    /// Limits: no contact response (a body that is about to bounce or be
    /// pushed is advanced as if free), bounded by
    /// [`MAX_BODY_EXTRAPOLATION_TICKS`].
    pub fn pose_at(&self, tick: f64) -> Pose {
        let m = self.motion;
        let ticks = (tick - m.snapshot_tick as f64)
            .clamp(-MAX_BODY_EXTRAPOLATION_TICKS, MAX_BODY_EXTRAPOLATION_TICKS);
        crate::replica::advance_pose(
            &self.pose,
            m.linear_velocity_m_s,
            m.angular_velocity_rad_s,
            m.sleeping,
            ticks,
            60.0,
        )
    }
}

struct MirroredBody {
    physics: BodyId,
    topology_version: u64,
    grid: OccupancyGrid,
    staged_translation_m: [f32; 3],
    staged_rotation: [f32; 4],
    active: bool,
    emitter: Option<LocalEmitter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalEmitter {
    Showcase,
    Plinko,
}

/// A physics world that mirrors replica terrain and detached-body colliders, so
/// the predictor sweeps the capsule against the geometry the server used.
pub struct ClientPhysics {
    world: PhysicsWorld,
    terrain: Option<BodyId>,
    /// Query-only fixed mirrors of authoritative detached bodies. Their poses
    /// are refreshed from motion snapshots; the client never simulates them.
    bodies: BTreeMap<u64, MirroredBody>,
    /// Bricks the collider currently installed on `terrain` was actually built
    /// from, as of the last [`set_terrain`](Self::set_terrain) — empty
    /// whenever nothing is resident yet. Residency streams bricks
    /// independently of each other, so this set is routinely non-contiguous
    /// for a step or two while the player walks (one trailing brick evicted,
    /// one leading brick still mid-reload); [`covers`](Self::covers) is what
    /// tells a caller whether *the player's own* brick is actually one of
    /// them, as opposed to merely "some geometry exists somewhere".
    resident_bricks: BTreeSet<BrickCoord>,
    /// ENG-69 round 18: [`Self::sweep`]'s own small window, tried before
    /// falling back to `terrain` (`spall_physics::query_cache`'s own doc has
    /// the full design/rationale — this is the client half of the same fix
    /// `spall_sim::world::SimWorld::advance_players` applies server-side).
    /// Kept *alongside* `terrain`, not instead of it: a window build fails
    /// outright (rather than silently approximating) whenever its margin
    /// would reach a cell the replica hasn't streamed in yet — common near
    /// the edge of the client's own residency radius, an everyday case here
    /// in a way it mostly isn't for the server's complete-knowledge terrain
    /// — so `terrain`'s existing, already-validated full-resident-set
    /// collider stays as the fallback for exactly that case.
    query_cache: CharacterQueryCache,
    /// Bumped every [`Self::set_terrain`] call — the `revision` token
    /// [`Self::query_cache`] uses to know its window is stale even when the
    /// player hasn't moved far enough to cross it on position alone (an
    /// edit landing inside an otherwise-unmoved window).
    revision: u64,
    /// Live counters proving [`Self::query_cache`] is actually in the
    /// sweep path, not just present and unused — ENG-69 round 18 asked for
    /// this explicitly after the round-17 prototype never got wired to
    /// anything live. Surfaced through [`Self::window_stats`] to the
    /// interactive HUD (`window.rs`'s `Hud::report`). The type itself lives
    /// in `spall_physics::query_cache` (re-exported here) so
    /// `spall_sim::world::SimWorld` can track the same shape server-side —
    /// see its own `window_stats` field.
    window_stats: WindowStats,
    /// Testing-only mode: detached bodies are dynamic and stepped here; server
    /// motion snapshots seed topology/initial poses but never correct them.
    client_authoritative: bool,
    local_tick: u64,
}

impl Default for ClientPhysics {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientPhysics {
    pub fn new() -> Self {
        Self {
            // No client body enables CCD, and this world's colliders (terrain
            // window, bodies) are rebuilt constantly; rapier's CCD pass can
            // then hit a stale proxy and panic ("No element at index") on the
            // physics thread -- see `PhysicsConfig::disable_ccd`.
            world: PhysicsWorld::new(PhysicsConfig {
                disable_ccd: true,
                ..PhysicsConfig::default()
            }),
            terrain: None,
            bodies: BTreeMap::new(),
            resident_bricks: BTreeSet::new(),
            query_cache: CharacterQueryCache::new(),
            revision: 0,
            window_stats: WindowStats::default(),
            client_authoritative: false,
            local_tick: 0,
        }
    }

    /// Enables the deliberately non-network-correct local physics sandbox.
    pub fn set_client_authoritative(&mut self, enabled: bool) {
        self.client_authoritative = enabled;
    }

    /// Live proof [`Self::query_cache`] is actually serving sweeps, not
    /// just present — see [`WindowStats`]'s own fields.
    pub fn window_stats(&self) -> WindowStats {
        self.window_stats
    }

    /// (Re)builds the terrain collider from `volume`. The caller gates this on a
    /// cheap dirty check (`ReplicaWorld::terrain_hash`).
    ///
    /// Unlike [`OccupancyGrid::from_volume`], this tolerates a *non-contiguous*
    /// resident set: it still takes the tight bounding box of every resident
    /// solid cell, but a cell whose brick is not (yet) resident is treated as
    /// empty — no collision — rather than failing the whole extraction. Under
    /// eviction/reload churn the resident set routinely has a gap for a step
    /// or two (one trailing brick evicted, one leading brick still mid-reload
    /// after a lossy repair round trip); failing the entire collider over that
    /// gap would blank out geometry that *is* loaded and correct, well away
    /// from the gap. An empty resident set (no solid cell anywhere) still
    /// drops the collider entirely so the capsule falls through a
    /// fully-removed floor.
    ///
    /// Always builds [`Representation::NativeVoxels`], **not**
    /// [`spall_physics::choose_representation`]'s budget-based pick, even
    /// though that budget is exactly what `spall_sim::collider::plan_collider`
    /// uses for the server's own terrain body. ENG-69 round 7 tried matching
    /// the server's policy (commit `196d9ff`, since reverted in spirit here)
    /// on the theory that a representation *mismatch* was the bug; a live
    /// retest with the same terrain came back with an unchanged ~0.15-0.2 m
    /// horizontal-only correction, disproving it — the terrain's greedy box
    /// count apparently never crossed the budget on either side, so both
    /// sides were already building `MergedCuboids`.
    ///
    /// The real issue is more fundamental than *which* representation: this
    /// grid's extent is the tight bounding box of whatever bricks happen to
    /// be resident *right now* (see the non-contiguous-resident-set note
    /// above), while the server's grid for the same logical terrain spans the
    /// complete, fixed volume. `greedy_boxes`' box-growth loop only stops
    /// early where the *next cell* isn't solid — and at this grid's edge,
    /// "not solid" and "not yet loaded" are indistinguishable from inside the
    /// array. For any solid region larger than the client's own streaming
    /// radius (an ordinary large flat floor easily qualifies), a box that
    /// would run further on the server's complete grid gets cut short here,
    /// planting a seam the server's collider does not have — anywhere within
    /// the resident set, not only at its visible boundary, because greedy
    /// merging is a global, order-dependent process (an earlier box's extent
    /// changes what is left `consumed` for a later one). `MergedCuboids`
    /// resolves that seam as a real geometric discontinuity a sliding
    /// kinematic character can catch on. `NativeVoxels` has no internal
    /// seams at all — parry's `Voxels` shape suppresses contact response
    /// between connected adjacent voxels — so while its own resident-boundary
    /// edge is still an (unavoidable, partial-knowledge) approximation, nothing
    /// *interior* to what the client has already loaded can produce a false
    /// seam the player might be standing on or walking across. This is
    /// heavier to rebuild than a small `MergedCuboids` compound would be
    /// (cost scales with the resident cell count, not the box count — see
    /// `spall_sim::collider`'s own doc on the same trade-off), but rebuilds
    /// are already bounded by [`crate::residency`]'s / the replica's own
    /// resident-cell ceiling, and correctness here matters more than shaving
    /// that cost.
    pub fn set_terrain(&mut self, volume: &Volume) {
        self.resident_bricks = volume.resident_brick_coords().into_iter().collect();
        match lenient_occupancy(volume) {
            Some(grid) => match self.terrain {
                Some(id) => {
                    self.world
                        .rebuild_collider(id, &grid, Representation::NativeVoxels);
                }
                None => {
                    let id = self.world.add_body(BodySpec {
                        kind: BodyKind::Fixed,
                        representation: Representation::NativeVoxels,
                        grid,
                        cell_m: CELL_M,
                        density_kg_m3: 1.0,
                        mass_properties: None,
                        translation_m: [0.0; 3],
                        linvel_m_s: [0.0; 3],
                    });
                    self.terrain = Some(id);
                }
            },
            None => {
                if let Some(id) = self.terrain {
                    self.world.remove_collider(id);
                }
            }
        }
        // Refresh the broad-phase BVH so the next character sweep sees the new
        // collider (the sweep runs no physics step of its own).
        self.world.sync_queries();
        self.revision = self.revision.wrapping_add(1);
    }

    /// Mirrors all currently replicated detached bodies into the prediction
    /// query world. They are fixed/query-only locally because server motion is
    /// authoritative; the character sweep still collides with their exact
    /// voxel shapes at the newest interpolated pose.
    #[cfg(test)]
    pub(crate) fn sync_bodies(&mut self, snapshots: &[ClientBodyCollision]) {
        self.sync_bodies_at(snapshots, None);
    }

    /// [`Self::sync_bodies`], with each body advanced to server tick `tick`
    /// ([`ClientBodyCollision::pose_at`]) instead of held at its snapshot pose.
    /// Prediction and replay call this with the tick they are about to simulate
    /// so the character collides with bodies where the server has them *then*.
    pub(crate) fn sync_bodies_at(&mut self, snapshots: &[ClientBodyCollision], tick: Option<f64>) {
        let live: BTreeSet<u64> = snapshots.iter().map(|body| body.entity.get()).collect();
        let retired: Vec<u64> = self
            .bodies
            .keys()
            .copied()
            .filter(|entity| !live.contains(entity))
            .collect();
        for entity in retired {
            if let Some(body) = self.bodies.remove(&entity) {
                self.world.retire_body(body.physics);
            }
        }

        for snapshot in snapshots {
            let entity = snapshot.entity.get();
            let pose = tick.map_or(snapshot.pose, |t| snapshot.pose_at(t));
            let translation = pose.translation_m.map(|value| value as f32);
            let rotation = pose.rotation.to_unit().unwrap_or([0.0, 0.0, 0.0, 1.0]);

            if let Some(existing) = self.bodies.get_mut(&entity) {
                if existing.topology_version != snapshot.topology_version {
                    let Some(volume) = &snapshot.volume else {
                        continue;
                    };
                    let Ok(Some(grid)) = OccupancyGrid::from_volume(volume) else {
                        let removed = self.bodies.remove(&entity).expect("entry exists");
                        self.world.retire_body(removed.physics);
                        continue;
                    };
                    let representation = self.world.representation(existing.physics);
                    self.world
                        .rebuild_collider(existing.physics, &grid, representation);
                    if self.client_authoritative {
                        let mass_properties =
                            analytic_mass_properties(&grid, volume.cell_size().metres(), |_| {
                                2_000.0
                            });
                        self.world.set_mass_properties(
                            existing.physics,
                            mass_properties.to_body_properties(),
                        );
                    }
                    existing.grid = grid;
                    existing.topology_version = snapshot.topology_version;
                }
                if !self.client_authoritative {
                    self.world
                        .set_body_pose(existing.physics, translation, rotation);
                }
                continue;
            }

            let Some(volume) = &snapshot.volume else {
                continue;
            };
            let Ok(Some(grid)) = OccupancyGrid::from_volume(volume) else {
                continue;
            };
            let local_dynamic = self.client_authoritative;
            let mass_properties = local_dynamic.then(|| {
                analytic_mass_properties(&grid, volume.cell_size().metres(), |_| 2_000.0)
                    .to_body_properties()
            });
            // Plinko balls are the only local bodies pushed and rolled around
            // by hand; a faceted voxel collider makes them catch and step, so
            // they get a smooth convex hull instead.
            let smooth = local_dynamic && translation[1] < -40.0 && translation[2] >= 20.0;
            let physics = self.world.add_body(BodySpec {
                kind: if local_dynamic {
                    BodyKind::Dynamic { ccd: false }
                } else {
                    BodyKind::Fixed
                },
                representation: if smooth {
                    Representation::SmoothConvex
                } else {
                    Representation::NativeVoxels
                },
                grid: grid.clone(),
                cell_m: volume.cell_size().metres() as f32,
                density_kg_m3: if local_dynamic { 2_000.0 } else { 1.0 },
                mass_properties,
                translation_m: translation,
                linvel_m_s: [0.0; 3],
            });
            self.world.set_body_pose(physics, translation, rotation);
            let emitter = if translation[1] < -40.0 {
                Some(if translation[2] >= 20.0 {
                    LocalEmitter::Plinko
                } else {
                    LocalEmitter::Showcase
                })
            } else {
                None
            };
            let active = !local_dynamic || emitter.is_none();
            if local_dynamic {
                self.world.set_restitution(
                    physics,
                    if emitter == Some(LocalEmitter::Plinko) {
                        PLINKO_RESTITUTION
                    } else {
                        SHOWCASE_RESTITUTION
                    },
                );
                if !active {
                    self.world.deactivate_body(physics);
                }
            } else {
                self.world.set_query_only(physics);
            }
            self.bodies.insert(
                entity,
                MirroredBody {
                    physics,
                    topology_version: snapshot.topology_version,
                    grid,
                    staged_translation_m: translation,
                    staged_rotation: rotation,
                    active,
                    emitter,
                },
            );
        }
        self.world.sync_queries();
    }

    fn release_next(&mut self, emitter: LocalEmitter, height_m: f32, restitution: f32) {
        let Some(entity) = self.bodies.iter().find_map(|(entity, body)| {
            (!body.active && body.emitter == Some(emitter)).then_some(*entity)
        }) else {
            return;
        };
        let body = self.bodies.get_mut(&entity).expect("selected body exists");
        body.staged_translation_m[1] = height_m;
        self.world.reactivate_body(
            body.physics,
            &body.grid,
            body.staged_translation_m,
            body.staged_rotation,
            [0.0; 3],
            [0.0; 3],
        );
        self.world.set_restitution(body.physics, restitution);
        body.active = true;
    }

    /// Advances testing-only local rigid-body authority by one fixed tick.
    pub fn step_client_authority(&mut self) {
        if !self.client_authoritative {
            return;
        }
        if self.local_tick.is_multiple_of(300) {
            self.release_next(LocalEmitter::Showcase, 7.0, SHOWCASE_RESTITUTION);
        }
        if self.local_tick.is_multiple_of(60) {
            self.release_next(LocalEmitter::Plinko, 10.5, PLINKO_RESTITUTION);
        }
        self.world.step();
        self.local_tick = self.local_tick.saturating_add(1);
    }

    /// Unquantized locally-simulated body states, for interpolated rendering.
    pub fn local_body_states(&self) -> BTreeMap<u64, crate::interactive::LocalPose> {
        if !self.client_authoritative {
            return BTreeMap::new();
        }
        self.bodies
            .iter()
            .map(|(entity, body)| {
                let (translation_m, rotation) = if body.active {
                    let state = self.world.body_state(body.physics);
                    (state.translation_m, state.rotation)
                } else {
                    (body.staged_translation_m, body.staged_rotation)
                };
                (
                    *entity,
                    crate::interactive::LocalPose {
                        translation_m: translation_m.map(f64::from),
                        rotation,
                    },
                )
            })
            .collect()
    }

    /// Latest locally-simulated poses for renderer override in testing mode.
    pub fn local_body_poses(&self) -> BTreeMap<u64, Pose> {
        if !self.client_authoritative {
            return BTreeMap::new();
        }
        self.bodies
            .iter()
            .map(|(entity, body)| {
                let (translation_m, rotation) = if body.active {
                    let state = self.world.body_state(body.physics);
                    (state.translation_m, state.rotation)
                } else {
                    (body.staged_translation_m, body.staged_rotation)
                };
                let q = spall_core::QuantizedQuat::from_unit(
                    rotation[0],
                    rotation[1],
                    rotation[2],
                    rotation[3],
                )
                .unwrap_or_else(|_| {
                    spall_core::QuantizedQuat::from_unit(0.0, 0.0, 0.0, 1.0).unwrap()
                });
                (
                    *entity,
                    Pose {
                        translation_m: translation_m.map(f64::from),
                        rotation: q,
                    },
                )
            })
            .collect()
    }

    /// Sweeps the capsule one tick, preferring [`Self::query_cache`]'s own
    /// small `NativeVoxels` window (real terrain excluded, since the window
    /// replaces it exactly) over `terrain`'s whole-resident-set collider —
    /// falling back to the latter, unfiltered, whenever the window can't be
    /// built this call (see [`Self::query_cache`]'s own doc for why that
    /// happens routinely here, unlike server-side). `volume` is the
    /// replica's current terrain — the caller already holds/clones it each
    /// tick for the existing dirty check, so this asks for nothing new.
    pub fn sweep(
        &mut self,
        volume: &Volume,
        params: CharacterParams,
        feet_m: [f64; 3],
        desired_m: [f32; 3],
        dt_s: f32,
    ) -> CharacterMove {
        if let Some(terrain_id) = self.terrain
            && let Ok((Some(_window_id), cost)) = self.query_cache.ensure_covers(
                &mut self.world,
                volume,
                CELL_M,
                feet_m,
                self.revision,
            )
        {
            self.window_stats.window_sweeps += 1;
            if cost.is_some() {
                self.window_stats.window_rebuilds += 1;
            }
            return if self.client_authoritative {
                self.world.sweep_character_pushing_excluding(
                    params,
                    feet_m,
                    desired_m,
                    dt_s,
                    &[terrain_id],
                )
            } else {
                self.world
                    .sweep_character_excluding(params, feet_m, desired_m, dt_s, &[terrain_id])
            };
        }
        // No window *this call* — either the cache couldn't build one (too
        // close to the residency edge) or it legitimately found no solid
        // cell nearby. Falls back to the whole-resident-set sweep, but must
        // also exclude `query_cache`'s own body if one still exists: a
        // failed `ensure_covers` returns early (`CharacterQueryCache::
        // rebuild`) *before* touching the cache's stored body, so a
        // previous call's successfully-built window can still be sitting in
        // `self.world`, unrefreshed, at a stale position — ENG-69 round 19
        // found this was never excluded from the fallback sweep, letting a
        // stale window silently double up with real terrain in the same
        // query.
        self.window_stats.terrain_fallbacks += 1;
        if let Some(stale_window_id) = self.query_cache.window_body_id() {
            return if self.client_authoritative {
                self.world.sweep_character_pushing_excluding(
                    params,
                    feet_m,
                    desired_m,
                    dt_s,
                    &[stale_window_id],
                )
            } else {
                self.world.sweep_character_excluding(
                    params,
                    feet_m,
                    desired_m,
                    dt_s,
                    &[stale_window_id],
                )
            };
        }
        if self.client_authoritative {
            self.world
                .sweep_character_pushing_excluding(params, feet_m, desired_m, dt_s, &[])
        } else {
            self.world.sweep_character(params, feet_m, desired_m, dt_s)
        }
    }

    pub fn has_terrain(&self) -> bool {
        self.terrain.is_some()
    }

    /// Whether the brick under `feet_m` was actually resident (and so part of
    /// the collider) as of the last [`set_terrain`]. `has_terrain` alone
    /// cannot tell "the player's own footing is loaded" from "some other,
    /// unrelated patch of the world is loaded" — the collider body persists
    /// across a residency gap so a later refill can rebuild it, and stays
    /// "present" throughout. A caller predicting movement should hold rather
    /// than extrapolate through a point this returns `false` for: it means
    /// the client does not yet know whether that ground is solid, not that it
    /// has confirmed open air.
    pub fn covers(&self, feet_m: [f64; 3]) -> bool {
        let cell_m = f64::from(CELL_M);
        let cell = GlobalCell::new(
            (feet_m[0] / cell_m).floor() as i64,
            (feet_m[1] / cell_m).floor() as i64,
            (feet_m[2] / cell_m).floor() as i64,
        );
        self.resident_bricks.contains(&cell.split().0)
    }
}

/// See [`ClientPhysics::set_terrain`]. `None` when no resident brick holds a
/// solid cell at all (nothing to collide with yet, or a genuinely empty
/// world); `Some` grid otherwise, built over the tight bounding box of every
/// resident solid cell with non-resident cells inside that box left empty.
fn lenient_occupancy(volume: &Volume) -> Option<OccupancyGrid> {
    let coords = volume.resident_brick_coords();
    let mut min = [i64::MAX; 3];
    let mut max = [i64::MIN; 3];
    for c in &coords {
        let base = [c.x * BRICK_CELLS, c.y * BRICK_CELLS, c.z * BRICK_CELLS];
        for lz in 0..BRICK_CELLS {
            for ly in 0..BRICK_CELLS {
                for lx in 0..BRICK_CELLS {
                    let cell = GlobalCell::new(base[0] + lx, base[1] + ly, base[2] + lz);
                    if let Ok(Sample::Filled(_)) = volume.sample(cell) {
                        min[0] = min[0].min(cell.x);
                        min[1] = min[1].min(cell.y);
                        min[2] = min[2].min(cell.z);
                        max[0] = max[0].max(cell.x);
                        max[1] = max[1].max(cell.y);
                        max[2] = max[2].max(cell.z);
                    }
                }
            }
        }
    }
    if min[0] > max[0] {
        return None;
    }
    let dims = [
        (max[0] - min[0] + 1) as u32,
        (max[1] - min[1] + 1) as u32,
        (max[2] - min[2] + 1) as u32,
    ];
    let cells = dims[0] as usize * dims[1] as usize * dims[2] as usize;
    let mut solid = vec![false; cells];
    let mut material = vec![MaterialId::AIR; cells];
    for gz in 0..dims[2] {
        for gy in 0..dims[1] {
            for gx in 0..dims[0] {
                let cell =
                    GlobalCell::new(min[0] + gx as i64, min[1] + gy as i64, min[2] + gz as i64);
                if let Ok(Sample::Filled(m)) = volume.sample(cell) {
                    let idx = (gx + dims[0] * (gy + dims[1] * gz)) as usize;
                    solid[idx] = true;
                    material[idx] = m;
                }
                // `Sample::Empty` and `Sample::Unknown` both leave this cell
                // as air: real air collides with nothing, and an unresident
                // cell must not be treated as solid either — but nor may it
                // fail the whole box the way `OccupancyGrid::from_region`
                // rightly does for callers who need a *complete* region (a
                // body's own geometry, a checkpoint capture). `covers` is
                // this module's answer to "was this cell actually resident",
                // kept separately from the grid itself.
            }
        }
    }
    OccupancyGrid::from_solid_mask(
        GlobalCell::new(min[0], min[1], min[2]),
        dims,
        solid,
        material,
    )
    .ok()
}

/// One individual predicted-vs-authoritative comparison — [`ReconcileOutcome::
/// comparison`], `Some` only when a `history` record was tagged for the
/// exact tick `authoritative` represents. See that field's own doc for what
/// it means when there isn't one.
#[derive(Debug, Clone, Copy)]
pub struct CorrectionEvent {
    /// The matched record's own input sequence — which locally-sent
    /// `InputFrame` this comparison actually used, for cross-referencing
    /// against a client-side send log (e.g. the `sent_log` pattern
    /// `crates/spall_client/tests/g1_realistic_input_timing_trace.rs` keeps).
    pub seq: InputSeq,
    /// Full 3-D distance between what was predicted for that tick and what
    /// the server actually reported, metres.
    pub error_m: f64,
    /// `|Y|` component of the same gap.
    pub vertical_m: f64,
    /// `XZ`-plane magnitude of the same gap.
    pub horizontal_m: f64,
    /// Whether the record's input was exactly [`PlayerInput::NEUTRAL`] (no
    /// movement, no buttons) — see [`PredictedPlayer::idle_corrections`].
    pub idle: bool,
}

/// Everything one [`PredictedPlayer::reconcile`] call actually did —
/// returned unconditionally, not only when a [`Self::comparison`] was
/// possible. ENG-69 round 21: an earlier version returned
/// `Option<CorrectionEvent>`, `None` whenever no `history` record could be
/// matched to `server_tick` — including the case where *every* record got
/// silently dropped and `predicted` hard-snapped straight onto
/// `authoritative`. A caller that only acts on `Some` cannot tell "a clean
/// small correction" from "a large silent resync just happened and there's
/// no comparison for it" — precisely the blind spot that let a synthetic,
/// single-process, lockstep test report "zero corrections" while a real,
/// independently-scheduled client's felt jitter visibly got worse: the test
/// never diverged from its own count-based assumption (client and server
/// ticked in the same loop iteration, always exactly once each), so it
/// never exercised the branch a real client's clock drift, stalls, or a
/// delayed first snapshot actually hit.
#[derive(Debug, Clone, Copy)]
pub struct ReconcileOutcome {
    pub server_tick: Tick,
    /// `server_tick` minus the previous call's `server_tick` (or minus
    /// `spawn_tick` on the first call), signed and *not* saturated —
    /// reported as-is so a real ordering bug (a snapshot arriving out of
    /// order) would show up as a negative delta instead of being clamped
    /// away and looking identical to "nothing happened yet".
    pub server_tick_delta: i64,
    /// `history.len()` before this call touched it.
    pub history_len_before: usize,
    /// Records dropped as "the server has already covered this tick" — not
    /// replayed.
    pub records_removed: usize,
    /// Records kept and re-simulated from `authoritative` — this call's
    /// actual replay depth. `history.len()` after is exactly this.
    pub records_replayed: usize,
    pub predicted_before: CharacterState,
    pub predicted_after: CharacterState,
    /// `Some` only when a `history` record was tagged for exactly
    /// `server_tick` (see `Record::tick`'s doc for the tagging/re-anchoring
    /// scheme). A hand-authored fixed-offset or single-process lockstep
    /// test harness hits this every time by construction; a real client
    /// does not — clock drift between two independently-scheduled ~60 Hz
    /// loops, a stall, a delayed first snapshot, or `PREDICTION_HISTORY`
    /// eviction can all legitimately leave no record at that exact tick.
    /// `None` here does **not** mean nothing happened: `records_removed`
    /// and `predicted_before` vs. `predicted_after` (or
    /// [`PredictedPlayer::unmatched_reconciles`]/
    /// `max_unmatched_displacement_m`) are what actually happened this
    /// call regardless.
    pub comparison: Option<CorrectionEvent>,
}

/// One predicted input, kept so it can be re-simulated after a correction.
#[derive(Debug, Clone, Copy)]
struct Record {
    /// The local clock's best estimate of which server tick this record
    /// represents. Assigned from [`PredictedPlayer`]'s own free-running
    /// `next_tick` counter (incremented once per [`PredictedPlayer::tick`]
    /// call, starting from [`PredictedPlayer::new`]'s `spawn_tick`) and
    /// re-anchored *exactly* onto the true `server_tick` at the end of
    /// every [`PredictedPlayer::reconcile`] call — so whatever drift
    /// accumulated between two reconciles (an independently-scheduled
    /// client clock running at a slightly different rate than the server,
    /// or a stall that skipped some local ticks) can never compound past
    /// one reconcile interval.
    ///
    /// ENG-69 round 21: this field replaces round 19/20's positional/count-
    /// based `history` draining (`keep_from = min(server_tick_delta,
    /// history.len())`, dropping *however many records that count implies*
    /// rather than the *specific* ones the server actually covered).
    /// Counting only gives the right answer when local-tick-count equals
    /// elapsed-server-tick-count between two reconciles — true in a
    /// single-process test that ticks client and server together every
    /// loop iteration, not guaranteed for a real client on its own
    /// wall-clock timer. Retaining by explicit per-record `tick` comparison
    /// (`tick.0 > server_tick.0`) instead of by count is correct regardless
    /// of whether that correspondence held this interval.
    tick: Tick,
    /// The input sequence this record was predicted for — kept for
    /// identity (a `ReconcileOutcome`/log line can name exactly which input
    /// a comparison used), not load-bearing for retain/replay itself (see
    /// `tick` above, which round 19 found `seq` cannot correctly stand in
    /// for once held-input reuse is in play).
    seq: InputSeq,
    input: PlayerInput,
    dt: f32,
    predicted_after: CharacterState,
}

/// The local player's predicted state plus the history needed to reconcile it.
pub struct PredictedPlayer {
    pub params: CharacterParams,
    predicted: CharacterState,
    authoritative: CharacterState,
    acked: InputSeq,
    /// The tick the *next* pushed [`Record`] will be tagged with — see that
    /// field's own doc for the full self-healing tagging scheme.
    next_tick: Tick,
    /// `server_tick` of the last [`Self::reconcile`] call — `spawn_tick`
    /// (`Self::new`) before the first one. Purely diagnostic
    /// (`ReconcileOutcome::server_tick_delta`); retain/replay itself no
    /// longer uses a delta at all, see `Record::tick`.
    last_server_tick: Tick,
    history: VecDeque<Record>,
    start_pos_m: [f64; 3],
    max_distance_from_start_m: f64,
    /// **Testing only.** `true` disables [`Self::reconcile`]'s rebase step:
    /// the server's `authoritative` snapshot is still recorded (so
    /// `authoritative()`, the correction-magnitude metrics, and
    /// `unmatched_reconciles` all keep reporting exactly what they always
    /// have — how far the server disagrees), but `predicted` is never
    /// replaced or replayed from it, so the player's on-screen position
    /// never snaps. Ordinary `tick()`-driven local prediction is completely
    /// unaffected either way. Never enable this outside a local, single-
    /// player debug session: over real network conditions the server and
    /// client will simply diverge without limit, and (unlike a normal
    /// prediction gap) nothing ever pulls them back together.
    pub client_authoritative: bool,

    // --- metrics -------------------------------------------------------------
    /// Snapshots whose predicted-at-ack state differed from authoritative.
    pub corrections: u64,
    /// Largest such difference, metres.
    pub max_correction_m: f64,
    /// Of `corrections`, how many happened on a record whose input was
    /// perfectly neutral (no movement, no buttons) — isolates a resting-contact
    /// disagreement (both sides idle, nothing to sweep) from a collision-sweep
    /// difference incurred while actually walking, which the plain lifetime
    /// counters above cannot tell apart (see ENG-69's round-6/7 investigation
    /// into corrections that keep firing "even while standing still").
    pub idle_corrections: u64,
    /// Largest correction magnitude ever seen on an idle record.
    pub max_idle_correction_m: f64,
    /// Largest vertical (Y) component of `err` ever seen across all
    /// corrections — large relative to `max_correction_m` points at the two
    /// sides resting at different heights (a terrain/ground-snap disagreement);
    /// small relative to it (with `max_horizontal_correction_m` large instead)
    /// points at a swept-move/collision-response disagreement instead.
    pub max_vertical_correction_m: f64,
    /// Largest horizontal (XZ) component of `err` ever seen across all
    /// corrections. See `max_vertical_correction_m`.
    pub max_horizontal_correction_m: f64,
    pub total_ticks: u64,
    pub grounded_ticks: u64,
    /// Set only if the predictor ever reported "grounded" while authority was
    /// clearly falling — the bug "removing a floor during replay leaves the
    /// player hovering". Correct code never sets it.
    pub hovered_after_floor_removal: bool,
    /// `reconcile` calls whose `ReconcileOutcome::comparison` was `None` —
    /// see that field's own doc. ENG-69 round 21: tracked separately from
    /// `corrections` precisely so "zero corrections" can never silently
    /// mean "we stopped being able to tell" instead of "nothing happened".
    pub unmatched_reconciles: u64,
    /// Largest `predicted_before`-to-`predicted_after` displacement ever
    /// seen on an unmatched reconcile — the actually-felt jump size for the
    /// cases `corrections`/`max_correction_m` cannot see at all.
    pub max_unmatched_displacement_m: f64,
}

impl PredictedPlayer {
    /// `spawn_tick` is the server tick `spawn` itself is authoritative as of
    /// — always available at construction time in practice, since a
    /// `PredictedPlayer` is only ever created from a `CharacterState` that
    /// just arrived *with* a tick attached (`net.rs`'s first `MotionSnapshot`
    /// for this player; a test harness's `Simulation::current_tick()` before
    /// its first `tick()` call, i.e. `Tick(0)`). This is what lets
    /// [`Self::reconcile`] treat every call uniformly, the very first one
    /// included, instead of needing a "no anchor yet" special case — ENG-69
    /// round 19's own first-draft fix special-cased the first call as "drop
    /// nothing," which left one stale record stuck at the front of
    /// `history` forever, silently shifting every later comparison one tick
    /// off (invisible except right when input changes between the
    /// mis-selected record and the current tick).
    pub fn new(params: CharacterParams, spawn: CharacterState, spawn_tick: Tick) -> Self {
        Self {
            params,
            predicted: spawn,
            authoritative: spawn,
            acked: InputSeq(0),
            next_tick: Tick(spawn_tick.0 + 1),
            last_server_tick: spawn_tick,
            history: VecDeque::new(),
            start_pos_m: spawn.position_m,
            max_distance_from_start_m: 0.0,
            client_authoritative: false,
            corrections: 0,
            max_correction_m: 0.0,
            idle_corrections: 0,
            max_idle_correction_m: 0.0,
            max_vertical_correction_m: 0.0,
            max_horizontal_correction_m: 0.0,
            total_ticks: 0,
            grounded_ticks: 0,
            hovered_after_floor_removal: false,
            unmatched_reconciles: 0,
            max_unmatched_displacement_m: 0.0,
        }
    }

    pub fn predicted(&self) -> CharacterState {
        self.predicted
    }

    pub fn authoritative(&self) -> CharacterState {
        self.authoritative
    }

    /// The server tick the next predicted step simulates.
    pub(crate) fn next_tick(&self) -> Tick {
        self.next_tick
    }

    /// Advances the prediction one tick and records the input for replay.
    /// `volume` is the replica's current terrain, passed through to
    /// [`ClientPhysics::sweep`]'s own window cache — the caller already
    /// holds/clones it each tick for the existing terrain-hash dirty check.
    /// `seq` is the input's own sequence number — kept on the pushed
    /// [`Record`] for identity, not for retain/replay (see that field's own
    /// doc).
    pub fn tick(
        &mut self,
        phys: &mut ClientPhysics,
        volume: &Volume,
        input: PlayerInput,
        seq: InputSeq,
        dt_s: f32,
    ) -> CharacterState {
        let params = self.params;
        self.predicted = step_character(self.predicted, input, dt_s, |p, d| {
            phys.sweep(volume, params, p, d, dt_s)
        });
        self.history.push_back(Record {
            tick: self.next_tick,
            seq,
            input,
            dt: dt_s,
            predicted_after: self.predicted,
        });
        self.next_tick = Tick(self.next_tick.0 + 1);
        while self.history.len() > PREDICTION_HISTORY {
            self.history.pop_front();
        }
        self.total_ticks += 1;
        if self.predicted.grounded {
            self.grounded_ticks += 1;
        }
        self.max_distance_from_start_m = self
            .max_distance_from_start_m
            .max(self.distance_travelled_m());
        self.predicted
    }

    /// Reconciles against an authoritative snapshot at `server_tick`. Drops
    /// exactly the `history` records the server has actually simulated as of
    /// that tick (identified by each record's own [`Record::tick`], not a
    /// count), and replays the rest — the still-genuinely-outstanding
    /// ones — from the authoritative state to produce the new predicted
    /// "now". Returns a [`ReconcileOutcome`] describing exactly what this
    /// call did, unconditionally — see that type's own doc for why it is
    /// not `Option<CorrectionEvent>` any more (ENG-69 round 21: a caller
    /// that only reacted to `Some` could not tell a clean small correction
    /// apart from a large, completely uncounted resync).
    ///
    /// `acked` (the last input sequence the server *accepted* — i.e. treated
    /// as fresh, `Simulation::player_acked_input`/`MotionSnapshot::
    /// acked_input`) is still recorded (`Self::acked`... no external reader
    /// today, kept for parity with the wire concept it names) but is
    /// deliberately **not** used to decide what counts as "already
    /// simulated by the server" — that was ENG-69 round 19's bug.
    /// `spall_sim::player::Player::last_input_seq` (what `acked` reports)
    /// only advances on a *fresh* accepted frame, but the server steps the
    /// player forward every tick regardless, reusing the last input via
    /// `effective_input()` whenever a fresh frame hasn't arrived
    /// (`HELD_INPUT_TIMEOUT_TICKS`). `server_tick`
    /// (`MotionSnapshot::server_tick`) has no such gap — it is the server's
    /// own tick counter, advanced every tick unconditionally.
    pub fn reconcile(
        &mut self,
        phys: &mut ClientPhysics,
        volume: &Volume,
        authoritative: CharacterState,
        acked: InputSeq,
        server_tick: Tick,
    ) -> ReconcileOutcome {
        self.reconcile_with_bodies(phys, volume, authoritative, acked, server_tick, None)
    }

    /// [`Self::reconcile`], replaying each surviving record against the
    /// replicated bodies *as of the tick that record simulates*
    /// (`bodies` advanced to `server_tick + 1 + i`), not one fixed arrangement.
    /// `None` keeps whatever poses `phys` currently holds.
    pub(crate) fn reconcile_with_bodies(
        &mut self,
        phys: &mut ClientPhysics,
        volume: &Volume,
        authoritative: CharacterState,
        acked: InputSeq,
        server_tick: Tick,
        bodies: Option<&[ClientBodyCollision]>,
    ) -> ReconcileOutcome {
        let server_tick_delta = server_tick.0 as i64 - self.last_server_tick.0 as i64;
        self.last_server_tick = server_tick;

        let history_len_before = self.history.len();
        let predicted_before = self.predicted;

        // Identity-based lookup, *before* any mutation: the record (if any)
        // whose own tag is exactly `server_tick` — not an index derived from
        // a count. `Record` is `Copy`, so this ends the borrow on
        // `self.history` immediately and the running-counter updates below
        // can freely borrow `self` mutably.
        let matched = self.history.iter().find(|r| r.tick == server_tick).copied();
        let comparison = matched.map(|rec| {
            let err = rec.predicted_after.distance_m(&authoritative);
            // A record only ever holds the *sanitized* input actually fed to
            // `step_character` (see `tick`), so this is exactly the input
            // that produced `predicted_after` — comparing it to `NEUTRAL`
            // tells whether the two sides had anything to sweep at all.
            let idle = rec.input.movement == [0.0, 0.0, 0.0] && rec.input.buttons == 0;
            if err > 1.0e-4 {
                self.corrections += 1;
                if idle {
                    self.idle_corrections += 1;
                    self.max_idle_correction_m = self.max_idle_correction_m.max(err);
                }
            }
            self.max_correction_m = self.max_correction_m.max(err);
            let dy = (rec.predicted_after.position_m[1] - authoritative.position_m[1]).abs();
            let dx = rec.predicted_after.position_m[0] - authoritative.position_m[0];
            let dz = rec.predicted_after.position_m[2] - authoritative.position_m[2];
            let horizontal_m = (dx * dx + dz * dz).sqrt();
            self.max_vertical_correction_m = self.max_vertical_correction_m.max(dy);
            self.max_horizontal_correction_m = self.max_horizontal_correction_m.max(horizontal_m);
            CorrectionEvent {
                seq: rec.seq,
                error_m: err,
                vertical_m: dy,
                horizontal_m,
                idle,
            }
        });

        if !authoritative.grounded
            && authoritative.velocity_m_s[1] < -1.0
            && self.predicted.grounded
            && self.predicted.position_m[1] > authoritative.position_m[1] + 0.3
        {
            self.hovered_after_floor_removal = true;
        }

        // The wire carries neither `grounded` nor `jump_held_last`, so
        // `authoritative` holds guesses for both. Replaying held-jump inputs
        // from a guessed "grounded, not yet holding jump" state (the velocity
        // heuristic also reads true at the jump apex) re-fires the jump in
        // mid-air. Take both from the newest record the server has covered
        // (matched or not): it is this client's own step of that same tick, so
        // when it agrees with authority on position it also knows the flags.
        let mut authoritative = authoritative;
        if let Some(covered) = self.history.iter().rfind(|r| r.tick <= server_tick) {
            authoritative.jump_held_last = covered.input.wants_jump();
            if covered.predicted_after.distance_m(&authoritative) < 0.1 {
                authoritative.grounded = covered.predicted_after.grounded;
            }
        }

        self.authoritative = authoritative;
        self.acked = acked;
        // Drop exactly the records the server has covered — identified by
        // each one's own tag, not a count (see `Record::tick`'s doc for why
        // that distinction matters).
        self.history.retain(|r| r.tick.0 > server_tick.0);
        let records_replayed = self.history.len();
        let records_removed = history_len_before - records_replayed;

        // Re-anchor every surviving record's tag exactly onto `server_tick`
        // — closes whatever drift accumulated *this* interval in one step
        // (an independently-scheduled client clock running fractionally
        // faster or slower than the server, or a stall that skipped some
        // local ticks, both show up as drift here). The retain decision
        // above used each record's tag as carried into this call, however
        // drifted; everything from here on is exact again, `next_tick`
        // included, so drift can never compound past one reconcile
        // interval.
        for (i, rec) in self.history.iter_mut().enumerate() {
            rec.tick = Tick(server_tick.0 + 1 + i as u64);
        }
        self.next_tick = Tick(server_tick.0 + 1 + self.history.len() as u64);

        let params = self.params;
        let mut state = authoritative;
        for (i, rec) in self.history.iter_mut().enumerate() {
            if let Some(bodies) = bodies {
                phys.sync_bodies_at(bodies, Some((server_tick.0 + 1 + i as u64) as f64));
            }
            let dt = rec.dt;
            state = step_character(state, rec.input, dt, |p, d| {
                phys.sweep(volume, params, p, d, dt)
            });
            rec.predicted_after = state;
        }
        // `client_authoritative` (testing only — see its own doc): the
        // rebase-from-authoritative-and-replay above still runs, so
        // `rec.predicted_after` and the correction/displacement metrics stay
        // exactly as informative as ever; only this one assignment — the
        // part that would actually move the player's on-screen position —
        // is skipped, so nothing the server sends can ever snap it.
        if !self.client_authoritative {
            self.predicted = state;
        }

        if comparison.is_none() {
            self.unmatched_reconciles += 1;
            let displacement = predicted_before.distance_m(&self.predicted);
            self.max_unmatched_displacement_m = self.max_unmatched_displacement_m.max(displacement);
        }

        ReconcileOutcome {
            server_tick,
            server_tick_delta,
            history_len_before,
            records_removed,
            records_replayed,
            predicted_before,
            predicted_after: self.predicted,
            comparison,
        }
    }

    /// A committed edit invalidated the geometry the predicted history walked
    /// through: drop it and rebase on the last authoritative state rather than
    /// replaying movement through geometry that no longer exists.
    pub fn invalidate(&mut self) {
        self.history.clear();
        self.predicted = self.authoritative;
    }

    /// Fraction of predicted ticks spent grounded.
    pub fn ground_contact_ratio(&self) -> f64 {
        if self.total_ticks == 0 {
            0.0
        } else {
            self.grounded_ticks as f64 / self.total_ticks as f64
        }
    }

    /// Horizontal distance the predicted feet have travelled from spawn, metres.
    pub fn distance_travelled_m(&self) -> f64 {
        let dx = self.predicted.position_m[0] - self.start_pos_m[0];
        let dz = self.predicted.position_m[2] - self.start_pos_m[2];
        (dx * dx + dz * dz).sqrt()
    }

    /// Horizontal distance the *authoritative* feet have travelled from spawn,
    /// metres — see the comment on [`Self::summary`]'s `distance_travelled_m`
    /// for why the final reported figure uses this instead of the predicted
    /// equivalent.
    fn authoritative_distance_travelled_m(
        authoritative: CharacterState,
        start_pos_m: [f64; 3],
    ) -> f64 {
        let dx = authoritative.position_m[0] - start_pos_m[0];
        let dz = authoritative.position_m[2] - start_pos_m[2];
        (dx * dx + dz * dz).sqrt()
    }

    /// Current gap between the predicted and authoritative feet, metres.
    pub fn prediction_error_m(&self) -> f64 {
        self.predicted.distance_m(&self.authoritative)
    }

    /// A machine-readable summary of one scripted run.
    pub fn summary(&self, held_button_release_ok: bool) -> PlayerMovementSummary {
        PlayerMovementSummary {
            ticks: self.total_ticks,
            // T23 / G3 row 16: not `self.distance_travelled_m()` (which reads
            // `self.predicted`). The mover only advances prediction while
            // `ClientPhysics::covers` holds for the predicted position (T23 /
            // G3 increment 16) — a residency reload round trip legitimately
            // *holds* (does not advance) `self.predicted` for a while, exactly
            // like the `at_rest` check a few lines below already accounts for.
            // `self.authoritative` "keeps updating from every snapshot
            // regardless" (same comment) and so is always at least as far
            // along. If the run's final summary happens to be captured while
            // predicted is held but authoritative has already progressed
            // further, distance-from-predicted under-reports a run that
            // otherwise converged correctly everywhere else — the reported
            // symptom, "the predictor's own bookkeeping was the casualty, not
            // the simulation". `self.authoritative` never has this gap.
            distance_travelled_m: Self::authoritative_distance_travelled_m(
                self.authoritative,
                self.start_pos_m,
            ),
            max_distance_from_start_m: self.max_distance_from_start_m,
            final_prediction_error_m: self.prediction_error_m(),
            corrections: self.corrections,
            max_correction_m: self.max_correction_m,
            ground_contact_ratio: self.ground_contact_ratio(),
            hovered_after_floor_removal: self.hovered_after_floor_removal,
            held_button_release_ok,
            final_predicted_pos_m: self.predicted.position_m,
            final_authoritative_pos_m: self.authoritative.position_m,
        }
    }
}

/// Reported per scripted player in [`crate::ClientSummary`].
#[derive(Debug, Clone, Serialize)]
pub struct PlayerMovementSummary {
    pub ticks: u64,
    pub distance_travelled_m: f64,
    /// Farthest horizontal displacement reached at any predicted tick. Paired
    /// with the final displacement to prove an outbound-and-return traversal.
    pub max_distance_from_start_m: f64,
    pub final_prediction_error_m: f64,
    pub corrections: u64,
    pub max_correction_m: f64,
    pub ground_contact_ratio: f64,
    pub hovered_after_floor_removal: bool,
    pub held_button_release_ok: bool,
    pub final_predicted_pos_m: [f64; 3],
    pub final_authoritative_pos_m: [f64; 3],
}

#[cfg(test)]
mod body_collision_tests {
    use super::*;
    use spall_core::{CellSizeCode, QuantizedQuat, VolumeId};
    use spall_voxel::EditPlan;

    fn pose(x: f64) -> Pose {
        pose_at([x, 0.0, 0.0])
    }

    fn pose_at(translation_m: [f64; 3]) -> Pose {
        Pose {
            translation_m,
            rotation: QuantizedQuat::from_unit(0.0, 0.0, 0.0, 1.0).unwrap(),
        }
    }

    fn one_voxel_body(id: u64) -> Volume {
        let body_id = VolumeId::new(id).unwrap();
        let mut body = Volume::new(body_id, CellSizeCode::Quarter);
        body.apply_edit(&EditPlan::filled_box(
            body_id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
        body
    }

    #[test]
    fn replicated_body_blocks_prediction_and_tracks_its_new_pose() {
        let terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let body_id = VolumeId::new(2).unwrap();
        let mut body = Volume::new(body_id, CellSizeCode::Quarter);
        body.apply_edit(&EditPlan::filled_box(
            body_id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(3, 3, 3),
            MaterialId(1),
        ))
        .unwrap();
        let entity = EntityId::new(2).unwrap();
        let version = body.next_revision().get();
        let mut physics = ClientPhysics::new();

        physics.sync_bodies(&[ClientBodyCollision {
            entity,
            topology_version: version,
            volume: Some(body.clone()),
            pose: pose(2.0),
            motion: BodyMotion::STATIC,
        }]);
        let blocked = physics.sweep(
            &terrain,
            CharacterParams::DEFAULT,
            [0.0, 0.0, 0.5],
            [3.0, 0.0, 0.0],
            1.0 / 60.0,
        );
        assert!(
            blocked.translation_m[0] < 1.75,
            "replicated body did not block the capsule: {blocked:?}"
        );

        physics.sync_bodies(&[ClientBodyCollision {
            entity,
            topology_version: version,
            volume: None,
            pose: pose(5.0),
            motion: BodyMotion::STATIC,
        }]);
        let cleared = physics.sweep(
            &terrain,
            CharacterParams::DEFAULT,
            [0.0, 0.0, 0.5],
            [3.0, 0.0, 0.0],
            1.0 / 60.0,
        );
        assert!(
            cleared.translation_m[0] > 2.9,
            "moved collider left stale collision behind: {cleared:?}"
        );
    }

    #[test]
    fn client_authority_integrates_body_and_ignores_server_pose_updates() {
        let body = one_voxel_body(2);
        let entity = EntityId::new(2).unwrap();
        let version = body.next_revision().get();
        let mut physics = ClientPhysics::new();
        physics.set_client_authoritative(true);

        physics.sync_bodies(&[ClientBodyCollision {
            entity,
            topology_version: version,
            volume: Some(body),
            pose: pose_at([2.0, 5.0, 2.0]),
            motion: BodyMotion::STATIC,
        }]);
        physics.step_client_authority();
        let locally_fallen = physics.local_body_poses()[&entity.get()].translation_m;
        assert!(
            locally_fallen[1] < 5.0,
            "body did not fall: {locally_fallen:?}"
        );

        physics.sync_bodies(&[ClientBodyCollision {
            entity,
            topology_version: version,
            volume: None,
            pose: pose_at([50.0, 50.0, 50.0]),
            motion: BodyMotion::STATIC,
        }]);
        let after_server_update = physics.local_body_poses()[&entity.get()].translation_m;
        assert!(
            (after_server_update[0] - locally_fallen[0]).abs() < 1.0e-5
                && (after_server_update[1] - locally_fallen[1]).abs() < 1.0e-5,
            "server pose replaced local authority: before={locally_fallen:?} after={after_server_update:?}"
        );
    }

    #[test]
    fn client_authority_refreshes_mass_after_server_topology_update() {
        let body = one_voxel_body(2);
        let entity = EntityId::new(2).unwrap();
        let initial_version = body.next_revision().get();
        let mut physics = ClientPhysics::new();
        physics.set_client_authoritative(true);
        physics.sync_bodies(&[ClientBodyCollision {
            entity,
            topology_version: initial_version,
            volume: Some(body.clone()),
            pose: pose_at([0.0, 5.0, 0.0]),
            motion: BodyMotion::STATIC,
        }]);
        let initial_mass = physics.local_body_poses();
        let body_physics = physics.bodies[&entity.get()].physics;
        let initial_mass_kg = physics.world.body_state(body_physics).mass_kg;

        let mut expanded = body;
        expanded
            .apply_edit(&EditPlan::filled_box(
                expanded.id(),
                GlobalCell::new(1, 0, 0),
                GlobalCell::new(1, 0, 0),
                MaterialId(1),
            ))
            .unwrap();
        let next_version = expanded.next_revision().get();
        physics.sync_bodies(&[ClientBodyCollision {
            entity,
            topology_version: next_version,
            volume: Some(expanded),
            pose: pose_at([50.0, 50.0, 50.0]),
            motion: BodyMotion::STATIC,
        }]);

        let updated_mass_kg = physics.world.body_state(body_physics).mass_kg;
        assert!(
            updated_mass_kg > initial_mass_kg,
            "mass did not follow new server topology: {initial_mass_kg} -> {updated_mass_kg}"
        );
        let pose_after_topology_update = physics.local_body_poses()[&entity.get()].translation_m;
        assert_eq!(
            pose_after_topology_update,
            initial_mass[&entity.get()].translation_m
        );
    }

    #[test]
    fn client_authority_releases_both_playground_emitters_on_local_schedule() {
        let showcase = one_voxel_body(2);
        let plinko = one_voxel_body(3);
        let showcase_entity = EntityId::new(2).unwrap();
        let plinko_entity = EntityId::new(3).unwrap();
        let mut physics = ClientPhysics::new();
        physics.set_client_authoritative(true);
        physics.sync_bodies(&[
            ClientBodyCollision {
                entity: showcase_entity,
                topology_version: showcase.next_revision().get(),
                volume: Some(showcase),
                pose: pose_at([4.0, -80.0, 16.0]),
                motion: BodyMotion::STATIC,
            },
            ClientBodyCollision {
                entity: plinko_entity,
                topology_version: plinko.next_revision().get(),
                volume: Some(plinko),
                pose: pose_at([4.0, -80.0, 21.0]),
                motion: BodyMotion::STATIC,
            },
        ]);
        let staged = physics.local_body_poses();
        assert_eq!(staged[&showcase_entity.get()].translation_m[1], -80.0);
        assert_eq!(staged[&plinko_entity.get()].translation_m[1], -80.0);

        physics.step_client_authority();
        let released = physics.local_body_poses();
        let showcase_y = released[&showcase_entity.get()].translation_m[1];
        let plinko_y = released[&plinko_entity.get()].translation_m[1];
        assert!((6.9..7.0).contains(&showcase_y), "showcase y={showcase_y}");
        assert!((10.4..10.5).contains(&plinko_y), "plinko y={plinko_y}");
    }

    #[test]
    fn client_authority_player_sweep_pushes_a_light_body() {
        let terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let body = one_voxel_body(2);
        let entity = EntityId::new(2).unwrap();
        let mut physics = ClientPhysics::new();
        physics.set_client_authoritative(true);
        physics.sync_bodies(&[ClientBodyCollision {
            entity,
            topology_version: body.next_revision().get(),
            volume: Some(body),
            pose: pose_at([1.0, 0.0, 0.5]),
            motion: BodyMotion::STATIC,
        }]);

        let swept = physics.sweep(
            &terrain,
            CharacterParams::DEFAULT,
            [0.0, 0.0, 0.5],
            [1.5, 0.0, 0.0],
            1.0 / 60.0,
        );
        assert!(
            swept.translation_m[0] > 0.0,
            "player failed to advance: {swept:?}"
        );
        physics.step_client_authority();
        let moved_x = physics.local_body_poses()[&entity.get()].translation_m[0];
        assert!(
            moved_x > 1.0,
            "player impulse did not move light body: x={moved_x}"
        );
    }
}

#[cfg(test)]
mod restitution_parity_tests {
    #[test]
    fn local_playground_restitution_matches_the_servers() {
        assert_eq!(super::PLINKO_RESTITUTION, spall_sim::PLINKO_RESTITUTION);
        assert_eq!(super::SHOWCASE_RESTITUTION, spall_sim::SHOWCASE_RESTITUTION);
    }
}

#[cfg(test)]
mod moving_body_replay_tests {
    use super::*;
    use spall_core::{CellSizeCode, QuantizedQuat, VolumeId};
    use spall_voxel::EditPlan;

    fn identity() -> QuantizedQuat {
        QuantizedQuat::from_unit(0.0, 0.0, 0.0, 1.0).unwrap()
    }

    fn cube(id: u64) -> Volume {
        let vid = VolumeId::new(id).unwrap();
        let mut v = Volume::new(vid, CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            vid,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(3, 3, 3),
            MaterialId(1),
        ))
        .unwrap();
        v
    }

    fn body(x: f64, snapshot_tick: u64, vx: f32, sleeping: bool) -> ClientBodyCollision {
        body_at([x, 0.0, 0.5], snapshot_tick, vx, sleeping)
    }

    fn body_at(
        translation_m: [f64; 3],
        snapshot_tick: u64,
        vx: f32,
        sleeping: bool,
    ) -> ClientBodyCollision {
        let volume = cube(2);
        ClientBodyCollision {
            entity: EntityId::new(2).unwrap(),
            topology_version: volume.next_revision().get(),
            volume: Some(volume),
            pose: Pose {
                translation_m,
                rotation: identity(),
            },
            motion: BodyMotion {
                snapshot_tick,
                linear_velocity_m_s: [vx, 0.0, 0.0],
                angular_velocity_rad_s: [0.0; 3],
                sleeping,
            },
        }
    }

    #[test]
    fn pose_at_advances_translation_and_orientation_and_is_bounded() {
        let mut b = body(1.0, 10, 6.0, false);
        // 6 ticks = 0.1 s at 6 m/s.
        assert!((b.pose_at(16.0).translation_m[0] - 1.6).abs() < 1e-6);
        // Backwards too (a replayed tick older than the snapshot).
        assert!((b.pose_at(4.0).translation_m[0] - 0.4).abs() < 1e-6);
        // Bounded: never more than MAX_BODY_EXTRAPOLATION_TICKS from the snapshot.
        let far = b.pose_at(10_000.0).translation_m[0];
        assert!((far - (1.0 + 6.0 * (MAX_BODY_EXTRAPOLATION_TICKS / 60.0))).abs() < 1e-6);
        // A quarter turn about +Y in 0.25 s (1.5 rad at 6 rad/s -> use 15 ticks).
        b.motion.angular_velocity_rad_s = [0.0, 3.0, 0.0];
        b.motion.linear_velocity_m_s = [0.0; 3];
        let [_, y, _, w] = b.pose_at(20.0).rotation.to_unit().unwrap();
        let angle = 2.0 * y.atan2(w);
        assert!((angle - 3.0 * (10.0 / 60.0)).abs() < 1e-3, "angle {angle}");
        // Sleeping bodies stay exactly where the snapshot put them.
        b.motion.sleeping = true;
        b.motion.linear_velocity_m_s = [6.0, 0.0, 0.0];
        assert_eq!(b.pose_at(16.0), b.pose);
    }

    /// Walks a capsule +x for 8 ticks toward a cube coming the other way, then
    /// reconciles against the authoritative start. With per-tick body poses the
    /// replay meets the cube where it *is* at each replayed tick; against one
    /// frozen arrangement it walks much further before contact.
    fn replayed_x(per_tick_bodies: bool) -> f64 {
        // A floor whose top is y = 0.25 m, so the capsule stays grounded and walks.
        let floor_id = VolumeId::new(1).unwrap();
        let mut floor = Volume::new(floor_id, CellSizeCode::Quarter);
        floor
            .apply_edit(&EditPlan::filled_box(
                floor_id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(79, 0, 15),
                MaterialId(1),
            ))
            .unwrap();
        let list = [body_at([1.9, 0.25, 1.5], 10, -3.0, false)];
        let mut phys = ClientPhysics::new();
        phys.set_terrain(&floor);
        phys.sync_bodies(&list);
        let mut start = CharacterState::at([1.0, 0.25, 2.0]);
        start.grounded = true;
        let mut pl = PredictedPlayer::new(CharacterParams::DEFAULT, start, Tick(10));
        let input = PlayerInput {
            movement: [0.0, 0.0, 1.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        };
        for k in 0..8u64 {
            pl.tick(&mut phys, &floor, input, InputSeq(k + 1), 1.0 / 60.0);
        }
        let outcome = if per_tick_bodies {
            pl.reconcile_with_bodies(&mut phys, &floor, start, InputSeq(0), Tick(10), Some(&list))
        } else {
            pl.reconcile(&mut phys, &floor, start, InputSeq(0), Tick(10))
        };
        assert_eq!(outcome.records_replayed, 8);
        outcome.predicted_after.position_m[0]
    }

    #[test]
    fn replay_meets_a_moving_body_where_it_is_at_each_replayed_tick() {
        let frozen = replayed_x(false);
        let per_tick = replayed_x(true);
        assert!(
            per_tick < frozen - 0.1,
            "per-tick replay should stop earlier against the approaching cube: \
             per_tick={per_tick:.3} frozen={frozen:.3}"
        );
    }

    #[test]
    fn sync_bodies_at_parks_each_body_at_the_requested_tick() {
        let terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let list = [body(1.2, 0, 60.0, false)];
        let mut phys = ClientPhysics::new();
        let sweep = |phys: &mut ClientPhysics| {
            phys.sweep(
                &terrain,
                CharacterParams::DEFAULT,
                [0.0, 0.0, 0.5],
                [3.0, 0.0, 0.0],
                1.0 / 60.0,
            )
            .translation_m[0]
        };
        phys.sync_bodies_at(&list, Some(0.0));
        let at_snapshot = sweep(&mut phys);
        phys.sync_bodies_at(&list, Some(6.0)); // 60 m/s * 0.1 s = 6 m away
        let later = sweep(&mut phys);
        assert!(
            at_snapshot < 1.0,
            "cube at 1.2 m should block: {at_snapshot}"
        );
        assert!(
            later > at_snapshot + 1.0,
            "cube moved away by tick 6: {later}"
        );
    }
}

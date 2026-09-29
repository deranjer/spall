//! [`Simulation`] — the authoritative loop that ties staging, commit, and
//! physics together behind a single [`Simulation::tick`].
//!
//! One `tick` runs the in-scope steps of the `docs/architecture.md` tick order:
//! drain accepted intents, validate and commit prepared transactions in
//! server-assigned order with matching collision / ownership updates, advance
//! physics one fixed step, refresh the extracted body poses, and advance the
//! player capsules (T19). Contact-to-intent conversion (T21), replication (T10)
//! and persistence (T16) are handled by the integrator, not here.

use std::collections::HashSet;

use glam::DVec3;
use spall_core::{
    BRUSH_UNIT, BrushPoint, EntityId, GlobalCell, IdError, MaterialId, PlayerInput, SphereBrush,
    Tick,
};
use spall_physics::PhysicsOrigin;
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::{ActionStatus, InputSeq, RequestId};

use crate::commit::CommitError;
use crate::contact_damage::{ContactDamagePlan, ContactDamagePolicy, ContactEvent};
use crate::dormancy::{ActiveRegion, BodyDormancyInput, DormancyPlan, DormancyPolicy};
use crate::intent::{EditIntent, EditKind, EditTarget, IntentError};
use crate::journal::JournalSink;
use crate::player::transaction_world_box;
use crate::schedule::{EditPipeline, TickReport};
use crate::world::{SimWorld, TerrainColliderMode, WorldSetup};
use spall_voxel::Sample;

/// Reserved high bit for a server-authored [`RequestId`]. Contact-damage cuts
/// (T21) are minted by the server, not a client, so their request ids sit in a
/// band a wire `RequestId` never reaches — a client action claiming a value at
/// or above this is rejected at ingress.
pub const SERVER_REQUEST_ID_BAND: u64 = 1 << 62;

/// Journal provenance for a server-authored contact-damage cut. Authority is the
/// server's; `actor` is only journal provenance. Sits above the per-session
/// `actor_for` band and below the reserved player band (`1 << 48`).
pub const CONTACT_DAMAGE_ACTOR_ID: u64 = 1 << 40;

/// Journal provenance for a server-authored dam-gate toggle (an admin command,
/// not a player edit). Its own reserved id beside [`CONTACT_DAMAGE_ACTOR_ID`],
/// both below the reserved player band (`1 << 48`).
pub const DAM_GATE_ACTOR_ID: u64 = (1 << 40) + 1;

/// Largest single covering sphere [`Simulation::set_dam_gate`] will cut or
/// fill, in cells. A dam gate is typically authored as a flat, wide notch —
/// far from spherical — so covering it with clusters this size (rather than
/// one sphere sized to the whole shape) keeps the cut close to the authored
/// outline instead of ballooning out to the shape's own diagonal.
const DAM_GATE_MAX_SPHERE_RADIUS_CELLS: i64 = 5;

/// The fixed server timestep: 60 Hz (`docs/architecture.md`).
pub const TICK_DT_S: f32 = 1.0 / 60.0;

/// Tunables for a [`Simulation`].
pub struct SimulationConfig {
    pub world: WorldSetup,
    /// Per-brick collision is the adopted default. Whole-terrain is retained
    /// only for explicit comparison runs.
    pub terrain_collider_mode: TerrainColliderMode,
    /// Maximum accepted-but-unstaged intents held before backpressure.
    pub max_pending_intents: usize,
    /// Consecutive commit conflicts on one region before it is routed through
    /// the serial queue.
    pub serialize_threshold: u32,
    /// Optional bounded, fully resident authoritative fluid region. `None`
    /// preserves non-fluid fixtures and existing worlds.
    pub water: Option<crate::water::WaterSetup>,
}

impl SimulationConfig {
    /// Default accepted-but-unstaged intent backlog.
    pub const DEFAULT_MAX_PENDING_INTENTS: usize = 256;
    /// Default consecutive-conflict count before a region is serialized.
    pub const DEFAULT_SERIALIZE_THRESHOLD: u32 = 3;

    pub fn new(world: WorldSetup) -> Self {
        Self {
            world,
            terrain_collider_mode: TerrainColliderMode::PerBrick,
            max_pending_intents: Self::DEFAULT_MAX_PENDING_INTENTS,
            serialize_threshold: Self::DEFAULT_SERIALIZE_THRESHOLD,
            water: None,
        }
    }
}

/// Error advancing the simulation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TickError {
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error("tick counter exhausted")]
    TickExhausted,
    #[error("authoritative water tick failed: {0}")]
    Water(String),
}

/// What one [`Simulation::apply_contact_damage`] pass did: the pure
/// [`ContactDamagePlan`] plus how its cuts fared at admission.
#[derive(Debug, Clone, Default)]
pub struct ContactDamageReport {
    /// The policy's decision for this tick.
    pub plan: ContactDamagePlan,
    /// Planned cuts accepted into the edit pipeline.
    pub submitted: usize,
    /// Planned cuts refused admission (pipeline queue full) — bounded
    /// backpressure, not an error.
    pub rejected: usize,
}

/// The authoritative simulation.
pub struct Simulation {
    world: SimWorld,
    pipeline: EditPipeline,
    journal: JournalSink,
    tick: Tick,
    next_control_seq: u64,
    next_damage_seq: u64,
    water: Option<crate::water::AuthoritativeWater>,
}

impl Simulation {
    pub fn new(config: SimulationConfig) -> Result<Self, crate::world::WorldError> {
        Self::new_with_physics_origin(config, PhysicsOrigin::ZERO)
    }

    /// Creates a simulation with an explicit local physics frame. Authoritative
    /// world positions and protocol snapshots remain in global coordinates.
    pub fn new_with_physics_origin(
        config: SimulationConfig,
        physics_origin: PhysicsOrigin,
    ) -> Result<Self, crate::world::WorldError> {
        let water_setup = config.water;
        let world = SimWorld::new_with_terrain_collider_mode_and_origin(
            config.world,
            config.terrain_collider_mode,
            physics_origin,
        )?;
        let mut simulation = Self {
            world,
            pipeline: EditPipeline::new(config.max_pending_intents, config.serialize_threshold),
            journal: JournalSink::new(),
            tick: Tick::ZERO,
            next_control_seq: 1,
            next_damage_seq: 0,
            water: None,
        };
        if let Some(setup) = water_setup {
            simulation.water = Some(
                crate::water::AuthoritativeWater::new(&simulation.world.terrain().volume, setup)
                    .map_err(|error| {
                        crate::world::WorldError::WaterInitialization(error.to_string())
                    })?,
            );
        }
        Ok(simulation)
    }

    /// Rebuilds a simulation from a recovered [`SimWorld`] (T16). `tick` is the
    /// checkpoint tick the world was restored to (and journal suffix replayed
    /// onto). The in-memory journal starts empty — the durable journal lives in
    /// `spall_store` — and control-stream sequencing restarts at 1 for the fresh
    /// post-restart session.
    pub fn from_restored(world: SimWorld, tick: Tick) -> Self {
        Self {
            world,
            pipeline: EditPipeline::new(
                SimulationConfig::DEFAULT_MAX_PENDING_INTENTS,
                SimulationConfig::DEFAULT_SERIALIZE_THRESHOLD,
            ),
            journal: JournalSink::new(),
            tick,
            next_control_seq: 1,
            next_damage_seq: 0,
            // Canonical fluid state is added to checkpoints in ENG-105
            // increment 3; recovered simulations do not claim water yet.
            water: None,
        }
    }

    pub fn world(&self) -> &SimWorld {
        &self.world
    }

    /// Admin world reset: adopt `fresh`'s world, water, and (empty) edit
    /// pipeline while keeping this simulation's tick, journal, and control
    /// sequencing, so every id a client or journal has seen stays monotonic.
    /// Id allocation resumes from whichever counter is further along, so no
    /// entity, volume, transaction, or journal sequence is handed out twice.
    /// Players are not carried over; the caller re-adds them. Staged edits of
    /// the old world are dropped.
    pub fn replace_world(&mut self, fresh: Simulation) -> Result<(), crate::world::WorldError> {
        let (entity, volume, transaction, journal_seq) = self.world.registry().counters();
        let Simulation {
            mut world,
            pipeline,
            water,
            ..
        } = fresh;
        let (f_entity, f_volume, f_transaction, f_journal_seq) = world.registry().counters();
        world.resume_registry(
            entity.max(f_entity),
            volume.max(f_volume),
            transaction.max(f_transaction),
            journal_seq.max(f_journal_seq),
        )?;
        self.world = world;
        self.pipeline = pipeline;
        self.water = water;
        Ok(())
    }

    /// Read-only authoritative fluid state, absent when this world did not
    /// install a fluid region (or was restored before persistence support).
    pub fn water(&self) -> Option<&crate::water::AuthoritativeWater> {
        self.water.as_ref()
    }

    /// Sets this world's scene-authored gated spring(s) to `rate` (an admin
    /// control, e.g. "fill the reservoir"): `0` is off, `1..=3` selects an
    /// increasingly large authored footprint, i.e. a faster fill (clamped
    /// into range). A no-op — not an error — when this world has no water or
    /// the scene authored no gated spring.
    pub fn set_water_spring_rate(&self, rate: u8) {
        if let Some(water) = &self.water {
            water.set_gated_sources_rate(rate);
        }
    }

    /// The gated spring's current rate (`0` is off), `None` when this world
    /// has no water.
    pub fn water_spring_rate(&self) -> Option<u8> {
        self.water.as_ref().map(|w| w.gated_sources_rate())
    }

    /// Opens (cuts to air) or closes (refills with `closed_material`) a
    /// scene-authored dam gate: `cells` covered by a set of modest spheres
    /// (never one sphere sized to the whole shape's own bounding box — for a
    /// flat, elongated authored shape such as a notch, that would overshoot
    /// to the shape's diagonal and cut a crater far bigger than authored),
    /// each submitted as a server-authored edit exactly like a T21
    /// contact-damage cut — it goes through the normal commit pipeline
    /// (staged, then committed a few ticks later, exactly like a player's
    /// own dig), so it rebuilds the collider, replicates to clients, and
    /// dirties the water boundary on its own. Returns one admitted intent's
    /// status per covering sphere (empty when `cells` is empty — the scene
    /// authored no gate) — the caller decides whether admission alone is
    /// enough to report success, or whether to also inspect the eventual
    /// commit outcome of each.
    pub fn set_dam_gate(
        &mut self,
        cells: &[GlobalCell],
        open: bool,
        closed_material: MaterialId,
    ) -> Result<Vec<ActionStatus>, IntentError> {
        let actor = EntityId::new(DAM_GATE_ACTOR_ID).expect("non-zero reserved actor id");
        let kind = if open {
            EditKind::Cut
        } else {
            EditKind::Place(closed_material)
        };
        let mut statuses = Vec::new();
        for brush in covering_spheres(cells, DAM_GATE_MAX_SPHERE_RADIUS_CELLS) {
            let request_id = RequestId(SERVER_REQUEST_ID_BAND | self.next_damage_seq);
            self.next_damage_seq += 1;
            let intent = EditIntent {
                request_id,
                actor,
                target: EditTarget::Terrain,
                kind,
                brush,
                explosion: None,
            };
            statuses.push(self.submit(intent)?);
        }
        Ok(statuses)
    }

    /// Mutable world access, for standing up a scenario (spawning a pre-existing
    /// body) before ticking.
    pub fn world_mut(&mut self) -> &mut SimWorld {
        &mut self.world
    }

    pub fn journal(&self) -> &JournalSink {
        &self.journal
    }

    /// The journal cursor for a baseline transfer: the highest sequence the
    /// sink has ever owned, retained across pruning (ENG-50).
    pub fn journal_cursor(&self) -> u64 {
        self.journal.cursor()
    }

    /// Reserves the next contiguous [`spall_core::JournalSeq`] for the
    /// integrator to own — used for the periodic 20 Hz pose batches, which
    /// share one sequence space with the committed topology transactions
    /// (`docs/protocol.md`: "Journal periodic body pose batches at 20 Hz";
    /// ENG-50: "contiguous sequence ownership").
    pub fn reserve_journal_seq(&mut self) -> Result<spall_core::JournalSeq, IdError> {
        self.world.registry_mut().allocate_journal_seq()
    }

    /// Drops in-memory journal entries at or below `through` after the
    /// integrator has flushed them durably and a checkpoint covers them
    /// (ENG-50 bounded retention). Returns how many entries were removed.
    pub fn prune_journal(&mut self, through: u64) -> usize {
        self.journal.prune_through(through)
    }

    pub fn current_tick(&self) -> Tick {
        self.tick
    }

    /// `true` when every accepted intent has committed or been rejected.
    pub fn is_idle(&self) -> bool {
        self.pipeline.is_idle()
    }

    /// The committed transaction for a request id, if it committed.
    pub fn committed(&self, request_id: RequestId) -> Option<&crate::commit::Committed> {
        self.pipeline.committed(request_id)
    }

    /// T23 / G3 row 7 (ENG-30 row 7 increment 13): the bounded brick footprint
    /// every currently-queued (not yet staged/committed) intent targeting
    /// `volume` will need. A residency pass uses this as a preflight pin set —
    /// see [`crate::schedule::EditPipeline::pending_dependency_bricks`].
    pub fn pending_edit_bricks(
        &self,
        volume: spall_core::VolumeId,
    ) -> std::collections::HashSet<spall_core::BrickCoord> {
        self.pipeline.pending_dependency_bricks(volume)
    }

    /// The current status of an admitted request, if this simulation has seen
    /// it. Hosts use this before re-validating a reliable retry, because the
    /// original action may already have changed the geometry it targeted.
    pub fn action_status(&self, request_id: RequestId) -> Option<&ActionStatus> {
        self.pipeline.action_status(request_id)
    }

    /// Admits an edit intent for staging, or replays the stored status for a
    /// duplicate request id.
    ///
    /// An intent that targets a **dormant** body (T21) reactivates it first, so
    /// the edit always stages and commits against a live body — "sleeping
    /// bodies retain [...] future destructibility" (`docs/architecture.md`).
    pub fn submit(&mut self, intent: EditIntent) -> Result<ActionStatus, IntentError> {
        if let EditTarget::Body(entity) = intent.target
            && self.world.body_is_dormant(entity)
        {
            self.world.reactivate_body(entity);
        }
        self.pipeline.submit_intent(intent, &self.world)
    }

    /// Advances one server tick: run the edit pipeline, then step physics and
    /// refresh extracted body state.
    pub fn tick(&mut self) -> Result<TickReport, TickError> {
        self.tick = self
            .tick
            .checked_next()
            .map_err(|_: IdError| TickError::TickExhausted)?;
        let report = self.pipeline.run_tick(
            &mut self.world,
            &mut self.journal,
            self.tick,
            &mut self.next_control_seq,
        )?;
        let mut report = report;
        if let Some(water) = &mut self.water {
            report.water = Some(
                water
                    .tick(
                        &self.world.terrain().volume,
                        !report.committed.is_empty(),
                        f64::from(TICK_DT_S),
                    )
                    .map_err(|error| TickError::Water(error.to_string()))?,
            );
        }
        let physics_started = std::time::Instant::now();
        self.world.step_physics();
        let physics_duration = physics_started.elapsed();
        report.physics_duration = physics_duration;
        self.advance_players(&report);
        Ok(report)
    }

    /// Converts this tick's solver contacts into bounded terrain-damage cuts and
    /// admits them (T21, increment 1). Call **after** [`Self::tick`], passing the
    /// [`TickReport`] it returned so a body created this tick is excluded from
    /// the recursion guard.
    ///
    /// This is deliberately *not* run by [`Self::tick`]: the integrator opts in,
    /// owns the [`ContactDamagePolicy`] (its cooldown state), and decides the
    /// cadence. Admitted cuts stage off-tick and commit on a later tick, exactly
    /// like a client edit — so a contact can never mutate world state inside the
    /// physics step, and a damage cut cannot cascade into another cut the same
    /// tick.
    ///
    /// Increment 1 damaged **terrain only**. Increment 3 adds **body-on-body**
    /// fracture: a `(dynamic, dynamic)` contact carves a bounded cut into the
    /// body being struck — the slower of the pair, tie-broken to the lower mass
    /// (`ContactDamageConfig::still_speed_m_s`) — through the same threshold,
    /// per-region cooldown, and per-tick cap. The contact point is resolved into
    /// the struck body's local cell frame here, so the pure policy stays in cell
    /// coordinates and a moving body's cooldown spot does not drift.
    pub fn apply_contact_damage(
        &mut self,
        policy: &mut ContactDamagePolicy,
        report: &TickReport,
    ) -> ContactDamageReport {
        let mut out = ContactDamageReport::default();
        if self.world.body_count() == 0 {
            return out;
        }

        let terrain_volume = self.world.terrain_volume_id();
        let cell_m = self.world.terrain().cell_size().metres();
        let g = {
            let a = self.world.physics().gravity_m_s2();
            (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
        };
        let dt = TICK_DT_S;
        let still_speed = policy.config().still_speed_m_s;

        let born_this_tick: HashSet<u64> = report
            .committed
            .iter()
            .flat_map(|(_, c)| c.children.iter().map(|e| e.get()))
            .collect();

        let mut events: Vec<ContactEvent> = Vec::new();
        for contact in self.world.physics().contact_impulses() {
            if !contact.point_m.iter().all(|v| v.is_finite()) {
                continue;
            }
            let world_point =
                DVec3::from_array(self.world.physics_origin().to_world_f64(contact.point_m));
            match (contact.dynamic[0], contact.dynamic[1]) {
                // Exactly one side dynamic, the other terrain: a body striking
                // the world grid (increment 1).
                (true, false) | (false, true) => {
                    let striker_idx = if contact.dynamic[0] { 0 } else { 1 };
                    if !self
                        .world
                        .is_terrain_physics_body(contact.bodies[1 - striker_idx])
                    {
                        continue;
                    }
                    let striker_phys = contact.bodies[striker_idx];
                    let Some(striker) =
                        self.world.body_by_phys(striker_phys).and_then(|b| b.entity)
                    else {
                        continue;
                    };
                    let mass = self.world.physics().body_state(striker_phys).mass_kg;
                    let point_cell = (world_point / cell_m).to_array();
                    // Contact normals point from body 0 to body 1. Sampling
                    // steps against target-to-striker, so orient from target
                    // (terrain) toward the selected striker.
                    let normal =
                        target_to_striker_normal(contact.normal.map(f64::from), 1 - striker_idx);
                    let Some(target_material) =
                        sample_contact_material(&self.world, terrain_volume, point_cell, normal)
                    else {
                        continue;
                    };
                    events.push(ContactEvent {
                        target: EditTarget::Terrain,
                        target_volume: terrain_volume,
                        target_material,
                        point_cell,
                        normal,
                        impulse_n_s: contact.normal_impulse_n_s,
                        resting_impulse_n_s: mass * g * dt,
                        striker_born_this_tick: born_this_tick.contains(&striker.get()),
                    });
                }
                // Both sides dynamic: debris-on-debris (increment 3). Damage the
                // body being struck.
                (true, true) => {
                    let (pa, pb) = (contact.bodies[0], contact.bodies[1]);
                    let (Some(ba), Some(bb)) =
                        (self.world.body_by_phys(pa), self.world.body_by_phys(pb))
                    else {
                        continue;
                    };
                    let (Some(ea), Some(eb)) = (ba.entity, bb.entity) else {
                        continue;
                    };
                    debug_assert!(
                        !ba.dormant && !bb.dormant,
                        "a dormant body has no physics body, so it cannot appear in a live contact"
                    );
                    let sa = self.world.physics().body_state(pa);
                    let sb = self.world.physics().body_state(pb);
                    let (speed_a, speed_b) = (f64::from(sa.speed_m_s()), f64::from(sb.speed_m_s()));
                    // Struck = slower body; near-equal speeds -> lower mass;
                    // still equal -> lower entity id (fully deterministic).
                    let strike_a = if (speed_a - speed_b).abs() <= still_speed {
                        if (sa.mass_kg - sb.mass_kg).abs() <= f32::EPSILON {
                            ea.get() <= eb.get()
                        } else {
                            sa.mass_kg <= sb.mass_kg
                        }
                    } else {
                        speed_a < speed_b
                    };
                    let (struck_body, struck_entity, mass_struck) = if strike_a {
                        (ba, ea, sa.mass_kg)
                    } else {
                        (bb, eb, sb.mass_kg)
                    };
                    // World contact point -> the struck body's local cell frame,
                    // so the brush and the cooldown key travel with the body.
                    let point_cell = struck_body
                        .pose
                        .xform(struck_body.cell_size())
                        .world_to_local_cell(world_point)
                        .to_array();
                    let normal_target_to_striker = target_to_striker_normal(
                        contact.normal.map(f64::from),
                        if strike_a { 0 } else { 1 },
                    );
                    let normal_world = DVec3::from_array(normal_target_to_striker);
                    let normal_local = struck_body.pose.rotation.inverse() * normal_world;
                    let Some(target_material) = sample_contact_material(
                        &self.world,
                        struck_body.volume_id,
                        point_cell,
                        normal_local.to_array(),
                    ) else {
                        continue;
                    };
                    events.push(ContactEvent {
                        target: EditTarget::Body(struck_entity),
                        target_volume: struck_body.volume_id,
                        target_material,
                        point_cell,
                        normal: normal_target_to_striker,
                        impulse_n_s: contact.normal_impulse_n_s,
                        resting_impulse_n_s: mass_struck * g * dt,
                        // A body split out this tick is at its split instant, not
                        // a real impact — guard on either side of the pair.
                        striker_born_this_tick: born_this_tick.contains(&ea.get())
                            || born_this_tick.contains(&eb.get()),
                    });
                }
                // Only terrain is Fixed, so a fixed/fixed pair cannot occur.
                (false, false) => continue,
            }
        }

        let plan = policy.plan(self.tick.get(), &events);
        let actor = EntityId::new(CONTACT_DAMAGE_ACTOR_ID).expect("non-zero reserved actor id");
        for cut in &plan.damage {
            let request_id = RequestId(SERVER_REQUEST_ID_BAND | self.next_damage_seq);
            self.next_damage_seq += 1;
            let mut intent = EditIntent::cut(request_id, actor, cut.target, cut.brush);
            if let Some(explosion) = cut.explosion {
                intent = intent.with_explosion(explosion);
            }
            // Route through `submit` so the dormancy-wake guard covers a
            // body-targeted cut uniformly with client edits (a struck body is
            // live here, so this is a no-op in practice — but consistent).
            match self.submit(intent) {
                Ok(_) => out.submitted += 1,
                Err(_) => out.rejected += 1,
            }
        }
        out.plan = plan;
        out
    }

    /// Runs the T21 region-dormancy policy for the current tick and applies its
    /// decisions: settled detached bodies with a quiet interaction region are
    /// deactivated (dropped from the physics step, record kept), and dormant
    /// bodies a player or an awake body has approached are reactivated.
    ///
    /// Opt-in, like [`Self::apply_contact_damage`]: the integrator owns the
    /// [`DormancyPolicy`] and the cadence, and it is *not* run by [`Self::tick`].
    /// Dormancy never changes authoritative geometry, ownership, or damage, so
    /// [`SimWorld::world_hash`](crate::world::SimWorld::world_hash),
    /// conservation, and the checkpoint set are unaffected. An edit that targets
    /// a dormant body still wakes it immediately through [`Self::submit`],
    /// independent of this pass.
    ///
    /// Pass the [`TickReport`] from the matching [`Self::tick`]: a **terrain**
    /// transaction committed this tick that lands within
    /// [`DormancyConfig::wake_margin_m`](crate::dormancy::DormancyConfig) of a
    /// dormant body hard-wakes it — the ground a settled body rests on just
    /// changed, so it must not wait out the hysteresis window
    /// (`docs/architecture.md`: "nearby edits wake affected neighbors").
    pub fn apply_dormancy(
        &mut self,
        policy: &mut DormancyPolicy,
        report: &TickReport,
    ) -> DormancyPlan {
        if self.world.body_count() == 0 {
            return DormancyPlan::default();
        }

        // Active interaction regions: every player capsule, plus every body that
        // is genuinely moving (an approaching / rolling piece — "active body
        // trajectories", docs/architecture.md).
        let still_speed = policy.config().still_speed_m_s;
        let mut regions: Vec<ActiveRegion> = Vec::new();
        for player in self.world.players() {
            let (lo, hi) = player.capsule_aabb_m();
            let centre = [
                0.5 * (lo[0] + hi[0]),
                0.5 * (lo[1] + hi[1]),
                0.5 * (lo[2] + hi[2]),
            ];
            let radius = 0.5
                * ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2) + (hi[2] - lo[2]).powi(2))
                    .sqrt();
            regions.push(ActiveRegion {
                centre_m: centre,
                radius_m: radius,
            });
        }
        for body in self.world.bodies() {
            if body.dormant {
                continue;
            }
            let speed = {
                let v = body.linvel_m_s;
                (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
            };
            if speed > still_speed {
                let (centre_m, radius_m) = body.world_bounding_sphere();
                regions.push(ActiveRegion { centre_m, radius_m });
            }
        }

        // Terrain transactions committed this tick: their world boxes hard-wake
        // any dormant body resting within `wake_margin_m` of the cut (increment
        // 3). Order-independent — a body is woken iff *any* box is close enough.
        let wake_margin_m = policy.config().wake_margin_m;
        let terrain = self.world.terrain_volume_id();
        let terrain_cell_m = self.world.terrain().cell_size().metres();
        let terrain_edit_boxes: Vec<([f64; 3], [f64; 3])> = report
            .committed
            .iter()
            .filter_map(|(_, c)| transaction_world_box(&c.topology, terrain, terrain_cell_m))
            .collect();

        let inputs: Vec<BodyDormancyInput> = self
            .world
            .bodies()
            .filter_map(|body| {
                let entity = body.entity?;
                let (centre_m, radius_m) = body.world_bounding_sphere();
                let speed = {
                    let v = body.linvel_m_s;
                    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
                };
                // A body-targeted edit wakes its target through `submit` before
                // it is ever staged, so a dormant body never has a pending edit
                // against it here. A *terrain* cut under settled rubble is the
                // remaining case: hard-wake it so the support change is honoured
                // this tick, not after the hysteresis window.
                let hard_wake = body.dormant
                    && terrain_edit_boxes
                        .iter()
                        .any(|b| box_sphere_gap(*b, centre_m, radius_m) <= wake_margin_m);
                Some(BodyDormancyInput {
                    entity,
                    centre_m,
                    radius_m,
                    sleeping: body.sleeping,
                    speed_m_s: speed,
                    dormant: body.dormant,
                    hard_wake,
                })
            })
            .collect();

        let plan = policy.plan(self.tick.get(), &inputs, &regions);
        // A body with a queued or staged edit stays live until that edit commits (the commit
        // rebuilds its collider); it is retried on a later tick.
        let targeted = self.pipeline.targeted_bodies();
        for &entity in &plan.deactivate {
            if targeted.contains(&entity) {
                continue;
            }
            self.world.deactivate_body(entity);
        }
        for &entity in &plan.reactivate {
            self.world.reactivate_body(entity);
        }
        plan
    }

    /// Advances the player capsules after the physics step, so the character
    /// sweep runs against the broad-phase BVH this tick's [`SimWorld::step_physics`]
    /// just refreshed — including any collider a commit rebuilt. A player near a
    /// cell this tick's transactions edited has its prediction epoch bumped and
    /// is depenetrated (`crate::player`).
    fn advance_players(&mut self, report: &TickReport) {
        if self.world.player_count() == 0 {
            return;
        }
        let terrain = self.world.terrain_volume_id();
        let cell_m = self.world.terrain().cell_size().metres();
        let boxes: Vec<([f64; 3], [f64; 3])> = report
            .committed
            .iter()
            .filter_map(|(_, committed)| {
                transaction_world_box(&committed.topology, terrain, cell_m)
            })
            .collect();
        self.world.advance_players(TICK_DT_S, &boxes);
        if std::env::var_os("SPALL_DEBUG_PLAYER").is_some() {
            for p in self.world.players() {
                eprintln!(
                    "DBGPLAYER tick={} pos={:?} grounded={} vel={:?}",
                    self.tick.0, p.state.position_m, p.state.grounded, p.state.velocity_m_s
                );
            }
        }
    }

    /// Registers an authoritative player capsule at `feet_m` (metres). `entity`
    /// is a reserved-band id from [`spall_core::player_entity_for`].
    pub fn add_player(&mut self, entity: EntityId, feet_m: [f64; 3]) -> EntityId {
        self.world
            .add_player(entity, feet_m, CharacterParams::DEFAULT)
    }

    /// Feeds one validated input frame to a player (at most one per tick per
    /// player). Returns `false` for an unknown player or a stale / duplicate /
    /// non-finite frame.
    pub fn set_player_input(
        &mut self,
        entity: EntityId,
        input: PlayerInput,
        seq: InputSeq,
    ) -> bool {
        self.world.set_player_input(entity, input, seq)
    }

    /// The authoritative kinematic state of a player, if it exists.
    pub fn player_state(&self, entity: EntityId) -> Option<CharacterState> {
        self.world.player(entity).map(|p| p.state)
    }

    /// A player's prediction-invalidation epoch (bumped by a nearby commit).
    pub fn player_movement_epoch(&self, entity: EntityId) -> Option<u64> {
        self.world.player(entity).map(|p| p.movement_epoch)
    }

    /// The last input sequence this simulation accepted for a player.
    pub fn player_acked_input(&self, entity: EntityId) -> Option<InputSeq> {
        self.world.player(entity).map(|p| p.last_input_seq)
    }

    pub fn player_count(&self) -> usize {
        self.world.player_count()
    }

    /// Runs ticks until the pipeline is idle or `max_ticks` is reached. Returns
    /// the per-tick reports.
    pub fn run_until_idle(&mut self, max_ticks: u32) -> Result<Vec<TickReport>, TickError> {
        let mut reports = Vec::new();
        for _ in 0..max_ticks {
            let report = self.tick()?;
            let done = self.pipeline.is_idle();
            reports.push(report);
            if done {
                break;
            }
        }
        Ok(reports)
    }

    /// Steps physics only (no edits), for settling / observation.
    pub fn step_physics_only(&mut self) {
        self.world.step_physics();
    }
}

/// Covers `cells` with a set of modest spheres instead of one big bounding
/// sphere: partitions them into clusters no more than `max_radius_cells`
/// from a shared seed cell (first-remaining-cell order — simple, and correct
/// regardless of `cells`' own order), then covers each cluster with its own
/// tight [`bounding_sphere_brush`]. For a compact blob this converges to the
/// same single sphere `bounding_sphere_brush` alone would give; for a flat,
/// elongated shape (a dam gate's straight notch, not remotely spherical) it
/// keeps every individual cut close to cluster-sized instead of one sphere
/// stretched to the whole shape's own diagonal. Empty for an empty slice.
fn covering_spheres(cells: &[GlobalCell], max_radius_cells: i64) -> Vec<SphereBrush> {
    let mut remaining: Vec<GlobalCell> = cells.to_vec();
    let mut spheres = Vec::new();
    let r2 = max_radius_cells * max_radius_cells;
    while let Some(seed) = remaining.first().copied() {
        let mut cluster = Vec::new();
        remaining.retain(|&c| {
            let d2 = (c.x - seed.x).pow(2) + (c.y - seed.y).pow(2) + (c.z - seed.z).pow(2);
            if d2 <= r2 {
                cluster.push(c);
                false
            } else {
                true
            }
        });
        if let Some(brush) = bounding_sphere_brush(&cluster) {
            spheres.push(brush);
        }
    }
    spheres
}

/// The smallest sphere, centred on `cells`' bounding box, that covers every
/// cell in `cells` (plus one cell of slack so a `Place` on close reliably
/// refills every voxel a matching `Cut` opened). Radius is the true farthest
/// marked cell from that centre, not the bounding box's own half-diagonal —
/// for a compact cluster (what [`covering_spheres`] always passes), that
/// keeps the brush close to the cluster's actual size instead of its
/// bounding box's corner-to-corner distance. `None` for an empty slice.
fn bounding_sphere_brush(cells: &[GlobalCell]) -> Option<SphereBrush> {
    let first = *cells.first()?;
    let (mut lo, mut hi) = (first, first);
    for &c in &cells[1..] {
        lo = GlobalCell::new(lo.x.min(c.x), lo.y.min(c.y), lo.z.min(c.z));
        hi = GlobalCell::new(hi.x.max(c.x), hi.y.max(c.y), hi.z.max(c.z));
    }
    // Cell-centred bounding box: [lo, hi + 1) in whole cells.
    let centre = BrushPoint::from_units(
        (lo.x + hi.x + 1) * BRUSH_UNIT / 2,
        (lo.y + hi.y + 1) * BRUSH_UNIT / 2,
        (lo.z + hi.z + 1) * BRUSH_UNIT / 2,
    );
    let centre_cells = [
        (lo.x + hi.x + 1) as f64 / 2.0,
        (lo.y + hi.y + 1) as f64 / 2.0,
        (lo.z + hi.z + 1) as f64 / 2.0,
    ];
    let radius_cells = cells
        .iter()
        .map(|c| {
            let d = [
                c.x as f64 + 0.5 - centre_cells[0],
                c.y as f64 + 0.5 - centre_cells[1],
                c.z as f64 + 0.5 - centre_cells[2],
            ];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
        })
        .fold(0.0_f64, f64::max)
        + 1.0;
    let radius_units = (radius_cells * BRUSH_UNIT as f64).ceil() as i64;
    SphereBrush::new(centre, radius_units).ok()
}

/// Converts Rapier's body-0-to-body-1 contact normal to a target-to-striker
/// normal. `target_index` is the target's position in the contact pair.
fn target_to_striker_normal(pair_normal: [f64; 3], target_index: usize) -> [f64; 3] {
    if target_index == 0 {
        pair_normal
    } else {
        pair_normal.map(|component| -component)
    }
}

/// Resolves the solid material just inside a contact surface. Solver points can
/// lie on cell boundaries, so step half a cell against the target-to-striker
/// normal and fail closed when the backing data is not resident.
fn sample_contact_material(
    world: &SimWorld,
    volume_id: spall_core::VolumeId,
    point_cell: [f64; 3],
    normal: [f64; 3],
) -> Option<spall_core::MaterialId> {
    let volume = world.volume_ref(volume_id)?;
    for offset in [0.51, 1.01, 1.51] {
        let cell = GlobalCell::new(
            (point_cell[0] - normal[0] * offset).floor() as i64,
            (point_cell[1] - normal[1] * offset).floor() as i64,
            (point_cell[2] - normal[2] * offset).floor() as i64,
        );
        if let Ok(Sample::Filled(material)) = volume.sample(cell) {
            return Some(material);
        }
    }
    None
}

#[cfg(test)]
mod bounding_sphere_brush_tests {
    use super::bounding_sphere_brush;
    use spall_core::{BRUSH_UNIT, GlobalCell};

    #[test]
    fn empty_cells_has_no_brush() {
        assert!(bounding_sphere_brush(&[]).is_none());
    }

    #[test]
    fn covers_every_marked_cell_with_only_a_little_slack() {
        // A compact, roughly spherical blob (as a scene author would place a
        // gate marker) around (10, 5, 2), radius 3 cells.
        let mut cells = Vec::new();
        for x in -3..=3 {
            for y in -3..=3 {
                for z in -3..=3 {
                    if x * x + y * y + z * z <= 9 {
                        cells.push(GlobalCell::new(10 + x, 5 + y, 2 + z));
                    }
                }
            }
        }
        let brush = bounding_sphere_brush(&cells).expect("non-empty cells");
        for cell in &cells {
            let half = BRUSH_UNIT / 2;
            assert!(
                brush.contains_cell_centre(
                    cell.x * BRUSH_UNIT + half,
                    cell.y * BRUSH_UNIT + half,
                    cell.z * BRUSH_UNIT + half,
                ),
                "{cell:?} must be inside its own covering brush"
            );
        }
        // A blob of authored radius 3 should not need to balloon out to the
        // bounding box's own corner-to-corner reach (radius 3 * sqrt(3) ~= 5.2
        // cells): the fix in `bounding_sphere_brush` keeps it near 3 + 1 slack.
        assert!(
            brush.radius_units() <= 5 * BRUSH_UNIT,
            "brush grew past the authored blob: {} units",
            brush.radius_units()
        );
    }
}

#[cfg(test)]
mod covering_spheres_tests {
    use super::covering_spheres;
    use spall_core::{BRUSH_UNIT, GlobalCell};

    #[test]
    fn empty_cells_has_no_spheres() {
        assert!(covering_spheres(&[], 5).is_empty());
    }

    #[test]
    fn a_flat_wide_notch_is_covered_without_one_giant_sphere() {
        // A dam-gate-shaped notch: wide and tall, but only 8 cells thick —
        // nothing like a sphere. One bounding sphere over this would need
        // a radius of roughly the box's own half-diagonal (~14 cells for a
        // 24x14x8 box); no individual covering sphere here should come
        // anywhere near that.
        let mut cells = Vec::new();
        for x in 0..24 {
            for y in 0..14 {
                for z in 0..8 {
                    cells.push(GlobalCell::new(x, y, z));
                }
            }
        }
        let spheres = covering_spheres(&cells, 5);
        assert!(
            spheres.len() > 1,
            "a shape this size needs more than one sphere"
        );
        for brush in &spheres {
            assert!(
                brush.radius_units() <= 7 * BRUSH_UNIT,
                "a single covering sphere ballooned out to the whole notch: {} units",
                brush.radius_units()
            );
        }
        // Every authored cell must still land inside at least one sphere.
        let half = BRUSH_UNIT / 2;
        for cell in &cells {
            let covered = spheres.iter().any(|brush| {
                brush.contains_cell_centre(
                    cell.x * BRUSH_UNIT + half,
                    cell.y * BRUSH_UNIT + half,
                    cell.z * BRUSH_UNIT + half,
                )
            });
            assert!(covered, "{cell:?} is not covered by any sphere");
        }
    }
}

#[cfg(test)]
mod dam_gate_tests {
    use super::{Simulation, SimulationConfig};
    use crate::fixtures::{STONE, flat_terrain_setup};
    use spall_core::GlobalCell;
    use spall_voxel::Sample;

    /// A dam gate is a server-authored edit through the same commit pipeline
    /// as a player's dig: it stages, then commits a few ticks later, and
    /// closing with the original material exactly undoes opening.
    #[test]
    fn opens_and_closes_through_the_normal_edit_pipeline() {
        let mut sim = Simulation::new(SimulationConfig::new(flat_terrain_setup())).unwrap();
        let gate = [GlobalCell::new(10, 0, 10)];
        fn sample(sim: &Simulation) -> Sample {
            sim.world()
                .terrain()
                .volume
                .sample(GlobalCell::new(10, 0, 10))
                .unwrap()
        }
        assert!(matches!(sample(&sim), Sample::Filled(_)), "starts solid");

        sim.set_dam_gate(&gate, true, STONE).unwrap();
        for _ in 0..8 {
            sim.tick().unwrap();
        }
        assert!(matches!(sample(&sim), Sample::Empty { .. }), "open cuts it");

        sim.set_dam_gate(&gate, false, STONE).unwrap();
        for _ in 0..8 {
            sim.tick().unwrap();
        }
        assert!(
            matches!(sample(&sim), Sample::Filled(_)),
            "close refills it"
        );
    }

    /// An empty cell list (a scene with no authored gate) is a harmless no-op,
    /// not a rejected intent.
    #[test]
    fn empty_cells_is_a_no_op() {
        let mut sim = Simulation::new(SimulationConfig::new(flat_terrain_setup())).unwrap();
        sim.set_dam_gate(&[], true, STONE).unwrap();
        for _ in 0..4 {
            sim.tick().unwrap();
        }
    }
}

#[cfg(test)]
mod contact_normal_tests {
    use super::target_to_striker_normal;

    #[test]
    fn contact_pair_order_orients_sampling_into_either_target() {
        let body_zero_to_one = [1.0, 0.0, 0.0];
        // Target in slot 0: the struck material lies in +X, toward striker 1.
        assert_eq!(
            target_to_striker_normal(body_zero_to_one, 0),
            body_zero_to_one
        );
        // Target in slot 1: the struck material lies in -X, toward striker 0.
        assert_eq!(
            target_to_striker_normal(body_zero_to_one, 1),
            [-1.0, 0.0, 0.0]
        );
    }
}

/// Nearest-surface gap, metres, between a world-space AABB `(min, max)` and a
/// sphere. Negative when the sphere overlaps the box. Used to decide whether a
/// terrain cut this tick is close enough to a settled body to wake it.
fn box_sphere_gap(bbox: ([f64; 3], [f64; 3]), centre_m: [f64; 3], radius_m: f64) -> f64 {
    let (lo, hi) = bbox;
    let mut d2 = 0.0;
    for axis in 0..3 {
        let (a, b) = (lo[axis].min(hi[axis]), lo[axis].max(hi[axis]));
        let outside = (a - centre_m[axis]).max(centre_m[axis] - b).max(0.0);
        d2 += outside * outside;
    }
    d2.sqrt() - radius_m
}

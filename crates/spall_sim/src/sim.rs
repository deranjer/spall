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

use spall_core::{EntityId, IdError, PlayerInput, Tick};
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::{ActionStatus, InputSeq, RequestId};

use crate::commit::CommitError;
use crate::contact_damage::{ContactDamagePlan, ContactDamagePolicy, ContactEvent};
use crate::dormancy::{ActiveRegion, BodyDormancyInput, DormancyPlan, DormancyPolicy};
use crate::intent::{EditIntent, EditTarget, IntentError};
use crate::journal::JournalSink;
use crate::player::transaction_world_box;
use crate::schedule::{EditPipeline, TickReport};
use crate::world::{SimWorld, WorldSetup};

/// Reserved high bit for a server-authored [`RequestId`]. Contact-damage cuts
/// (T21) are minted by the server, not a client, so their request ids sit in a
/// band a wire `RequestId` never reaches — a client action claiming a value at
/// or above this is rejected at ingress.
pub const SERVER_REQUEST_ID_BAND: u64 = 1 << 62;

/// Journal provenance for a server-authored contact-damage cut. Authority is the
/// server's; `actor` is only journal provenance. Sits above the per-session
/// `actor_for` band and below the reserved player band (`1 << 48`).
pub const CONTACT_DAMAGE_ACTOR_ID: u64 = 1 << 40;

/// The fixed server timestep: 60 Hz (`docs/architecture.md`).
pub const TICK_DT_S: f32 = 1.0 / 60.0;

/// Tunables for a [`Simulation`].
pub struct SimulationConfig {
    pub world: WorldSetup,
    /// Maximum accepted-but-unstaged intents held before backpressure.
    pub max_pending_intents: usize,
    /// Consecutive commit conflicts on one region before it is routed through
    /// the serial queue.
    pub serialize_threshold: u32,
}

impl SimulationConfig {
    /// Default accepted-but-unstaged intent backlog.
    pub const DEFAULT_MAX_PENDING_INTENTS: usize = 256;
    /// Default consecutive-conflict count before a region is serialized.
    pub const DEFAULT_SERIALIZE_THRESHOLD: u32 = 3;

    pub fn new(world: WorldSetup) -> Self {
        Self {
            world,
            max_pending_intents: Self::DEFAULT_MAX_PENDING_INTENTS,
            serialize_threshold: Self::DEFAULT_SERIALIZE_THRESHOLD,
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
}

impl Simulation {
    pub fn new(config: SimulationConfig) -> Result<Self, crate::world::WorldError> {
        let world = SimWorld::new(config.world)?;
        Ok(Self {
            world,
            pipeline: EditPipeline::new(config.max_pending_intents, config.serialize_threshold),
            journal: JournalSink::new(),
            tick: Tick::ZERO,
            next_control_seq: 1,
            next_damage_seq: 0,
        })
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
        }
    }

    pub fn world(&self) -> &SimWorld {
        &self.world
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
        self.world.step_physics();
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
    /// Increment 1 damages **terrain only**: body-on-body contacts are counted
    /// (`plan.suppressed_*` do not include them; they are simply skipped here)
    /// and left for a later increment together with the region-sleep policy.
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
        let terrain_phys = self.world.terrain().phys;
        let cell_m = self.world.terrain().cell_size().metres();
        let g = {
            let a = self.world.physics().gravity_m_s2();
            (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
        };
        let dt = TICK_DT_S;

        let born_this_tick: HashSet<u64> = report
            .committed
            .iter()
            .flat_map(|(_, c)| c.children.iter().map(|e| e.get()))
            .collect();

        let mut events: Vec<ContactEvent> = Vec::new();
        for contact in self.world.physics().contact_impulses() {
            // Increment 1: exactly one side dynamic, the other side terrain.
            let striker_idx = match (contact.dynamic[0], contact.dynamic[1]) {
                (true, false) => 0,
                (false, true) => 1,
                _ => continue,
            };
            let fixed_idx = 1 - striker_idx;
            if contact.bodies[fixed_idx] != terrain_phys {
                continue;
            }
            if !contact.point_m.iter().all(|v| v.is_finite()) {
                continue;
            }
            let striker_phys = contact.bodies[striker_idx];
            let Some(striker) = self.world.body_by_phys(striker_phys).and_then(|b| b.entity) else {
                continue;
            };
            let mass = self.world.physics().body_state(striker_phys).mass_kg;

            events.push(ContactEvent {
                target_volume: terrain_volume,
                point_m: contact.point_m.map(f64::from),
                normal: contact.normal.map(f64::from),
                impulse_n_s: contact.normal_impulse_n_s,
                resting_impulse_n_s: mass * g * dt,
                striker_born_this_tick: born_this_tick.contains(&striker.get()),
            });
        }

        let plan = policy.plan(self.tick.get(), cell_m, &events);
        let actor = EntityId::new(CONTACT_DAMAGE_ACTOR_ID).expect("non-zero reserved actor id");
        for cut in &plan.damage {
            debug_assert_eq!(cut.target, EditTarget::Terrain);
            let request_id = RequestId(SERVER_REQUEST_ID_BAND | self.next_damage_seq);
            self.next_damage_seq += 1;
            let mut intent = EditIntent::cut(request_id, actor, cut.target, cut.brush);
            if let Some(explosion) = cut.explosion {
                intent = intent.with_explosion(explosion);
            }
            match self.pipeline.submit_intent(intent, &self.world) {
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
    pub fn apply_dormancy(&mut self, policy: &mut DormancyPolicy) -> DormancyPlan {
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
                Some(BodyDormancyInput {
                    entity,
                    centre_m,
                    radius_m,
                    sleeping: body.sleeping,
                    speed_m_s: speed,
                    dormant: body.dormant,
                    // A body-targeted edit wakes its target through `submit`
                    // before it is ever staged, so by here no dormant body has a
                    // pending edit against it. Terrain edits adjacent to a
                    // dormant neighbour waking it is increment 3.
                    hard_wake: false,
                })
            })
            .collect();

        let plan = policy.plan(self.tick.get(), &inputs, &regions);
        for &entity in &plan.deactivate {
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

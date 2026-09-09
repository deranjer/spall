//! [`Simulation`] — the authoritative loop that ties staging, commit, and
//! physics together behind a single [`Simulation::tick`].
//!
//! One `tick` runs the in-scope steps of the `docs/architecture.md` tick order:
//! drain accepted intents, validate and commit prepared transactions in
//! server-assigned order with matching collision / ownership updates, advance
//! physics one fixed step, refresh the extracted body poses, and advance the
//! player capsules (T19). Contact-to-intent conversion (T21), replication (T10)
//! and persistence (T16) are handled by the integrator, not here.

use spall_core::{EntityId, IdError, PlayerInput, Tick};
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::{ActionStatus, InputSeq, RequestId};

use crate::commit::CommitError;
use crate::intent::{EditIntent, IntentError};
use crate::journal::JournalSink;
use crate::player::transaction_world_box;
use crate::schedule::{EditPipeline, TickReport};
use crate::world::{SimWorld, WorldSetup};

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

/// The authoritative simulation.
pub struct Simulation {
    world: SimWorld,
    pipeline: EditPipeline,
    journal: JournalSink,
    tick: Tick,
    next_control_seq: u64,
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
    pub fn submit(&mut self, intent: EditIntent) -> Result<ActionStatus, IntentError> {
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

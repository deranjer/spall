//! [`Simulation`] — the authoritative loop that ties staging, commit, and
//! physics together behind a single [`Simulation::tick`].
//!
//! One `tick` runs the in-scope steps of the `docs/architecture.md` tick order:
//! drain accepted intents, validate and commit prepared transactions in
//! server-assigned order with matching collision / ownership updates, advance
//! physics one fixed step, and refresh the extracted body poses. Player
//! movement (T19), contact-to-intent conversion (T21), replication (T10) and
//! persistence (T16) are explicitly out of scope.

use spall_core::{IdError, Tick};

use crate::commit::CommitError;
use crate::intent::{EditIntent, IntentError};
use crate::journal::JournalSink;
use crate::schedule::{EditPipeline, TickReport};
use crate::world::{SimWorld, WorldSetup};

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
    pub fn committed(
        &self,
        request_id: spall_protocol::RequestId,
    ) -> Option<&crate::commit::Committed> {
        self.pipeline.committed(request_id)
    }

    /// Accepts an edit intent for staging.
    pub fn submit(&mut self, intent: EditIntent) -> Result<(), IntentError> {
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
        Ok(report)
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

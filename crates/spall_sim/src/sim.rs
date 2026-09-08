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
    pub fn new(world: WorldSetup) -> Self {
        Self {
            world,
            max_pending_intents: 256,
            serialize_threshold: 3,
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

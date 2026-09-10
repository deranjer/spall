//! `spall_sim` — the authoritative simulation: accepted edit intents become
//! staged transactions, staged transactions commit atomically at a tick
//! boundary, and a commit that disconnects material transfers it into a new
//! dynamic body with correct identity, geometry, mass, and velocity.
//!
//! No GPU, window, or network dependency. This crate owns the conversion
//! between authoritative state and [`spall_protocol`] records
//! ([`journal`]); it does not run transport (T10) or persistence (T16).
//!
//! # Pipeline
//!
//! 1. A client / bot / console action is accepted as an [`intent::EditIntent`]
//!    with a stable [`spall_protocol::RequestId`]. The brush is already
//!    quantised into the **target volume's local integer cell space** at accept
//!    time (a moving body's frame is sampled then).
//! 2. Off the tick, [`stage::stage_edit`] runs against an *immutable snapshot*:
//!    it builds the deterministic [`spall_voxel::EditPlan`], dry-runs it to
//!    predict the [`spall_voxel::EditOutcome`], runs [`spall_structure`] over the
//!    result to find every unsupported component, and captures a
//!    [`spall_jobs::JobToken`] over every brick it read. Staging is submitted to
//!    a bounded [`spall_jobs::Scheduler`] so preparation cannot starve the tick.
//! 3. At the tick boundary [`commit::commit`] re-validates the token against the
//!    *live* world. A stale token means an earlier commit this tick touched a
//!    shared brick: the intent is recomputed and retried in request order. A
//!    region that keeps losing that race is routed through a bounded serial
//!    queue so it still makes progress. A fresh token commits: the plan is
//!    applied to the live volume, unsupported components are split into new
//!    bodies at their exact world location with inherited mass / velocity, every
//!    affected collider is rebuilt in the same tick, and a
//!    [`spall_protocol::TopologyTransaction`] plus a journal entry are emitted.
//! 4. A repeated [`spall_protocol::RequestId`] returns the existing
//!    [`spall_protocol::ActionStatus`] and can never perform a second cut or a
//!    second impulse.
//!
//! [`sim::Simulation`] wires all of that together and exposes a single
//! [`sim::Simulation::tick`].

pub mod backing;
pub mod body;
pub mod collider;
pub mod commit;
pub mod contact_damage;
pub mod dormancy;
pub mod fixtures;
pub mod intent;
pub mod journal;
pub mod player;
pub mod registry;
pub mod replication;
pub mod schedule;
pub mod sim;
pub mod stage;
pub mod transfer;
pub mod world;

/// Re-exported so game/example code names one `RequestId` type, not a copy.
pub use spall_protocol::RequestId;

pub use backing::{BackingBrick, BrickBacking, MemoryBacking};
pub use body::{Body, BodyKind, BodyPose};
pub use collider::{
    ColliderInfeasible, ColliderPlan, MAX_ACTIVE_COLLIDER_CELLS, PRIMITIVE_BUDGET, plan_collider,
};
pub use commit::{CommitError, CommitOutcome, Committed};
pub use contact_damage::{
    ContactDamageConfig, ContactDamagePlan, ContactDamagePolicy, ContactEvent, PlannedDamage,
};
pub use dormancy::{ActiveRegion, BodyDormancyInput, DormancyConfig, DormancyPlan, DormancyPolicy};
pub use intent::{EditIntent, EditKind, EditTarget, ExplosionImpulse, IntentError};
pub use journal::{JournalEntry, JournalSink};
pub use player::{HELD_INPUT_TIMEOUT_TICKS, Player, transaction_world_box};
pub use registry::IdRegistry;
pub use replication::{
    MotionPublisher, ReplicationError, action_statuses, committed_transactions, repair_ops,
};
pub use schedule::{EditPipeline, RegionKey, TickReport};
pub use sim::{
    CONTACT_DAMAGE_ACTOR_ID, ContactDamageReport, SERVER_REQUEST_ID_BAND, Simulation,
    SimulationConfig, TICK_DT_S, TickError,
};
pub use stage::{StageError, StageInput, StagedEdit, stage_edit};
pub use transfer::{ChildBody, PlanChildError, plan_child};
pub use world::{RestoredBody, SimWorld, WorldError};
pub use world::{
    WorldSetup, canonical_logical_volume_for, canonical_volume_for, solid_cells,
    volume_topology_hash_for,
};

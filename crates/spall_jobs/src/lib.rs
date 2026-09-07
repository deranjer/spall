//! `spall_jobs` — bounded background job scheduling with deterministic dispatch
//! and mandatory result re-validation.
//!
//! CPU work that runs *outside* the server tick — meshing, structural analysis,
//! collider builds, world generation, compression — is submitted here as a
//! closure over an immutable snapshot plus a [`JobToken`] describing exactly
//! what world state that snapshot represents. The scheduler:
//!
//! - keeps a **separate queue and budget per [`Lane`]** so background work
//!   cannot starve or outspend player-visible work, and rejects submissions with
//!   an explicit [`SubmitReason`] once a lane is full (bounded saturation, never
//!   an unbounded backlog);
//! - hands jobs out in a **deterministic order** — by lane, then descending
//!   [`Priority`], then submission order — and never spawns a thread itself, so
//!   the tick loop and tests drive it directly;
//! - accepts completions in **any order** and, at [`Scheduler::install`] time,
//!   re-checks every result's token against the current world so an out-of-order
//!   or delayed completion can never install state the world has moved past;
//! - invalidates work on **world reload** ([`Scheduler::reload_world`] bumps the
//!   [`Generation`]) and on **unload/reload at reused coordinates** (the token's
//!   brick revisions and missing-neighbour sentinels no longer match);
//! - drains cleanly on **shutdown** ([`Scheduler::begin_shutdown`]): queued jobs
//!   are cancelled, dispatched jobs run to completion.
//!
//! [`ThreadJobPool`] is a thin `std::thread` executor around the same
//! [`Scheduler`] for real background execution.
//!
//! This crate depends only on `spall_core`. It has no GPU, window, network, or
//! filesystem dependency, and it does not know about voxel storage — a job's
//! input is whatever its closure captures.
//!
//! # Result type
//!
//! [`Scheduler<T>`] is generic over one output type `T`. A subsystem pool with a
//! single result kind uses that type directly; a shared scheduler carrying
//! several kinds uses a caller-defined `enum` (or `Box<dyn Any + Send>`). Lane
//! budgets are independent of `T`.

mod budget;
mod generation;
mod pool;
mod scheduler;
mod token;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use budget::{Lane, LaneBudget, LanePressure, Pressure, Priority, SchedulerConfig};
pub use generation::{CounterExhausted, Generation, TopologyEpoch};
pub use pool::ThreadJobPool;
pub use scheduler::{
    Completion, Discarded, Dispatch, InstallOutcome, Installed, JobHandle, JobId, JobRequest,
    Rejected, ReloadSummary, Scheduler, ShutdownSummary, SubmitReason,
};
pub use token::{BrickRef, BrickStatus, DepState, JobToken, ReadDep, Staleness, WorldView};

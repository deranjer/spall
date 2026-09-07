//! The bounded, deterministic job scheduler.
//!
//! [`Scheduler`] is the whole mechanism: it admits jobs against per-lane
//! budgets, hands them out in a deterministic priority order, accepts their
//! results in *any* order, and — crucially — re-validates every result against
//! the current world before it can be installed. It never spawns a thread. That
//! makes it directly usable as the tick-loop's job hook and as the unit under
//! test; [`crate::ThreadJobPool`] wraps it for real background execution.
//!
//! Lifecycle of one job:
//!
//! ```text
//! submit ─▶ Queued ─(dispatch)─▶ Running ─(complete)─▶ Completed ─(install)─▶ Installed
//!             │                     │                      │
//!         cancel/reload         cancel/reload          reload/stale
//!             ▼                     ▼                      ▼
//!         (dropped)          (result dropped)          (discarded)
//! ```

use std::collections::{HashMap, HashSet};

use crate::budget::{Lane, LanePressure, Pressure, Priority, SchedulerConfig};
use crate::generation::Generation;
use crate::token::{JobToken, Staleness, WorldView};

/// A scheduler-unique job identifier. Also the submission-order key used to
/// break priority ties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job#{}", self.0)
    }
}

/// A cheap reference to a submitted job, used to cancel it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobHandle {
    id: JobId,
    lane: Lane,
}

impl JobHandle {
    pub fn id(&self) -> JobId {
        self.id
    }
    pub fn lane(&self) -> Lane {
        self.lane
    }
}

type BoxedRun<T> = Box<dyn FnOnce() -> T + Send + 'static>;

/// A unit of work to submit to a [`Scheduler`].
pub struct JobRequest<T> {
    pub lane: Lane,
    pub priority: Priority,
    pub token: JobToken,
    /// Conservative declared maximum bytes retained by this job. The scheduler
    /// charges it once through each lifecycle stage — queued, running, then
    /// completed pending installation — so a caller that does not drain results
    /// cannot exceed the configured lane budget. Use `0` only for work whose
    /// retained inputs and output are deliberately not accounted for.
    pub cost_bytes: u64,
    run: BoxedRun<T>,
}

impl<T> JobRequest<T> {
    pub fn new(
        lane: Lane,
        priority: Priority,
        token: JobToken,
        run: impl FnOnce() -> T + Send + 'static,
    ) -> Self {
        Self {
            lane,
            priority,
            token,
            cost_bytes: 0,
            run: Box::new(run),
        }
    }

    #[must_use]
    pub fn with_cost_bytes(mut self, bytes: u64) -> Self {
        self.cost_bytes = bytes;
        self
    }
}

impl<T> std::fmt::Debug for JobRequest<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobRequest")
            .field("lane", &self.lane)
            .field("priority", &self.priority)
            .field("token", &self.token)
            .field("cost_bytes", &self.cost_bytes)
            .field("run", &"<closure>")
            .finish()
    }
}

/// Why a [`Scheduler::submit`] was refused. The lane's queue is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitReason {
    #[error("lane {lane:?} queue is full ({queued}/{limit} jobs)")]
    JobLimit { lane: Lane, queued: u32, limit: u32 },
    #[error("lane {lane:?} byte budget is full ({queued}+{needed} > {limit} bytes)")]
    ByteLimit {
        lane: Lane,
        queued: u64,
        needed: u64,
        limit: u64,
    },
    #[error("scheduler is shutting down")]
    ShuttingDown,
}

/// A refused submission: the reason plus the original request, handed back so the
/// caller can retry later without rebuilding it.
pub struct Rejected<T> {
    pub reason: SubmitReason,
    pub request: JobRequest<T>,
}

impl<T> std::fmt::Debug for Rejected<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rejected")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

/// A job the scheduler has handed out to be executed. Run it (on any thread) and
/// return the [`Completion`] to [`Scheduler::apply`].
#[must_use = "a dispatched job must be run and its Completion applied, or the scheduler never drains"]
pub struct Dispatch<T> {
    id: JobId,
    lane: Lane,
    token: JobToken,
    run: BoxedRun<T>,
}

impl<T> Dispatch<T> {
    pub fn id(&self) -> JobId {
        self.id
    }
    pub fn lane(&self) -> Lane {
        self.lane
    }
    /// The token captured when the job was submitted. Informational here;
    /// [`Scheduler::install`] is what enforces it.
    pub fn token(&self) -> &JobToken {
        &self.token
    }

    /// Executes the job closure and returns its output tagged with the job id.
    pub fn run(self) -> Completion<T> {
        let output = (self.run)();
        Completion {
            id: self.id,
            output,
        }
    }
}

impl<T> std::fmt::Debug for Dispatch<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatch")
            .field("id", &self.id)
            .field("lane", &self.lane)
            .finish_non_exhaustive()
    }
}

/// The output of a [`Dispatch`], ready to hand back to the scheduler.
#[derive(Debug)]
pub struct Completion<T> {
    pub id: JobId,
    pub output: T,
}

/// A validated result, safe to apply to the world.
#[derive(Debug)]
pub struct Installed<T> {
    pub id: JobId,
    pub lane: Lane,
    pub token: JobToken,
    pub output: T,
}

/// A completed result that failed re-validation and was dropped.
#[derive(Debug)]
pub struct Discarded {
    pub id: JobId,
    pub lane: Lane,
    pub token: JobToken,
    pub reason: Staleness,
}

/// The outcome of [`Scheduler::install`].
#[derive(Debug)]
pub struct InstallOutcome<T> {
    /// Fresh results, in completion order.
    pub installed: Vec<Installed<T>>,
    /// Stale results that were dropped, in completion order.
    pub discarded: Vec<Discarded>,
}

impl<T> InstallOutcome<T> {
    pub fn is_empty(&self) -> bool {
        self.installed.is_empty() && self.discarded.is_empty()
    }
}

/// Counts from [`Scheduler::reload_world`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadSummary {
    pub generation: Generation,
    pub cancelled_queued: u32,
    pub voided_running: u32,
    pub dropped_completed: u32,
}

/// Result of [`Scheduler::begin_shutdown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownSummary {
    pub cancelled_queued: u32,
    /// Jobs already dispatched; the owner must run them and call
    /// [`Scheduler::apply`] so the scheduler can drain.
    pub still_running: Vec<JobId>,
}

struct QueuedJob<T> {
    id: JobId,
    priority: Priority,
    token: JobToken,
    cost_bytes: u64,
    run: BoxedRun<T>,
}

struct RunningJob {
    lane: Lane,
    token: JobToken,
    cost_bytes: u64,
}

struct CompletedJob<T> {
    id: JobId,
    lane: Lane,
    token: JobToken,
    cost_bytes: u64,
    output: T,
}

/// A bounded job scheduler with deterministic dispatch and mandatory result
/// re-validation. See the module docs.
pub struct Scheduler<T> {
    config: SchedulerConfig,
    generation: Generation,
    next_id: u64,
    shutting_down: bool,
    queues: [Vec<QueuedJob<T>>; 5],
    running: HashMap<JobId, RunningJob>,
    /// Running jobs whose result must be dropped on completion (cancelled or
    /// superseded by a world reload).
    voided: HashSet<JobId>,
    completed: Vec<CompletedJob<T>>,
    rejected: [u64; 5],
}

impl<T> Scheduler<T> {
    /// A scheduler at [`Generation::START`].
    pub fn new(config: SchedulerConfig) -> Self {
        Self::with_generation(config, Generation::START)
    }

    /// A scheduler whose current world generation is `generation`.
    pub fn with_generation(config: SchedulerConfig, generation: Generation) -> Self {
        Self {
            config,
            generation,
            next_id: 0,
            shutting_down: false,
            queues: Default::default(),
            running: HashMap::new(),
            voided: HashSet::new(),
            completed: Vec::new(),
            rejected: [0; 5],
        }
    }

    pub fn config(&self) -> SchedulerConfig {
        self.config
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// `true` when nothing is queued and nothing is running (results may still be
    /// waiting for [`install`](Self::install)).
    pub fn has_active_jobs(&self) -> bool {
        !self.running.is_empty() || self.queues.iter().any(|q| !q.is_empty())
    }

    /// `true` when queued, running, and completed sets are all empty.
    pub fn is_drained(&self) -> bool {
        !self.has_active_jobs() && self.completed.is_empty()
    }

    /// Admit `request` to its lane, or hand it back with a [`SubmitReason`].
    pub fn submit(&mut self, request: JobRequest<T>) -> Result<JobHandle, Rejected<T>> {
        if self.shutting_down {
            return Err(Rejected {
                reason: SubmitReason::ShuttingDown,
                request,
            });
        }

        let lane = request.lane;
        let budget = self.config.lane(lane);
        let queue = &self.queues[lane.index()];

        let queued_jobs = queue.len() as u32;
        if queued_jobs >= budget.max_queued_jobs {
            self.rejected[lane.index()] += 1;
            return Err(Rejected {
                reason: SubmitReason::JobLimit {
                    lane,
                    queued: queued_jobs,
                    limit: budget.max_queued_jobs,
                },
                request,
            });
        }

        let retained_bytes = self.lane_retained_bytes(lane);
        if request.cost_bytes > budget.max_queued_bytes
            || request.cost_bytes > budget.max_queued_bytes.saturating_sub(retained_bytes)
        {
            self.rejected[lane.index()] += 1;
            return Err(Rejected {
                reason: SubmitReason::ByteLimit {
                    lane,
                    queued: retained_bytes,
                    needed: request.cost_bytes,
                    limit: budget.max_queued_bytes,
                },
                request,
            });
        }

        let id = JobId(self.next_id);
        self.next_id += 1;
        self.queues[lane.index()].push(QueuedJob {
            id,
            priority: request.priority,
            token: request.token,
            cost_bytes: request.cost_bytes,
            run: request.run,
        });
        Ok(JobHandle { id, lane })
    }

    /// Cancel a job by handle. A queued job is removed; a running job's result is
    /// dropped when it completes; an uninstalled completed result is dropped.
    /// Returns `false` if the job is unknown or already installed.
    pub fn cancel(&mut self, handle: JobHandle) -> bool {
        let queue = &mut self.queues[handle.lane.index()];
        if let Some(pos) = queue.iter().position(|j| j.id == handle.id) {
            queue.remove(pos);
            return true;
        }
        if self.running.contains_key(&handle.id) {
            self.voided.insert(handle.id);
            return true;
        }
        if let Some(pos) = self.completed.iter().position(|c| c.id == handle.id) {
            self.completed.remove(pos);
            return true;
        }
        false
    }

    /// Move the highest-priority admitted jobs into the running set, up to each
    /// lane's `max_in_flight`, and return them to be executed. Dispatch order is
    /// a deterministic function of queue contents: by lane, then descending
    /// priority, then ascending job id.
    ///
    /// Every returned [`Dispatch`] must be run and its [`Completion`] applied, or
    /// the job stays counted as running forever.
    #[must_use = "dispatched jobs must be run and applied"]
    pub fn dispatch(&mut self) -> Vec<Dispatch<T>> {
        let mut out = Vec::new();
        for lane in Lane::ALL {
            let budget = self.config.lane(lane);
            let completed = self.completed.iter().filter(|job| job.lane == lane).count() as u32;
            let in_flight = self.running.values().filter(|job| job.lane == lane).count() as u32;
            let completed_capacity = budget
                .max_completed_jobs
                .saturating_sub(completed.saturating_add(in_flight));
            let mut slots = budget
                .max_in_flight
                .saturating_sub(in_flight)
                .min(completed_capacity);

            while slots > 0 {
                let queue = &mut self.queues[lane.index()];
                if queue.is_empty() {
                    break;
                }
                let pick = Self::best_queued(queue);
                let job = queue.remove(pick);
                self.running.insert(
                    job.id,
                    RunningJob {
                        lane,
                        token: job.token.clone(),
                        cost_bytes: job.cost_bytes,
                    },
                );
                out.push(Dispatch {
                    id: job.id,
                    lane,
                    token: job.token,
                    run: job.run,
                });
                slots -= 1;
            }
        }
        out
    }

    fn best_queued(queue: &[QueuedJob<T>]) -> usize {
        let mut best = 0usize;
        for i in 1..queue.len() {
            let cand = &queue[i];
            let cur = &queue[best];
            let better =
                cand.priority > cur.priority || (cand.priority == cur.priority && cand.id < cur.id);
            if better {
                best = i;
            }
        }
        best
    }

    fn lane_retained_bytes(&self, lane: Lane) -> u64 {
        self.queues[lane.index()]
            .iter()
            .map(|job| job.cost_bytes)
            .chain(
                self.running
                    .values()
                    .filter(move |job| job.lane == lane)
                    .map(|job| job.cost_bytes),
            )
            .chain(
                self.completed
                    .iter()
                    .filter(move |job| job.lane == lane)
                    .map(|job| job.cost_bytes),
            )
            .sum()
    }

    /// Record a job's output. Accepts completions in any order. If the job was
    /// cancelled or superseded by a world reload the output is dropped; an
    /// unknown id is ignored.
    pub fn complete(&mut self, id: JobId, output: T) {
        let Some(job) = self.running.remove(&id) else {
            return;
        };
        if self.voided.remove(&id) {
            return;
        }
        self.completed.push(CompletedJob {
            id,
            lane: job.lane,
            token: job.token,
            cost_bytes: job.cost_bytes,
            output,
        });
    }

    /// Convenience wrapper for [`complete`](Self::complete) taking a
    /// [`Completion`].
    pub fn apply(&mut self, completion: Completion<T>) {
        self.complete(completion.id, completion.output);
    }

    /// Validate every completed-but-not-installed result against `world`.
    /// Fresh results are returned to be applied; stale ones are dropped and
    /// reported. This is the guarantee that out-of-order or delayed completions
    /// can never install a result the world has moved past.
    pub fn install<W: WorldView + ?Sized>(&mut self, world: &W) -> InstallOutcome<T> {
        let mut installed = Vec::new();
        let mut discarded = Vec::new();
        for job in std::mem::take(&mut self.completed) {
            match job.token.check(world) {
                Staleness::Fresh => installed.push(Installed {
                    id: job.id,
                    lane: job.lane,
                    token: job.token,
                    output: job.output,
                }),
                reason => discarded.push(Discarded {
                    id: job.id,
                    lane: job.lane,
                    token: job.token,
                    reason,
                }),
            }
        }
        InstallOutcome {
            installed,
            discarded,
        }
    }

    /// Reload the world: set the new generation, drop every queued job, mark
    /// every running job's result for discard on completion, and drop every
    /// uninstalled result. After this, only work submitted against
    /// `new_generation` can be installed.
    pub fn reload_world(&mut self, new_generation: Generation) -> ReloadSummary {
        debug_assert!(
            new_generation > self.generation,
            "world reload must advance the generation"
        );
        self.generation = new_generation;

        let cancelled_queued: u32 = self.queues.iter().map(|q| q.len() as u32).sum();
        for queue in &mut self.queues {
            queue.clear();
        }

        let voided_running = self.running.len() as u32;
        for id in self.running.keys().copied().collect::<Vec<_>>() {
            self.voided.insert(id);
        }

        let dropped_completed = self.completed.len() as u32;
        self.completed.clear();

        ReloadSummary {
            generation: self.generation,
            cancelled_queued,
            voided_running,
            dropped_completed,
        }
    }

    /// Stop accepting submissions and drop every queued job. Already-dispatched
    /// jobs must still be run and applied by their owner; their ids are
    /// returned so the caller can drain them.
    pub fn begin_shutdown(&mut self) -> ShutdownSummary {
        self.shutting_down = true;
        let cancelled_queued: u32 = self.queues.iter().map(|q| q.len() as u32).sum();
        for queue in &mut self.queues {
            queue.clear();
        }
        let mut still_running: Vec<JobId> = self.running.keys().copied().collect();
        still_running.sort_unstable();
        ShutdownSummary {
            cancelled_queued,
            still_running,
        }
    }

    /// Live per-lane load, for overload metrics.
    pub fn pressure(&self) -> Pressure {
        let mut pressure = Pressure::default();
        for lane in Lane::ALL {
            let queue = &self.queues[lane.index()];
            *pressure.lane_mut(lane) = LanePressure {
                queued_jobs: queue.len() as u32,
                queued_bytes: queue.iter().map(|j| j.cost_bytes).sum(),
                in_flight_jobs: 0,
                in_flight_bytes: 0,
                completed_waiting_install: 0,
                completed_bytes: 0,
                rejected: self.rejected[lane.index()],
            };
        }
        for job in self.running.values() {
            let lane = pressure.lane_mut(job.lane);
            lane.in_flight_jobs += 1;
            lane.in_flight_bytes += job.cost_bytes;
        }
        for job in &self.completed {
            let lane = pressure.lane_mut(job.lane);
            lane.completed_waiting_install += 1;
            lane.completed_bytes += job.cost_bytes;
        }
        pressure
    }
}

impl<T> std::fmt::Debug for Scheduler<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("generation", &self.generation)
            .field("shutting_down", &self.shutting_down)
            .field("queued", &self.queues.iter().map(Vec::len).sum::<usize>())
            .field("running", &self.running.len())
            .field("completed", &self.completed.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::LaneBudget;
    use crate::testkit::MapWorld;
    use spall_core::{BrickCoord, Revision, VolumeId};

    fn vol(n: u64) -> VolumeId {
        VolumeId::new(n).unwrap()
    }

    fn token_reading(generation: Generation, brick: i64, rev: u64) -> JobToken {
        JobToken::new(generation, crate::generation::TopologyEpoch::START).reading(
            vol(1),
            BrickCoord::new(brick, 0, 0),
            Revision(rev),
        )
    }

    /// A scheduler with generous, uniform budgets.
    fn open_scheduler() -> Scheduler<u64> {
        Scheduler::with_generation(
            SchedulerConfig::uniform(LaneBudget::new(64, 1 << 30, 32)),
            Generation(1),
        )
    }

    #[test]
    fn out_of_order_completion_cannot_install_stale_results() {
        let mut sched = open_scheduler();
        let mut world = MapWorld::new(Generation(1));
        for brick in 0..3 {
            world.set_brick(vol(1), BrickCoord::new(brick, 0, 0), Revision(1));
        }

        let mut handles = Vec::new();
        for brick in 0..3 {
            handles.push(
                sched
                    .submit(JobRequest::new(
                        Lane::Visual,
                        Priority::NORMAL,
                        token_reading(Generation(1), brick, 1),
                        move || brick as u64 * 10,
                    ))
                    .unwrap(),
            );
        }

        let dispatched = sched.dispatch();
        assert_eq!(dispatched.len(), 3);
        // The world moves the middle brick on while jobs are "running".
        world.set_brick(vol(1), BrickCoord::new(1, 0, 0), Revision(2));

        // Complete out of submission order: 2, 0, 1.
        let mut by_id: HashMap<JobId, Dispatch<u64>> =
            dispatched.into_iter().map(|d| (d.id(), d)).collect();
        for want in [handles[2].id(), handles[0].id(), handles[1].id()] {
            let completion = by_id.remove(&want).unwrap().run();
            sched.apply(completion);
        }

        let outcome = sched.install(&world);
        let installed_ids: Vec<_> = outcome.installed.iter().map(|i| i.id).collect();
        assert_eq!(
            installed_ids,
            vec![handles[2].id(), handles[0].id()],
            "fresh results install in completion order"
        );
        assert_eq!(outcome.discarded.len(), 1);
        assert_eq!(outcome.discarded[0].id, handles[1].id());
        assert!(matches!(
            outcome.discarded[0].reason,
            Staleness::BrickRevision { .. }
        ));
        assert!(sched.is_drained());
    }

    #[test]
    fn world_reload_invalidates_queued_running_and_completed_jobs() {
        let mut sched = open_scheduler();

        // One completed (buffered), one running, one still queued.
        let buffered = sched
            .submit(JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                token_reading(Generation(1), 0, 1),
                || 1,
            ))
            .unwrap();
        let d = sched.dispatch();
        assert_eq!(d.len(), 1);
        for dispatch in d {
            let c = dispatch.run();
            sched.apply(c);
        }
        let running = sched
            .submit(JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                token_reading(Generation(1), 1, 1),
                || 2,
            ))
            .unwrap();
        let running_dispatch = sched.dispatch();
        assert_eq!(running_dispatch.len(), 1);
        let _queued = sched
            .submit(JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                token_reading(Generation(1), 2, 1),
                || 3,
            ))
            .unwrap();

        let summary = sched.reload_world(Generation(2));
        assert_eq!(summary.generation, Generation(2));
        assert_eq!(summary.cancelled_queued, 1);
        assert_eq!(summary.voided_running, 1);
        assert_eq!(summary.dropped_completed, 1);
        let _ = buffered;

        // The running job now finishes; its result is dropped, not buffered.
        for dispatch in running_dispatch {
            let c = dispatch.run();
            sched.apply(c);
        }
        let world = MapWorld::new(Generation(2));
        assert!(sched.install(&world).is_empty());
        assert!(sched.is_drained());
        assert_eq!(sched.pressure().outstanding_jobs(), 0);
        let _ = running;
    }

    #[test]
    fn unload_then_reload_at_the_same_coordinates_is_caught_without_a_generation_bump() {
        let mut sched = open_scheduler();
        let mut world = MapWorld::new(Generation(1));
        world.set_brick(vol(1), BrickCoord::new(7, 0, 0), Revision(4));

        sched
            .submit(JobRequest::new(
                Lane::Topology,
                Priority::NORMAL,
                token_reading(Generation(1), 7, 4),
                || 99,
            ))
            .unwrap();
        let dispatched = sched.dispatch();

        // Same generation, same coordinates: the brick is unloaded and a fresh
        // one streams back in at revision 1.
        world.unload_brick(vol(1), BrickCoord::new(7, 0, 0));
        world.set_brick(vol(1), BrickCoord::new(7, 0, 0), Revision(1));

        for dispatch in dispatched {
            let c = dispatch.run();
            sched.apply(c);
        }
        let outcome = sched.install(&world);
        assert!(outcome.installed.is_empty());
        assert_eq!(outcome.discarded.len(), 1);
        assert!(matches!(
            outcome.discarded[0].reason,
            Staleness::BrickRevision {
                expected: crate::token::DepState::Revision(Revision(4)),
                current: crate::token::BrickStatus::Resident(Revision(1)),
                ..
            }
        ));
    }

    #[test]
    fn queue_saturation_reports_backpressure_and_never_exceeds_the_budget() {
        let config = SchedulerConfig::uniform(LaneBudget::new(2, 100, 1));
        let mut sched: Scheduler<u64> = Scheduler::with_generation(config, Generation(1));

        assert!(
            sched
                .submit(
                    JobRequest::new(
                        Lane::Edit,
                        Priority::NORMAL,
                        token_reading(Generation(1), 0, 1),
                        || 0
                    )
                    .with_cost_bytes(40)
                )
                .is_ok()
        );
        assert!(
            sched
                .submit(
                    JobRequest::new(
                        Lane::Edit,
                        Priority::NORMAL,
                        token_reading(Generation(1), 1, 1),
                        || 0
                    )
                    .with_cost_bytes(40)
                )
                .is_ok()
        );

        // Third job: over the job-count budget.
        let rejected = sched
            .submit(JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                token_reading(Generation(1), 2, 1),
                || 0,
            ))
            .unwrap_err();
        assert!(matches!(
            rejected.reason,
            SubmitReason::JobLimit {
                lane: Lane::Edit,
                queued: 2,
                limit: 2
            }
        ));
        // The request came back intact for a retry.
        assert_eq!(rejected.request.lane, Lane::Edit);

        assert_eq!(sched.pressure().lane(Lane::Edit).queued_jobs, 2);
        assert_eq!(sched.pressure().lane(Lane::Edit).rejected, 1);

        // Byte budget on a different lane.
        let byte_reject = sched
            .submit(
                JobRequest::new(
                    Lane::Visual,
                    Priority::NORMAL,
                    token_reading(Generation(1), 3, 1),
                    || 0,
                )
                .with_cost_bytes(200),
            )
            .unwrap_err();
        assert!(matches!(
            byte_reject.reason,
            SubmitReason::ByteLimit {
                lane: Lane::Visual,
                limit: 100,
                needed: 200,
                ..
            }
        ));
    }

    #[test]
    fn byte_budget_charges_each_job_once_through_running_and_completion() {
        let config = SchedulerConfig::uniform(LaneBudget::new(2, 100, 1));
        let mut sched: Scheduler<u64> = Scheduler::with_generation(config, Generation(1));
        sched
            .submit(
                JobRequest::new(
                    Lane::Topology,
                    Priority::NORMAL,
                    token_reading(Generation(1), 0, 1),
                    || 1,
                )
                .with_cost_bytes(100),
            )
            .unwrap();
        let dispatch = sched.dispatch().pop().unwrap();
        assert_eq!(sched.pressure().lane(Lane::Topology).in_flight_bytes, 100);

        // Dispatching moves the same charge from queued to running; it must not
        // open a second 100-byte admission slot.
        let rejected = sched
            .submit(
                JobRequest::new(
                    Lane::Topology,
                    Priority::NORMAL,
                    token_reading(Generation(1), 1, 1),
                    || 2,
                )
                .with_cost_bytes(100),
            )
            .unwrap_err();
        assert!(matches!(
            rejected.reason,
            SubmitReason::ByteLimit {
                queued: 100,
                needed: 100,
                limit: 100,
                ..
            }
        ));

        sched.apply(dispatch.run());
        let pressure = sched.pressure().lane(Lane::Topology);
        assert_eq!(pressure.in_flight_bytes, 0);
        assert_eq!(pressure.completed_bytes, 100);
        assert_eq!(pressure.completed_waiting_install, 1);
    }

    #[test]
    fn completed_result_limit_backpressures_dispatch_until_install_drains_it() {
        let config = SchedulerConfig::uniform(LaneBudget {
            max_queued_jobs: 2,
            max_queued_bytes: 100,
            max_in_flight: 1,
            max_completed_jobs: 1,
        });
        let mut sched: Scheduler<u64> = Scheduler::with_generation(config, Generation(1));
        sched
            .submit(JobRequest::new(
                Lane::Visual,
                Priority::NORMAL,
                token_reading(Generation(1), 0, 1),
                || 1,
            ))
            .unwrap();
        let first = sched.dispatch().pop().unwrap();
        sched.apply(first.run());
        sched
            .submit(JobRequest::new(
                Lane::Visual,
                Priority::NORMAL,
                token_reading(Generation(1), 1, 1),
                || 2,
            ))
            .unwrap();

        assert!(
            sched.dispatch().is_empty(),
            "completed output holds the lane until installation"
        );
        sched.install(&MapWorld::new(Generation(1)));
        assert_eq!(sched.dispatch().len(), 1, "install frees the result slot");
    }

    #[test]
    fn byte_budget_does_not_wrap_at_u64_max() {
        let config = SchedulerConfig::uniform(LaneBudget::new(2, u64::MAX, 1));
        let mut sched: Scheduler<u64> = Scheduler::with_generation(config, Generation(1));
        sched
            .submit(
                JobRequest::new(
                    Lane::Visual,
                    Priority::NORMAL,
                    token_reading(Generation(1), 0, 1),
                    || 0,
                )
                .with_cost_bytes(u64::MAX),
            )
            .unwrap();
        let _running = sched.dispatch();
        assert!(
            sched
                .submit(
                    JobRequest::new(
                        Lane::Visual,
                        Priority::NORMAL,
                        token_reading(Generation(1), 1, 1),
                        || 0
                    )
                    .with_cost_bytes(1)
                )
                .is_err()
        );
        assert_eq!(
            sched.pressure().lane(Lane::Visual).in_flight_bytes,
            u64::MAX
        );
    }

    #[test]
    fn dispatch_orders_by_priority_then_submission() {
        let config = SchedulerConfig::uniform(LaneBudget::new(16, 1 << 20, 1));
        let mut sched: Scheduler<u64> = Scheduler::with_generation(config, Generation(1));

        let low = sched
            .submit(JobRequest::new(
                Lane::Visual,
                Priority::LOW,
                token_reading(Generation(1), 0, 1),
                || 0,
            ))
            .unwrap();
        let critical = sched
            .submit(JobRequest::new(
                Lane::Visual,
                Priority::CRITICAL,
                token_reading(Generation(1), 1, 1),
                || 0,
            ))
            .unwrap();
        let normal_a = sched
            .submit(JobRequest::new(
                Lane::Visual,
                Priority::NORMAL,
                token_reading(Generation(1), 2, 1),
                || 0,
            ))
            .unwrap();
        let normal_b = sched
            .submit(JobRequest::new(
                Lane::Visual,
                Priority::NORMAL,
                token_reading(Generation(1), 3, 1),
                || 0,
            ))
            .unwrap();

        let mut order = Vec::new();
        for _ in 0..4 {
            let mut d = sched.dispatch();
            assert_eq!(d.len(), 1, "max_in_flight is 1");
            let dispatch = d.pop().unwrap();
            order.push(dispatch.id());
            let c = dispatch.run();
            sched.apply(c);
        }
        assert_eq!(
            order,
            vec![critical.id(), normal_a.id(), normal_b.id(), low.id()]
        );
    }

    #[test]
    fn in_flight_limit_bounds_concurrent_dispatch() {
        let config = SchedulerConfig::uniform(LaneBudget::new(16, 1 << 20, 2));
        let mut sched: Scheduler<u64> = Scheduler::with_generation(config, Generation(1));
        for brick in 0..5 {
            sched
                .submit(JobRequest::new(
                    Lane::Collision,
                    Priority::NORMAL,
                    token_reading(Generation(1), brick, 1),
                    move || brick as u64,
                ))
                .unwrap();
        }

        let batch1 = sched.dispatch();
        assert_eq!(batch1.len(), 2, "capped at max_in_flight");
        assert!(
            sched.dispatch().is_empty(),
            "no free slot until one finishes"
        );
        for dispatch in batch1 {
            let c = dispatch.run();
            sched.apply(c);
        }

        let batch2 = sched.dispatch();
        assert_eq!(batch2.len(), 2);
        for dispatch in batch2 {
            let c = dispatch.run();
            sched.apply(c);
        }

        let batch3 = sched.dispatch();
        assert_eq!(batch3.len(), 1, "only one job left");
        for dispatch in batch3 {
            let c = dispatch.run();
            sched.apply(c);
        }

        assert!(!sched.has_active_jobs());
        assert!(sched.dispatch().is_empty());
    }

    #[test]
    fn cancel_handles_queued_running_and_unknown_jobs() {
        let mut sched = open_scheduler();
        let queued = sched
            .submit(JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                token_reading(Generation(1), 0, 1),
                || 0,
            ))
            .unwrap();
        let also_queued = sched
            .submit(JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                token_reading(Generation(1), 1, 1),
                || 0,
            ))
            .unwrap();
        assert!(sched.cancel(queued));
        assert!(!sched.cancel(queued), "already gone");
        assert_eq!(sched.pressure().lane(Lane::Edit).queued_jobs, 1);

        let dispatched = sched.dispatch();
        assert_eq!(dispatched.len(), 1);
        assert!(sched.cancel(also_queued), "now running, still cancellable");
        for dispatch in dispatched {
            let c = dispatch.run();
            sched.apply(c); // voided: result dropped
        }
        assert!(sched.install(&MapWorld::new(Generation(1))).is_empty());
        assert!(sched.is_drained());
    }

    #[test]
    fn every_job_finishes_or_is_cancelled_on_shutdown() {
        let mut sched = open_scheduler();
        for (i, lane) in [Lane::Edit, Lane::Visual, Lane::Topology, Lane::Collision]
            .into_iter()
            .enumerate()
        {
            sched
                .submit(JobRequest::new(
                    lane,
                    Priority::NORMAL,
                    token_reading(Generation(1), i as i64, 1),
                    move || i as u64,
                ))
                .unwrap();
        }
        let dispatched = sched.dispatch();
        assert_eq!(dispatched.len(), 4, "all four lanes have a free slot");

        // Two more jobs that stay queued (we don't dispatch again) and will be
        // cancelled by shutdown.
        sched
            .submit(JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                token_reading(Generation(1), 10, 1),
                || 0,
            ))
            .unwrap();
        sched
            .submit(JobRequest::new(
                Lane::Visual,
                Priority::NORMAL,
                token_reading(Generation(1), 11, 1),
                || 0,
            ))
            .unwrap();

        let summary = sched.begin_shutdown();
        assert_eq!(summary.cancelled_queued, 2);
        assert_eq!(summary.still_running.len(), 4);
        assert!(
            sched
                .submit(JobRequest::new(
                    Lane::Edit,
                    Priority::NORMAL,
                    token_reading(Generation(1), 12, 1),
                    || 0
                ))
                .is_err()
        );

        for dispatch in dispatched {
            let c = dispatch.run();
            sched.apply(c);
        }
        assert!(
            !sched.has_active_jobs(),
            "nothing queued or running after drain"
        );
    }
}

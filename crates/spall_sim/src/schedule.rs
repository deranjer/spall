//! The edit pipeline: bounded off-tick staging, request-ordered commit, and the
//! conflict policy from `docs/architecture.md`.
//!
//! > Conflict rule: jobs may overlap in preparation, but commits revalidate
//! > every read dependency. Conflicting older work retries or is recomputed in
//! > request order. After repeated conflicts, serialize the affected region
//! > through a bounded queue to guarantee progress.
//!
//! [`EditPipeline`] owns a [`spall_jobs::Scheduler`] for staging, the accepted
//! intents not yet submitted, per-region conflict counts, the set of regions
//! promoted to strictly-serial commit, and the idempotency ledger that makes a
//! repeated [`RequestId`] a no-op.

use std::collections::{HashMap, HashSet, VecDeque};

use spall_core::{BrickCoord, Tick, VolumeId};
use spall_jobs::{
    JobId, JobRequest, JobToken, Lane, Priority, Scheduler, SchedulerConfig, ThreadJobPool,
};
use spall_protocol::{ActionOutcome, ActionStatus, ControlSeq, RequestId};

use crate::commit::{CommitError, CommitOutcome, Committed, commit};
use crate::intent::{EditIntent, EditTarget, IntentError};
use crate::journal::JournalSink;
use crate::stage::{StageError, StageInput, StagedEdit, stage_edit, stage_edit_owned};
use crate::world::SimWorld;

/// A coarse identity for the region an edit contends over: the brush centre's
/// brick in the target volume. Two edits on the same key can conflict at commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegionKey {
    pub volume: VolumeId,
    pub brick: BrickCoord,
}

impl RegionKey {
    fn of(volume: VolumeId, brush: &spall_core::SphereBrush) -> Self {
        let unit = spall_core::BRUSH_UNIT;
        let cell = spall_core::GlobalCell::new(
            brush.centre.x.div_euclid(unit),
            brush.centre.y.div_euclid(unit),
            brush.centre.z.div_euclid(unit),
        );
        Self {
            volume,
            brick: cell.split().0,
        }
    }
}

/// The default staging result type: staging can fail deterministically (an empty
/// brush, an out-of-bounds edit).
type StageResult = Result<StagedEdit, StageError>;

/// A panic payload caught on the worker, which the owning thread re-raises rather than losing.
type WorkerPanic = Box<dyn std::any::Any + Send>;

/// The staging result, or the panic inside staging.
type OffThreadResult = Result<StageResult, WorkerPanic>;

/// What the worker hands back.
enum WorkerOutput {
    /// An edit was staged.
    Staged(Box<OffThreadResult>),
    /// The terrain's structure index was built ahead of the next edit, for the volume state
    /// `stamp`.
    Prewarmed {
        volume: VolumeId,
        stamp: u64,
        built: Result<Option<std::sync::Arc<spall_structure::StructureIndex>>, WorkerPanic>,
    },
}

/// The worker thread that stages edits off the tick thread (see
/// [`EditPipeline::enable_off_thread_staging`]). Dropping it stops the thread; a job already
/// running is allowed to finish.
struct StagingWorker {
    pool: Option<ThreadJobPool<WorkerOutput>>,
}

impl StagingWorker {
    fn new() -> Self {
        Self {
            pool: Some(ThreadJobPool::new(SchedulerConfig::default(), 1)),
        }
    }

    fn pool(&self) -> &ThreadJobPool<WorkerOutput> {
        self.pool.as_ref().expect("the pool lives until drop")
    }
}

impl Drop for StagingWorker {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            let _ = pool.shutdown();
        }
    }
}

#[derive(Debug, Clone)]
struct QueuedIntent {
    intent: EditIntent,
    volume_id: VolumeId,
    region: RegionKey,
    attempts: u32,
    /// When the intent entered the queue.
    queued_at: std::time::Instant,
    /// When its staging job last started, and when the tick found the result.
    job_started: Option<std::time::Instant>,
    result_seen: Option<std::time::Instant>,
}

/// Where one committed edit's time went inside the pipeline, for the server's latency log.
#[derive(Debug, Clone, Copy, Default)]
pub struct EditTiming {
    /// Entering the queue until the staging job last started.
    pub queued: std::time::Duration,
    /// The staging job starting until the tick picked up its result: staging itself plus the
    /// wait for the next tick boundary.
    pub staging: std::time::Duration,
    /// Committing on the tick thread.
    pub commit: std::time::Duration,
}

/// What one [`EditPipeline::run_tick`] did.
#[derive(Debug, Default)]
pub struct TickReport {
    /// Time spent in the owning world's physics step, including extraction of
    /// updated body poses. This is populated by `Simulation::tick` so server
    /// telemetry can report the physics portion without timing a replica.
    pub physics_duration: std::time::Duration,
    /// Authoritative fluid work, when this simulation has an installed water
    /// region. The fluid step runs after committed terrain edits and before
    /// the Rapier step.
    pub water: Option<crate::water::WaterTickMetrics>,
    pub water_regions: Vec<crate::water::WaterTickMetrics>,
    /// Requests that committed this tick, with their transaction summary.
    pub committed: Vec<(RequestId, Committed)>,
    /// Requests whose commit lost a conflict and were re-queued for recompute.
    pub retried: Vec<RequestId>,
    /// Requests rejected by deterministic staging failure.
    pub rejected: Vec<(RequestId, String)>,
    /// Staged results dropped by the scheduler (generation / epoch moved).
    pub discarded_stale: Vec<RequestId>,
    /// Regions promoted to serial commit this tick.
    pub serialized_regions: Vec<RegionKey>,
    /// Intents still waiting after this tick.
    pub pending_after: usize,
    /// T23 / G3 row 7 (ENG-30 row 7 increment 13): every brick the pipeline
    /// itself reloaded this tick to satisfy a staging/commit
    /// `EvictedGeometryRequired`. A residency pass uses this to grant the
    /// reloaded brick a short pin so it is not evicted again before the
    /// re-queued intent's retry (next tick) can actually use it — the
    /// preflight/consumer-lifetime pinning the frozen contract calls for,
    /// rather than relying only on the reactive reload succeeding.
    pub reloaded_bricks: Vec<(VolumeId, BrickCoord)>,
}

/// The bounded staging + commit pipeline.
pub struct EditPipeline {
    scheduler: Scheduler<StageResult>,
    pending: VecDeque<QueuedIntent>,
    inflight: HashMap<JobId, QueuedIntent>,
    conflicts: HashMap<RegionKey, u32>,
    serialized: HashSet<RegionKey>,
    committed: HashMap<u64, Committed>,
    /// The idempotency ledger for every intent that passed admission.  A value
    /// remains `Queued` while it is pending or in flight, then becomes its
    /// terminal committed/rejected status.  This deliberately lives beside the
    /// pipeline rather than in a transport session: a reliable retry may arrive
    /// on a replacement connection.
    statuses: HashMap<u64, ActionStatus>,
    /// Regions with a job in flight or staged-not-committed this tick.
    active_regions: HashSet<RegionKey>,
    max_pending: usize,
    serialize_threshold: u32,
    /// Brick labels of the live terrain, memoized by revision so staging an
    /// edit relabels only the bricks it changed instead of the whole world.
    label_cache: spall_structure::LabelCache,
    /// `Some` once staging has moved off the tick thread.
    staging_worker: Option<StagingWorker>,
    /// Where each recently committed edit's time went, until the server reads it.
    timings: HashMap<u64, EditTiming>,
    /// The worker job currently building the terrain's structure index ahead of an edit.
    prewarm_job: Option<JobId>,
    /// The terrain state a prewarm was last started for, so one that cannot finish (or whose
    /// result is discarded) is not retried in a loop.
    last_prewarm: Option<(VolumeId, u64, spall_jobs::TopologyEpoch)>,
}

impl EditPipeline {
    pub fn new(max_pending: usize, serialize_threshold: u32) -> Self {
        Self {
            scheduler: Scheduler::new(SchedulerConfig::default()),
            pending: VecDeque::new(),
            inflight: HashMap::new(),
            conflicts: HashMap::new(),
            serialized: HashSet::new(),
            committed: HashMap::new(),
            statuses: HashMap::new(),
            active_regions: HashSet::new(),
            max_pending,
            serialize_threshold: serialize_threshold.max(1),
            label_cache: spall_structure::LabelCache::new(),
            staging_worker: None,
            timings: HashMap::new(),
            prewarm_job: None,
            last_prewarm: None,
        }
    }

    /// Moves staging onto a worker thread. The tick thread then only snapshots the world,
    /// installs a finished result, and commits it, so staging cost no longer lengthens a tick.
    ///
    /// One request is staged at a time, in request order, against the world as the previous
    /// commit left it; a result whose world moved on is discarded and staged again. What is
    /// committed, and in which order, is the same as inline staging, but *which tick* a commit
    /// lands on now depends on how long staging took in wall-clock time, so this is for a
    /// real-time server; headless and deterministic runs keep the inline default.
    pub fn enable_off_thread_staging(&mut self) {
        if self.staging_worker.is_none() {
            self.staging_worker = Some(StagingWorker::new());
        }
    }

    /// Whether staging runs on a worker thread.
    pub fn stages_off_thread(&self) -> bool {
        self.staging_worker.is_some()
    }

    /// Labels every terrain brick into the label cache now, so the first edit
    /// of a large world does not pay for cold local labelling and content hashes.
    /// Global support assembly still runs on edits. Safe to skip: an unwarmed
    /// cache fills on the first staging pass.
    pub fn warm_labels(&self, world: &SimWorld) {
        let _span = crate::prof::Span::start("startup.local_labels_and_hashes");
        let workers = std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1));
        self.label_cache
            .warm_volume(&world.terrain().volume, workers);
    }

    /// The committed [`Committed`] for `request_id`, if it has committed.
    pub fn committed(&self, request_id: RequestId) -> Option<&Committed> {
        self.committed.get(&request_id.0)
    }

    /// Takes the recorded pipeline timing of a committed request, if still held.
    pub fn take_timing(&mut self, request_id: RequestId) -> Option<EditTiming> {
        self.timings.remove(&request_id.0)
    }

    /// The current authoritative status for an admitted request.  A repeated
    /// request id must receive this exact value without re-staging the intent.
    pub fn action_status(&self, request_id: RequestId) -> Option<&ActionStatus> {
        self.statuses.get(&request_id.0)
    }

    /// `true` when nothing is pending, in flight, or awaiting install.
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.inflight.is_empty() && self.scheduler.is_drained()
    }

    /// Bodies targeted by an intent that is queued or staged but not yet committed. Dormancy
    /// must not deactivate these: a commit rebuilds the target's collider, which needs a live
    /// physics body.
    pub fn targeted_bodies(&self) -> HashSet<spall_core::EntityId> {
        self.pending
            .iter()
            .chain(self.inflight.values())
            .filter_map(|q| match q.intent.target {
                EditTarget::Body(e) => Some(e),
                EditTarget::Terrain => None,
            })
            .collect()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Whether `region` has been promoted to strictly-serial commit.
    pub fn is_serialized(&self, region: RegionKey) -> bool {
        self.serialized.contains(&region)
    }

    /// Admits an accepted intent, returning its authoritative status.
    ///
    /// A first admission returns `Queued`. A duplicate returns the existing
    /// `Queued`, `Committed`, or deterministic `Rejected` status without
    /// changing the queue, world, journal, or one-time impulse state.
    pub fn submit_intent(
        &mut self,
        intent: EditIntent,
        world: &SimWorld,
    ) -> Result<ActionStatus, IntentError> {
        let request = intent.request_id;
        if let Some(status) = self.statuses.get(&request.0) {
            return Ok(status.clone());
        }
        if self.pending.len() >= self.max_pending {
            return Err(IntentError::QueueFull {
                limit: self.max_pending,
            });
        }

        let volume_id = match intent.target {
            EditTarget::Terrain => world.terrain_volume_id(),
            EditTarget::Body(entity) => world
                .body(entity)
                .map(|b| b.volume_id)
                .ok_or(IntentError::UnknownBody(entity))?,
        };
        let region = RegionKey::of(volume_id, &intent.brush);
        self.pending.push_back(QueuedIntent {
            intent,
            volume_id,
            region,
            attempts: 0,
            queued_at: std::time::Instant::now(),
            job_started: None,
            result_seen: None,
        });
        let status = crate::commit::queued_status(request);
        self.statuses.insert(request.0, status.clone());
        Ok(status)
    }

    /// Runs one submit → stage → install → commit round.
    pub fn run_tick(
        &mut self,
        world: &mut SimWorld,
        journal: &mut JournalSink,
        server_tick: Tick,
        next_control_seq: &mut u64,
    ) -> Result<TickReport, CommitError> {
        if self.staging_worker.is_some() {
            return Ok(self.run_tick_off_thread(world, journal, server_tick, next_control_seq));
        }
        let mut report = TickReport::default();
        // A structural commit advances the global topology epoch.  Results
        // staged later in the same batch therefore fail the commit-time token
        // check even when they target an independent region.  Rebase a
        // bounded number of those requests in this tick so a burst does not
        // collapse to one topology commit per tick.  This deliberately keeps
        // the global epoch contract (and its conservative invalidation) intact;
        // region-scoped epochs are a separate design change.
        const MAX_REBASE_ROUNDS: usize = 4;
        let mut rebase_round = 0usize;
        // The edit lane's capacity is also the submission budget for the entire tick, including
        // any stale-result rebase rounds below.
        let max_submit = self.scheduler.config().lane(Lane::Edit).max_in_flight as usize;
        let mut submitted = 0usize;
        let mut serialized_regions_this_tick = HashSet::new();
        // Geometry reloads are an external residency transition, not a stale
        // topology rebase. Keep their intents out of `pending` until this
        // tick has fully drained: otherwise an unrelated stale result can
        // enter another same-tick round and execute a reload retry despite
        // the retry contract promising the next tick.
        let mut reload_retries_next_tick = VecDeque::new();

        loop {
            self.active_regions.clear();

            // 1. Submit eligible pending intents to the staging scheduler.
            let generation = world.generation();
            let epoch = world.topology_epoch();
            let mut deferred: VecDeque<QueuedIntent> = VecDeque::new();
            // Keep jobs waiting for a scheduler dispatch slot out of its queue. A queued job
            // holds this tick's topology token; a commit later in this round invalidates it
            // before it can run, wasting a slot and starving fresh work under sustained load.
            // Deferring the intent lets it be submitted with a current token on the next tick.
            while let Some(queued) = self.pending.pop_front() {
                if submitted >= max_submit {
                    deferred.push_back(queued);
                    continue;
                }
                // A serialized region admits at most one job per tick.  The
                // separate per-tick set keeps that lane's contract intact
                // across same-tick rebase rounds, while independent regions
                // can still make progress in the same tick.
                if self.serialized.contains(&queued.region)
                    && (self.active_regions.contains(&queued.region)
                        || serialized_regions_this_tick.contains(&queued.region))
                {
                    deferred.push_back(queued);
                    continue;
                }

                let Some(snapshot) = world.volume_ref(queued.volume_id).cloned() else {
                    let reason = "target volume vanished".to_string();
                    self.record_rejection(queued.intent.request_id, reason.clone());
                    report.rejected.push((queued.intent.request_id, reason));
                    continue;
                };
                let input_stamp = snapshot.state_stamp();
                // Stagings against one unchanged terrain share one whole-world index: the first
                // builds it, the rest clone it (staging runs here, single-threaded, so nothing
                // can change the volume in between).
                let warm_index = (queued.volume_id == world.terrain_volume_id())
                    .then(|| {
                        world
                            .warm_structure_index(queued.volume_id, input_stamp)
                            .or_else(|| {
                                let _span = crate::prof::Span::start("schedule.warm_index_build");
                                let built = std::sync::Arc::new(
                                    spall_structure::StructureIndex::build_cached(
                                        &snapshot,
                                        world.anchor(),
                                        spall_structure::ResidencyMode::AllResident,
                                        generation,
                                        epoch,
                                        &spall_structure::CancelToken::new(),
                                        &self.label_cache,
                                    )
                                    .ok()?,
                                );
                                world.set_warm_structure(
                                    queued.volume_id,
                                    input_stamp,
                                    built.clone(),
                                    None,
                                );
                                Some(built)
                            })
                    })
                    .flatten();
                let input = StageInput::new(
                    &queued.intent,
                    queued.volume_id,
                    snapshot,
                    world.evicted(queued.volume_id).clone(),
                    world.anchor(),
                    generation,
                    epoch,
                )
                .with_label_cache(self.label_cache.clone())
                .with_warm_index(warm_index)
                .with_warm_removal(world.warm_structure_removal(queued.volume_id, input_stamp));
                let request = JobRequest::new(
                    Lane::Edit,
                    Priority::NORMAL,
                    JobToken::new(generation, epoch),
                    move || stage_edit(&input),
                );
                match self.scheduler.submit(request) {
                    Ok(handle) => {
                        submitted += 1;
                        self.active_regions.insert(queued.region);
                        if self.serialized.contains(&queued.region) {
                            serialized_regions_this_tick.insert(queued.region);
                        }
                        self.inflight.insert(handle.id(), queued);
                    }
                    Err(_rejected) => {
                        // Lane full: hold this and everything after it for next tick.
                        deferred.push_front(queued);
                        break;
                    }
                }
            }
            self.pending.append(&mut deferred);

            // 2. Run every dispatched staging job (deterministic, no threads).
            for dispatch in self.scheduler.dispatch() {
                let completion = dispatch.run();
                self.scheduler.apply(completion);
            }

            // 3. Install: fresh staged results in completion (== request) order.
            let installed = self.scheduler.install(&*world);
            for discarded in installed.discarded {
                if let Some(queued) = self.inflight.remove(&discarded.id) {
                    report.discarded_stale.push(queued.intent.request_id);
                    self.pending.push_back(queued); // recompute against the new world
                }
            }

            // 4. Commit in order.
            let mut rebase_requested = false;
            for entry in installed.installed {
                let Some(queued) = self.inflight.remove(&entry.id) else {
                    continue;
                };
                rebase_requested |= self.finish_staged(
                    world,
                    journal,
                    server_tick,
                    next_control_seq,
                    queued,
                    entry.output,
                    &mut report,
                    &mut reload_retries_next_tick,
                    &mut serialized_regions_this_tick,
                    false,
                );
            }

            // Only a commit-time stale result requests an in-tick rebase.  All
            // other retries (for example an evicted-brick reload) remain
            // queued for the next tick.  The hard round limit guarantees a
            // burst cannot turn one server tick into an unbounded retry loop.
            if !rebase_requested || rebase_round == MAX_REBASE_ROUNDS {
                break;
            }
            rebase_round += 1;
        }

        self.pending.append(&mut reload_retries_next_tick);
        report.pending_after = self.pending.len();
        Ok(report)
    }

    /// One tick with staging on the worker thread: commit what has finished, then start the
    /// next request against the world as it now stands.
    fn run_tick_off_thread(
        &mut self,
        world: &mut SimWorld,
        journal: &mut JournalSink,
        server_tick: Tick,
        next_control_seq: &mut u64,
    ) -> TickReport {
        let mut report = TickReport::default();
        let mut reload_retries_next_tick = VecDeque::new();
        let mut serialized_regions_this_tick = HashSet::new();

        // 1. Take what the worker finished. Install re-checks the generation and topology
        //    epoch; the staged edit's own token is re-checked again at commit.
        let outcome = self
            .staging_worker
            .as_ref()
            .expect("checked by the caller")
            .pool()
            .install(&*world);
        for discarded in outcome.discarded {
            if self.prewarm_job == Some(discarded.id) {
                self.prewarm_job = None;
            } else if let Some(queued) = self.inflight.remove(&discarded.id) {
                report.discarded_stale.push(queued.intent.request_id);
                self.pending.push_front(queued);
            }
        }
        for entry in outcome.installed {
            if self.prewarm_job == Some(entry.id) {
                self.prewarm_job = None;
                if let WorkerOutput::Prewarmed {
                    volume,
                    stamp,
                    built,
                } = entry.output
                {
                    match built {
                        // Keep it only if the volume is still in the state it was built from.
                        Ok(Some(index))
                            if world.volume_ref(volume).map(|v| v.state_stamp()) == Some(stamp) =>
                        {
                            world.set_warm_structure(volume, stamp, index, None);
                        }
                        Ok(_) => {}
                        Err(payload) => std::panic::resume_unwind(payload),
                    }
                }
                continue;
            }
            let Some(mut queued) = self.inflight.remove(&entry.id) else {
                continue;
            };
            queued.result_seen = Some(std::time::Instant::now());
            let output = match entry.output {
                WorkerOutput::Staged(staged) => match *staged {
                    Ok(output) => output,
                    // A panic in staging is a bug: fail where it is visible, with its
                    // message, not by silently losing the request.
                    Err(payload) => std::panic::resume_unwind(payload),
                },
                WorkerOutput::Prewarmed { .. } => unreachable!("a prewarm result has no request"),
            };
            self.finish_staged(
                world,
                journal,
                server_tick,
                next_control_seq,
                queued,
                output,
                &mut report,
                &mut reload_retries_next_tick,
                &mut serialized_regions_this_tick,
                true,
            );
        }

        // 2. Start the next request. It snapshots the world after the commits above, so it is
        //    not stale on arrival unless something else changes the world in the meantime.
        while self.inflight.is_empty() && self.prewarm_job.is_none() {
            let Some(queued) = self.pending.pop_front() else {
                break;
            };
            let Some(snapshot) = world.volume_ref(queued.volume_id).cloned() else {
                let reason = "target volume vanished".to_string();
                self.record_rejection(queued.intent.request_id, reason.clone());
                report.rejected.push((queued.intent.request_id, reason));
                continue;
            };
            let generation = world.generation();
            let epoch = world.topology_epoch();
            let stamp = snapshot.state_stamp();
            // The worker takes the warm index and edits it in place; the commit hands the edited
            // one back. If this staging is discarded the index is gone, so forget that a prewarm
            // was tried for this state and let an idle tick build another.
            let (warm_index, warm_removal) = match (queued.volume_id == world.terrain_volume_id())
                .then(|| world.take_warm_structure(queued.volume_id, stamp))
                .flatten()
            {
                Some((index, removal)) => {
                    self.last_prewarm = None;
                    (Some(index), removal)
                }
                None => (None, None),
            };
            let input = StageInput::new(
                &queued.intent,
                queued.volume_id,
                snapshot,
                world.evicted(queued.volume_id).clone(),
                world.anchor(),
                generation,
                epoch,
            )
            .with_label_cache(self.label_cache.clone())
            .with_warm_index(warm_index)
            .with_warm_removal(warm_removal);
            let request = JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                JobToken::new(generation, epoch),
                move || {
                    WorkerOutput::Staged(Box::new(std::panic::catch_unwind(
                        std::panic::AssertUnwindSafe(|| stage_edit_owned(input)),
                    )))
                },
            );
            let worker = self.staging_worker.as_ref().expect("checked by the caller");
            match worker.pool().submit(request) {
                Ok(handle) => {
                    let mut queued = queued;
                    queued.job_started = Some(std::time::Instant::now());
                    self.inflight.insert(handle.id(), queued);
                }
                Err(_rejected) => {
                    self.pending.push_front(queued);
                    break;
                }
            }
        }

        // 3. With nothing waiting, build the terrain's structure index for its current state on
        //    the worker, so the next edit finds it ready instead of building it itself (about
        //    0.4 s on a large world). This covers the first edit after startup, after a world
        //    reset, and after a commit that split a body, which leaves no index behind.
        if self.inflight.is_empty() && self.pending.is_empty() && self.prewarm_job.is_none() {
            self.start_prewarm(world);
        }

        self.pending.append(&mut reload_retries_next_tick);
        report.pending_after = self.pending.len();
        report
    }

    /// Starts building the terrain's structure index on the worker, unless it already has one
    /// for the volume's current state or a build for this exact state was already tried.
    fn start_prewarm(&mut self, world: &SimWorld) {
        let volume = world.terrain_volume_id();
        let Some(snapshot) = world.volume_ref(volume) else {
            return;
        };
        let stamp = snapshot.state_stamp();
        let epoch = world.topology_epoch();
        if world.warm_structure_index(volume, stamp).is_some()
            || self.last_prewarm == Some((volume, stamp, epoch))
        {
            return;
        }
        let snapshot = snapshot.clone();
        let generation = world.generation();
        let anchor = world.anchor();
        let cache = self.label_cache.clone();
        let request = JobRequest::new(
            Lane::Edit,
            Priority::NORMAL,
            JobToken::new(generation, epoch),
            move || {
                let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    spall_structure::StructureIndex::build_cached(
                        &snapshot,
                        anchor,
                        spall_structure::ResidencyMode::AllResident,
                        generation,
                        epoch,
                        &spall_structure::CancelToken::new(),
                        &cache,
                    )
                    .ok()
                    .map(std::sync::Arc::new)
                }));
                WorkerOutput::Prewarmed {
                    volume,
                    stamp,
                    built,
                }
            },
        );
        let worker = self.staging_worker.as_ref().expect("off-thread staging");
        if let Ok(handle) = worker.pool().submit(request) {
            self.prewarm_job = Some(handle.id());
            self.last_prewarm = Some((volume, stamp, epoch));
        }
    }

    /// Handles one staged result for `queued`: commits it, or reloads, rejects or re-queues it.
    /// Returns whether a commit-time stale result asks for an in-tick rebase. With
    /// `requeue_stale_first` a stale intent goes to the front of the queue so request order
    /// survives (the off-thread path stages one request at a time).
    #[allow(clippy::too_many_arguments)]
    fn finish_staged(
        &mut self,
        world: &mut SimWorld,
        journal: &mut JournalSink,
        server_tick: Tick,
        next_control_seq: &mut u64,
        queued: QueuedIntent,
        output: StageResult,
        report: &mut TickReport,
        reload_retries_next_tick: &mut VecDeque<QueuedIntent>,
        serialized_regions_this_tick: &mut HashSet<RegionKey>,
        requeue_stale_first: bool,
    ) -> bool {
        let mut rebase_requested = false;
        let request = queued.intent.request_id;
        let region = queued.region;

        let staged = match output {
            Ok(staged) => staged,
            // T23 / G3 row 7, slice C: the edit needs an evicted brick's
            // cells. Reload it from the backing and re-stage next tick; if
            // no backing has it, reject with a bounded explicit failure.
            Err(StageError::EvictedGeometryRequired(bricks)) => {
                self.conflicts.remove(&region);
                match world.reload_bricks(queued.volume_id, bricks.iter().copied()) {
                    Ok(true) => {
                        report.retried.push(request);
                        report
                            .reloaded_bricks
                            .extend(bricks.iter().map(|&b| (queued.volume_id, b)));
                        reload_retries_next_tick.push_back(QueuedIntent {
                            attempts: queued.attempts + 1,
                            ..queued
                        });
                    }
                    _ => {
                        let reason = format!("evicted geometry unavailable for reload: {bricks:?}");
                        self.record_rejection(request, reason.clone());
                        report.rejected.push((request, reason));
                    }
                }
                return false;
            }
            Err(err) => {
                let reason = err.to_string();
                self.record_rejection(request, reason.clone());
                report.rejected.push((request, reason));
                self.conflicts.remove(&region);
                return false;
            }
        };

        if self.committed.contains_key(&request.0) {
            return false; // idempotent: already committed
        }

        // Defensive: a body-targeted commit rebuilds its collider, which needs the live
        // physics body. `submit` reactivates a dormant target and dormancy skips targeted
        // bodies, but never let a commit reach a dormant body (it would panic in the solver).
        if let EditTarget::Body(entity) = queued.intent.target
            && world.body_is_dormant(entity)
        {
            world.reactivate_body(entity);
        }

        let control_seq = ControlSeq(*next_control_seq);
        let commit_started = std::time::Instant::now();
        match commit(world, journal, &staged, server_tick, control_seq) {
            Ok(CommitOutcome::Committed(done)) => {
                if self.timings.len() >= 4096 {
                    self.timings.clear();
                }
                self.timings.insert(
                    request.0,
                    EditTiming {
                        queued: queued
                            .job_started
                            .map_or_else(Default::default, |at| at - queued.queued_at),
                        staging: queued
                            .job_started
                            .zip(queued.result_seen)
                            .map_or_else(Default::default, |(started, seen)| seen - started),
                        commit: commit_started.elapsed(),
                    },
                );
                *next_control_seq += 1;
                self.conflicts.remove(&region);
                self.committed.insert(request.0, done.clone());
                self.statuses.insert(request.0, done.action_status(request));
                report.committed.push((request, done));
            }
            // T23 / G3 row 7, slice C: the collider rebuild needs an evicted
            // brick's cells. Reload from the backing and re-commit next
            // tick; reject if unavailable.
            Err(CommitError::EvictedGeometryRequired { volume, bricks }) => {
                self.conflicts.remove(&region);
                match world.reload_bricks(volume, bricks.iter().copied()) {
                    Ok(true) => {
                        report.retried.push(request);
                        report
                            .reloaded_bricks
                            .extend(bricks.iter().map(|&b| (volume, b)));
                        reload_retries_next_tick.push_back(QueuedIntent {
                            attempts: queued.attempts + 1,
                            ..queued
                        });
                    }
                    _ => {
                        let reason = format!("evicted geometry unavailable for reload: {bricks:?}");
                        self.record_rejection(request, reason.clone());
                        report.rejected.push((request, reason));
                    }
                }
            }
            Err(err) => {
                // The commit candidate failed a fallible step (id exhaustion,
                // DTO validation, op-budget) and was discarded before any
                // live state changed (`ENG-54`). Reject the request
                // deterministically; the tick continues and every other
                // staged request still commits.
                self.conflicts.remove(&region);
                let reason = err.to_string();
                self.record_rejection(request, reason.clone());
                report.rejected.push((request, reason));
            }
            Ok(CommitOutcome::Stale(_reason)) => {
                let count = self.conflicts.entry(region).or_insert(0);
                *count += 1;
                if *count >= self.serialize_threshold && self.serialized.insert(region) {
                    report.serialized_regions.push(region);
                    serialized_regions_this_tick.insert(region);
                }
                report.retried.push(request);
                let retry = QueuedIntent {
                    attempts: queued.attempts + 1,
                    ..queued
                };
                if requeue_stale_first {
                    self.pending.push_front(retry);
                } else {
                    self.pending.push_back(retry);
                }
                rebase_requested = true;
            }
        }

        rebase_requested
    }

    /// T23 / G3 row 7 (ENG-30 row 7 increment 13): the bounded brick footprint
    /// every currently-queued (not yet staged/committed) intent targeting
    /// `volume` will need once it stages — its brush AABB grown by one brick,
    /// the same halo a structural search can spill into. A residency pass
    /// pins these so a pending edit's dependencies are reserved *before*
    /// staging discovers them reactively via `EvictedGeometryRequired`, per
    /// the frozen preflight-pinning contract
    /// (`docs/reports/G3-residency-hash.md`).
    ///
    /// Bounded: `spall_core::SphereBrush` already caps a single edit's radius
    /// at `MAX_BRUSH_RADIUS_CELLS` (256 cells), so one intent contributes at
    /// most a bounded cube of bricks; a pending queue whose combined
    /// footprint would exceed `MAX_PENDING_PIN_BRICKS` falls back to pinning
    /// only each remaining intent's centre brick, so this call is never
    /// unbounded allocation over an adversarial queue.
    pub fn pending_dependency_bricks(&self, volume: VolumeId) -> HashSet<BrickCoord> {
        const MAX_PENDING_PIN_BRICKS: usize = 4096;
        // One brick of margin beyond the brush's own cell bounds, for a
        // structural search spilling into a neighbour
        // (`docs/architecture.md`: "Meshing includes a one-cell halo ...").
        const HALO_BRICKS: i64 = 1;
        let mut out = HashSet::new();
        for queued in self.pending.iter().chain(self.inflight.values()) {
            if queued.volume_id != volume {
                continue;
            }
            let brush = &queued.intent.brush;
            let unit = spall_core::BRUSH_UNIT;
            let r = brush.radius_units().div_euclid(unit)
                + i64::from(brush.radius_units().rem_euclid(unit) != 0);
            let centre_x = brush.centre.x.div_euclid(unit);
            let centre_y = brush.centre.y.div_euclid(unit);
            let centre_z = brush.centre.z.div_euclid(unit);
            // A per-axis cell AABB converted to its own brick bounds, not a
            // brick radius applied uniformly around the centre brick -- a
            // small brush interior to one brick must not pin a disproportionate
            // box just because a large brush centred at a brick boundary
            // hypothetically could.
            let (min_b, _) =
                spall_core::GlobalCell::new(centre_x - r, centre_y - r, centre_z - r).split();
            let (max_b, _) =
                spall_core::GlobalCell::new(centre_x + r, centre_y + r, centre_z + r).split();
            let lo = BrickCoord::new(
                min_b.x - HALO_BRICKS,
                min_b.y - HALO_BRICKS,
                min_b.z - HALO_BRICKS,
            );
            let hi = BrickCoord::new(
                max_b.x + HALO_BRICKS,
                max_b.y + HALO_BRICKS,
                max_b.z + HALO_BRICKS,
            );
            let nx = (hi.x - lo.x + 1).max(0);
            let ny = (hi.y - lo.y + 1).max(0);
            let nz = (hi.z - lo.z + 1).max(0);
            let cube = (nx as i128) * (ny as i128) * (nz as i128);
            if out.len() >= MAX_PENDING_PIN_BRICKS || cube > MAX_PENDING_PIN_BRICKS as i128 {
                // Pathological (near-maximal-radius) brush: fall back to just
                // the centre brick rather than skip the intent's dependency
                // entirely.
                let (centre_brick, _) =
                    spall_core::GlobalCell::new(centre_x, centre_y, centre_z).split();
                out.insert(centre_brick);
                continue;
            }
            'brush: for z in lo.z..=hi.z {
                for y in lo.y..=hi.y {
                    for x in lo.x..=hi.x {
                        out.insert(BrickCoord::new(x, y, z));
                        if out.len() >= MAX_PENDING_PIN_BRICKS {
                            break 'brush;
                        }
                    }
                }
            }
        }
        out
    }

    fn record_rejection(&mut self, request: RequestId, reason: String) {
        self.statuses.insert(
            request.0,
            ActionStatus {
                request_id: request,
                outcome: ActionOutcome::Rejected { reason },
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::units::{BRUSH_UNIT, BrushPoint};
    use spall_core::{EntityId, SphereBrush};
    use spall_protocol::ActionOutcome;

    fn actor() -> EntityId {
        EntityId::new(1).unwrap()
    }

    fn cut(request: u64, x: i64, y: i64, z: i64, radius_cells: i64) -> EditIntent {
        EditIntent::cut(
            RequestId(request),
            actor(),
            EditTarget::Terrain,
            SphereBrush::new(
                BrushPoint::from_units(
                    x * BRUSH_UNIT + BRUSH_UNIT / 2,
                    y * BRUSH_UNIT + BRUSH_UNIT / 2,
                    z * BRUSH_UNIT + BRUSH_UNIT / 2,
                ),
                radius_cells * BRUSH_UNIT,
            )
            .unwrap(),
        )
    }

    #[test]
    fn region_key_is_the_brush_centre_brick() {
        let vid = VolumeId::new(1).unwrap();
        // centre near global cell (40, 3, -1) -> brick (1, 0, -1).
        let brush = SphereBrush::new(
            BrushPoint::from_units(40 * BRUSH_UNIT + 10, 3 * BRUSH_UNIT, -BRUSH_UNIT + 5),
            BRUSH_UNIT,
        )
        .unwrap();
        let key = RegionKey::of(vid, &brush);
        assert_eq!(key.volume, vid);
        assert_eq!(key.brick, BrickCoord::new(1, 0, -1));

        // Two nearby cuts in the same brick share a key; a far one does not.
        let near = SphereBrush::new(
            BrushPoint::from_units(45 * BRUSH_UNIT, 10 * BRUSH_UNIT, -5 * BRUSH_UNIT),
            BRUSH_UNIT,
        )
        .unwrap();
        assert_eq!(RegionKey::of(vid, &near), key);
        let far =
            SphereBrush::new(BrushPoint::from_units(200 * BRUSH_UNIT, 0, 0), BRUSH_UNIT).unwrap();
        assert_ne!(RegionKey::of(vid, &far), key);
    }

    #[test]
    fn retransmission_replays_queued_status_while_pending_and_inflight() {
        let world = SimWorld::new(crate::fixtures::flat_terrain_setup()).unwrap();
        let mut pipeline = EditPipeline::new(2, 3);
        let request = RequestId(41);
        let queued = pipeline
            .submit_intent(cut(request.0, 2, 1, 2, 1), &world)
            .unwrap();
        assert!(matches!(queued.outcome, ActionOutcome::Queued));

        // A different payload with the same id cannot replace the queued
        // operation. The queue remains exactly one entry.
        assert_eq!(
            pipeline
                .submit_intent(cut(request.0, 20, 1, 20, 1), &world)
                .unwrap(),
            queued
        );
        assert_eq!(pipeline.pending.len(), 1);

        // Put that same request in the scheduler's in-flight set without
        // executing it. This models a reliable retry while off-tick staging is
        // active; submitting it again must still be a pure status replay.
        let pending = pipeline.pending.pop_front().unwrap();
        let handle = pipeline
            .scheduler
            .submit(JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                JobToken::new(world.generation(), world.topology_epoch()),
                || -> StageResult { unreachable!("test job is never dispatched") },
            ))
            .unwrap();
        pipeline.inflight.insert(handle.id(), pending);
        assert_eq!(
            pipeline
                .submit_intent(cut(request.0, 8, 1, 8, 1), &world)
                .unwrap(),
            queued
        );
        assert!(pipeline.pending.is_empty());
        assert_eq!(pipeline.inflight.len(), 1);
    }

    #[test]
    fn submits_no_more_than_the_edit_lane_budget_per_tick() {
        let mut world = SimWorld::new(crate::fixtures::flat_terrain_setup()).unwrap();
        let mut pipeline = EditPipeline::new(8, 3);
        let budget = spall_jobs::LaneBudget::new(8, 1024 * 1024, 1);
        pipeline.scheduler =
            Scheduler::new(SchedulerConfig::default().with_lane(Lane::Edit, budget));
        for request in 1..=3 {
            pipeline
                .submit_intent(cut(request, request as i64 * 10, 1, 2, 1), &world)
                .unwrap();
        }

        let mut journal = JournalSink::new();
        let mut next_control_seq = 1;
        let report = pipeline
            .run_tick(&mut world, &mut journal, Tick(1), &mut next_control_seq)
            .unwrap();

        assert_eq!(report.committed.len(), 1);
        assert!(report.discarded_stale.is_empty());
        assert_eq!(report.pending_after, 2);
    }

    #[test]
    fn retransmission_replays_terminal_status_without_mutating_world_or_journal() {
        let mut world = SimWorld::new(crate::fixtures::bridged_terrain_setup()).unwrap();
        let mut pipeline = EditPipeline::new(2, 3);
        let mut journal = JournalSink::new();
        let mut next_control_seq = 1;
        let request = RequestId(42);

        pipeline
            .submit_intent(cut(request.0, 10, 4, 1, 2), &world)
            .unwrap();
        let report = pipeline
            .run_tick(&mut world, &mut journal, Tick(1), &mut next_control_seq)
            .unwrap();
        assert_eq!(report.committed.len(), 1);
        let committed = pipeline.action_status(request).cloned().unwrap();
        assert!(matches!(committed.outcome, ActionOutcome::Committed { .. }));
        let world_before = world.world_hash();
        let journal_before = journal.entries().to_vec();

        assert_eq!(
            pipeline
                .submit_intent(cut(request.0, 1, 1, 1, 1), &world)
                .unwrap(),
            committed
        );
        assert_eq!(world.world_hash(), world_before);
        assert_eq!(journal.entries(), journal_before.as_slice());

        // A deterministic staging failure also becomes a terminal, replayable
        // status rather than allowing a later payload to take the same id.
        let rejected_request = RequestId(43);
        let zero = EditIntent::cut(
            rejected_request,
            actor(),
            EditTarget::Terrain,
            SphereBrush::new(BrushPoint::from_cells(0, 0, 0).unwrap(), 0).unwrap(),
        );
        pipeline.submit_intent(zero, &world).unwrap();
        let report = pipeline
            .run_tick(&mut world, &mut journal, Tick(2), &mut next_control_seq)
            .unwrap();
        assert_eq!(report.rejected.len(), 1);
        let rejected = pipeline.action_status(rejected_request).cloned().unwrap();
        assert!(matches!(rejected.outcome, ActionOutcome::Rejected { .. }));
        let world_before = world.world_hash();
        let journal_before = journal.entries().to_vec();
        assert_eq!(
            pipeline
                .submit_intent(cut(rejected_request.0, 1, 1, 1, 1), &world)
                .unwrap(),
            rejected
        );
        assert_eq!(world.world_hash(), world_before);
        assert_eq!(journal.entries(), journal_before.as_slice());
    }
}

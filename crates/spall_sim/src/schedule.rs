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
use spall_jobs::{JobId, JobRequest, JobToken, Lane, Priority, Scheduler, SchedulerConfig};
use spall_protocol::{ControlSeq, RequestId};

use crate::commit::{CommitError, CommitOutcome, Committed, commit};
use crate::intent::{EditIntent, EditTarget, IntentError};
use crate::journal::JournalSink;
use crate::stage::{StageError, StageInput, StagedEdit, stage_edit};
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

#[derive(Debug, Clone)]
struct QueuedIntent {
    intent: EditIntent,
    volume_id: VolumeId,
    region: RegionKey,
    attempts: u32,
}

/// What one [`EditPipeline::run_tick`] did.
#[derive(Debug, Default)]
pub struct TickReport {
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
}

/// The bounded staging + commit pipeline.
pub struct EditPipeline {
    scheduler: Scheduler<StageResult>,
    pending: VecDeque<QueuedIntent>,
    inflight: HashMap<JobId, QueuedIntent>,
    conflicts: HashMap<RegionKey, u32>,
    serialized: HashSet<RegionKey>,
    committed: HashMap<u64, Committed>,
    /// Regions with a job in flight or staged-not-committed this tick.
    active_regions: HashSet<RegionKey>,
    max_pending: usize,
    serialize_threshold: u32,
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
            active_regions: HashSet::new(),
            max_pending,
            serialize_threshold: serialize_threshold.max(1),
        }
    }

    /// The committed [`Committed`] for `request_id`, if it has committed.
    pub fn committed(&self, request_id: RequestId) -> Option<&Committed> {
        self.committed.get(&request_id.0)
    }

    /// `true` when nothing is pending, in flight, or awaiting install.
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.inflight.is_empty() && self.scheduler.is_drained()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Whether `region` has been promoted to strictly-serial commit.
    pub fn is_serialized(&self, region: RegionKey) -> bool {
        self.serialized.contains(&region)
    }

    /// Admits an accepted intent. Rejects a duplicate request id and a full
    /// queue.
    pub fn submit_intent(
        &mut self,
        intent: EditIntent,
        world: &SimWorld,
    ) -> Result<(), IntentError> {
        let request = intent.request_id;
        if self.committed.contains_key(&request.0)
            || self.pending.iter().any(|q| q.intent.request_id == request)
            || self
                .inflight
                .values()
                .any(|q| q.intent.request_id == request)
        {
            return Err(IntentError::DuplicateRequest(request));
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
        });
        Ok(())
    }

    /// Runs one submit → stage → install → commit round.
    pub fn run_tick(
        &mut self,
        world: &mut SimWorld,
        journal: &mut JournalSink,
        server_tick: Tick,
        next_control_seq: &mut u64,
    ) -> Result<TickReport, CommitError> {
        let mut report = TickReport::default();
        self.active_regions.clear();

        // 1. Submit eligible pending intents to the staging scheduler.
        let generation = world.generation();
        let epoch = world.topology_epoch();
        let mut deferred: VecDeque<QueuedIntent> = VecDeque::new();
        while let Some(queued) = self.pending.pop_front() {
            // A serialized region admits at most one job per tick.
            if self.serialized.contains(&queued.region)
                && self.active_regions.contains(&queued.region)
            {
                deferred.push_back(queued);
                continue;
            }

            let Some(snapshot) = world.volume_ref(queued.volume_id).cloned() else {
                report
                    .rejected
                    .push((queued.intent.request_id, "target volume vanished".into()));
                continue;
            };
            let input = StageInput::new(
                &queued.intent,
                queued.volume_id,
                snapshot,
                world.anchor(),
                generation,
                epoch,
            );
            let request = JobRequest::new(
                Lane::Edit,
                Priority::NORMAL,
                JobToken::new(generation, epoch),
                move || stage_edit(&input),
            );
            match self.scheduler.submit(request) {
                Ok(handle) => {
                    self.active_regions.insert(queued.region);
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
        for entry in installed.installed {
            let Some(queued) = self.inflight.remove(&entry.id) else {
                continue;
            };
            let request = queued.intent.request_id;
            let region = queued.region;

            let staged = match entry.output {
                Ok(staged) => staged,
                Err(err) => {
                    report.rejected.push((request, err.to_string()));
                    self.conflicts.remove(&region);
                    continue;
                }
            };

            if self.committed.contains_key(&request.0) {
                continue; // idempotent: already committed
            }

            let control_seq = ControlSeq(*next_control_seq);
            match commit(world, journal, &staged, server_tick, control_seq) {
                Ok(CommitOutcome::Committed(done)) => {
                    *next_control_seq += 1;
                    self.conflicts.remove(&region);
                    self.committed.insert(request.0, done.clone());
                    report.committed.push((request, done));
                }
                Err(err) => {
                    // The commit candidate failed a fallible step (id exhaustion,
                    // DTO validation, op-budget) and was discarded before any
                    // live state changed (`ENG-54`). Reject the request
                    // deterministically; the tick continues and every other
                    // staged request still commits.
                    self.conflicts.remove(&region);
                    report.rejected.push((request, err.to_string()));
                }
                Ok(CommitOutcome::Stale(_reason)) => {
                    let count = self.conflicts.entry(region).or_insert(0);
                    *count += 1;
                    if *count >= self.serialize_threshold && self.serialized.insert(region) {
                        report.serialized_regions.push(region);
                    }
                    report.retried.push(request);
                    self.pending.push_back(QueuedIntent {
                        attempts: queued.attempts + 1,
                        ..queued
                    });
                }
            }
        }

        report.pending_after = self.pending.len();
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::SphereBrush;
    use spall_core::units::{BRUSH_UNIT, BrushPoint};

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
}

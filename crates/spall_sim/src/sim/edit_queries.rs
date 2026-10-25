//! Edit-intake, journal and status accessors.

use super::*;

impl Simulation {
    /// Stages edits on a worker thread instead of inline on the tick thread (see
    /// [`EditPipeline::enable_off_thread_staging`]). Survives [`Self::replace_world`].
    pub fn enable_off_thread_staging(&mut self) {
        self.pipeline.enable_off_thread_staging();
    }

    /// Where a committed request's time went inside the pipeline, taken once.
    pub fn take_edit_timing(
        &mut self,
        request_id: spall_protocol::RequestId,
    ) -> Option<crate::EditTiming> {
        self.pipeline.take_timing(request_id)
    }

    /// Whether edits are staged on a worker thread.
    pub fn stages_off_thread(&self) -> bool {
        self.pipeline.stages_off_thread()
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

    /// `true` when every accepted intent has committed or been rejected and
    /// no carcass is still being placed.
    pub fn is_idle(&self) -> bool {
        self.pipeline.is_idle() && self.carcasses.is_empty()
    }

    /// Conservative count of allocator IDs the upcoming edit tick may need
    /// for oversized split baselines. This intentionally excludes unrelated
    /// scheduled work such as physics prewarm jobs.
    pub fn transfer_reservation_bound(&self) -> usize {
        self.pipeline.transfer_reservation_bound()
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

    /// Records the answer to a request the host settled itself (a world item
    /// pickup) in the same admission ledger, so [`Self::action_status`]
    /// replays it for a retry instead of the retry acting again.
    pub fn record_action_status(&mut self, status: ActionStatus) {
        self.pipeline.record_status(status);
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
}

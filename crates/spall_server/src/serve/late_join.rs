use super::*;

/// How the server is currently treating one connected client.
/// One entry in a joining client's catch-up queue. A giant split (T17
/// increment 2) enqueues its marker transaction *and* the out-of-band
/// `BaselineTransfer` that carries its geometry, drained in order.
pub(super) enum QueuedItem {
    Tx(Arc<TopologyTransaction>),
    Blob(Arc<BaselineTransfer>),
    /// Ordered reliable records (creature spawns and despawns).
    Records(Arc<Vec<WireRecord>>),
}

pub(super) enum Phase {
    /// Normal replication: every committed transaction is pushed immediately.
    Live,
    /// A late-join baseline is in flight. Transactions committed after the
    /// capture tick are buffered until the client acks the transfer; a burst
    /// past the cap cancels and re-captures a fresher baseline.
    Joining {
        transfer_id: TransferId,
        queue: VecDeque<QueuedItem>,
        retries: u32,
    },
}

pub(super) struct ClientLink {
    pub(super) session: SessionId,
    pub(super) segmented: bool,
    pub(super) region_radius: Option<i64>,
    /// The client negotiated the deferred catalogue: its regional baseline omits
    /// the distant digests, which follow as chunks.
    pub(super) deferred: bool,
    pub(super) phase: Phase,
}

/// All the per-run late-join / reconnect bookkeeping the sim loop needs, kept
/// out of the loop body. Also the unit-test surface for the catch-up bound and
/// the session-generation guard.
/// A small fixed-size worker pool bounding how many background baseline
/// captures (`LateJoin::on_baseline_ack`) run concurrently. ENG-30 / T23 row
/// 11, increment 30: replaces one raw `std::thread::spawn` per join request
/// with a bounded set of long-lived workers pulling from a shared job queue
/// -- see [`default_capture_workers`] and `docs/reports/G3.md` increment 30
/// for the measurement this pool's default size is derived from. A job past
/// the worker count is queued, never rejected or dropped: this bounds
/// *concurrency*, not the number of simultaneously in-flight joins (that is
/// [`HostConfig::catch_up_cap`] / [`HostConfig::max_join_retries`]'s job).
///
/// Rayon (mentioned only as a design inspiration in this workspace's README,
/// not an actual `[workspace.dependencies]` entry -- confirmed via `cargo
/// tree -i rayon`, which finds no such package) was considered first per
/// this ticket's instructions, but adding it as a new real dependency of
/// `spall_server` for one bounded-queue primitive would cut against this
/// project's stated preference for minimal dependencies; a channel plus a
/// fixed set of threads is a handful of lines and needs no new crate.
pub(super) struct CapturePool {
    pub(super) job_tx: std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>,
}

impl CapturePool {
    /// Spawns `workers` (clamped to at least `1`) long-lived OS threads
    /// sharing one job queue.
    pub(super) fn new(workers: usize) -> Self {
        let workers = workers.max(1);
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            std::thread::spawn(move || {
                loop {
                    // Hold the lock only long enough to pull the next job --
                    // the actual capture work below runs unlocked, so workers
                    // never serialize on anything but the handoff itself.
                    let job = {
                        let rx = job_rx.lock().unwrap_or_else(|poison| poison.into_inner());
                        rx.recv()
                    };
                    match job {
                        Ok(job) => job(),
                        Err(_) => break, // every `CapturePool` (sender) is gone
                    }
                }
            });
        }
        Self { job_tx }
    }

    /// Enqueues `job` for the next free worker. Never blocks the caller (the
    /// simulation tick loop) -- the queue itself is unbounded, only
    /// concurrent *execution* is bounded by the worker count.
    pub(super) fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        // The only failure mode is every worker thread having panicked and
        // dropped its receiver handle -- the job closure here never panics on
        // any known input. If it ever did happen, the caller's
        // `pending_captures` entry simply never resolves rather than
        // panicking the tick loop.
        let _ = self.job_tx.send(Box::new(job));
    }
}

pub(super) struct LateJoin {
    /// `session.raw()` → link.
    pub(super) links: HashMap<u64, ClientLink>,
    /// slot → highest session generation seen. A record from a lower generation
    /// is from a superseded (reconnected-past) session.
    pub(super) latest_gen: HashMap<u32, u32>,
    pub(super) transfer_ids: TransferIdAllocator,
    pub(super) catch_up_cap: usize,
    pub(super) max_retries: u32,
    pub(super) completed: u64,
    pub(super) retries: u64,
    pub(super) failed: u64,
    pub(super) expired_actions: u64,
    pub(super) baseline_bytes: u64,
    /// Repair requests not answered because the client already had too many patches queued.
    pub(super) repairs_deferred: u64,
    /// T23 / G3 row 7, slice D: the residency pass's durable backing, so a
    /// late-join baseline / repair patch can fill a brick the server has
    /// evicted. `None` when residency is off.
    pub(super) backing: Option<std::sync::Arc<dyn spall_sim::BrickBacking>>,
    /// The most recent immutable topology baseline. A current join may reuse
    /// it with a fresh transfer id; transactions after its cursor remain in the
    /// ordinary per-client catch-up queue.
    pub(super) cached_baseline: Option<std::sync::Arc<BaselineTransfer>>,
    /// Bounded worker results keyed by the joining session. The simulation owns
    /// publication and only polls these at tick boundaries.
    pub(super) pending_captures: HashMap<
        u64,
        (
            TransferId,
            std::sync::mpsc::Receiver<Result<BaselineTransfer, baseline::BaselineError>>,
        ),
    >,
    /// ENG-30 / T23 row 11, increment 30: bounds concurrent background
    /// baseline captures. See [`CapturePool`].
    pub(super) capture_pool: CapturePool,
    /// Game creature service, when the world has animals.
    pub(super) creatures: Option<crate::creature_service::CreatureService>,
    /// ENG-223: the health each live session was last sent for its own player.
    pub(super) health_sent: HashMap<u64, u16>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum LateJoinResetError {
    #[error("reset transfer ID reservation failed: {0}")]
    TransferId(#[from] TransferIdError),
    #[error("connected sessions changed after reset IDs were reserved")]
    SessionsChanged,
    #[error("reset baseline capture failed: {0}")]
    Baseline(#[from] baseline::BaselineError),
}

pub(super) struct ResetGroupReservation {
    pub(super) sessions: Vec<SessionId>,
    pub(super) transfer_ids: TransferIdReservation,
}

impl LateJoin {
    pub(super) fn new(catch_up_cap: usize, max_retries: u32, capture_workers: usize) -> Self {
        Self {
            links: HashMap::new(),
            latest_gen: HashMap::new(),
            transfer_ids: TransferIdAllocator::new(),
            catch_up_cap,
            max_retries,
            completed: 0,
            retries: 0,
            failed: 0,
            expired_actions: 0,
            baseline_bytes: 0,
            repairs_deferred: 0,
            backing: None,
            cached_baseline: None,
            pending_captures: HashMap::new(),
            capture_pool: CapturePool::new(capture_workers),
            creatures: None,
            health_sent: HashMap::new(),
        }
    }

    /// Initial catch-up queue for a joining client: the creatures held right
    /// now, ahead of every spawn and transaction committed after this instant.
    pub(super) fn creature_seed(&self, sim: &Simulation) -> VecDeque<QueuedItem> {
        let mut queue = VecDeque::new();
        let Some(service) = self.creatures.as_ref() else {
            return queue;
        };
        let records: Vec<WireRecord> = service
            .join_records(sim)
            .into_iter()
            .map(WireRecord::CreatureSpawn)
            .collect();
        if !records.is_empty() {
            queue.push_back(QueuedItem::Records(Arc::new(records)));
        }
        queue
    }

    /// Announces creature spawns/despawns: live clients get them now, clients
    /// still installing a baseline get them after it, in order.
    pub(super) fn fan_out_creature_records(
        &mut self,
        records: Vec<WireRecord>,
        clients: &ClientMap,
    ) {
        let records = Arc::new(records);
        for link in self.links.values_mut() {
            match &mut link.phase {
                Phase::Live => send_to(clients, link.session, Outbound::Water(records.clone())),
                Phase::Joining { queue, .. } => {
                    queue.push_back(QueuedItem::Records(records.clone()))
                }
            }
        }
    }

    pub(super) fn backing_ref(&self) -> Option<&dyn spall_sim::BrickBacking> {
        self.backing.as_deref()
    }

    /// The terrain catalogue of `sim`'s current world, if some deferred client holds all of it, so
    /// a world reset about to replace that world can send those clients only what changed.
    pub(super) fn capture_catalogue_basis(
        &self,
        sim: &Simulation,
        clients: &ClientMap,
    ) -> Option<baseline::CatalogueBasis> {
        let guard = clients.lock().unwrap_or_else(|e| e.into_inner());
        self.links
            .iter()
            .any(|(raw, link)| {
                link.deferred
                    && guard
                        .get(raw)
                        .is_some_and(OutboundHandle::catalogue_complete)
            })
            .then(|| baseline::CatalogueBasis::capture(sim))
    }

    /// The water a session needs: for a regional client, the regions around its
    /// player, one more than the terrain radius it streams (so the edge of the
    /// view has water); everything for other clients and for sessions without
    /// a player yet.
    pub(super) fn water_interest(
        &self,
        sim: &Simulation,
        session: SessionId,
    ) -> Option<crate::column_water_replication::WaterInterest> {
        let radius = self.links.get(&session.raw())?.region_radius?;
        let player = sim.player_state(session_player_entity(session))?;
        Some(crate::column_water_replication::WaterInterest::around(
            player.position_m,
            radius as i32 + 1,
        ))
    }

    pub(super) fn snow_interest(&self, sim: &Simulation, session: SessionId) -> Option<[i32; 4]> {
        let radius = self.links.get(&session.raw())?.region_radius.unwrap_or(11);
        let player = sim.player_state(session_player_entity(session))?;
        let tile = |meters: f64| (meters / 4.0).floor() as i64;
        let x = tile(player.position_m[0]);
        let z = tile(player.position_m[2]);
        let radius_tiles = radius.saturating_mul(2).saturating_add(2);
        Some([
            i32::try_from(x.saturating_sub(radius_tiles)).ok()?,
            i32::try_from(x.saturating_add(radius_tiles)).ok()?,
            i32::try_from(z.saturating_sub(radius_tiles)).ok()?,
            i32::try_from(z.saturating_add(radius_tiles)).ok()?,
        ])
    }

    pub(super) fn snapshot_for_session(
        &self,
        sim: &Simulation,
        session: SessionId,
    ) -> baseline::BaselineSnapshot {
        let snapshot = baseline::snapshot_world(sim, self.backing_ref());
        match self.links.get(&session.raw()).and_then(|l| l.region_radius) {
            Some(radius) => snapshot.with_region(
                sim.player_state(session_player_entity(session))
                    .map_or([0.0; 3], |p| p.position_m),
                radius,
            ),
            None => snapshot,
        }
    }

    pub(super) fn on_joined(&mut self, session: SessionId) {
        self.latest_gen
            .entry(session.slot().0)
            .and_modify(|g| *g = (*g).max(session.generation()))
            .or_insert(session.generation());
        self.links.insert(
            session.raw(),
            ClientLink {
                session,
                segmented: false,
                region_radius: None,
                deferred: false,
                phase: Phase::Live,
            },
        );
    }

    pub(super) fn on_gone(&mut self, session: SessionId) {
        // Keep `latest_gen` so a straggler record from this session is still
        // rejected after the link is gone.
        self.links.remove(&session.raw());
        self.pending_captures.remove(&session.raw());
        self.health_sent.remove(&session.raw());
    }

    /// Every connected session, live or joining.
    pub(super) fn sessions(&self) -> impl Iterator<Item = SessionId> + '_ {
        self.links.values().map(|link| link.session)
    }

    /// Reserves reset IDs for a stable snapshot of every connected session.
    /// The caller must do this before replacing the simulation world.
    pub(super) fn reserve_reset_group(
        &mut self,
    ) -> Result<ResetGroupReservation, LateJoinResetError> {
        let mut sessions: Vec<SessionId> = self.sessions().collect();
        sessions.sort_by_key(|session| session.raw());
        let transfer_ids = self
            .transfer_ids
            .reserve(TransferNamespace::Reset, sessions.len())?;
        Ok(ResetGroupReservation {
            sessions,
            transfer_ids,
        })
    }

    /// Admin world reset: `sim` already holds the rebuilt world. Every
    /// connected client — live or mid-join — gets one full baseline flagged
    /// with [`spall_protocol::WORLD_RESET_TRANSFER_ID_BIT`], which its replica
    /// installs as a replacement, followed by a motion keyframe. In-flight join
    /// captures described the old world and are abandoned; their clients take
    /// the reset baseline as their join baseline. Returns the clients reset.
    pub(super) fn reset_world(
        &mut self,
        sim: &Simulation,
        clients: &ClientMap,
        motion: &mut MotionPublisher,
        catalogue_basis: Option<&baseline::CatalogueBasis>,
        reservation: ResetGroupReservation,
    ) -> Result<usize, LateJoinResetError> {
        let mut current_sessions: Vec<SessionId> = self.sessions().collect();
        current_sessions.sort_by_key(|session| session.raw());
        if current_sessions != reservation.sessions {
            return Err(LateJoinResetError::SessionsChanged);
        }
        let ResetGroupReservation {
            sessions,
            transfer_ids: mut reset_ids,
        } = reservation;
        let session_count = sessions.len();
        self.cached_baseline = None;
        self.pending_captures.clear();
        let phase_started = std::time::Instant::now();
        let snapshot = baseline::snapshot_world(sim, self.backing_ref());
        let snapshot_ms = phase_started.elapsed().as_millis() as u64;
        let mut full_transfers: HashMap<bool, baseline::BaselineTransfer> = HashMap::new();
        let mut new_world_hash = None;
        let keyframe = Arc::new(motion.full_snapshots(sim.world(), sim.current_tick()));
        for session in sessions {
            let id = reset_ids
                .next_id()
                .expect("the reset reservation covers every collected session");
            let link = &self.links[&session.raw()];
            let transfer = if let Some(radius) = link.region_radius {
                let center = sim
                    .player_state(session_player_entity(session))
                    .map_or([0.0; 3], |p| p.position_m);
                if link.deferred {
                    // A client that holds the previous world's complete catalogue needs only
                    // the bricks that differ. One still receiving it keeps what it has
                    // already verified (the segments within the chunks it has processed) and
                    // needs the rest; one that has received nothing needs everything.
                    let handle = clients
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&session.raw())
                        .cloned();
                    let complete = handle
                        .as_ref()
                        .is_some_and(OutboundHandle::catalogue_complete);
                    let partial = if complete {
                        None
                    } else {
                        handle
                            .as_ref()
                            .and_then(OutboundHandle::resumable_catalogue)
                            .map(|(layout, written)| (layout.held_after(written), written))
                    };
                    let reset = if complete {
                        catalogue_basis.map(|basis| (basis, None))
                    } else {
                        partial
                            .as_ref()
                            .map(|(held, written)| (held, Some(*written)))
                    }
                    .map(|(basis, resumed)| {
                        (
                            basis,
                            *new_world_hash
                                .get_or_insert_with(|| sim.world().geometry_topology_hash()),
                            resumed,
                        )
                    });
                    baseline::transfer_from_snapshot_deferred_resuming(
                        snapshot.clone().with_region(center, radius),
                        id,
                        InterestEpoch(1),
                        reset,
                    )?
                } else {
                    baseline::transfer_from_snapshot_supported(
                        snapshot.clone().with_region(center, radius),
                        id,
                        InterestEpoch(1),
                        true,
                    )?
                }
            } else if let Some(transfer) = full_transfers.get(&link.segmented) {
                transfer.reissue(id)
            } else {
                let transfer = baseline::transfer_from_snapshot_supported(
                    snapshot.clone(),
                    id,
                    InterestEpoch(1),
                    link.segmented,
                )?;
                full_transfers.insert(link.segmented, transfer.clone());
                transfer
            };
            self.baseline_bytes += transfer.payload_bytes() as u64;
            send_to(clients, session, Outbound::Baseline(Arc::new(transfer)));
            if !keyframe.is_empty() {
                send_to(clients, session, Outbound::Motion(Arc::clone(&keyframe)));
            }
            if let Some(link) = self.links.get_mut(&session.raw()) {
                link.phase = Phase::Live;
            }
        }
        tracing::info!(
            snapshot_ms,
            transfers_ms = phase_started.elapsed().as_millis() as u64 - snapshot_ms,
            sessions = session_count,
            "world reset baselines"
        );
        Ok(session_count)
    }

    /// Every connected client currently in normal (`Live`) replication. A client
    /// still pulling a late-join baseline is excluded — it gets a motion
    /// keyframe when its catch-up barrier is reached, not the live 20 Hz feed
    /// (`docs/protocol.md` late-join step 4).
    pub(super) fn is_live(&self, session: SessionId) -> bool {
        self.links
            .get(&session.raw())
            .is_some_and(|link| matches!(link.phase, Phase::Live))
    }

    /// The square of the world a regional client draws plants for: its streamed
    /// radius (in bricks) around its player, plus room for a tree's crown.
    /// `None` (everything) for a client that is not regional or has no player yet.
    pub(super) fn vegetation_window(
        &self,
        sim: &Simulation,
        session: SessionId,
    ) -> Option<spall_ecology::living::Window> {
        let radius = self.links.get(&session.raw())?.region_radius?;
        let player = sim.player_state(session_player_entity(session))?;
        let cells = |m: f64| (m / 0.25).floor() as i64;
        Some(spall_ecology::living::Window::around(
            cells(player.position_m[0]),
            cells(player.position_m[2]),
            (radius + 2) * 32,
        ))
    }

    pub(super) fn live_sessions(&self) -> impl Iterator<Item = SessionId> + '_ {
        self.links
            .values()
            .filter(|link| matches!(link.phase, Phase::Live))
            .map(|link| link.session)
    }

    /// `true` while any connected client is still mid-baseline: it has sent its
    /// `BaselineAck` sentinel (or been re-captured after a catch-up overflow)
    /// but has not yet sent back the confirming ack that promotes it to `Live`.
    /// A run must never be declared quiescent while this holds — an impaired
    /// transport can stretch a bulk transfer well past `quiescence_ticks` of
    /// otherwise-idle ticks, and ending the session early strands that client's
    /// connection before its transfer completes (it never gets a chance to
    /// replicate, let alone run its scripted movement/residency).
    pub(super) fn any_joining(&self) -> bool {
        self.links
            .values()
            .any(|link| matches!(link.phase, Phase::Joining { .. }))
    }

    /// Whether any regional client is still to receive something the server owes it. `None` when
    /// no regional client is connected, in which case there is nothing to wait for.
    pub(super) fn regional_clients_draining(&self, clients: &ClientMap) -> Option<bool> {
        let guard = clients.lock().unwrap_or_else(|e| e.into_inner());
        let mut any = false;
        let mut draining = false;
        for (raw, link) in &self.links {
            if link.region_radius.is_none() {
                continue;
            }
            any = true;
            if matches!(link.phase, Phase::Joining { .. })
                || guard.get(raw).is_some_and(OutboundHandle::draining)
            {
                draining = true;
            }
        }
        any.then_some(draining)
    }

    /// `true` if `session`'s generation has been superseded by a reconnect on
    /// the same slot (`docs/protocol.md`: "Reconnect uses a new session
    /// generation; old queued inputs and packets are invalid").
    pub(super) fn session_expired(&self, session: SessionId) -> bool {
        self.latest_gen
            .get(&session.slot().0)
            .is_some_and(|&g| session.generation() < g)
    }

    pub(super) fn reserve_id(
        &mut self,
        namespace: TransferNamespace,
    ) -> Result<TransferId, TransferIdError> {
        self.transfer_ids
            .reserve(namespace, 1)?
            .next_id()
            .ok_or(TransferIdError::Exhausted)
    }

    pub(super) fn reserve_ids(
        &mut self,
        namespace: TransferNamespace,
        count: usize,
    ) -> Result<Vec<TransferId>, TransferIdError> {
        let mut reservation = self.transfer_ids.reserve(namespace, count)?;
        (0..count)
            .map(|_| reservation.next_id().ok_or(TransferIdError::Exhausted))
            .collect()
    }

    pub(super) fn seed_transfer_ids_from_recovery(
        &mut self,
        sim: &Simulation,
        recovered: &[TransferId],
    ) -> Result<(), TransferIdError> {
        let current = sim.journal().entries().iter().flat_map(|entry| {
            entry.transaction.ops.iter().filter_map(|op| match op {
                spall_protocol::TopologyOp::SplitOffBulkBaseline { transfer_id, .. }
                | spall_protocol::TopologyOp::SourcePatchBulkBaseline { transfer_id, .. } => {
                    Some(TransferId(*transfer_id))
                }
                _ => None,
            })
        });
        self.transfer_ids
            .seed_past_retained(recovered.iter().copied().chain(current))
    }

    /// Handles a `BaselineAck`: the sentinel starts a late-join transfer; a real
    /// id that matches the in-flight transfer flushes the catch-up queue, sends
    /// a motion keyframe, and promotes the client to live.
    pub(super) fn on_baseline_ack(
        &mut self,
        session: SessionId,
        ack: BaselineAck,
        sim: &Simulation,
        clients: &ClientMap,
        motion: &mut MotionPublisher,
    ) {
        if self.session_expired(session) {
            return;
        }
        let want_baseline = ack.transfer_id == BASELINE_REQUEST_SENTINEL;
        if !self.links.contains_key(&session.raw()) {
            return;
        }

        if want_baseline {
            let deferred_radius =
                spall_protocol::segment::regional_deferred_radius(ack.verified_manifest_hash);
            let deferred = deferred_radius.is_some();
            let region_radius =
                spall_protocol::segment::regional_radius(ack.verified_manifest_hash)
                    .or(deferred_radius);
            let segmented = ack.verified_manifest_hash
                == spall_protocol::segment::baseline_cap_streamed()
                || region_radius.is_some();
            let id = match self.reserve_id(TransferNamespace::Baseline) {
                Ok(id) => id,
                Err(error) => {
                    tracing::error!(session = session.raw(), %error, "transfer ID allocation exhausted");
                    return;
                }
            };
            let link = self.links.get_mut(&session.raw()).unwrap();
            link.segmented = segmented;
            link.region_radius = region_radius;
            link.deferred = deferred;
            match self.cached_baseline.as_ref().filter(|b| {
                region_radius.is_none()
                    && b.begin.journal_cursor == JournalSeq(sim.journal_cursor())
                    && (b.begin.world_version
                        == spall_protocol::segment::BASELINE_STREAMED_WORLD_VERSION)
                        == segmented
            }) {
                Some(transfer) => {
                    let transfer = transfer.reissue(id);
                    self.baseline_bytes += transfer.payload_bytes() as u64;
                    let seed = self.creature_seed(sim);
                    if let Some(link) = self.links.get_mut(&session.raw()) {
                        link.phase = Phase::Joining {
                            transfer_id: id,
                            queue: seed,
                            retries: 0,
                        };
                    }
                    send_to(clients, session, Outbound::Baseline(Arc::new(transfer)));
                }
                None => {
                    let snapshot = self.snapshot_for_session(sim, session);
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    self.capture_pool.spawn(move || {
                        let _ = tx.send(if deferred {
                            baseline::transfer_from_snapshot_deferred(
                                snapshot,
                                id,
                                InterestEpoch(1),
                                None,
                            )
                        } else {
                            baseline::transfer_from_snapshot_supported(
                                snapshot,
                                id,
                                InterestEpoch(1),
                                segmented,
                            )
                        });
                    });
                    let seed = self.creature_seed(sim);
                    if let Some(link) = self.links.get_mut(&session.raw()) {
                        link.phase = Phase::Joining {
                            transfer_id: id,
                            queue: seed,
                            retries: 0,
                        };
                        self.pending_captures.insert(session.raw(), (id, rx));
                    }
                }
            }
            return;
        }

        // Confirmation of an in-flight transfer.
        let matches = self.links.get(&session.raw()).is_some_and(|link| {
            matches!(&link.phase, Phase::Joining { transfer_id, .. } if *transfer_id == ack.transfer_id)
        });
        if !matches {
            return; // stale / duplicate ack
        }
        let drained: Vec<QueuedItem> = match self.links.get_mut(&session.raw()) {
            Some(ClientLink {
                phase: Phase::Joining { queue, .. },
                ..
            }) => queue.drain(..).collect(),
            _ => Vec::new(),
        };
        for item in drained {
            match item {
                QueuedItem::Tx(tx) => send_to(clients, session, Outbound::Transaction(tx)),
                QueuedItem::Blob(t) => send_to(clients, session, Outbound::Baseline(t)),
                QueuedItem::Records(records) => send_to(clients, session, Outbound::Water(records)),
            }
        }
        // A current motion keyframe for every body (`docs/protocol.md` step 4).
        let keyframe = motion.full_snapshots(sim.world(), sim.current_tick());
        if !keyframe.is_empty() {
            send_to(clients, session, Outbound::Motion(Arc::new(keyframe)));
        }
        if let Some(link) = self.links.get_mut(&session.raw()) {
            link.phase = Phase::Live;
        }
        self.completed += 1;
    }

    /// Routes one committed transaction: live clients get it now; joining
    /// clients get it queued, and a queue past the cap triggers a bounded
    /// re-capture (or an explicit drop once the retry budget is spent).
    ///
    /// `split_transfer` is the out-of-band `BaselineWorld` for a giant split
    /// (T17 increment 2): a live client also gets it as an `Outbound::Baseline`
    /// right after the marker transaction; a joining client gets both queued.
    pub(super) fn fan_out_transaction(
        &mut self,
        tx: Arc<TopologyTransaction>,
        split_transfer: Option<Arc<BaselineTransfer>>,
        sim: &Simulation,
        clients: &ClientMap,
    ) {
        // A topology transaction invalidates the snapshot/cursor pairing. Body
        // motion alone does not: the promotion keyframe supplies current poses.
        self.cached_baseline = None;
        if let Some(t) = &split_transfer {
            self.baseline_bytes += t.payload_bytes() as u64;
        }
        let mut overflowed: Vec<u64> = Vec::new();
        for (raw, link) in self.links.iter_mut() {
            match &mut link.phase {
                Phase::Live => {
                    send_to(clients, link.session, Outbound::Transaction(tx.clone()));
                    if let Some(t) = &split_transfer {
                        send_to(clients, link.session, Outbound::Baseline(t.clone()));
                    }
                }
                Phase::Joining { queue, .. } => {
                    queue.push_back(QueuedItem::Tx(tx.clone()));
                    if let Some(t) = &split_transfer {
                        queue.push_back(QueuedItem::Blob(t.clone()));
                    }
                    if queue.len() > self.catch_up_cap {
                        overflowed.push(*raw);
                    }
                }
            }
        }
        for raw in overflowed {
            let (session, retries) = match self.links.get(&raw) {
                Some(ClientLink {
                    session,
                    phase: Phase::Joining { retries, .. },
                    ..
                }) => (*session, retries.saturating_add(1)),
                _ => continue,
            };
            if retries > self.max_retries {
                self.failed += 1;
                self.links.remove(&raw);
                send_to(
                    clients,
                    session,
                    Outbound::Shutdown(Connection::BYE_REASON_CATCH_UP_EXHAUSTED),
                );
                continue;
            }
            let id = match self.reserve_id(TransferNamespace::Baseline) {
                Ok(id) => id,
                Err(error) => {
                    tracing::error!(session = session.raw(), %error, "retry transfer ID allocation exhausted");
                    continue;
                }
            };
            if let Some(ClientLink {
                phase: Phase::Joining {
                    retries: current, ..
                },
                ..
            }) = self.links.get_mut(&raw)
            {
                *current = retries;
            }
            self.retries += 1;
            // A synchronous retry supersedes any older background capture for this session.
            self.pending_captures.remove(&raw);
            let segmented = self.links.get(&raw).is_some_and(|link| link.segmented);
            let regional = self
                .links
                .get(&raw)
                .is_some_and(|l| l.region_radius.is_some());
            let deferred = self.links.get(&raw).is_some_and(|l| l.deferred);
            let transfer = if regional && deferred {
                baseline::transfer_from_snapshot_deferred(
                    self.snapshot_for_session(sim, session),
                    id,
                    InterestEpoch(1),
                    None,
                )
                .ok()
            } else if regional {
                baseline::transfer_from_snapshot_supported(
                    self.snapshot_for_session(sim, session),
                    id,
                    InterestEpoch(1),
                    true,
                )
                .ok()
            } else {
                self.capture_for_format(sim, id, segmented)
            };
            match transfer {
                Some(transfer) => {
                    self.baseline_bytes += transfer.payload_bytes() as u64;
                    let seed = self.creature_seed(sim);
                    if let Some(link) = self.links.get_mut(&raw) {
                        link.phase = Phase::Joining {
                            transfer_id: id,
                            queue: seed,
                            retries,
                        };
                    }
                    send_to(clients, session, Outbound::Baseline(Arc::new(transfer)));
                }
                None => {
                    self.failed += 1;
                    self.links.remove(&raw);
                    send_to(
                        clients,
                        session,
                        Outbound::Shutdown(Connection::BYE_REASON_CATCH_UP_EXHAUSTED),
                    );
                }
            }
        }
    }

    /// Answers a brick `RepairRequest` with an authoritative one-brick baseline
    /// patch (full parity, revision included).
    pub(super) fn answer_repair(
        &mut self,
        session: SessionId,
        req: &RepairRequest,
        sim: &Simulation,
        clients: &ClientMap,
    ) {
        let queue_full = clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session.raw())
            .is_some_and(|handle| handle.queued_baselines() >= MAX_QUEUED_REPAIRS_PER_CLIENT);
        if queue_full {
            self.repairs_deferred += 1;
            return;
        }
        let Some(world) = baseline::logical_brick_repair_patch(sim, req, self.backing_ref()) else {
            return;
        };
        let id = match self.reserve_id(TransferNamespace::Baseline) {
            Ok(id) => id,
            Err(error) => {
                tracing::error!(session = session.raw(), %error, "repair transfer ID allocation exhausted");
                return;
            }
        };
        let cursor = JournalSeq(sim.journal_cursor());
        if let Ok(transfer) = baseline::transfer_from_world(world, id, InterestEpoch(1), cursor) {
            self.baseline_bytes += transfer.payload_bytes() as u64;
            send_to(clients, session, Outbound::Baseline(Arc::new(transfer)));
        }
    }

    /// Captures a baseline transfer at the current tick / journal cursor, over
    /// the logical brick set (evicted bricks filled from the residency
    /// backing when one is installed).
    #[cfg(test)]
    pub(super) fn capture_for(
        &mut self,
        sim: &Simulation,
        id: TransferId,
    ) -> Option<BaselineTransfer> {
        self.capture_for_format(sim, id, false)
    }

    pub(super) fn capture_for_format(
        &mut self,
        sim: &Simulation,
        id: TransferId,
        segmented: bool,
    ) -> Option<BaselineTransfer> {
        let cursor = JournalSeq(sim.journal_cursor());
        if let Some(cached) = &self.cached_baseline
            && cached.begin.journal_cursor == cursor
            && (cached.begin.world_version
                == spall_protocol::segment::BASELINE_STREAMED_WORLD_VERSION)
                == segmented
        {
            return Some(cached.reissue(id));
        }
        let transfer = baseline::transfer_from_snapshot_supported(
            baseline::snapshot_world(sim, self.backing_ref()),
            id,
            InterestEpoch(1),
            segmented,
        )
        .ok()?;
        self.cached_baseline = Some(std::sync::Arc::new(transfer.reissue(id)));
        Some(transfer)
    }

    pub(super) fn publish_ready_captures(&mut self, clients: &ClientMap) {
        let ready: Vec<(u64, Result<BaselineTransfer, baseline::BaselineError>)> = self
            .pending_captures
            .iter()
            .filter_map(|(&raw, (_, rx))| rx.try_recv().ok().map(|result| (raw, result)))
            .collect();
        for (raw, result) in ready {
            let Some((id, _)) = self.pending_captures.remove(&raw) else {
                continue;
            };
            match result {
                Ok(transfer) => {
                    self.baseline_bytes += transfer.payload_bytes() as u64;
                    if !matches!(
                        transfer.begin.world_version,
                        spall_protocol::segment::BASELINE_REGIONAL_WORLD_VERSION
                            | spall_protocol::segment::BASELINE_DEFERRED_REGIONAL_WORLD_VERSION
                    ) {
                        self.cached_baseline = Some(Arc::new(transfer.reissue(id)));
                    }
                    if let Some(link) = self.links.get(&raw) {
                        send_to(
                            clients,
                            link.session,
                            Outbound::Baseline(Arc::new(transfer)),
                        );
                    }
                }
                Err(error) => {
                    tracing::error!(%error, session = raw, "baseline capture failed");
                    self.failed += 1;
                    if let Some(link) = self.links.remove(&raw) {
                        send_to(
                            clients,
                            link.session,
                            Outbound::Shutdown(Connection::BYE_REASON_BASELINE_CAPACITY),
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simultaneous_joiners_reuse_the_same_immutable_baseline_at_one_cursor() {
        let sim = Scene::BridgeCut.simulation();
        let mut lj = LateJoin::new(
            DEFAULT_CATCH_UP_CAP,
            DEFAULT_MAX_JOIN_RETRIES,
            default_capture_workers(),
        );
        let first = lj.capture_for(&sim, TransferId(1)).unwrap();
        let second = lj.capture_for(&sim, TransferId(2)).unwrap();

        assert_eq!(first.begin.journal_cursor, second.begin.journal_cursor);
        assert_eq!(first.begin.transfer_id, TransferId(1));
        assert_eq!(second.begin.transfer_id, TransferId(2));
        assert!(std::sync::Arc::ptr_eq(
            first.world.as_ref().unwrap(),
            second.world.as_ref().unwrap()
        ));
        assert!(
            second
                .parts
                .iter()
                .all(|part| part.transfer_id == TransferId(2))
        );

        let tx = Arc::new(TopologyTransaction {
            transaction_id: spall_core::TransactionId::new(1).unwrap(),
            server_tick: spall_core::Tick(1),
            control_seq: spall_protocol::ControlSeq(0),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![],
            after: vec![],
            ops: vec![],
            result_hashes: vec![],
        });
        lj.fan_out_transaction(tx, None, &sim, &empty_clients());
        assert!(
            lj.cached_baseline.is_none(),
            "a topology commit invalidates the cache"
        );
    }

    #[test]
    fn reset_id_exhaustion_preflight_preserves_world_and_late_join_state() {
        let mut sim = Scene::BridgeCut.simulation();
        let player = EntityId::new(9001).unwrap();
        sim.add_player(player, [3.0, 4.0, 5.0]);
        let world_hash = sim.world().world_hash();
        let registry_counters = sim.world().registry().counters();
        let journal_cursor = sim.journal_cursor();
        let player_position = sim.player_state(player).unwrap().position_m;
        let mut lj = LateJoin::new(4, 2, 1);
        let session = sess(0, 1);
        lj.on_joined(session);
        if let Some(link) = lj.links.get_mut(&session.raw()) {
            link.phase = Phase::Joining {
                transfer_id: TransferId(77),
                queue: VecDeque::new(),
                retries: 1,
            };
        }
        lj.cached_baseline = Some(Arc::new(lj.capture_for(&sim, TransferId(12)).unwrap()));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        drop(sender);
        lj.pending_captures
            .insert(session.raw(), (TransferId(78), receiver));
        lj.transfer_ids.set_next_low_for_test(1_u64 << 61);

        // This is the production Admin ResetWorld preflight. Exhaustion returns
        // before the handler captures/replaces the world or mutates transfer state.
        let result = lj.reserve_reset_group();

        assert!(result.is_err());
        assert_eq!(sim.world().world_hash(), world_hash);
        assert_eq!(sim.world().registry().counters(), registry_counters);
        assert_eq!(sim.journal_cursor(), journal_cursor);
        assert_eq!(
            sim.player_state(player).unwrap().position_m,
            player_position
        );
        assert!(lj.cached_baseline.is_some());
        assert!(lj.pending_captures.contains_key(&session.raw()));
        assert!(matches!(
            lj.links.get(&session.raw()).map(|link| &link.phase),
            Some(Phase::Joining {
                transfer_id: TransferId(77),
                retries: 1,
                ..
            })
        ));
        assert_eq!(lj.transfer_ids.next_low(), 1_u64 << 61);
    }

    /// ENG-30 / T23 row 11, increment 30: a 1-worker [`CapturePool`] must run
    /// submitted jobs strictly one after another, never overlapping -- the
    /// whole point of bounding `LateJoin::on_baseline_ack`'s background
    /// captures instead of one raw `std::thread::spawn` per join request.
    #[test]
    fn capture_pool_bounds_concurrent_execution_to_its_worker_count() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let pool = CapturePool::new(1);
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        for _ in 0..4 {
            let concurrent = Arc::clone(&concurrent);
            let max_concurrent = Arc::clone(&max_concurrent);
            let done_tx = done_tx.clone();
            pool.spawn(move || {
                let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                concurrent.fetch_sub(1, Ordering::SeqCst);
                let _ = done_tx.send(());
            });
        }
        drop(done_tx);
        for _ in 0..4 {
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("every queued job eventually runs");
        }
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "a 1-worker pool must never run two jobs at once"
        );
    }

    /// The complement of the test above: a pool sized for real concurrency
    /// must actually provide it, not accidentally serialize everything
    /// (which would defeat the point of a *pool* rather than a single
    /// background thread).
    #[test]
    fn capture_pool_allows_real_concurrency_up_to_its_worker_count() {
        use std::sync::Barrier;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let pool = CapturePool::new(2);
        let barrier = Arc::new(Barrier::new(2));
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            let concurrent = Arc::clone(&concurrent);
            let max_concurrent = Arc::clone(&max_concurrent);
            let done_tx = done_tx.clone();
            pool.spawn(move || {
                let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(now, Ordering::SeqCst);
                // Both jobs must reach this point "at once" -- a serialized
                // pool would deadlock here and fail the test via timeout.
                barrier.wait();
                concurrent.fetch_sub(1, Ordering::SeqCst);
                let _ = done_tx.send(());
            });
        }
        drop(done_tx);
        for _ in 0..2 {
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("both jobs run concurrently, not serialized");
        }
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn catch_up_overflow_recaptures_then_drops_after_the_retry_budget() {
        let sim = Scene::BridgeCut.simulation();
        let clients = empty_clients();
        // cap 2, one retry allowed.
        let mut lj = LateJoin::new(2, 1, 2);
        let joiner = sess(0, 1);
        lj.on_joined(joiner);
        lj.on_baseline_ack(
            joiner,
            BaselineAck {
                transfer_id: BASELINE_REQUEST_SENTINEL,
                verified_manifest_hash: spall_protocol::Hash32::ZERO,
                installed_cursor: spall_core::JournalSeq(0),
            },
            &sim,
            &clients,
            &mut MotionPublisher::new(60, 20),
        );
        assert!(matches!(
            lj.links.get(&joiner.raw()).map(|l| &l.phase),
            Some(Phase::Joining { .. })
        ));
        assert!(matches!(
            lj.links.get(&joiner.raw()).map(|l| &l.phase),
            Some(Phase::Joining {
                transfer_id: TransferId(1),
                ..
            })
        ));

        let tx = || {
            Arc::new(TopologyTransaction {
                transaction_id: spall_core::TransactionId::new(1).unwrap(),
                server_tick: spall_core::Tick(1),
                control_seq: spall_protocol::ControlSeq(0),
                algorithm_version: 1,
                dependencies: vec![],
                before: vec![],
                after: vec![],
                ops: vec![],
                result_hashes: vec![],
            })
        };

        // Fill past the cap → first overflow → one re-capture (retry 1).
        for _ in 0..3 {
            lj.fan_out_transaction(tx(), None, &sim, &clients);
        }
        assert_eq!(lj.retries, 1);
        assert!(lj.links.contains_key(&joiner.raw()));
        assert!(matches!(
            lj.links.get(&joiner.raw()).map(|l| &l.phase),
            Some(Phase::Joining {
                transfer_id: TransferId(2),
                retries: 1,
                ..
            })
        ));

        // Fill the fresh queue past the cap again → retry 2 > budget → dropped.
        for _ in 0..3 {
            lj.fan_out_transaction(tx(), None, &sim, &clients);
        }
        assert_eq!(lj.failed, 1);
        assert!(
            !lj.links.contains_key(&joiner.raw()),
            "the joiner was dropped after exhausting its retry budget; other clients are untouched"
        );
    }

    /// T23 / G3 row 10: the joiner dropped after its retry budget is exhausted
    /// must be told with a `Bye` reason distinct from an ordinary end of
    /// session, so a client that sees its control loop end can tell the two
    /// apart. Regression for the gap `docs/reports/G3.md` recorded: this used
    /// to be the same bare `Outbound::Shutdown` sent at run end.
    #[test]
    fn catch_up_exhaustion_gives_the_dropped_joiner_a_distinguishable_bye_reason() {
        let sim = Scene::BridgeCut.simulation();
        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        let joiner = sess(0, 1);
        let handle = OutboundHandle::new();
        clients.lock().unwrap().insert(joiner.raw(), handle.clone());

        let mut lj = LateJoin::new(2, 1, 2);
        lj.on_joined(joiner);
        lj.on_baseline_ack(
            joiner,
            BaselineAck {
                transfer_id: BASELINE_REQUEST_SENTINEL,
                verified_manifest_hash: spall_protocol::Hash32::ZERO,
                installed_cursor: spall_core::JournalSeq(0),
            },
            &sim,
            &clients,
            &mut MotionPublisher::new(60, 20),
        );

        let tx = || {
            Arc::new(TopologyTransaction {
                transaction_id: spall_core::TransactionId::new(1).unwrap(),
                server_tick: spall_core::Tick(1),
                control_seq: spall_protocol::ControlSeq(0),
                algorithm_version: 1,
                dependencies: vec![],
                before: vec![],
                after: vec![],
                ops: vec![],
                result_hashes: vec![],
            })
        };
        // First overflow (re-capture, retry 1), then a second past the budget
        // (dropped).
        for _ in 0..6 {
            lj.fan_out_transaction(tx(), None, &sim, &clients);
        }
        assert_eq!(lj.failed, 1);

        let shutdowns: Vec<&'static str> = handle
            .take()
            .reliable
            .into_iter()
            .filter_map(|msg| match msg {
                Outbound::Shutdown(reason) => Some(reason),
                _ => None,
            })
            .collect();
        assert_eq!(
            shutdowns,
            vec![Connection::BYE_REASON_CATCH_UP_EXHAUSTED],
            "the dropped joiner must see the catch-up-exhaustion reason, not the ordinary end-of-session one"
        );
    }

    /// A late joiner's baseline/catch-up barrier supplies its first motion
    /// keyframe. Sending the ordinary live feed before then can saturate a
    /// shaped link with supersedable traffic and starve the reliable baseline.
    #[test]
    fn motion_is_held_until_a_joiner_is_live() {
        let clients = empty_clients();
        let live = sess(1, 1);
        let joining = sess(2, 1);
        let live_handle = OutboundHandle::new();
        let joining_handle = OutboundHandle::new();
        clients
            .lock()
            .unwrap()
            .insert(live.raw(), live_handle.clone());
        clients
            .lock()
            .unwrap()
            .insert(joining.raw(), joining_handle.clone());

        let mut lj = LateJoin::new(1, 1, 1);
        lj.on_joined(live);
        lj.on_joined(joining);
        lj.links.get_mut(&joining.raw()).unwrap().phase = Phase::Joining {
            transfer_id: TransferId(1),
            queue: VecDeque::new(),
            retries: 0,
        };
        let batch = Arc::new(Vec::new());
        send_motion_to_sessions(&clients, lj.live_sessions(), Arc::clone(&batch));

        assert!(Arc::ptr_eq(
            &live_handle
                .take()
                .motion
                .expect("live client receives motion"),
            &batch
        ));
        assert!(
            joining_handle.take().motion.is_none(),
            "joining client must wait for the post-baseline keyframe"
        );

        lj.links.get_mut(&joining.raw()).unwrap().phase = Phase::Live;
        send_motion_to_sessions(&clients, lj.live_sessions(), batch);
        assert!(
            joining_handle.take().motion.is_some(),
            "once the baseline barrier completes, normal motion resumes"
        );
    }

    #[test]
    fn a_live_client_is_never_queued() {
        let sim = Scene::BridgeCut.simulation();
        let clients = empty_clients();
        let mut lj = LateJoin::new(1, 1, 1);
        let live = sess(1, 1);
        lj.on_joined(live);
        for _ in 0..50 {
            lj.fan_out_transaction(
                Arc::new(TopologyTransaction {
                    transaction_id: spall_core::TransactionId::new(1).unwrap(),
                    server_tick: spall_core::Tick(1),
                    control_seq: spall_protocol::ControlSeq(0),
                    algorithm_version: 1,
                    dependencies: vec![],
                    before: vec![],
                    after: vec![],
                    ops: vec![],
                    result_hashes: vec![],
                }),
                None,
                &sim,
                &clients,
            );
        }
        assert_eq!(lj.retries, 0);
        assert_eq!(lj.failed, 0);
        assert!(matches!(
            lj.links.get(&live.raw()).map(|l| &l.phase),
            Some(Phase::Live)
        ));
    }
}

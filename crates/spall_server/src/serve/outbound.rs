use super::*;

/// One client's share of a vegetation keyframe.
pub(super) struct VegetationTarget {
    pub(super) session: SessionId,
    pub(super) window: Option<spall_ecology::living::Window>,
    pub(super) with_field: bool,
}

pub(super) type VegetationFrames = Result<Vec<(SessionId, Arc<Vec<WireRecord>>)>, String>;

/// Encodes a keyframe for each target: the whole world for a client without a
/// window (encoded once and shared), the window's plants for the others.
pub(super) fn vegetation_frames(
    frame: &spall_ecology::living::VisualFrame,
    tick: u64,
    targets: &[VegetationTarget],
) -> VegetationFrames {
    let records =
        |frame: &spall_ecology::living::VisualFrame| -> Result<Arc<Vec<WireRecord>>, String> {
            let chunks = spall_protocol::vegetation::chunks(tick, &frame.encode()?)
                .map_err(|e| e.to_string())?;
            Ok(Arc::new(
                chunks
                    .into_iter()
                    .map(WireRecord::VegetationSnapshot)
                    .collect(),
            ))
        };
    let mut whole: Option<Arc<Vec<WireRecord>>> = None;
    let mut out = Vec::with_capacity(targets.len());
    for target in targets {
        let shared = match target.window {
            Some(window) => records(&frame.windowed(window, target.with_field))?,
            None => match &whole {
                Some(shared) => shared.clone(),
                None => {
                    let shared = records(frame)?;
                    whole = Some(shared.clone());
                    shared
                }
            },
        };
        out.push((target.session, shared));
    }
    Ok(out)
}

/// One message from the sim bridge to a client's writer task. Repair replies and
/// baseline transfers are already routed to one client's queue, so they need no
/// session tag here.
#[derive(Debug, Clone)]
pub(super) enum Outbound {
    Transaction(Arc<TopologyTransaction>),
    Status(Arc<ActionStatus>),
    Progression(Arc<spall_protocol::ProgressionResponse>),
    Admin(Arc<spall_protocol::AdminStatus>),
    Motion(Arc<Vec<MotionSnapshot>>),
    /// Ordered water update chunks, counted against the reliable backlog.
    Water(Arc<Vec<WireRecord>>),
    /// A baseline transfer: `BaselineBegin` on control, parts on a bulk stream,
    /// `BaselineEnd` on control. Carries the whole world for a first late join,
    /// or one brick for a hash repair — the client decides replace vs. merge
    /// from whether it has installed a baseline yet.
    Baseline(Arc<BaselineTransfer>),
    /// Ends the connection with the given `Bye` reason (T23 / G3 row 10: a
    /// catch-up-exhaustion give-up carries a reason distinct from an ordinary
    /// end of session, so the client can tell the two apart on the wire).
    Shutdown(&'static str),
}

pub(super) type ClientMap = Arc<Mutex<HashMap<u64, OutboundHandle>>>;

/// A bounded per-client outbound queue (ENG-48).
///
/// * Reliable traffic — committed [`TopologyTransaction`]s, [`ActionStatus`],
///   baseline transfers, the shutdown marker — is FIFO and counted against
///   [`MAX_RELIABLE_BACKLOG`] / [`MAX_RELIABLE_BACKLOG_BYTES`]. A stalled
///   reliable reader that blows either bound is disconnected (and re-baselines
///   on reconnect); the backlog already accepted is still flushed, so committed
///   topology is never silently discarded to stay under budget.
/// * One version-3 baseline has a separate 256 MiB compressed slot, in the
///   same FIFO. The writer may own one transfer while one more waits; ordinary
///   reliable traffic retains its 8 MiB limit.
/// * Motion is lossy: only the newest unsent batch is retained, so a slow
///   reader accumulates no stale motion (`docs/protocol.md`: "Drop superseded
///   unsent motion snapshots").
#[derive(Default)]
pub(super) struct OutboundQueue {
    pub(super) reliable: VecDeque<Outbound>,
    pub(super) reliable_bytes: usize,
    pub(super) streamed_baseline_pending: bool,
    pub(super) motion: Option<Arc<Vec<MotionSnapshot>>>,
    /// Set once a reliable push blew the bound. The writer flushes what is
    /// already queued, says goodbye, and exits.
    pub(super) overflowed: bool,
}

/// The reliable backlog blew [`MAX_RELIABLE_BACKLOG`] /
/// [`MAX_RELIABLE_BACKLOG_BYTES`]; the caller must drop this client from the
/// fan-out set.
#[derive(Debug)]
pub(super) struct OutboundOverflow;

impl OutboundQueue {
    pub(super) fn push(&mut self, msg: Outbound) -> Result<(), OutboundOverflow> {
        match msg {
            // Lossy: keep only the newest unsent batch.
            Outbound::Motion(snaps) => {
                self.motion = Some(snaps);
                Ok(())
            }
            // The shutdown marker always goes through — it ends the stream.
            Outbound::Shutdown(reason) => {
                self.reliable.push_back(Outbound::Shutdown(reason));
                Ok(())
            }
            Outbound::Baseline(transfer)
                if spall_protocol::segment::is_streamed_world(transfer.begin.world_version) =>
            {
                // One bounded streaming transfer has its own slot; counting its
                // compressed bytes as control backlog would reject it before send.
                if self.overflowed
                    || self.streamed_baseline_pending
                    || self.reliable.len() >= MAX_RELIABLE_BACKLOG
                    || transfer.payload_bytes()
                        > spall_protocol::segment::MAX_STREAMED_BASELINE_COMPRESSED
                {
                    self.overflowed = true;
                    return Err(OutboundOverflow);
                }
                self.streamed_baseline_pending = true;
                self.reliable.push_back(Outbound::Baseline(transfer));
                Ok(())
            }
            reliable => {
                if self.overflowed {
                    return Err(OutboundOverflow);
                }
                let add = reliable_msg_bytes(&reliable);
                if self.reliable.len() >= MAX_RELIABLE_BACKLOG
                    || self.reliable_bytes.saturating_add(add) > MAX_RELIABLE_BACKLOG_BYTES
                {
                    // Do not enqueue and do not discard the accepted backlog:
                    // the writer still flushes it, then the connection closes
                    // and the client re-baselines.
                    let mut kinds = std::collections::BTreeMap::<&'static str, usize>::new();
                    for queued in &self.reliable {
                        *kinds.entry(outbound_kind(queued)).or_default() += 1;
                    }
                    tracing::warn!(
                        ?kinds,
                        queued_messages = self.reliable.len(),
                        queued_bytes = self.reliable_bytes,
                        incoming_bytes = add,
                        message = outbound_kind(&reliable),
                        "client reliable backlog bound exceeded"
                    );
                    self.overflowed = true;
                    return Err(OutboundOverflow);
                }
                self.reliable_bytes += add;
                self.reliable.push_back(reliable);
                Ok(())
            }
        }
    }
}

/// Short message-kind name for backlog diagnostics.
pub(super) fn outbound_kind(msg: &Outbound) -> &'static str {
    match msg {
        Outbound::Transaction(_) => "transaction",
        Outbound::Status(_) => "action-status",
        Outbound::Progression(_) => "progression",
        Outbound::Admin(_) => "admin",
        Outbound::Baseline(_) => "baseline",
        Outbound::Water(_) => "water",
        Outbound::Motion(_) => "motion",
        Outbound::Shutdown(_) => "shutdown",
    }
}

/// Rough serialized size of one reliable outbound message, for the byte cap.
pub(super) fn reliable_msg_bytes(msg: &Outbound) -> usize {
    match msg {
        Outbound::Transaction(tx) => {
            let blob_bytes: usize = tx
                .ops
                .iter()
                .map(|op| match op {
                    spall_protocol::TopologyOp::SplitOffBaseline { blob, .. }
                    | spall_protocol::TopologyOp::SourcePatchBaseline { blob, .. } => blob.len(),
                    _ => 0,
                })
                .sum();
            128 + tx.ops.len() * 48
                + blob_bytes
                + tx.before.len() * 24
                + tx.after.len() * 24
                + tx.dependencies.len() * 16
                + tx.result_hashes.len() * 40
        }
        Outbound::Status(_) => 96,
        Outbound::Progression(reply) => 64 + reply.inventory.len() * 8,
        Outbound::Admin(status) => 64 + status.message.len(),
        Outbound::Baseline(t) => 64 + t.payload_bytes(),
        Outbound::Water(records) => records
            .iter()
            .map(|r| {
                r.encode_framed()
                    .map_or(MAX_RELIABLE_BACKLOG_BYTES, |b| b.len() + 32)
            })
            .sum(),
        Outbound::Motion(_) | Outbound::Shutdown(_) => 0,
    }
}

/// What one [`OutboundHandle::take`] pass handed the writer.
pub(super) struct OutboundBatch {
    pub(super) reliable: Vec<Outbound>,
    pub(super) motion: Option<Arc<Vec<MotionSnapshot>>>,
    pub(super) overflowed: bool,
}

impl OutboundBatch {
    pub(super) fn is_empty(&self) -> bool {
        self.reliable.is_empty() && self.motion.is_none()
    }
}

/// A cloneable handle: the sim bridge enqueues through it, the client's writer
/// task drains it.
#[derive(Clone)]
pub(super) struct OutboundHandle {
    pub(super) inner: Arc<OutboundShared>,
}

pub(super) struct OutboundShared {
    pub(super) queue: Mutex<OutboundQueue>,
    pub(super) wake: Notify,
    /// Water/vegetation messages queued or being sent but not yet written.
    pub(super) water_unsent: std::sync::atomic::AtomicUsize,
    /// The writer is feeding this client its deferred terrain catalogue.
    pub(super) catalogue_streaming: std::sync::atomic::AtomicBool,
    /// Credit the client has returned for the catalogue being fed:
    /// `(transfer_id, chunks acknowledged)`.
    pub(super) catalogue_credit: Mutex<(u64, u32)>,
    /// How far the newest full baseline's catalogue has got: `(transfer_id, state)`.
    pub(super) catalogue_progress: Mutex<(u64, CatalogueState)>,
    /// The newest full baseline's catalogue layout, to resume it after a world reset:
    /// `(transfer_id, layout)`; `None` when that baseline carried no catalogue.
    pub(super) catalogue_layout: Mutex<Option<(u64, Arc<baseline::CatalogueLayout>)>>,
    /// Chunks of that catalogue the writer has written: `(transfer_id, chunks)`. The control
    /// stream is ordered, so the client processes every one of them before any record queued
    /// after them, such as a world reset's baseline.
    pub(super) catalogue_written: Mutex<(u64, u32)>,
    /// Application bytes the connection has written, as last reported by the writer.
    pub(super) written_bytes: std::sync::atomic::AtomicU64,
    /// The control-stream offset just past the last record a client needs to converge (anything
    /// but water). Once a probe echo confirms this offset, the client has everything essential
    /// the server has sent, however much water has been streamed since.
    pub(super) essential_written_bytes: std::sync::atomic::AtomicU64,
    /// Round trips on the control stream, to see how much is queued ahead of a new record.
    pub(super) probes: Mutex<ControlProbes>,
}

/// Outstanding and recently answered [`spall_protocol::ControlProbe`]s of one connection.
pub(super) struct ControlProbes {
    pub(super) next_id: u64,
    /// `(id, sent at, application bytes the connection had written when it was sent)`.
    pub(super) outstanding: VecDeque<(u64, std::time::Instant, u64)>,
    pub(super) last_delay: std::time::Duration,
    /// Application bytes written by the time of the newest echoed probe: everything up to here
    /// has reached the client, because the control stream is ordered.
    pub(super) confirmed_bytes: u64,
    /// When the newest echo arrived and the bytes it confirmed, for the delivery-rate estimate.
    pub(super) last_echo: Option<(std::time::Instant, u64)>,
    /// Smoothed bytes per second the link has delivered, from successive echoes.
    pub(super) delivery_rate: f64,
}

impl Default for ControlProbes {
    fn default() -> Self {
        Self {
            next_id: 0,
            outstanding: VecDeque::new(),
            last_delay: std::time::Duration::ZERO,
            confirmed_bytes: 0,
            last_echo: None,
            // Generous until measured, so a healthy connection is never throttled at the start.
            delivery_rate: 4.0 * 1024.0 * 1024.0,
        }
    }
}

/// Droppable traffic may run ahead of the client's last confirmed byte by this long at the
/// measured delivery rate, within [`MIN_UNCONFIRMED_BYTES`]..=[`MAX_UNCONFIRMED_BYTES`].
pub(super) const UNCONFIRMED_WINDOW: std::time::Duration = std::time::Duration::from_millis(1500);
pub(super) const MIN_UNCONFIRMED_BYTES: f64 = 48.0 * 1024.0;
pub(super) const MAX_UNCONFIRMED_BYTES: f64 = 1024.0 * 1024.0;

/// How often the writer probes the control stream's queueing delay.
pub(super) const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// The least time between two control probes. An echo wakes the writer, which then sends the next
/// probe, so on a slow link a chain of probes follows the queue closely (the congestion check
/// reads the delay of the newest answers); on a fast link the echo comes back inside this gap and
/// the chain ends, leaving the idle timer's [`PROBE_INTERVAL`]. With no gap at all every wake-up
/// sent a probe and every echo caused another: tens of thousands of records a second per client.
pub(super) const PROBE_MIN_GAP: std::time::Duration = std::time::Duration::from_millis(25);

/// Whether a control probe may be sent now: none yet, or the last one is [`PROBE_MIN_GAP`] old.
pub(super) fn probe_due(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last.is_none_or(|sent| now.saturating_duration_since(sent) >= PROBE_MIN_GAP)
}
/// Probes allowed in flight; more would only measure the same backlog again.
pub(super) const MAX_OUTSTANDING_PROBES: usize = 16;
/// Control-stream queueing delay above which droppable traffic (water, vegetation, catalogue
/// chunks) is held back. The delay of one record is the time the link needs for everything
/// already queued, so this bounds what a new baseline or reset waits behind on any link speed.
pub(super) const CONTROL_CONGESTION_DELAY: std::time::Duration =
    std::time::Duration::from_millis(1500);

/// Delivery of the catalogue that completes the newest full baseline sent to one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CatalogueState {
    /// No full baseline has been sent.
    None,
    /// A baseline with a catalogue is queued or being sent; the chunks have not started.
    Queued,
    /// The chunks are going out.
    Streaming,
    /// Every chunk was written (the control stream is ordered, so the client has them before
    /// anything written later), or the baseline needed none.
    Complete,
}

/// Most unsent water/vegetation messages one client may have outstanding before the
/// server stops producing more for it. Water is presentation-only and every keyframe
/// supersedes the last, so a client behind a slow link should see the newest frame later,
/// not an ever longer queue of stale ones that crowds out transactions and baselines.
pub(super) const MAX_UNSENT_WATER_MESSAGES: usize = 4;

impl OutboundHandle {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(OutboundShared {
                queue: Mutex::new(OutboundQueue::default()),
                wake: Notify::new(),
                water_unsent: std::sync::atomic::AtomicUsize::new(0),
                catalogue_streaming: std::sync::atomic::AtomicBool::new(false),
                catalogue_credit: Mutex::new((0, 0)),
                catalogue_progress: Mutex::new((0, CatalogueState::None)),
                catalogue_layout: Mutex::new(None),
                catalogue_written: Mutex::new((0, 0)),
                written_bytes: std::sync::atomic::AtomicU64::new(0),
                essential_written_bytes: std::sync::atomic::AtomicU64::new(0),
                probes: Mutex::new(ControlProbes::default()),
            }),
        }
    }

    /// Enqueues one message and wakes the writer. `Err(OutboundOverflow)` means
    /// the reliable backlog blew its bound and the caller must drop this client
    /// from the fan-out set (its writer is now draining-then-closing).
    pub(super) fn push(&self, msg: Outbound) -> Result<(), OutboundOverflow> {
        // A full baseline (not a one-brick repair or a split blob, which are version 1) resets
        // what the client holds, so track how much of its catalogue is still to come.
        if let Outbound::Baseline(transfer) = &msg
            && transfer.begin.world_version
                >= spall_protocol::segment::BASELINE_SEGMENTED_WORLD_VERSION
        {
            self.set_catalogue_progress(
                transfer.begin.transfer_id.0,
                if transfer.catalogue.is_some() {
                    CatalogueState::Queued
                } else {
                    CatalogueState::Complete
                },
            );
            *self
                .inner
                .catalogue_layout
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = transfer
                .catalogue
                .as_ref()
                .map(|c| (transfer.begin.transfer_id.0, Arc::clone(&c.layout)));
        }
        if matches!(msg, Outbound::Water(_)) {
            self.inner
                .water_unsent
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let res = {
            let mut q = self.inner.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.push(msg)
        };
        self.inner.wake.notify_one();
        res
    }

    /// How many baseline transfers (repair patches among them) are queued and unwritten.
    pub(super) fn queued_baselines(&self) -> usize {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reliable
            .iter()
            .filter(|msg| matches!(msg, Outbound::Baseline(_)))
            .count()
    }

    /// Takes everything queued in one pass.
    pub(super) fn take(&self) -> OutboundBatch {
        let mut q = self.inner.queue.lock().unwrap_or_else(|e| e.into_inner());
        // The writer now owns everything queued: reset the byte count with the
        // queue, or the cap would count every reliable byte ever sent on this
        // connection rather than what is waiting.
        q.reliable_bytes = 0;
        q.streamed_baseline_pending = false;
        OutboundBatch {
            reliable: q.reliable.drain(..).collect(),
            motion: q.motion.take(),
            overflowed: q.overflowed,
        }
    }

    /// Whether the server should hold back further water for this client: it
    /// already has unsent water outstanding, or its catalogue is streaming and
    /// should have the link to itself.
    pub(super) fn water_backlogged(&self) -> bool {
        self.inner
            .water_unsent
            .load(std::sync::atomic::Ordering::Relaxed)
            >= MAX_UNSENT_WATER_MESSAGES
            || self
                .inner
                .catalogue_streaming
                .load(std::sync::atomic::Ordering::Relaxed)
            || self.control_congested()
    }

    /// Whether records already written to the control stream are taking longer to arrive than
    /// [`CONTROL_CONGESTION_DELAY`]: the oldest unanswered probe has waited that long, or the
    /// last answered one did.
    pub(super) fn control_congested(&self) -> bool {
        let probes = self.inner.probes.lock().unwrap_or_else(|e| e.into_inner());
        let delay = probes
            .outstanding
            .front()
            .map_or(probes.last_delay, |(_, sent, _)| sent.elapsed());
        // Delay alone reacts only after the queue is already long when the producers outrun the
        // link several times over, so also bound the bytes written past the last confirmed
        // byte: about UNCONFIRMED_WINDOW of what the link has recently delivered.
        let window = (probes.delivery_rate * UNCONFIRMED_WINDOW.as_secs_f64())
            .clamp(MIN_UNCONFIRMED_BYTES, MAX_UNCONFIRMED_BYTES);
        let written = self
            .inner
            .written_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let unconfirmed = written.saturating_sub(probes.confirmed_bytes) as f64;
        delay > CONTROL_CONGESTION_DELAY || unconfirmed > window
    }

    /// The writer wrote a record a client needs to converge, ending at `bytes`.
    pub(super) fn note_essential(&self, bytes: u64) {
        self.inner
            .essential_written_bytes
            .fetch_max(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether this client is still to receive something the server already owes it and it needs
    /// to converge: a catalogue not fully written, an essential record still queued, or one
    /// written whose delivery no probe echo has confirmed yet. Water does not count: it is
    /// presentation-only and replaced by every newer frame.
    pub(super) fn draining(&self) -> bool {
        let catalogue_pending = matches!(
            self.inner
                .catalogue_progress
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .1,
            CatalogueState::Queued | CatalogueState::Streaming
        );
        let essential_queued = self
            .inner
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reliable
            .iter()
            .any(|msg| !matches!(msg, Outbound::Water(_)));
        let essential = self
            .inner
            .essential_written_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let confirmed = self
            .inner
            .probes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .confirmed_bytes;
        catalogue_pending || essential_queued || essential > confirmed
    }

    /// The writer reports the application bytes the connection has written so far.
    pub(super) fn note_written(&self, bytes: u64) {
        self.inner
            .written_bytes
            .fetch_max(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Reserves the next probe id. Only the newest [`MAX_OUTSTANDING_PROBES`] stay tracked: a
    /// client that cannot echo yet (it is installing a baseline and skips records it does not
    /// expect) leaves some unanswered, and a window that never slid would keep the stream
    /// "congested" forever instead of recovering on the first echo that follows.
    pub(super) fn next_probe(&self) -> u64 {
        let written = self
            .inner
            .written_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut probes = self.inner.probes.lock().unwrap_or_else(|e| e.into_inner());
        while probes.outstanding.len() >= MAX_OUTSTANDING_PROBES {
            probes.outstanding.pop_front();
        }
        let id = probes.next_id;
        probes.next_id += 1;
        probes
            .outstanding
            .push_back((id, std::time::Instant::now(), written));
        id
    }

    /// The client echoed probe `id`: its delay is the stream's current queueing delay, and any
    /// older probe it overtook is answered too (the stream is ordered).
    pub(super) fn probe_returned(&self, id: u64) {
        {
            let mut probes = self.inner.probes.lock().unwrap_or_else(|e| e.into_inner());
            while let Some((front, sent, bytes)) = probes.outstanding.front().copied() {
                if front > id {
                    break;
                }
                probes.outstanding.pop_front();
                if front == id {
                    probes.last_delay = sent.elapsed();
                    let now = std::time::Instant::now();
                    if let Some((then, then_bytes)) = probes.last_echo {
                        let dt = now.duration_since(then).as_secs_f64();
                        // Only intervals that confirmed data carry a rate: an idle link
                        // delivers nothing because nothing was sent, not because it is slow.
                        if dt > 0.05 && bytes > then_bytes {
                            let sample = (bytes - then_bytes) as f64 / dt;
                            probes.delivery_rate = 0.7 * probes.delivery_rate + 0.3 * sample;
                        }
                    }
                    probes.last_echo = Some((now, bytes));
                    probes.confirmed_bytes = probes.confirmed_bytes.max(bytes);
                }
            }
        }
        self.inner.wake.notify_one();
    }

    /// The writer finished (or abandoned) one water message.
    pub(super) fn water_sent(&self) {
        let _ = self.inner.water_unsent.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |n| Some(n.saturating_sub(1)),
        );
    }

    /// Records the client's acknowledgement of catalogue chunk `index` of `transfer_id` and wakes
    /// the writer, which may now send another chunk.
    pub(super) fn catalogue_ack(&self, transfer_id: u64, index: u32) {
        {
            let mut credit = self
                .inner
                .catalogue_credit
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if credit.0 == transfer_id {
                credit.1 = credit.1.max(index.saturating_add(1));
            }
        }
        self.inner.wake.notify_one();
    }

    pub(super) fn set_catalogue_progress(&self, transfer_id: u64, state: CatalogueState) {
        *self
            .inner
            .catalogue_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (transfer_id, state);
    }

    /// Advances the catalogue of `transfer_id` unless a newer baseline has replaced it.
    pub(super) fn advance_catalogue_progress(&self, transfer_id: u64, state: CatalogueState) {
        let mut progress = self
            .inner
            .catalogue_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if progress.0 == transfer_id {
            progress.1 = state;
        }
    }

    /// Whether the client holds the complete catalogue of the newest full baseline sent to it,
    /// so a world reset may send only what changed.
    pub(super) fn catalogue_complete(&self) -> bool {
        self.inner
            .catalogue_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            == CatalogueState::Complete
    }

    /// Records that the writer has written the first `chunks` chunks of `transfer_id`'s catalogue.
    pub(super) fn set_catalogue_written(&self, transfer_id: u64, chunks: u32) {
        *self
            .inner
            .catalogue_written
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (transfer_id, chunks);
    }

    /// The newest catalogue, if the client is partway through it: its layout and how many
    /// chunks it is certain to have processed. `None` when it is complete, has not started, or
    /// the layout belongs to a different baseline.
    pub(super) fn resumable_catalogue(&self) -> Option<(Arc<baseline::CatalogueLayout>, u32)> {
        let (transfer, state) = *self
            .inner
            .catalogue_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state != CatalogueState::Streaming {
            return None;
        }
        let layout = {
            let guard = self
                .inner
                .catalogue_layout
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some((id, layout)) if *id == transfer => Arc::clone(layout),
                _ => return None,
            }
        };
        let (written_for, written) = *self
            .inner
            .catalogue_written
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (written_for == transfer && written > 0).then_some((layout, written))
    }

    /// Starts crediting a new catalogue: nothing acknowledged yet.
    pub(super) fn begin_catalogue(&self, transfer_id: u64) {
        *self
            .inner
            .catalogue_credit
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (transfer_id, 0);
    }

    pub(super) fn catalogue_acked(&self, transfer_id: u64) -> u32 {
        let credit = self
            .inner
            .catalogue_credit
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if credit.0 == transfer_id { credit.1 } else { 0 }
    }

    pub(super) fn set_catalogue_streaming(&self, streaming: bool) {
        self.inner
            .catalogue_streaming
            .store(streaming, std::sync::atomic::Ordering::Relaxed);
    }

    /// Takes only the newest unsent motion batch, leaving reliable traffic
    /// queued, so the writer can send it between slow reliable transfers.
    pub(super) fn take_motion(&self) -> Option<Arc<Vec<MotionSnapshot>>> {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .motion
            .take()
    }

    /// Current queued reliable bytes (test-only introspection of the byte cap).
    #[cfg(test)]
    pub(super) fn reliable_bytes(&self) -> usize {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reliable_bytes
    }

    pub(super) async fn woken(&self) {
        self.inner.wake.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_baseline_has_one_bounded_fifo_slot_and_drain_releases_it() {
        let sim = Simulation::new(spall_sim::SimulationConfig::new(
            spall_sim::fixtures::bridged_terrain_setup(),
        ))
        .unwrap();
        let mut transfer = baseline::transfer_from_snapshot_supported(
            baseline::snapshot_world(&sim, None),
            TransferId(1),
            InterestEpoch(1),
            true,
        )
        .unwrap();
        let payload = vec![0u8; spall_protocol::limits::MAX_BULK_PART];
        let hash = spall_protocol::Hash32::of(&payload);
        transfer.parts = (0..9)
            .map(|i| spall_protocol::BaselinePart {
                transfer_id: TransferId(1),
                part_index: i,
                part_hash: hash,
                payload: payload.clone(),
            })
            .collect();
        transfer.begin.total_bytes = transfer.payload_bytes() as u64;
        transfer.begin.part_count = 9;
        let transfer = Arc::new(transfer);
        assert!(transfer.payload_bytes() > MAX_RELIABLE_BACKLOG_BYTES);
        let handle = OutboundHandle::new();
        handle
            .push(Outbound::Baseline(Arc::clone(&transfer)))
            .unwrap();
        assert_eq!(handle.reliable_bytes(), 0, "separate bounded transfer slot");
        let batch = handle.take();
        assert_eq!(batch.reliable.len(), 1);
        assert!(!batch.overflowed);
        handle
            .push(Outbound::Baseline(Arc::clone(&transfer)))
            .unwrap();
        assert!(
            handle.push(Outbound::Baseline(transfer)).is_err(),
            "second queued transfer must be bounded"
        );
        let batch = handle.take();
        assert_eq!(
            batch.reliable.len(),
            1,
            "accepted transfer is never discarded"
        );
        assert!(batch.overflowed);
    }

    // --- ENG-48: bounded replication host queues ----------------------------

    fn empty_tx() -> Outbound {
        Outbound::Transaction(Arc::new(TopologyTransaction {
            transaction_id: spall_core::TransactionId::new(1).unwrap(),
            server_tick: spall_core::Tick(1),
            control_seq: spall_protocol::ControlSeq(0),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![],
            after: vec![],
            ops: vec![],
            result_hashes: vec![],
        }))
    }

    #[test]
    fn the_reliable_backlog_is_bounded_and_keeps_accepted_topology() {
        let h = OutboundHandle::new();
        let mut accepted = 0usize;
        let mut overflowed = false;
        // Sustained committed topology into a reader that never drains.
        for _ in 0..(MAX_RELIABLE_BACKLOG * 8) {
            match h.push(empty_tx()) {
                Ok(()) => accepted += 1,
                Err(_) => {
                    overflowed = true;
                    break;
                }
            }
        }
        assert!(overflowed, "the reliable backlog bound is enforced");
        assert!(
            accepted <= MAX_RELIABLE_BACKLOG,
            "memory plateaus at the bound ({accepted} accepted)"
        );
        assert!(h.reliable_bytes() <= MAX_RELIABLE_BACKLOG_BYTES);
        // The accepted backlog is still handed to the writer for delivery —
        // committed topology is never silently discarded to stay under budget.
        let batch = h.take();
        assert_eq!(batch.reliable.len(), accepted);
        assert!(
            batch.overflowed,
            "the writer is told to flush-then-disconnect this client"
        );
        assert!(
            h.push(empty_tx()).is_err(),
            "further reliable traffic stays refused; the client is being dropped"
        );
    }

    /// The control stream counts as congested while records written to it take longer than
    /// the congestion delay to be echoed, and recovers on a fresh fast echo.
    #[test]
    fn a_control_probe_is_sent_at_most_once_per_minimum_gap() {
        let t0 = std::time::Instant::now();
        assert!(probe_due(None, t0), "the first probe is always due");
        assert!(
            !probe_due(Some(t0), t0 + PROBE_MIN_GAP / 2),
            "a wake-up inside the gap (a loopback echo arriving) sends no probe"
        );
        assert!(probe_due(Some(t0), t0 + PROBE_MIN_GAP));
    }

    #[test]
    fn the_control_stream_is_congested_while_probes_come_back_slowly() {
        let h = OutboundHandle::new();
        assert!(!h.control_congested(), "no probes yet");
        let slow = h.next_probe();
        assert!(!h.control_congested(), "a probe just sent has not waited");
        {
            let mut probes = h.inner.probes.lock().unwrap();
            probes.outstanding.front_mut().unwrap().1 =
                std::time::Instant::now() - 2 * CONTROL_CONGESTION_DELAY;
        }
        assert!(
            h.control_congested(),
            "an unanswered probe has waited too long"
        );
        h.probe_returned(slow);
        assert!(
            h.control_congested(),
            "the last answered probe was slow, so the queue may not have drained yet"
        );
        let fresh = h.next_probe();
        h.probe_returned(fresh);
        assert!(!h.control_congested(), "a fast echo ends the congestion");
        // A client that cannot echo yet leaves probes unanswered: the window slides, so the
        // first echo after it starts answering ends the congestion.
        for _ in 0..4 * MAX_OUTSTANDING_PROBES {
            h.next_probe();
        }
        assert_eq!(
            h.inner.probes.lock().unwrap().outstanding.len(),
            MAX_OUTSTANDING_PROBES
        );
        let latest = h.next_probe();
        h.probe_returned(latest);
        assert!(!h.control_congested());
        assert!(h.inner.probes.lock().unwrap().outstanding.is_empty());
    }

    /// Droppable traffic may run only a bounded number of bytes ahead of what the client has
    /// confirmed; the bound follows the delivery rate the echoes measure, so a slow link keeps a
    /// short queue and a fast one is not throttled.
    #[test]
    fn unconfirmed_bytes_are_bounded_by_the_measured_delivery_rate() {
        let h = OutboundHandle::new();
        assert!(!h.control_congested());
        // Far more written than any window allows, and nothing confirmed.
        h.note_written(10 * 1024 * 1024);
        assert!(h.control_congested(), "written bytes outrun confirmation");
        // An echo of a probe sent after those bytes confirms them.
        let probe = h.next_probe();
        h.probe_returned(probe);
        assert!(!h.control_congested(), "everything written is confirmed");

        // A slow link: the window shrinks to its floor.
        h.inner.probes.lock().unwrap().delivery_rate = 30.0 * 1024.0;
        let confirmed = h.inner.probes.lock().unwrap().confirmed_bytes;
        h.note_written(confirmed + 40 * 1024);
        assert!(!h.control_congested(), "inside the floor window");
        h.note_written(confirmed + 60 * 1024);
        assert!(
            h.control_congested(),
            "beyond what a slow link should queue"
        );

        // A fast link: the same backlog is nothing.
        h.inner.probes.lock().unwrap().delivery_rate = 8.0 * 1024.0 * 1024.0;
        assert!(!h.control_congested(), "a fast link may queue much more");
    }

    /// Catalogue credit follows the catalogue being fed: acknowledgements of a replaced
    /// catalogue, or arriving before one is announced, grant nothing.
    #[test]
    fn catalogue_credit_follows_the_catalogue_being_fed() {
        let h = OutboundHandle::new();
        h.catalogue_ack(5, 0);
        assert_eq!(h.catalogue_acked(5), 0, "no catalogue announced yet");
        h.begin_catalogue(5);
        assert_eq!(h.catalogue_acked(5), 0);
        h.catalogue_ack(5, 0);
        h.catalogue_ack(5, 2);
        h.catalogue_ack(5, 1);
        assert_eq!(h.catalogue_acked(5), 3, "credit never moves backwards");
        h.catalogue_ack(4, 9);
        assert_eq!(
            h.catalogue_acked(5),
            3,
            "a stale catalogue's ack is ignored"
        );
        h.begin_catalogue(6);
        assert_eq!(h.catalogue_acked(5), 0);
        assert_eq!(
            h.catalogue_acked(6),
            0,
            "a replacement starts without credit"
        );
    }

    /// A client behind a slow link gets the newest water later instead of a growing queue of
    /// stale frames, and none while its terrain catalogue owns the link.
    #[test]
    fn water_is_held_back_while_unsent_water_or_a_catalogue_is_outstanding() {
        let h = OutboundHandle::new();
        assert!(!h.water_backlogged());
        for _ in 0..MAX_UNSENT_WATER_MESSAGES {
            h.push(Outbound::Water(Arc::new(Vec::new()))).unwrap();
        }
        assert!(h.water_backlogged(), "enough unsent water is outstanding");
        h.water_sent();
        assert!(!h.water_backlogged(), "one message was written");
        h.set_catalogue_streaming(true);
        assert!(h.water_backlogged(), "the catalogue has the link");
        h.set_catalogue_streaming(false);
        assert!(!h.water_backlogged());
        // The counter never underflows if the writer reports more than was queued.
        for _ in 0..8 {
            h.water_sent();
        }
        assert!(!h.water_backlogged());
    }

    /// A writer busy with slow reliable transfers must be able to pick up the
    /// newest motion batch without taking (and reordering) the reliable queue.
    #[test]
    fn motion_can_be_taken_without_draining_the_reliable_queue() {
        let h = OutboundHandle::new();
        h.push(empty_tx()).unwrap();
        h.push(Outbound::Motion(Arc::new(Vec::new()))).unwrap();
        assert!(h.take_motion().is_some());
        assert!(h.take_motion().is_none(), "motion is taken once");
        // A newer batch replaces an unsent one.
        h.push(Outbound::Motion(Arc::new(Vec::new()))).unwrap();
        assert!(h.take_motion().is_some());
        let batch = h.take();
        assert_eq!(batch.reliable.len(), 1, "reliable traffic stays queued");
        assert!(batch.motion.is_none());
    }

    /// Regression: the byte cap used to count every reliable byte ever queued on
    /// the connection (`take` never reset it), so any long-lived client was
    /// eventually disconnected however promptly its writer drained.
    #[test]
    fn a_draining_writer_never_trips_the_byte_cap() {
        let h = OutboundHandle::new();
        let per_msg = reliable_msg_bytes(&empty_tx());
        let rounds = MAX_RELIABLE_BACKLOG_BYTES / per_msg * 3;
        for i in 0..rounds {
            assert!(
                h.push(empty_tx()).is_ok(),
                "push {i} refused by a drained queue"
            );
            assert_eq!(h.take().reliable.len(), 1);
            assert_eq!(
                h.reliable_bytes(),
                0,
                "take hands the writer the whole queue"
            );
        }
    }

    #[test]
    fn unsent_motion_is_superseded_not_accumulated() {
        let sim = bridge_after_cut();
        let mut mp = MotionPublisher::new(60, 20);
        let h = OutboundHandle::new();
        for t in 1..=5_000u64 {
            let snaps = mp.snapshots(sim.world(), spall_core::Tick(t));
            h.push(Outbound::Motion(Arc::new(snaps)))
                .expect("motion never overflows the queue");
        }
        let batch = h.take();
        assert!(batch.reliable.is_empty());
        let motion = batch.motion.expect("the newest motion batch is retained");
        assert!(!motion.is_empty());
        assert_eq!(
            motion[0].server_tick.get(),
            5_000,
            "only the newest batch survives a stalled reader"
        );
        assert!(h.take().is_empty(), "nothing accumulated behind it");
    }

    #[test]
    fn a_broadcast_drops_only_the_client_whose_backlog_overflows() {
        let clients = empty_clients();
        let keeps_up = sess(0, 1);
        let stalled = sess(1, 1);
        {
            let mut g = clients.lock().unwrap_or_else(|e| e.into_inner());
            g.insert(keeps_up.raw(), OutboundHandle::new());
            g.insert(stalled.raw(), OutboundHandle::new());
        }
        for _ in 0..(MAX_RELIABLE_BACKLOG * 3) {
            broadcast(&clients, empty_tx());
            // The healthy reader drains every round; the stalled one never does.
            if let Some(h) = clients
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&keeps_up.raw())
            {
                let _ = h.take();
            }
        }
        let g = clients.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            g.contains_key(&keeps_up.raw()),
            "a reader that keeps up stays in the fan-out set"
        );
        assert!(
            !g.contains_key(&stalled.raw()),
            "the stalled reader is removed once its reliable backlog blows the bound"
        );
    }
}

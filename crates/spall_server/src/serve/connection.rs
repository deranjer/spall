use super::*;

/// Admission control for live connections. The cap counts connections that are
/// live *now*: a slot is released when the connection's task ends, however it
/// ends. (It used to count every connection ever accepted, so a server that had
/// admitted `max_clients` sessions over its life refused every later join
/// forever, however many had since left.)
pub(super) struct AdmissionGate {
    pub(super) max: usize,
    pub(super) live: std::sync::atomic::AtomicUsize,
    pub(super) refused: std::sync::atomic::AtomicU64,
}

/// One admitted connection's slot; dropping it frees the slot.
pub(super) struct AdmissionSlot(pub(super) Arc<AdmissionGate>);

impl AdmissionGate {
    pub(super) fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            max,
            live: std::sync::atomic::AtomicUsize::new(0),
            refused: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// `Some(slot)` if fewer than `max` connections are live; otherwise counts a
    /// refusal and returns `None`.
    pub(super) fn try_admit(self: &Arc<Self>) -> Option<AdmissionSlot> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.live.load(Relaxed) >= self.max {
            self.refused.fetch_add(1, Relaxed);
            return None;
        }
        self.live.fetch_add(1, Relaxed);
        Some(AdmissionSlot(Arc::clone(self)))
    }
}

impl Drop for AdmissionSlot {
    fn drop(&mut self) {
        self.0
            .live
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// One session's input-frame arrival over a logging window: how many frames
/// arrived, how far ahead of (or behind) the server tick their `intended_tick`
/// was, and how far the newest frame's sequence ran past the applied one.
#[derive(Default)]
pub(super) struct InputArrivalWindow {
    pub(super) frames: u64,
    pub(super) min_lead_ticks: i64,
    pub(super) max_lead_ticks: i64,
    pub(super) max_seq_gap: u64,
    pub(super) newest_seq: u64,
}

impl InputArrivalWindow {
    pub(super) fn record(&mut self, seq: u64, lead_ticks: i64, acked: u64) {
        if self.frames == 0 {
            self.min_lead_ticks = lead_ticks;
            self.max_lead_ticks = lead_ticks;
        }
        self.frames += 1;
        self.min_lead_ticks = self.min_lead_ticks.min(lead_ticks);
        self.max_lead_ticks = self.max_lead_ticks.max(lead_ticks);
        self.max_seq_gap = self.max_seq_gap.max(seq.saturating_sub(acked));
        self.newest_seq = self.newest_seq.max(seq);
    }

    pub(super) fn report_and_reset(&mut self, session: u64, tick: u64) {
        if self.frames > 0 {
            tracing::info!(
                session,
                tick,
                frames = self.frames,
                min_lead_ticks = self.min_lead_ticks,
                max_lead_ticks = self.max_lead_ticks,
                max_seq_gap = self.max_seq_gap,
                newest_seq = self.newest_seq,
                "player input arrival"
            );
        }
        *self = Self::default();
    }
}

pub(super) fn parse_player_credentials(
    bytes: &[u8],
) -> Result<Vec<spall_net::PlayerCredential>, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "credential registry is not valid UTF-8".to_string())?;
    let mut credentials = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let player_text = fields.next();
        let token_text = fields.next();
        if fields.next().is_some() {
            return Err(format!(
                "credential registry line {} has extra fields",
                line_index + 1
            ));
        }
        let (Some(player_text), Some(token_text)) = (player_text, token_text) else {
            return Err(format!(
                "credential registry line {} is incomplete",
                line_index + 1
            ));
        };
        let player_id = spall_protocol::PlayerId::from_hex(player_text).ok_or_else(|| {
            format!(
                "credential registry line {} has invalid player ID",
                line_index + 1
            )
        })?;
        let token = spall_net::JoinToken::from_hex(token_text).ok_or_else(|| {
            format!(
                "credential registry line {} has invalid token",
                line_index + 1
            )
        })?;
        credentials.push(spall_net::PlayerCredential { player_id, token });
        if credentials.len() > 4096 {
            return Err("credential registry exceeds the 4096-entry limit".into());
        }
    }
    Ok(credentials)
}

/// Returns a replayable status only for an action that passed authoritative
/// validation and was admitted to the simulation. Pre-admission validation and
/// rate-limit refusals intentionally are not held here: no edit was staged, so
/// a corrected retry is a new admission attempt (see `docs/protocol.md`).
pub(super) fn replay_admitted_status(sim: &Simulation, request: RequestId) -> Option<ActionStatus> {
    sim.action_status(request).cloned()
}

/// Pushes one baseline transfer to a client: `BaselineBegin` on the control
/// stream, every part on a fresh bulk stream, then `BaselineEnd` on control.
/// Returns `false` if any leg fails (the writer loop then tears the connection
/// down).
pub(super) async fn send_baseline(conn: &Connection, transfer: &BaselineTransfer) -> bool {
    let started = std::time::Instant::now();
    // A transfer that never finishes would otherwise leave no trace at all.
    let (transfer_id, world_version) = (transfer.begin.transfer_id.0, transfer.begin.world_version);
    let watchdog = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        tracing::warn!(
            transfer = transfer_id,
            world_version,
            "baseline transfer still in progress after 10 s; the writer is blocked behind it"
        );
    });
    let result = send_baseline_phases(conn, transfer, started).await;
    watchdog.abort();
    result
}

pub(super) async fn send_baseline_phases(
    conn: &Connection,
    transfer: &BaselineTransfer,
    started: std::time::Instant,
) -> bool {
    if conn
        .send_record(WireRecord::BaselineBegin(transfer.begin.clone()))
        .await
        .is_err()
    {
        return false;
    }
    let world_version = transfer.begin.world_version;
    let begun = started.elapsed();
    let mut bulk = match conn.open_bulk().await {
        Ok(b) => b,
        Err(_) => return false,
    };
    let opened = started.elapsed();
    for part in transfer.parts.iter() {
        if bulk.send_part(part).await.is_err() {
            return false;
        }
    }
    let sent = started.elapsed();
    // A one-brick repair patch or a split blob (world version 1) is sent mid-session on a live
    // connection, which delivers it without our waiting: only the endpoint closing could lose
    // it. Waiting for the peer's acknowledgement held the writer for a delayed-ack interval per
    // patch (about 10 ms), which capped repair throughput at about 170 patches a second
    // however fast the client applied them. Everything else (baselines, resets) still waits.
    let finished_ok = if world_version == 1 {
        bulk.finish().is_ok()
    } else {
        bulk.finish_and_wait().await.is_ok()
    };
    if !finished_ok {
        return false;
    }
    let finished = started.elapsed();
    let ended = conn
        .send_record(WireRecord::BaselineEnd(transfer.end))
        .await
        .is_ok();
    // The writer sends one transfer at a time, so a slow one holds up everything behind it.
    if started.elapsed() > std::time::Duration::from_secs(5) {
        tracing::info!(
            transfer = transfer.begin.transfer_id.0,
            world_version = transfer.begin.world_version,
            bytes = transfer.payload_bytes(),
            begin_ms = begun.as_millis() as u64,
            open_bulk_ms = (opened - begun).as_millis() as u64,
            parts_ms = (sent - opened).as_millis() as u64,
            finish_ms = (finished - sent).as_millis() as u64,
            total_ms = started.elapsed().as_millis() as u64,
            "slow baseline transfer"
        );
    }
    ended
}

/// Sends one motion batch as datagrams; `false` if the connection failed.
pub(super) async fn send_motion_batch(
    conn: &Connection,
    snaps: &[MotionSnapshot],
    motion_seq: &mut u64,
) -> bool {
    for snap in snaps {
        if conn.send_datagram(*motion_seq, snap).await.is_err() {
            return false;
        }
        *motion_seq += 1;
    }
    true
}

pub(super) async fn wait_true(mut rx: watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// One client connection: a reader that forwards records to the bridge and a
/// writer that drains this client's outbound queue.
#[allow(clippy::too_many_arguments)]
pub(super) async fn serve_conn(
    conn: Arc<Connection>,
    inbound: mpsc::Sender<Inbound>,
    repair_inbound: mpsc::Sender<Inbound>,
    drops: Arc<InboundDrops>,
    handle: OutboundHandle,
    clients: ClientMap,
    conns: Arc<Mutex<HashMap<u64, Arc<Connection>>>>,
    egress_closed: Arc<Mutex<HashMap<u64, (u64, u64)>>>,
    stop: watch::Receiver<bool>,
    _slot: AdmissionSlot,
) {
    debug_assert_eq!(conn.role(), Role::Server);
    let session = conn.session();

    // Announce the join in order so the sim loop can supersede an earlier
    // session on the same slot (reconnect) and drive a late-join baseline.
    let _ = inbound
        .send(Inbound::Joined(session, conn.player_id()))
        .await;

    let liveness = tokio::spawn(conn.clone().run_liveness(stop.clone()));

    let reader = {
        let conn = conn.clone();
        let inbound = inbound.clone();
        let credit_handle = handle.clone();
        tokio::spawn(async move {
            loop {
                match conn.recv_record().await {
                    // ENG-48: never block the reader on a full bridge. A
                    // flooding client's surplus is dropped here (the records
                    // are client-retryable and rate-limited); the bounded
                    // channel keeps host memory flat. A well-behaved client
                    // stays far under the cap.
                    Ok(Some(WireRecord::ActionRequest(req))) => {
                        match inbound.try_send(Inbound::Action(session, req)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(dropped)) => {
                                drops.note(&drops.actions, "action");
                                // Tell the client, so it retries instead of waiting forever for
                                // an answer that will never come.
                                if let Inbound::Action(_, req) = dropped {
                                    let _ = credit_handle.push(Outbound::Status(Arc::new(
                                        ActionStatus {
                                            request_id: req.request_id,
                                            outcome: ActionOutcome::Rejected {
                                                reason:
                                                    "overloaded: server input queue full, retry"
                                                        .to_string(),
                                            },
                                        },
                                    )));
                                }
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    Ok(Some(WireRecord::ProgressionRequest(req))) => {
                        match inbound.try_send(Inbound::Progression(session, req)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                drops.note(&drops.progression, "progression");
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    Ok(Some(WireRecord::RepairRequest(req))) => {
                        match repair_inbound.try_send(Inbound::Repair(session, req)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                drops.note(&drops.repairs, "repair");
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    Ok(Some(WireRecord::BaselineAck(ack))) => {
                        if inbound.send(Inbound::Baseline(session, ack)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Some(WireRecord::CatalogueAck(ack))) => {
                        credit_handle.catalogue_ack(ack.transfer_id.0, ack.index);
                    }
                    Ok(Some(WireRecord::ControlProbe(probe))) if probe.echo => {
                        credit_handle.probe_returned(probe.id);
                    }
                    Ok(Some(WireRecord::AdminRequest(req))) => {
                        tracing::info!(request = req.request_id.0, "admin request received");
                        match inbound.try_send(Inbound::Admin(session, req)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                drops.note(&drops.admin, "admin");
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            let _ = inbound.send(Inbound::Gone(session)).await;
        })
    };

    // T19: player movement input arrives as datagrams. Same non-blocking policy
    // as the reliable reader — a flooding client's surplus is dropped, and the
    // sim applies at most one frame per player per tick anyway.
    let dgram_reader = {
        let conn = conn.clone();
        let inbound = inbound.clone();
        tokio::spawn(async move {
            loop {
                match conn.recv_datagram().await {
                    Ok(Some(DatagramRecord::Input(frame))) => {
                        match inbound.try_send(Inbound::Input(session, frame)) {
                            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        })
    };

    let mut motion_seq = 0u64;
    // The deferred terrain catalogue still to send after the baseline, one
    // bounded chunk at a time, only while nothing else is waiting.
    let mut catalogue_feed: Option<(Arc<[spall_protocol::CatalogueChunk]>, usize)> = None;
    let mut last_probe: Option<std::time::Instant> = None;
    'writer: loop {
        // Flush everything queued, in commit order, before parking.
        loop {
            let batch = handle.take();
            if batch.is_empty() && !batch.overflowed {
                if let Some((chunks, next)) = catalogue_feed.as_mut() {
                    // Credit-based window: never more than a few chunks beyond what the
                    // client has processed, so the transport's send buffer cannot hide
                    // minutes of catalogue ahead of the next baseline or reset.
                    let transfer = chunks[*next].transfer_id.0;
                    if (*next as u32).saturating_sub(handle.catalogue_acked(transfer))
                        >= spall_protocol::CATALOGUE_WINDOW_CHUNKS
                        || handle.control_congested()
                    {
                        break;
                    }
                    let chunk = chunks[*next].clone();
                    if conn
                        .send_record(WireRecord::CatalogueChunk(chunk))
                        .await
                        .is_err()
                    {
                        break 'writer;
                    }
                    handle.note_written(conn.stats().app_bytes_sent);
                    handle.note_essential(conn.stats().app_bytes_sent);
                    *next += 1;
                    handle.set_catalogue_written(transfer, *next as u32);
                    if *next >= chunks.len() {
                        catalogue_feed = None;
                        handle.set_catalogue_streaming(false);
                        handle.advance_catalogue_progress(transfer, CatalogueState::Complete);
                    }
                    continue;
                }
                break;
            }
            // Motion is lossy and latency-critical (a mover reconciles its
            // prediction against it), while reliable transfers can each wait a
            // round trip. Send motion first, and again after every reliable
            // message, so a long run of bulk repairs cannot delay it.
            if let Some(snaps) = batch.motion
                && !send_motion_batch(&conn, &snaps, &mut motion_seq).await
            {
                break 'writer;
            }
            for msg in batch.reliable {
                let essential = !matches!(msg, Outbound::Water(_));
                let ok = match msg {
                    Outbound::Transaction(tx) => {
                        let sent = conn
                            .send_record(WireRecord::TopologyTransaction((*tx).clone()))
                            .await;
                        if let Err(error) = &sent {
                            // Never a silent disconnect: an encode failure here
                            // (e.g. a record over the control limit) ends this
                            // client's stream, so say why.
                            tracing::error!(
                                transaction = tx.transaction_id.get(),
                                %error,
                                "topology transaction could not be sent; closing client stream"
                            );
                        }
                        sent.is_ok()
                    }
                    Outbound::Status(s) => conn
                        .send_record(WireRecord::ActionStatus((*s).clone()))
                        .await
                        .is_ok(),
                    Outbound::Progression(reply) => conn
                        .send_record(WireRecord::ProgressionResponse((*reply).clone()))
                        .await
                        .is_ok(),
                    Outbound::Admin(status) => conn
                        .send_record(WireRecord::AdminStatus((*status).clone()))
                        .await
                        .is_ok(),
                    Outbound::Baseline(transfer) => {
                        let sent = send_baseline(&conn, &transfer).await;
                        // A deferred baseline is followed by its catalogue; a later one
                        // (join retry or world reset) supersedes any catalogue still going.
                        if sent
                            && transfer.begin.world_version
                                == spall_protocol::segment::BASELINE_DEFERRED_REGIONAL_WORLD_VERSION
                        {
                            catalogue_feed = transfer
                                .catalogue
                                .as_ref()
                                .map(|catalogue| (Arc::clone(&catalogue.chunks), 0usize));
                            if let Some(catalogue) = &transfer.catalogue {
                                handle.begin_catalogue(catalogue.transfer_id.0);
                                handle.advance_catalogue_progress(
                                    catalogue.transfer_id.0,
                                    CatalogueState::Streaming,
                                );
                            }
                            handle.set_catalogue_streaming(catalogue_feed.is_some());
                        }
                        sent
                    }
                    // Motion and water are never queued as reliable; ignore
                    // defensively.
                    Outbound::Motion(_) => true,
                    Outbound::Water(records) => {
                        let mut ok = true;
                        for record in records.iter() {
                            if conn.send_record(record.clone()).await.is_err() {
                                ok = false;
                                break;
                            }
                        }
                        handle.water_sent();
                        ok
                    }
                    Outbound::Shutdown(reason) => {
                        let _ = conn.say_bye(reason).await;
                        false
                    }
                };
                handle.note_written(conn.stats().app_bytes_sent);
                if essential {
                    handle.note_essential(conn.stats().app_bytes_sent);
                }
                if !ok {
                    break 'writer;
                }
                if let Some(snaps) = handle.take_motion()
                    && !send_motion_batch(&conn, &snaps, &mut motion_seq).await
                {
                    break 'writer;
                }
            }

            if batch.overflowed {
                // ENG-48: this client's reliable backlog blew its bound. The
                // accepted backlog above has been flushed; end the connection
                // so it re-baselines on reconnect rather than the host holding
                // an unbounded queue or discarding committed topology in place.
                let _ = conn.say_bye("reliable backlog exceeded").await;
                break 'writer;
            }
        }
        tokio::select! {
            _ = handle.woken() => {}
            _ = tokio::time::sleep(PROBE_INTERVAL) => {}
            _ = wait_true(stop.clone()) => break 'writer,
        }
        // Measure how long the control stream is taking to deliver what is queued. At most one
        // probe per `PROBE_MIN_GAP`: an echo wakes this loop (`probe_returned`), so a probe sent
        // on every wake-up made each probe's echo trigger the next probe, a storm of tens of
        // thousands of records a second per idle client.
        if !probe_due(last_probe, std::time::Instant::now()) {
            continue;
        }
        last_probe = Some(std::time::Instant::now());
        handle.note_written(conn.stats().app_bytes_sent);
        let id = handle.next_probe();
        if conn
            .send_record(WireRecord::ControlProbe(spall_protocol::ControlProbe {
                id,
                echo: false,
            }))
            .await
            .is_err()
        {
            break 'writer;
        }
    }

    clients
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&session.raw());
    // T20 egress accounting: fold this connection's final byte counters into the
    // closed-connection accumulator and drop its handle, both under the `conns`
    // lock so teardown counts every connection exactly once.
    {
        let mut conns_guard = conns.lock().unwrap_or_else(|e| e.into_inner());
        if conns_guard.remove(&session.raw()).is_some() {
            let mut acc = egress_closed.lock().unwrap_or_else(|e| e.into_inner());
            acc.insert(
                session.raw(),
                (
                    conn.stats().app_bytes_sent,
                    conn.transport_stats().udp_tx.bytes,
                ),
            );
        }
    }
    conn.close("connection complete");
    reader.abort();
    dgram_reader.abort();
    liveness.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_credential_registry_parser_never_echoes_tokens() {
        let player_id = "11".repeat(16);
        let token = "a5".repeat(32);
        let content = format!("# operator registry\n{player_id} {token}\n");
        let credentials = parse_player_credentials(content.as_bytes()).unwrap();
        assert_eq!(credentials.len(), 1);
        assert_eq!(credentials[0].player_id.to_hex(), player_id);
        assert!(!format!("{:?}", credentials[0]).contains(&token));

        let bad_token = "z1".repeat(32);
        let malformed = format!("{player_id} {bad_token}");
        let error = parse_player_credentials(malformed.as_bytes()).unwrap_err();
        assert!(!error.contains(&bad_token));
    }

    /// Regression: admission counted connections ever accepted, so after
    /// `max_clients` lifetime sessions every later join was refused forever.
    #[test]
    fn admission_counts_live_connections_and_frees_slots() {
        let gate = AdmissionGate::new(2);
        let a = gate.try_admit().expect("first");
        let b = gate.try_admit().expect("second");
        assert!(
            gate.try_admit().is_none(),
            "third refused while two are live"
        );
        assert_eq!(gate.refused.load(std::sync::atomic::Ordering::Relaxed), 1);
        drop(a);
        let c = gate
            .try_admit()
            .expect("a slot freed by a departed connection");
        assert!(gate.try_admit().is_none());
        drop((b, c));
        // Far more sessions than `max` over the gate's life still admit.
        for _ in 0..50 {
            let _s = gate.try_admit().expect("no permanent lockout");
        }
    }
}

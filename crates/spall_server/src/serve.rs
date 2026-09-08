//! The T10 networked authoritative host.
//!
//! [`serve`] binds a [`spall_net`] QUIC endpoint, stands up a
//! [`spall_sim::Simulation`] on a dedicated thread, and bridges the two:
//!
//! * inbound reliable `ActionRequest` / `RepairRequest` records become
//!   [`spall_sim::EditIntent`]s / repair lookups;
//! * every committed [`spall_protocol::TopologyTransaction`] and its
//!   `ActionStatus` are pushed to every client on their reliable control
//!   stream, in server commit order;
//! * 20 Hz [`spall_protocol::MotionSnapshot`] batches go out as datagrams.
//!
//! The run is bounded: it stops at `max_ticks`, or early once the edit pipeline
//! has been idle for `quiescence_ticks` consecutive ticks after at least one
//! commit, then tells every client goodbye and tears the endpoint down.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use spall_core::{
    EntityId, JournalSeq, JsonlError, JsonlLog, ProcessEvent, ProcessRecord, ProcessRole,
};
use spall_net::{
    Connection, DevIdentity, JoinToken, NetServer, Role, TransportConfig, TransportError,
    WireRecord,
};
use spall_physics::PhysicsConfig;
use spall_protocol::{
    ActionKind, ActionOutcome, ActionRequest, ActionStatus, AlgorithmVersions, BaselineAck,
    ClaimedTarget, Handshake, Hash32, InterestEpoch, MotionSnapshot, NegotiatedLimits,
    PROTOCOL_VERSION, RepairRequest, RequestId, SessionId, SlotId, TopologyTransaction, TransferId,
};
use spall_sim::{
    EditIntent, EditKind, EditTarget, MotionPublisher, Simulation, SimulationConfig,
    action_statuses, committed_transactions, fixtures,
};
use spall_store::Writer;
use spall_structure::AnchorPlane;
use tokio::sync::{mpsc, watch};

use crate::baseline::{self, BaselineTransfer};
use crate::persist::{self, PersistConfig};
use crate::persist_pipeline::{PersistPipeline, PipelineConfig};

/// A joining client's sentinel `BaselineAck.transfer_id`: "I am a late-join
/// replica, send me a baseline". A real transfer id is always `>= 1`, so `0`
/// can never collide with an install confirmation.
pub const BASELINE_REQUEST_SENTINEL: TransferId = TransferId(0);

/// Default cap on a joining client's catch-up queue. A burst past this while the
/// baseline is still installing cancels the transfer and re-captures a fresher
/// one (`docs/protocol.md`: "If the client cannot catch up within memory/time
/// limits, cancel the transfer and create a fresher snapshot").
pub const DEFAULT_CATCH_UP_CAP: usize = 512;

/// Default bounded retry count for late-join transfer restarts.
pub const DEFAULT_MAX_JOIN_RETRIES: u32 = 3;

/// Content-manifest tag both ends of a T10 session agree on out of band. Real
/// manifest negotiation is T16/T17; this keeps the handshake honest meanwhile.
pub const T10_CONTENT_TAG: &[u8] = b"spall-t10-bridge-v1";
/// World id used by the built-in T10 scene.
pub const T10_WORLD_ID: u128 = 0x5A11_0000_0000_7010;

/// Which built-in scene to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    /// Anchored floor + single column + raised beam; cutting the column
    /// detaches the beam. See [`spall_voxel::fixtures::bridge_scene`].
    BridgeCut,
}

impl Scene {
    fn simulation(self) -> Simulation {
        let setup = match self {
            Scene::BridgeCut => spall_sim::fixtures::bridged_terrain_setup(),
        };
        Simulation::new(SimulationConfig::new(setup)).expect("built-in scene is valid")
    }
}

/// Inputs to [`serve`].
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub listen: SocketAddr,
    pub scene: Scene,
    pub join_token: JoinToken,
    /// Hard cap on server ticks for this run.
    pub max_ticks: u64,
    /// Stop early after the pipeline is idle this many consecutive ticks
    /// (once at least one transaction has committed). `0` disables early stop.
    pub quiescence_ticks: u64,
    /// Wait for this many clients before the tick loop starts.
    pub min_clients: usize,
    /// Refuse connections past this many.
    pub max_clients: usize,
    /// Fail if `min_clients` have not connected within this long.
    pub startup_timeout: Duration,
    /// Real-time pacing at 60 Hz. `false` runs ticks back to back (headless).
    pub paced: bool,
    pub log_json: PathBuf,
    pub summary_json: Option<PathBuf>,
    /// Write the server certificate fingerprint (hex) here for clients.
    pub fingerprint_out: Option<PathBuf>,
    /// Write the OS-resolved bound `ip:port` here once listening.
    pub addr_out: Option<PathBuf>,
    pub transport: TransportConfig,
    /// On-disk world database (T16). When set, the run recovers from it on
    /// start (if it exists), journals every committed transaction, and
    /// checkpoints every `checkpoint_interval_ticks` and on clean shutdown.
    pub save: Option<PathBuf>,
    /// Ticks between engine checkpoints (`1800` == 30 s at 60 Hz). `0` disables
    /// periodic checkpoints (a shutdown checkpoint still happens).
    pub checkpoint_interval_ticks: u64,
    /// World seed stamped into the save metadata.
    pub seed: u64,
    /// T17: cap on a joining client's catch-up queue before the transfer is
    /// cancelled and re-captured fresher.
    pub catch_up_cap: usize,
    /// T17: bounded late-join transfer restarts before the client is dropped
    /// with an explicit failure (connected clients keep running).
    pub max_join_retries: u32,
}

impl ServeConfig {
    /// A headless bounded config for `scene` on `listen` with `token`.
    pub fn headless(listen: SocketAddr, scene: Scene, token: JoinToken) -> Self {
        Self {
            listen,
            scene,
            join_token: token,
            max_ticks: 1_200,
            quiescence_ticks: 45,
            min_clients: 1,
            max_clients: 8,
            startup_timeout: Duration::from_secs(15),
            paced: false,
            log_json: PathBuf::from(".local/runs/server.jsonl"),
            summary_json: None,
            fingerprint_out: None,
            addr_out: None,
            transport: TransportConfig::for_tests(),
            save: None,
            checkpoint_interval_ticks: 1_800,
            seed: 0,
            catch_up_cap: DEFAULT_CATCH_UP_CAP,
            max_join_retries: DEFAULT_MAX_JOIN_RETRIES,
        }
    }
}

/// Machine-readable result of a [`serve`] run.
#[derive(Debug, Clone, Serialize)]
pub struct ServeSummary {
    pub version: u32,
    pub result: String,
    pub scene: String,
    pub bound_addr: String,
    pub ticks_run: u64,
    pub clients_connected: usize,
    pub transactions_committed: u64,
    pub actions_rejected: u64,
    pub final_world_hash: String,
    pub total_solid_cells: u64,
    pub body_count: usize,
    /// Engine checkpoints published this run (T16). `0` when `--save` is unset.
    pub checkpoints_published: u64,
    /// Topology journal records written this run.
    pub journal_records_written: u64,
    /// Measured mean journal payload bytes per durable journal commit.
    pub persist_bytes_per_write: f64,
    /// Measured durable payload bytes per second of `COMMIT` time.
    pub persist_commit_bytes_per_sec: f64,
    /// T17: late-join baselines that reached `BaselineAck` and went live.
    pub late_joins_completed: u64,
    /// T17: late-join transfers cancelled + re-captured on catch-up overflow.
    pub late_join_retries: u64,
    /// T17: joining clients dropped after exhausting the retry budget.
    pub late_joins_failed: u64,
    /// T17: `ActionRequest`s rejected because their session generation was
    /// superseded by a reconnect.
    pub expired_actions_rejected: u64,
    /// T17: total baseline bulk payload bytes pushed this run.
    pub baseline_bytes_sent: u64,
}

/// Anything that stops a [`serve`] run.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Log(#[from] JsonlError),
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("no client connected within {0:?}")]
    StartupTimeout(Duration),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("tokio runtime: {0}")]
    Runtime(String),
}

/// Runs the networked host to completion and returns its summary. Builds its own
/// current-thread-free multi-thread Tokio runtime.
pub fn serve(config: ServeConfig) -> Result<ServeSummary, ServeError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ServeError::Runtime(e.to_string()))?;
    runtime.block_on(serve_async(config))
}

fn server_handshake() -> Handshake {
    Handshake {
        protocol_version: PROTOCOL_VERSION,
        content_manifest_hash: Hash32::of(T10_CONTENT_TAG),
        world_id: spall_core::WorldId::from_u128(T10_WORLD_ID),
        generator_version: 1,
        algorithms: AlgorithmVersions {
            integer_brush: 1,
            structure_graph: 1,
            topology_hash: 1,
        },
        server_tick_hz: 60,
        motion_snapshot_hz: 20,
        cell_size_codes: vec![spall_core::CellSizeCode::Quarter],
        session: SessionId::from_parts(SlotId(0), 1),
        limits: NegotiatedLimits::DEFAULT,
    }
}

/// One reliable/repair message from a client to the sim bridge.
#[derive(Debug)]
enum Inbound {
    /// A connection was accepted; the sim loop learns its session (and, from the
    /// generation, whether it supersedes an earlier one on the same slot).
    Joined(SessionId),
    Action(SessionId, ActionRequest),
    Repair(SessionId, RepairRequest),
    /// A `BaselineAck`: the sentinel (`transfer_id == 0`) asks for a late-join
    /// baseline; a real id confirms one is installed.
    Baseline(SessionId, BaselineAck),
    Gone(SessionId),
}

/// One message from the sim bridge to a client's writer task. Repair replies and
/// baseline transfers are already routed to one client's queue, so they need no
/// session tag here.
#[derive(Debug, Clone)]
enum Outbound {
    Transaction(Arc<TopologyTransaction>),
    Status(Arc<ActionStatus>),
    Motion(Arc<Vec<MotionSnapshot>>),
    /// A baseline transfer: `BaselineBegin` on control, parts on a bulk stream,
    /// `BaselineEnd` on control. Carries the whole world for a first late join,
    /// or one brick for a hash repair — the client decides replace vs. merge
    /// from whether it has installed a baseline yet.
    Baseline(Arc<BaselineTransfer>),
    Shutdown,
}

type ClientMap = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Outbound>>>>;

async fn serve_async(config: ServeConfig) -> Result<ServeSummary, ServeError> {
    let mut log = JsonlLog::create(&config.log_json)?;
    log.write(&ProcessRecord::new(
        ProcessEvent::Started,
        ProcessRole::Server,
        Some(format!(
            "scene={:?}, listen={}, max_ticks={}",
            config.scene, config.listen, config.max_ticks
        )),
    ))?;

    let identity = DevIdentity::generate()?;
    if let Some(path) = &config.fingerprint_out {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, identity.fingerprint().to_hex())?;
        std::fs::rename(&tmp, path)?;
    }

    let server = Arc::new(
        NetServer::bind(
            config.listen,
            &identity,
            config.join_token,
            server_handshake(),
            config.transport,
        )
        .await?,
    );
    let bound = server.local_addr()?;
    if let Some(path) = &config.addr_out {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Write atomically-ish: temp then rename, so a reader never sees a
        // half-written address.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bound.to_string())?;
        std::fs::rename(&tmp, path)?;
    }
    log.write(&ProcessRecord::new(
        ProcessEvent::Ready,
        ProcessRole::Server,
        Some(format!("bound={bound}")),
    ))?;

    let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
    let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<Inbound>();
    let (count_tx, mut count_rx) = watch::channel(0usize);
    let (stop_tx, stop_rx) = watch::channel(false);

    // Accept loop.
    let accept = {
        let server = server.clone();
        let clients = clients.clone();
        let inbound_tx = inbound_tx.clone();
        let stop_rx = stop_rx.clone();
        let max_clients = config.max_clients;
        tokio::spawn(async move {
            let mut connected = 0usize;
            loop {
                if *stop_rx.borrow() {
                    break;
                }
                let accepted = tokio::select! {
                    r = server.accept() => r,
                    _ = wait_true(stop_rx.clone()) => break,
                };
                let conn = match accepted {
                    Ok(c) => Arc::new(c),
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        continue;
                    }
                };
                if connected >= max_clients {
                    conn.close("server at capacity");
                    continue;
                }
                connected += 1;
                let _ = count_tx.send(connected);
                let (out_tx, out_rx) = mpsc::unbounded_channel::<Outbound>();
                clients
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(conn.session().raw(), out_tx);
                tokio::spawn(serve_conn(
                    conn,
                    inbound_tx.clone(),
                    out_rx,
                    clients.clone(),
                    stop_rx.clone(),
                ));
            }
        })
    };

    // Wait for the first `min_clients` (or time out).
    if config.min_clients > 0 {
        let wait = async {
            loop {
                if *count_rx.borrow() >= config.min_clients {
                    return;
                }
                if count_rx.changed().await.is_err() {
                    return;
                }
            }
        };
        if tokio::time::timeout(config.startup_timeout, wait)
            .await
            .is_err()
        {
            let _ = stop_tx.send(true);
            server.close();
            return Err(ServeError::StartupTimeout(config.startup_timeout));
        }
    }
    log.write(&ProcessRecord::new(
        ProcessEvent::Ready,
        ProcessRole::Server,
        Some(format!(
            "clients={} — tick loop starting",
            *count_rx.borrow()
        )),
    ))?;

    // The sim runs on a blocking thread; it owns `inbound_rx` and fans out
    // through the per-client senders.
    let paced = config.paced;
    let max_ticks = config.max_ticks;
    let quiescence = config.quiescence_ticks;
    let scene = config.scene;
    let clients_for_sim = clients.clone();
    let save = config.save.clone();
    let checkpoint_interval = config.checkpoint_interval_ticks;
    let catch_up_cap = config.catch_up_cap.max(1);
    let max_join_retries = config.max_join_retries;
    let persist_cfg = PersistConfig {
        world_id: T10_WORLD_ID,
        seed: config.seed,
        generator_version: 1,
    };

    let sim_join = tokio::task::spawn_blocking(move || -> SimResult {
        // Open the world database (T16). If it already holds a checkpoint,
        // recover from it; otherwise start the built-in scene and publish an
        // initial checkpoint so recovery always has a floor.
        let Persistence {
            mut sim,
            mut pipeline,
            mut journalled_through,
            mut checkpoints_published,
        } = match setup_persistence(save.as_deref(), scene, &persist_cfg) {
            Ok(parts) => parts,
            Err(e) => {
                return SimResult::error(format!("persistence setup failed: {e}"), 0);
            }
        };
        let mut journal_records_written: u64 = 0;

        let mut motion = MotionPublisher::new(60, 20);
        let mut committed_total = 0u64;
        let mut rejected_total = 0u64;
        let mut idle_streak = 0u64;
        let mut ticks_run = 0u64;
        let tick_dt = Duration::from_nanos(1_000_000_000 / 60);

        // T17 late-join / reconnect state.
        let mut lj = LateJoin::new(catch_up_cap, max_join_retries);

        for _ in 0..max_ticks {
            let started = std::time::Instant::now();

            // Drain everything the clients have sent since the last tick.
            let mut repairs: Vec<(SessionId, RepairRequest)> = Vec::new();
            let mut saw_client_work = false;
            while let Ok(msg) = inbound_rx.try_recv() {
                match msg {
                    Inbound::Joined(session) => lj.on_joined(session),
                    Inbound::Action(session, req) => {
                        saw_client_work = true;
                        if lj.session_expired(session) {
                            reject(&clients_for_sim, session, req.request_id, "expired session");
                            rejected_total += 1;
                            lj.expired_actions += 1;
                            continue;
                        }
                        match intent_from_request(session, &req) {
                            Some(intent) => {
                                if let Err(e) = sim.submit(intent) {
                                    reject(
                                        &clients_for_sim,
                                        session,
                                        req.request_id,
                                        &e.to_string(),
                                    );
                                    rejected_total += 1;
                                }
                            }
                            None => {
                                reject(
                                    &clients_for_sim,
                                    session,
                                    req.request_id,
                                    "unsupported target",
                                );
                                rejected_total += 1;
                            }
                        }
                    }
                    Inbound::Repair(session, req) => {
                        saw_client_work = true;
                        if !lj.session_expired(session) {
                            repairs.push((session, req));
                        }
                    }
                    Inbound::Baseline(session, ack) => {
                        saw_client_work = true;
                        lj.on_baseline_ack(session, ack, &sim, &clients_for_sim, &mut motion);
                    }
                    Inbound::Gone(session) => lj.on_gone(session),
                }
            }

            let report = match sim.tick() {
                Ok(r) => r,
                Err(e) => return SimResult::error(format!("tick failed: {e}"), ticks_run),
            };
            ticks_run += 1;
            let tick = sim.current_tick();

            for tx in committed_transactions(&report).map(|(_, t)| t.clone()) {
                committed_total += 1;
                lj.fan_out_transaction(Arc::new(tx), &sim, &clients_for_sim);
            }
            for status in action_statuses(&report) {
                if matches!(status.outcome, ActionOutcome::Rejected { .. }) {
                    rejected_total += 1;
                }
                broadcast(&clients_for_sim, Outbound::Status(Arc::new(status)));
            }
            // The 20 Hz motion batch: broadcast it to replicas *and* keep it for
            // the durable pose journal below.
            let pose_batch: Option<Vec<MotionSnapshot>> = if motion.due(tick) {
                let snaps = motion.snapshots(sim.world(), tick);
                if !snaps.is_empty() {
                    broadcast(&clients_for_sim, Outbound::Motion(Arc::new(snaps.clone())));
                }
                Some(snaps)
            } else {
                None
            };
            for (session, req) in repairs {
                lj.answer_repair(session, &req, &sim, &clients_for_sim);
            }

            // ENG-50: queue immutable records to the bounded off-thread writer.
            // The sim thread never blocks on the disk; when the backlog fills or
            // a durable write has failed, the run stops rather than silently
            // continuing an unsavable world (`docs/protocol.md` Persistence).
            if let Some(pipe) = pipeline.as_ref() {
                let batch = match tick_journal_batch(
                    &mut sim,
                    &mut journalled_through,
                    pose_batch.as_deref(),
                    tick.get(),
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        return SimResult::error(format!("journal encode failed: {e}"), ticks_run);
                    }
                };
                if let Err(e) = pipe.submit_journal(batch) {
                    return SimResult::error(format!("persistence: {e}"), ticks_run);
                }

                // A checkpoint's journal cursor is `journalled_through`: the FIFO
                // writer commits every journal record up to that cursor *before*
                // this checkpoint, so a durable checkpoint always has a durable
                // journal prefix behind it. Retain right after, so disk use
                // stays bounded during the run — not only at shutdown.
                if checkpoint_interval > 0 && tick.get().is_multiple_of(checkpoint_interval) {
                    match persist::capture(&sim, &persist_cfg, journalled_through) {
                        Ok(cp) => {
                            if let Err(e) = pipe
                                .submit_checkpoint(cp)
                                .and_then(|()| pipe.submit_retain(RETAIN_CHECKPOINTS))
                            {
                                return SimResult::error(format!("persistence: {e}"), ticks_run);
                            }
                        }
                        Err(e) => {
                            return SimResult::error(
                                format!("checkpoint capture failed: {e}"),
                                ticks_run,
                            );
                        }
                    }
                }

                // Drop the in-memory journal suffix the store has acknowledged.
                sim.prune_journal(pipe.durable_seq());

                if let Some(err) = pipe.error() {
                    return SimResult::error(format!("persistence failed: {err}"), ticks_run);
                }
            }

            // Quiesce only after the pipeline is drained *and* no client has
            // sent anything for `quiescence` ticks — a late scripted action from
            // one client keeps the run alive for the others.
            if sim.is_idle() && !saw_client_work && report.committed.is_empty() {
                idle_streak += 1;
            } else {
                idle_streak = 0;
            }
            if quiescence > 0 && committed_total > 0 && idle_streak >= quiescence {
                break;
            }

            if paced && let Some(rem) = tick_dt.checked_sub(started.elapsed()) {
                std::thread::sleep(rem);
            }
        }

        broadcast(&clients_for_sim, Outbound::Shutdown);

        // Clean shutdown: queue the final journal tail + checkpoint, then block
        // until the writer thread has drained and exited (`docs/protocol.md`:
        // "Clean shutdown waits for a final checkpoint/flush").
        let mut persist_bytes_per_write = 0.0;
        let mut persist_commit_bytes_per_sec = 0.0;
        let mut persist_error: Option<String> = None;
        if let Some(pipe) = pipeline.take() {
            let final_tick = sim.current_tick().get();
            let tail = tick_journal_batch(&mut sim, &mut journalled_through, None, final_tick)
                .unwrap_or_default();
            let _ = pipe.submit_journal(tail);
            if let Ok(cp) = persist::capture(&sim, &persist_cfg, journalled_through) {
                let _ = pipe
                    .submit_checkpoint(cp)
                    .and_then(|()| pipe.submit_retain(RETAIN_CHECKPOINTS));
            }
            let outcome = pipe.shutdown();
            journal_records_written = outcome.status.journal_records;
            checkpoints_published += outcome.status.checkpoints_published;
            persist_bytes_per_write = outcome.metrics.bytes_per_journal_write();
            persist_commit_bytes_per_sec = outcome.metrics.commit_bytes_per_sec();
            persist_error = outcome.status.error;
            tracing::info!(
                durable_seq = outcome.status.durable_seq,
                journal_records = outcome.status.journal_records,
                checkpoints = outcome.status.checkpoints_published,
                max_queue_depth = outcome.status.max_queue_depth,
                last_flush_us = outcome.status.last_journal_flush.as_micros() as u64,
                max_flush_us = outcome.status.max_journal_flush.as_micros() as u64,
                max_checkpoint_us = outcome.status.max_checkpoint.as_micros() as u64,
                "persistence pipeline drained"
            );
        }
        if let Some(err) = persist_error {
            return SimResult::error(format!("persistence failed: {err}"), ticks_run);
        }

        SimResult {
            ok: true,
            error: None,
            ticks_run,
            committed_total,
            rejected_total,
            final_world_hash: sim.world().world_hash().to_string(),
            total_solid_cells: sim.world().total_solid_cells(),
            body_count: sim.world().body_count(),
            checkpoints_published,
            journal_records_written,
            persist_bytes_per_write,
            persist_commit_bytes_per_sec,
            late_joins_completed: lj.completed,
            late_join_retries: lj.retries,
            late_joins_failed: lj.failed,
            expired_actions_rejected: lj.expired_actions,
            baseline_bytes_sent: lj.baseline_bytes,
        }
    });

    let sim_result = sim_join
        .await
        .map_err(|e| ServeError::Runtime(format!("sim thread panicked: {e}")))?;

    // Tear down.
    let _ = stop_tx.send(true);
    server.close();
    let _ = tokio::time::timeout(Duration::from_secs(3), server.wait_idle()).await;
    let _ = accept.await;

    let clients_connected = *count_rx.borrow();
    let result = if sim_result.ok { "passed" } else { "failed" };
    log.write(&ProcessRecord::new(
        if sim_result.ok {
            ProcessEvent::Stopped
        } else {
            ProcessEvent::Failed
        },
        ProcessRole::Server,
        Some(format!(
            "result={result}, ticks={}, committed={}, hash={}",
            sim_result.ticks_run, sim_result.committed_total, sim_result.final_world_hash
        )),
    ))?;

    let summary = ServeSummary {
        version: 1,
        result: result.to_string(),
        scene: format!("{scene:?}"),
        bound_addr: bound.to_string(),
        ticks_run: sim_result.ticks_run,
        clients_connected,
        transactions_committed: sim_result.committed_total,
        actions_rejected: sim_result.rejected_total,
        final_world_hash: sim_result.final_world_hash.clone(),
        total_solid_cells: sim_result.total_solid_cells,
        body_count: sim_result.body_count,
        checkpoints_published: sim_result.checkpoints_published,
        journal_records_written: sim_result.journal_records_written,
        persist_bytes_per_write: sim_result.persist_bytes_per_write,
        persist_commit_bytes_per_sec: sim_result.persist_commit_bytes_per_sec,
        late_joins_completed: sim_result.late_joins_completed,
        late_join_retries: sim_result.late_join_retries,
        late_joins_failed: sim_result.late_joins_failed,
        expired_actions_rejected: sim_result.expired_actions_rejected,
        baseline_bytes_sent: sim_result.baseline_bytes_sent,
    };
    if let Some(path) = &config.summary_json {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&summary).expect("serializable"),
        )?;
    }
    if let Some(err) = sim_result.error {
        return Err(ServeError::Runtime(err));
    }
    Ok(summary)
}

struct SimResult {
    ok: bool,
    error: Option<String>,
    ticks_run: u64,
    committed_total: u64,
    rejected_total: u64,
    final_world_hash: String,
    total_solid_cells: u64,
    body_count: usize,
    checkpoints_published: u64,
    journal_records_written: u64,
    persist_bytes_per_write: f64,
    persist_commit_bytes_per_sec: f64,
    late_joins_completed: u64,
    late_join_retries: u64,
    late_joins_failed: u64,
    expired_actions_rejected: u64,
    baseline_bytes_sent: u64,
}

impl SimResult {
    fn error(msg: String, ticks_run: u64) -> Self {
        Self {
            ok: false,
            error: Some(msg),
            ticks_run,
            committed_total: 0,
            rejected_total: 0,
            final_world_hash: String::new(),
            total_solid_cells: 0,
            body_count: 0,
            checkpoints_published: 0,
            journal_records_written: 0,
            persist_bytes_per_write: 0.0,
            persist_commit_bytes_per_sec: 0.0,
            late_joins_completed: 0,
            late_join_retries: 0,
            late_joins_failed: 0,
            expired_actions_rejected: 0,
            baseline_bytes_sent: 0,
        }
    }
}

// --- T17 late-join / catch-up / session renewal ---------------------------

/// How the server is currently treating one connected client.
enum Phase {
    /// Normal replication: every committed transaction is pushed immediately.
    Live,
    /// A late-join baseline is in flight. Transactions committed after the
    /// capture tick are buffered until the client acks the transfer; a burst
    /// past the cap cancels and re-captures a fresher baseline.
    Joining {
        transfer_id: TransferId,
        queue: VecDeque<Arc<TopologyTransaction>>,
        retries: u32,
    },
}

struct ClientLink {
    session: SessionId,
    phase: Phase,
}

/// All the per-run late-join / reconnect bookkeeping the sim loop needs, kept
/// out of the loop body. Also the unit-test surface for the catch-up bound and
/// the session-generation guard.
struct LateJoin {
    /// `session.raw()` → link.
    links: HashMap<u64, ClientLink>,
    /// slot → highest session generation seen. A record from a lower generation
    /// is from a superseded (reconnected-past) session.
    latest_gen: HashMap<u32, u32>,
    next_transfer_id: u64,
    catch_up_cap: usize,
    max_retries: u32,
    completed: u64,
    retries: u64,
    failed: u64,
    expired_actions: u64,
    baseline_bytes: u64,
}

impl LateJoin {
    fn new(catch_up_cap: usize, max_retries: u32) -> Self {
        Self {
            links: HashMap::new(),
            latest_gen: HashMap::new(),
            next_transfer_id: 1,
            catch_up_cap,
            max_retries,
            completed: 0,
            retries: 0,
            failed: 0,
            expired_actions: 0,
            baseline_bytes: 0,
        }
    }

    fn on_joined(&mut self, session: SessionId) {
        self.latest_gen
            .entry(session.slot().0)
            .and_modify(|g| *g = (*g).max(session.generation()))
            .or_insert(session.generation());
        self.links.insert(
            session.raw(),
            ClientLink {
                session,
                phase: Phase::Live,
            },
        );
    }

    fn on_gone(&mut self, session: SessionId) {
        // Keep `latest_gen` so a straggler record from this session is still
        // rejected after the link is gone.
        self.links.remove(&session.raw());
    }

    /// `true` if `session`'s generation has been superseded by a reconnect on
    /// the same slot (`docs/protocol.md`: "Reconnect uses a new session
    /// generation; old queued inputs and packets are invalid").
    fn session_expired(&self, session: SessionId) -> bool {
        self.latest_gen
            .get(&session.slot().0)
            .is_some_and(|&g| session.generation() < g)
    }

    fn next_id(&mut self) -> TransferId {
        let id = TransferId(self.next_transfer_id);
        self.next_transfer_id += 1;
        id
    }

    /// Handles a `BaselineAck`: the sentinel starts a late-join transfer; a real
    /// id that matches the in-flight transfer flushes the catch-up queue, sends
    /// a motion keyframe, and promotes the client to live.
    fn on_baseline_ack(
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
        let Some(link) = self.links.get(&session.raw()) else {
            return;
        };

        if want_baseline {
            let id = self.next_id();
            match capture_for(sim, id) {
                Some(transfer) => {
                    self.baseline_bytes += transfer.payload_bytes() as u64;
                    if let Some(link) = self.links.get_mut(&session.raw()) {
                        link.phase = Phase::Joining {
                            transfer_id: id,
                            queue: VecDeque::new(),
                            retries: 0,
                        };
                    }
                    send_to(clients, session, Outbound::Baseline(Arc::new(transfer)));
                }
                None => {
                    self.failed += 1;
                    self.links.remove(&session.raw());
                    send_to(clients, session, Outbound::Shutdown);
                }
            }
            return;
        }

        // Confirmation of an in-flight transfer.
        let matches = matches!(&link.phase, Phase::Joining { transfer_id, .. } if *transfer_id == ack.transfer_id);
        if !matches {
            return; // stale / duplicate ack
        }
        let drained: Vec<Arc<TopologyTransaction>> = match self.links.get_mut(&session.raw()) {
            Some(ClientLink {
                phase: Phase::Joining { queue, .. },
                ..
            }) => queue.drain(..).collect(),
            _ => Vec::new(),
        };
        for tx in drained {
            send_to(clients, session, Outbound::Transaction(tx));
        }
        // A current motion keyframe for every body (`docs/protocol.md` step 4).
        let keyframe = motion.snapshots(sim.world(), sim.current_tick());
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
    fn fan_out_transaction(
        &mut self,
        tx: Arc<TopologyTransaction>,
        sim: &Simulation,
        clients: &ClientMap,
    ) {
        let mut overflowed: Vec<u64> = Vec::new();
        for (raw, link) in self.links.iter_mut() {
            match &mut link.phase {
                Phase::Live => {
                    send_to(clients, link.session, Outbound::Transaction(tx.clone()));
                }
                Phase::Joining { queue, .. } => {
                    queue.push_back(tx.clone());
                    if queue.len() > self.catch_up_cap {
                        overflowed.push(*raw);
                    }
                }
            }
        }
        for raw in overflowed {
            let (session, retries) = match self.links.get_mut(&raw) {
                Some(ClientLink {
                    session,
                    phase: Phase::Joining { retries, .. },
                }) => {
                    *retries += 1;
                    (*session, *retries)
                }
                _ => continue,
            };
            if retries > self.max_retries {
                self.failed += 1;
                self.links.remove(&raw);
                send_to(clients, session, Outbound::Shutdown);
                continue;
            }
            self.retries += 1;
            let id = self.next_id();
            match capture_for(sim, id) {
                Some(transfer) => {
                    self.baseline_bytes += transfer.payload_bytes() as u64;
                    if let Some(link) = self.links.get_mut(&raw) {
                        link.phase = Phase::Joining {
                            transfer_id: id,
                            queue: VecDeque::new(),
                            retries,
                        };
                    }
                    send_to(clients, session, Outbound::Baseline(Arc::new(transfer)));
                }
                None => {
                    self.failed += 1;
                    self.links.remove(&raw);
                    send_to(clients, session, Outbound::Shutdown);
                }
            }
        }
    }

    /// Answers a brick `RepairRequest` with an authoritative one-brick baseline
    /// patch (full parity, revision included).
    fn answer_repair(
        &mut self,
        session: SessionId,
        req: &RepairRequest,
        sim: &Simulation,
        clients: &ClientMap,
    ) {
        let Some(world) = baseline::brick_repair_patch(sim, req) else {
            return;
        };
        let id = self.next_id();
        let cursor = JournalSeq(sim.journal_cursor());
        if let Ok(transfer) = baseline::transfer_from_world(world, id, InterestEpoch(1), cursor) {
            self.baseline_bytes += transfer.payload_bytes() as u64;
            send_to(clients, session, Outbound::Baseline(Arc::new(transfer)));
        }
    }
}

/// Captures a baseline transfer at the current tick / journal cursor.
fn capture_for(sim: &Simulation, id: TransferId) -> Option<BaselineTransfer> {
    let cursor = JournalSeq(sim.journal_cursor());
    baseline::capture_transfer(sim, id, InterestEpoch(1), cursor).ok()
}

/// Complete checkpoints kept on disk at each retention pass. Older ones — and
/// the journal rows they alone covered — are pruned (`docs/protocol.md`: "Cap
/// retention; lagging joins get a fresh baseline").
const RETAIN_CHECKPOINTS: usize = 2;

/// What [`setup_persistence`] hands back: the simulation, the bounded async
/// persistence pipeline (`None` when `--save` is unset), the highest journal
/// sequence already durable at startup, and how many checkpoints were published
/// synchronously during setup.
struct Persistence {
    sim: Simulation,
    pipeline: Option<PersistPipeline>,
    journalled_through: u64,
    checkpoints_published: u64,
}

/// Opens the world database, recovering from it when it already holds a
/// checkpoint and otherwise starting the built-in `scene` and publishing an
/// initial checkpoint. The [`Writer`] is then handed to a [`PersistPipeline`]
/// so every later durable write happens off the simulation thread (ENG-50).
/// `None` save path → no persistence.
fn setup_persistence(
    save: Option<&std::path::Path>,
    scene: Scene,
    cfg: &PersistConfig,
) -> Result<Persistence, String> {
    let Some(path) = save else {
        return Ok(Persistence {
            sim: scene.simulation(),
            pipeline: None,
            journalled_through: 0,
            checkpoints_published: 0,
        });
    };
    let mut writer = Writer::open(path).map_err(|e| e.to_string())?;
    let (sim, journalled_through, checkpoints_published) = match writer.recover() {
        Ok(recovery) => {
            let (sim, seq) = persist::restore(
                &recovery,
                fixtures::stone_manifest(),
                AnchorPlane::at(0),
                PhysicsConfig::default(),
            )
            .map_err(|e| e.to_string())?;
            (sim, seq, 0)
        }
        Err(spall_store::StoreError::NoCheckpoint) => {
            let sim = scene.simulation();
            let checkpoint = persist::capture(&sim, cfg, 0).map_err(|e| e.to_string())?;
            writer
                .publish_checkpoint(&checkpoint)
                .map_err(|e| e.to_string())?;
            (sim, 0, 1)
        }
        Err(e) => return Err(e.to_string()),
    };
    let pipeline = PersistPipeline::spawn(writer, PipelineConfig::default());
    Ok(Persistence {
        sim,
        pipeline: Some(pipeline),
        journalled_through,
        checkpoints_published,
    })
}

/// Builds the contiguous journal batch owed for one tick: every committed
/// topology entry past `journalled_through`, followed by this tick's 20 Hz pose
/// batch (if any). The pose sequence is reserved from the simulation so it is
/// contiguous with the topology sequences (`docs/protocol.md`: "Journal
/// periodic body pose batches at 20 Hz"; ENG-50: "contiguous sequence
/// ownership"). Advances `journalled_through` to the last sequence in the
/// batch.
fn tick_journal_batch(
    sim: &mut Simulation,
    journalled_through: &mut u64,
    pose_batch: Option<&[MotionSnapshot]>,
    tick: u64,
) -> Result<Vec<spall_store::JournalRecord>, String> {
    let mut batch = persist::journal_records(sim.journal().entries_after(*journalled_through))
        .map_err(|e| e.to_string())?;
    if let Some(snaps) = pose_batch.filter(|s| !s.is_empty()) {
        let seq = sim.reserve_journal_seq().map_err(|e| e.to_string())?;
        batch.push(persist::pose_batch_record(seq.0, tick, snaps).map_err(|e| e.to_string())?);
    }
    if let Some(last) = batch.last() {
        *journalled_through = last.seq;
    }
    Ok(batch)
}

fn intent_from_request(session: SessionId, req: &ActionRequest) -> Option<EditIntent> {
    let target = match req.claimed_target {
        ClaimedTarget::Terrain => EditTarget::Terrain,
        ClaimedTarget::Body(entity) => EditTarget::Body(entity),
    };
    let kind = match req.action {
        ActionKind::Cut => EditKind::Cut,
        ActionKind::Place => EditKind::Place(spall_voxel::fixtures::STONE),
    };
    // A stable per-session actor id; authority is the server's, this is only
    // journal provenance.
    let actor = EntityId::new(1 + u64::from(session.slot().0)).unwrap_or(EntityId::new(1).unwrap());
    Some(EditIntent {
        request_id: req.request_id,
        actor,
        target,
        kind,
        brush: req.claimed_brush,
        explosion: None,
    })
}

fn broadcast(clients: &ClientMap, msg: Outbound) {
    let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|_, tx| tx.send(msg.clone()).is_ok());
}

fn send_to(clients: &ClientMap, session: SessionId, msg: Outbound) {
    let guard = clients.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = guard.get(&session.raw()) {
        let _ = tx.send(msg);
    }
}

fn reject(clients: &ClientMap, session: SessionId, request: RequestId, reason: &str) {
    send_to(
        clients,
        session,
        Outbound::Status(Arc::new(ActionStatus {
            request_id: request,
            outcome: ActionOutcome::Rejected {
                reason: reason.to_string(),
            },
        })),
    );
}

/// Pushes one baseline transfer to a client: `BaselineBegin` on the control
/// stream, every part on a fresh bulk stream, then `BaselineEnd` on control.
/// Returns `false` if any leg fails (the writer loop then tears the connection
/// down).
async fn send_baseline(conn: &Connection, transfer: &BaselineTransfer) -> bool {
    if conn
        .send_record(WireRecord::BaselineBegin(transfer.begin.clone()))
        .await
        .is_err()
    {
        return false;
    }
    let mut bulk = match conn.open_bulk().await {
        Ok(b) => b,
        Err(_) => return false,
    };
    for part in &transfer.parts {
        if bulk.send_part(part).await.is_err() {
            return false;
        }
    }
    if bulk.finish().is_err() {
        return false;
    }
    conn.send_record(WireRecord::BaselineEnd(transfer.end))
        .await
        .is_ok()
}

async fn wait_true(mut rx: watch::Receiver<bool>) {
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
async fn serve_conn(
    conn: Arc<Connection>,
    inbound: mpsc::UnboundedSender<Inbound>,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
    clients: ClientMap,
    stop: watch::Receiver<bool>,
) {
    debug_assert_eq!(conn.role(), Role::Server);
    let session = conn.session();

    // Announce the join in order so the sim loop can supersede an earlier
    // session on the same slot (reconnect) and drive a late-join baseline.
    let _ = inbound.send(Inbound::Joined(session));

    let liveness = tokio::spawn(conn.clone().run_liveness(stop.clone()));

    let reader = {
        let conn = conn.clone();
        let inbound = inbound.clone();
        tokio::spawn(async move {
            loop {
                match conn.recv_record().await {
                    Ok(Some(WireRecord::ActionRequest(req))) => {
                        let _ = inbound.send(Inbound::Action(session, req));
                    }
                    Ok(Some(WireRecord::RepairRequest(req))) => {
                        let _ = inbound.send(Inbound::Repair(session, req));
                    }
                    Ok(Some(WireRecord::BaselineAck(ack))) => {
                        let _ = inbound.send(Inbound::Baseline(session, ack));
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            let _ = inbound.send(Inbound::Gone(session));
        })
    };

    let mut motion_seq = 0u64;
    loop {
        tokio::select! {
            msg = outbound.recv() => {
                let Some(msg) = msg else { break };
                let ok = match msg {
                    Outbound::Transaction(tx) => conn
                        .send_record(WireRecord::TopologyTransaction((*tx).clone()))
                        .await
                        .is_ok(),
                    Outbound::Status(s) => conn
                        .send_record(WireRecord::ActionStatus((*s).clone()))
                        .await
                        .is_ok(),
                    Outbound::Baseline(transfer) => {
                        send_baseline(&conn, &transfer).await
                    }
                    Outbound::Motion(snaps) => {
                        let mut ok = true;
                        for snap in snaps.iter() {
                            if conn.send_datagram(motion_seq, snap).await.is_err() {
                                ok = false;
                                break;
                            }
                            motion_seq += 1;
                        }
                        ok
                    }
                    Outbound::Shutdown => {
                        let _ = conn.say_bye("server complete").await;
                        false
                    }
                };
                if !ok {
                    break;
                }
            }
            _ = wait_true(stop.clone()) => break,
        }
    }

    clients
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&session.raw());
    conn.close("connection complete");
    reader.abort();
    liveness.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_clients() -> ClientMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn sess(slot: u32, generation: u32) -> SessionId {
        SessionId::from_parts(SlotId(slot), generation)
    }

    #[test]
    fn a_reconnect_supersedes_the_old_session_generation() {
        let mut lj = LateJoin::new(DEFAULT_CATCH_UP_CAP, DEFAULT_MAX_JOIN_RETRIES);
        let old = sess(0, 1);
        let new = sess(0, 2);
        lj.on_joined(old);
        assert!(!lj.session_expired(old));
        lj.on_joined(new);
        assert!(lj.session_expired(old), "the old generation is now expired");
        assert!(!lj.session_expired(new));
        // A straggler after the old link is gone is still rejected.
        lj.on_gone(old);
        assert!(lj.session_expired(old));
        // A slot that never connected is not "expired".
        assert!(!lj.session_expired(sess(3, 1)));
    }

    #[test]
    fn catch_up_overflow_recaptures_then_drops_after_the_retry_budget() {
        let sim = Scene::BridgeCut.simulation();
        let clients = empty_clients();
        // cap 2, one retry allowed.
        let mut lj = LateJoin::new(2, 1);
        let joiner = sess(0, 1);
        lj.on_joined(joiner);
        lj.on_baseline_ack(
            joiner,
            BaselineAck {
                transfer_id: BASELINE_REQUEST_SENTINEL,
                verified_manifest_hash: Hash32::ZERO,
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
            lj.fan_out_transaction(tx(), &sim, &clients);
        }
        assert_eq!(lj.retries, 1);
        assert!(lj.links.contains_key(&joiner.raw()));

        // Fill the fresh queue past the cap again → retry 2 > budget → dropped.
        for _ in 0..3 {
            lj.fan_out_transaction(tx(), &sim, &clients);
        }
        assert_eq!(lj.failed, 1);
        assert!(
            !lj.links.contains_key(&joiner.raw()),
            "the joiner was dropped after exhausting its retry budget; other clients are untouched"
        );
    }

    #[test]
    fn a_live_client_is_never_queued() {
        let sim = Scene::BridgeCut.simulation();
        let clients = empty_clients();
        let mut lj = LateJoin::new(1, 1);
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

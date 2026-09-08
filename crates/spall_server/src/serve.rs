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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use spall_core::{EntityId, JsonlError, JsonlLog, ProcessEvent, ProcessRecord, ProcessRole, Tick};
use spall_net::{
    Connection, DevIdentity, JoinToken, NetServer, Role, TransportConfig, TransportError,
    WireRecord,
};
use spall_physics::PhysicsConfig;
use spall_protocol::{
    ActionKind, ActionOutcome, ActionRequest, ActionStatus, AlgorithmVersions, ClaimedTarget,
    Handshake, Hash32, MotionSnapshot, NegotiatedLimits, PROTOCOL_VERSION, RepairRequest,
    RequestId, SessionId, SlotId, TopologyTransaction,
};
use spall_sim::{
    EditIntent, EditKind, EditTarget, MotionPublisher, Simulation, SimulationConfig,
    action_statuses, committed_transactions, fixtures, repair_ops,
};
use spall_store::Writer;
use spall_structure::AnchorPlane;
use tokio::sync::{mpsc, watch};

use crate::persist::{self, PersistConfig};

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
    Action(SessionId, ActionRequest),
    Repair(SessionId, RepairRequest),
    Gone,
}

/// One message from the sim bridge to a client's writer task. Repair replies are
/// already routed to one client's queue, so they need no session tag here.
#[derive(Debug, Clone)]
enum Outbound {
    Transaction(Arc<TopologyTransaction>),
    Status(Arc<ActionStatus>),
    Motion(Arc<Vec<MotionSnapshot>>),
    Repair(Arc<TopologyTransaction>),
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
    let persist_cfg = PersistConfig {
        world_id: T10_WORLD_ID,
        seed: config.seed,
        generator_version: 1,
    };

    let sim_join = tokio::task::spawn_blocking(move || -> SimResult {
        // Open the world database (T16). If it already holds a checkpoint,
        // recover from it; otherwise start the built-in scene and publish an
        // initial checkpoint so recovery always has a floor.
        let (mut sim, mut durable_seq, mut store, mut checkpoints_published) =
            match setup_persistence(save.as_deref(), scene, &persist_cfg) {
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

        for _ in 0..max_ticks {
            let started = std::time::Instant::now();

            // Drain everything the clients have sent since the last tick.
            let mut repairs: Vec<(SessionId, RepairRequest)> = Vec::new();
            let mut saw_client_work = false;
            while let Ok(msg) = inbound_rx.try_recv() {
                match msg {
                    Inbound::Action(session, req) => {
                        saw_client_work = true;
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
                        repairs.push((session, req));
                    }
                    Inbound::Gone => {}
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
                broadcast(&clients_for_sim, Outbound::Transaction(Arc::new(tx)));
            }
            for status in action_statuses(&report) {
                if matches!(status.outcome, ActionOutcome::Rejected { .. }) {
                    rejected_total += 1;
                }
                broadcast(&clients_for_sim, Outbound::Status(Arc::new(status)));
            }
            if motion.due(tick) {
                let snaps = motion.snapshots(sim.world(), tick);
                if !snaps.is_empty() {
                    broadcast(&clients_for_sim, Outbound::Motion(Arc::new(snaps)));
                }
            }
            for (session, req) in repairs {
                if let Some(ops) = repair_ops(sim.world(), &req)
                    && !ops.is_empty()
                {
                    let tx = synthetic_repair_tx(tick, ops);
                    send_to(&clients_for_sim, session, Outbound::Repair(Arc::new(tx)));
                }
            }

            // T16: journal every newly committed transaction, then checkpoint
            // on the interval. A durable-write failure stops the run rather
            // than silently continuing an unsavable world.
            if let Some(writer) = store.as_mut() {
                match flush_journal(writer, &sim, &mut durable_seq) {
                    Ok(n) => journal_records_written += n,
                    Err(e) => {
                        return SimResult::error(format!("journal flush failed: {e}"), ticks_run);
                    }
                }
                if checkpoint_interval > 0 && tick.get().is_multiple_of(checkpoint_interval) {
                    match publish_checkpoint(writer, &sim, &persist_cfg, durable_seq) {
                        Ok(()) => checkpoints_published += 1,
                        Err(e) => {
                            return SimResult::error(format!("checkpoint failed: {e}"), ticks_run);
                        }
                    }
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

        // Clean-shutdown checkpoint: flush the journal tail, publish a final
        // checkpoint, and prune to the last two.
        let mut persist_bytes_per_write = 0.0;
        let mut persist_commit_bytes_per_sec = 0.0;
        if let Some(writer) = store.as_mut() {
            if let Ok(n) = flush_journal(writer, &sim, &mut durable_seq) {
                journal_records_written += n;
            }
            if publish_checkpoint(writer, &sim, &persist_cfg, durable_seq).is_ok() {
                checkpoints_published += 1;
            }
            let _ = writer.retain(2);
            let m = writer.metrics();
            persist_bytes_per_write = m.bytes_per_journal_write();
            persist_commit_bytes_per_sec = m.commit_bytes_per_sec();
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
        }
    }
}

/// Opens the world database, recovering from it when it already holds a
/// checkpoint and otherwise starting the built-in `scene` and publishing an
/// initial checkpoint. `None` save path → no persistence.
fn setup_persistence(
    save: Option<&std::path::Path>,
    scene: Scene,
    cfg: &PersistConfig,
) -> Result<(Simulation, u64, Option<Writer>, u64), String> {
    let Some(path) = save else {
        return Ok((scene.simulation(), 0, None, 0));
    };
    let mut writer = Writer::open(path).map_err(|e| e.to_string())?;
    match writer.recover() {
        Ok(recovery) => {
            let (sim, seq) = persist::restore(
                &recovery,
                fixtures::stone_manifest(),
                AnchorPlane::at(0),
                PhysicsConfig::default(),
            )
            .map_err(|e| e.to_string())?;
            Ok((sim, seq, Some(writer), 0))
        }
        Err(spall_store::StoreError::NoCheckpoint) => {
            let sim = scene.simulation();
            let checkpoint = persist::capture(&sim, cfg, 0).map_err(|e| e.to_string())?;
            writer
                .publish_checkpoint(&checkpoint)
                .map_err(|e| e.to_string())?;
            Ok((sim, 0, Some(writer), 1))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Appends every simulation journal entry past `durable_seq` to the store and
/// advances `durable_seq` to the acknowledged sequence. Returns how many
/// records were written.
fn flush_journal(
    writer: &mut Writer,
    sim: &Simulation,
    durable_seq: &mut u64,
) -> Result<u64, String> {
    let pending: Vec<_> = sim
        .journal()
        .entries()
        .iter()
        .filter(|e| e.seq.0 > *durable_seq)
        .cloned()
        .collect();
    if pending.is_empty() {
        return Ok(0);
    }
    let records = persist::journal_records(&pending).map_err(|e| e.to_string())?;
    let durable = writer.append_journal(&records).map_err(|e| e.to_string())?;
    *durable_seq = durable.journal_seq.0;
    Ok(records.len() as u64)
}

fn publish_checkpoint(
    writer: &mut Writer,
    sim: &Simulation,
    cfg: &PersistConfig,
    durable_seq: u64,
) -> Result<(), String> {
    let checkpoint = persist::capture(sim, cfg, durable_seq).map_err(|e| e.to_string())?;
    writer
        .publish_checkpoint(&checkpoint)
        .map_err(|e| e.to_string())
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

fn synthetic_repair_tx(tick: Tick, ops: Vec<spall_protocol::TopologyOp>) -> TopologyTransaction {
    TopologyTransaction {
        transaction_id: spall_core::TransactionId::new(1).unwrap(),
        server_tick: tick,
        control_seq: spall_protocol::ControlSeq(0),
        algorithm_version: 1,
        dependencies: vec![],
        before: vec![],
        after: vec![],
        ops,
        result_hashes: vec![],
    }
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
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            let _ = inbound.send(Inbound::Gone);
        })
    };

    let mut motion_seq = 0u64;
    loop {
        tokio::select! {
            msg = outbound.recv() => {
                let Some(msg) = msg else { break };
                let ok = match msg {
                    Outbound::Transaction(tx) | Outbound::Repair(tx) => conn
                        .send_record(WireRecord::TopologyTransaction((*tx).clone()))
                        .await
                        .is_ok(),
                    Outbound::Status(s) => conn
                        .send_record(WireRecord::ActionStatus((*s).clone()))
                        .await
                        .is_ok(),
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

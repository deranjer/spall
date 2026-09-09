//! Headless replication client for T10.
//!
//! [`run_replication_client`] connects to a [`spall_server`] host over real
//! QUIC, installs the same fixed-scene baseline the server started from, then:
//!
//! * applies every reliable [`spall_protocol::TopologyTransaction`] to a
//!   [`ReplicaWorld`] and, on a `before`-revision gap, sends the
//!   [`spall_protocol::RepairRequest`]s the replica asked for;
//! * feeds every [`spall_protocol::MotionSnapshot`] datagram into the replica's
//!   motion tracks;
//! * fires a script of [`spall_protocol::ActionRequest`]s once the observed
//!   server tick reaches each entry's `at_tick`.
//!
//! It stops when the server finishes its bounded run (the control stream ends)
//! or an idle-grace timeout elapses, and reports the replica's final topology
//! hash so a harness can compare it against the server's.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    EntityId, JsonlError, JsonlLog, PlayerInput, ProcessEvent, ProcessRecord, ProcessRole,
    SphereBrush, Tick, VolumeId,
};
use spall_net::{
    Connection, DatagramRecord, Fingerprint, JoinToken, TransportConfig, TransportError,
    WireRecord, connect,
};
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::{
    ActionKind, ActionOutcome, ActionRequest, AlgorithmVersions, BaselineAck, BaselineWorld,
    ClaimedTarget, Handshake, Hash32, InputFrame, InputSeq, MotionSnapshot, NegotiatedLimits,
    PROTOCOL_VERSION, RecentInput, RequestId, TransferId, session_player_entity,
};

use crate::predict::{ClientPhysics, PlayerMovementSummary, PredictedPlayer};
use crate::replica::{ApplyOutcome, ReplicaConfig, ReplicaWorld};

/// One leg of a scripted movement path: hold `input` from tick `from` up to (not
/// including) tick `to`.
#[derive(Debug, Clone, Copy)]
pub struct MovementStep {
    pub from_tick: u64,
    pub to_tick: u64,
    pub movement: [f32; 3],
    pub view_dir: [f32; 3],
    pub buttons: u32,
}

impl MovementStep {
    fn input(&self) -> PlayerInput {
        PlayerInput {
            movement: self.movement,
            view_dir: self.view_dir,
            buttons: self.buttons,
        }
    }
}

/// The scripted input for an observed server tick: the last step whose window
/// contains it, else neutral (so a silent tail after the script lets the
/// server's 250 ms held-input timeout fire).
fn scripted_input(script: &[MovementStep], tick: u64) -> PlayerInput {
    script
        .iter()
        .rev()
        .find(|s| tick >= s.from_tick && tick < s.to_tick)
        .map(MovementStep::input)
        .unwrap_or(PlayerInput::NEUTRAL)
}

/// The last tick any step covers — the client keeps predicting a little past it
/// with neutral input to prove it comes to rest.
fn script_end_tick(script: &[MovementStep]) -> u64 {
    script.iter().map(|s| s.to_tick).max().unwrap_or(0)
}

/// Client prediction timestep — the fixed 60 Hz server tick.
const MOVEMENT_DT_S: f32 = 1.0 / 60.0;

/// `BaselineAck.transfer_id` a late-join replica sends to request a baseline
/// (mirrors `spall_server::serve::BASELINE_REQUEST_SENTINEL`).
const BASELINE_REQUEST_SENTINEL: TransferId = TransferId(0);

/// Content-manifest tag; must match [`spall_server`]'s `T10_CONTENT_TAG`.
pub const T10_CONTENT_TAG: &[u8] = b"spall-t10-bridge-v1";
/// World id of the built-in T10 scene; must match the server.
pub const T10_WORLD_ID: u128 = 0x5A11_0000_0000_7010;
/// The T10 terrain volume id (`spall_sim` allocates volume 1 for terrain).
pub const TERRAIN_VOLUME: u64 = 1;

/// What a [`ScriptedAction`] aims at. `Terrain` uses the request verbatim;
/// `DetachedBody` rewrites `claimed_target` to the sole live detached body at
/// fire time (its entity id is not known when the script is built).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScriptTarget {
    #[default]
    Terrain,
    DetachedBody,
}

/// One scripted tool use.
#[derive(Debug, Clone)]
pub struct ScriptedAction {
    /// Fire once the observed server tick is at least this.
    pub at_tick: u64,
    pub request: ActionRequest,
    /// Retarget the request at the detached body before sending it.
    pub target: ScriptTarget,
}

/// A `Cut` request with a sphere brush centred on `(cell_x, cell_y, cell_z)` in
/// the terrain volume's local cell space.
pub fn cut_request(
    request_id: u64,
    input_seq: u64,
    cell: [i64; 3],
    radius_cells: i64,
) -> ActionRequest {
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            cell[0] * BRUSH_UNIT + h,
            cell[1] * BRUSH_UNIT + h,
            cell[2] * BRUSH_UNIT + h,
        ),
        radius_cells * BRUSH_UNIT,
    )
    .expect("radius within brush limits");
    ActionRequest {
        request_id: RequestId(request_id),
        input_seq: InputSeq(input_seq),
        action: ActionKind::Cut,
        tool: 0,
        aim_origin_m: [0.0, 1.0, 0.0],
        aim_dir: [0.0, 0.0, 1.0],
        claimed_target: ClaimedTarget::Terrain,
        claimed_brush: brush,
    }
}

/// Which fixed baseline scene a live (non-late-join) replica installs. Must
/// match the scene the server is running or the topology hashes never converge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BaselineScene {
    /// [`spall_voxel::fixtures::bridge_scene`] — single brick.
    #[default]
    BridgeCut,
    /// [`spall_voxel::fixtures::cross_brick_bridge_scene`] — column + beam cross
    /// the `x = 32` brick boundary.
    CrossBridgeCut,
}

impl BaselineScene {
    /// Parses the harness `--scene` value; `None` for an unknown name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "bridge-cut" | "bridgecut" | "bridge" => Some(Self::BridgeCut),
            "cross-bridge-cut" | "cross-brick-bridge" | "crossbridgecut" => {
                Some(Self::CrossBridgeCut)
            }
            _ => None,
        }
    }

    fn baseline(self, id: VolumeId) -> spall_voxel::Volume {
        match self {
            Self::BridgeCut => spall_voxel::fixtures::bridge_scene(id),
            Self::CrossBridgeCut => spall_voxel::fixtures::cross_brick_bridge_scene(id),
        }
    }
}

/// Inputs to [`run_replication_client`].
#[derive(Debug, Clone)]
pub struct ClientNetConfig {
    /// Server (or UDP proxy) address to send packets to.
    pub connect_addr: SocketAddr,
    pub server_fingerprint: Fingerprint,
    pub join_token: JoinToken,
    pub script: Vec<ScriptedAction>,
    /// T19: a scripted movement path for this client's player capsule. When
    /// non-empty the client pulls a baseline (any scene), predicts capsule
    /// movement locally, sends `InputFrame` datagrams, and reconciles against
    /// the server's player snapshots.
    pub movement_script: Vec<MovementStep>,
    /// T17: request a full late-join baseline over a bulk transfer instead of
    /// installing the fixed `bridge_scene`. The replica reaches the server's
    /// current topology hash with no edit replay from world creation.
    pub late_join: bool,
    /// Fixed baseline a live replica installs (ignored when `late_join`). Must
    /// match the server's `--scene`.
    pub baseline_scene: BaselineScene,
    /// Stop once the observed server tick reaches this (0 = only stop on close).
    pub run_ticks: u64,
    /// Stop after this long with no new record once at least one has arrived.
    pub idle_grace: Duration,
    /// Hard cap on the whole session.
    pub overall_timeout: Duration,
    pub log_json: PathBuf,
    pub summary_json: Option<PathBuf>,
    pub transport: TransportConfig,
}

/// Machine-readable result of a client run.
#[derive(Debug, Clone, Serialize)]
pub struct ClientSummary {
    pub version: u32,
    pub result: String,
    pub connected: bool,
    pub transactions_applied: u64,
    pub repair_requests_sent: u64,
    pub transactions_rejected: u64,
    pub motion_snapshots: u64,
    /// Motion datagrams delivered out of `snapshot_seq` order — non-zero only
    /// when the transport (a lossy/jittered proxy) actually reordered them.
    pub motion_snapshots_out_of_order: u64,
    pub actions_sent: u64,
    pub last_server_tick: u64,
    pub final_world_hash: String,
    pub total_solid_cells: u64,
    pub body_count: usize,
    /// T17: whether this run installed a late-join baseline transfer.
    pub late_join: bool,
    /// T17: resident bricks in the installed baseline (`0` unless `late_join`).
    pub baseline_bricks: u64,
    /// T17: hash-repair baseline patches applied mid-session.
    pub repairs_applied: u64,
    /// Farthest a replicated body moved from its first observed pose, metres.
    /// A body that only ever reported a stationary snapshot reads `0.0`.
    pub max_body_displacement_m: f64,
    /// A `DetachedBody`-targeted scripted cut was sent and the body it named
    /// lost solid cells afterwards (the body cut committed).
    pub body_cut_committed: bool,
    /// T19: prediction / reconciliation result for a scripted player, if this
    /// client ran a `movement_script`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub movement: Option<PlayerMovementSummary>,
}

/// Anything that stops a client run before it can report.
#[derive(Debug, thiserror::Error)]
pub enum ClientNetError {
    #[error(transparent)]
    Log(#[from] JsonlError),
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("tokio runtime: {0}")]
    Runtime(String),
    #[error("late-join baseline: {0}")]
    Baseline(String),
}

/// Connects, replicates, scripts, and reports. Builds its own Tokio runtime.
pub fn run_replication_client(config: ClientNetConfig) -> Result<ClientSummary, ClientNetError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ClientNetError::Runtime(e.to_string()))?;
    runtime.block_on(run_async(config))
}

fn client_handshake() -> Handshake {
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
        session: spall_protocol::SessionId::from_parts(spall_protocol::SlotId(0), 1),
        limits: NegotiatedLimits::DEFAULT,
    }
}

#[derive(Default)]
struct Counters {
    applied: AtomicU64,
    repairs: AtomicU64,
    rejected: AtomicU64,
    motion: AtomicU64,
    /// Motion datagrams that arrived carrying a `snapshot_seq` lower than one
    /// already delivered — proof the transport reordered snapshot datagrams
    /// (the server assigns a strictly increasing per-publisher `snapshot_seq`).
    motion_reordered: AtomicU64,
    /// Highest `snapshot_seq` observed so far, for the reorder check above.
    motion_seq_hwm: AtomicU64,
    actions: AtomicU64,
    last_tick: AtomicU64,
    /// Hash-repair baseline patches applied mid-session (T17).
    patches: AtomicU64,
    /// Resident bricks in the installed late-join baseline (T17).
    baseline_bricks: AtomicU64,
    /// Entity id (+1, so `0` means "never fired") a `DetachedBody` scripted cut
    /// was aimed at, and that body's solid-cell count captured just before the
    /// cut was sent. Together they let the run confirm the body cut committed.
    body_cut_entity_plus1: AtomicU64,
    body_cut_pre_cells: AtomicU64,
}

/// The `CharacterState` carried by a player [`MotionSnapshot`]. Orientation is
/// discarded (players walk upright); `grounded` / `jump_held_last` are not on
/// the wire and the next reconcile / step re-derives them.
fn state_from_snapshot(snap: &MotionSnapshot) -> CharacterState {
    CharacterState {
        position_m: snap.pose.translation_m,
        velocity_m_s: snap.linear_velocity,
        grounded: snap.linear_velocity[1].abs() < 0.6,
        jump_held_last: false,
    }
}

/// T19 predicted-player state shared by the mover and motion tasks. The mover
/// owns terrain rebuilds and prediction ticks; the motion task only reconciles.
struct Predictor {
    entity: EntityId,
    params: CharacterParams,
    phys: ClientPhysics,
    player: Option<PredictedPlayer>,
    terrain_hash: Option<Hash32>,
    input_seq: u64,
    recent: std::collections::VecDeque<RecentInput>,
}

impl Predictor {
    fn new(entity: EntityId) -> Self {
        Self {
            entity,
            params: CharacterParams::DEFAULT,
            phys: ClientPhysics::new(),
            player: None,
            terrain_hash: None,
            input_seq: 0,
            recent: std::collections::VecDeque::new(),
        }
    }
}

/// Receives one baseline transfer whose `BaselineBegin` has already been read:
/// accepts the bulk stream, reassembles + decodes the payload, then consumes
/// records until `BaselineEnd`. Returns the decoded world, or `None` on any
/// transport / decode failure.
async fn receive_baseline_body(conn: &Connection) -> Option<BaselineWorld> {
    let parts = conn.accept_bulk().await.ok()?.collect_parts().await.ok()?;
    let mut bytes = Vec::new();
    for part in &parts {
        bytes.extend_from_slice(&part.payload);
    }
    let world = BaselineWorld::decode(&bytes).ok()?;
    // Drain until BaselineEnd (nothing else is interleaved for this client
    // while a transfer is in flight — the server writes it as one unit).
    loop {
        match conn.recv_record().await {
            Ok(Some(WireRecord::BaselineEnd(end))) => {
                if end.assembled_hash != Hash32::of(&world.encode()) {
                    return None;
                }
                return Some(world);
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

/// The initial late-join handshake: ask for a baseline, install it, confirm it.
async fn perform_late_join(
    conn: &Connection,
    replica: &Mutex<ReplicaWorld>,
    counters: &Counters,
) -> Result<(), ClientNetError> {
    conn.send_record(WireRecord::BaselineAck(BaselineAck {
        transfer_id: BASELINE_REQUEST_SENTINEL,
        verified_manifest_hash: Hash32::ZERO,
        installed_cursor: spall_core::JournalSeq(0),
    }))
    .await
    .map_err(ClientNetError::Transport)?;

    // Wait for BaselineBegin, skipping any stray pre-baseline records.
    let begin = loop {
        match conn.recv_record().await {
            Ok(Some(WireRecord::BaselineBegin(b))) => break b,
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => {
                return Err(ClientNetError::Baseline(
                    "connection closed before the baseline arrived".into(),
                ));
            }
        }
    };
    let Some(world) = receive_baseline_body(conn).await else {
        return Err(ClientNetError::Baseline(
            "transfer failed to assemble / verify".into(),
        ));
    };

    {
        let mut guard = replica.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .install_baseline_world(&world)
            .map_err(ClientNetError::Baseline)?;
    }
    counters
        .baseline_bricks
        .store(world.brick_count() as u64, Ordering::Relaxed);
    counters
        .last_tick
        .fetch_max(world.checkpoint_tick, Ordering::Relaxed);

    conn.send_record(WireRecord::BaselineAck(BaselineAck {
        transfer_id: begin.transfer_id,
        verified_manifest_hash: Hash32::of(&world.encode()),
        installed_cursor: begin.journal_cursor,
    }))
    .await
    .map_err(ClientNetError::Transport)?;
    Ok(())
}

async fn run_async(config: ClientNetConfig) -> Result<ClientSummary, ClientNetError> {
    let mut log = JsonlLog::create(&config.log_json)?;
    log.write(&ProcessRecord::new(
        ProcessEvent::Started,
        ProcessRole::Client,
        Some(format!("connect={}", config.connect_addr)),
    ))?;

    let conn = match connect(
        config.connect_addr,
        config.server_fingerprint,
        config.join_token,
        client_handshake(),
        config.transport,
    )
    .await
    {
        Ok(c) => Arc::new(c),
        Err(e) => {
            log.write(&ProcessRecord::new(
                ProcessEvent::Failed,
                ProcessRole::Client,
                Some(format!("connect failed: {e}")),
            ))?;
            return Err(ClientNetError::Transport(e));
        }
    };
    log.write(&ProcessRecord::new(
        ProcessEvent::Ready,
        ProcessRole::Client,
        Some(format!("session={}", conn.session())),
    ))?;

    // A movement client pulls a baseline like a late joiner so it works with any
    // scene the server runs (T19 uses the `walk` arena).
    let want_baseline = config.late_join || !config.movement_script.is_empty();

    let replica = Arc::new(Mutex::new(if want_baseline {
        ReplicaWorld::empty(ReplicaConfig::default())
    } else {
        ReplicaWorld::from_baseline(
            config
                .baseline_scene
                .baseline(VolumeId::new(TERRAIN_VOLUME).unwrap()),
            ReplicaConfig::default(),
        )
    }));
    let counters = Arc::new(Counters::default());

    // T17: pull a full baseline over a bulk transfer before touching the
    // replication stream, so the replica starts at the server's current
    // topology with no edit replay from world creation.
    if want_baseline {
        if let Err(e) = perform_late_join(&conn, &replica, &counters).await {
            log.write(&ProcessRecord::new(
                ProcessEvent::Failed,
                ProcessRole::Client,
                Some(format!("late join failed: {e}")),
            ))?;
            let _ = conn.say_bye("late join failed").await;
            conn.close("late join failed");
            return Err(e);
        }
        log.write(&ProcessRecord::new(
            ProcessEvent::Ready,
            ProcessRole::Client,
            Some(format!(
                "late-join baseline installed: {} bricks",
                counters.baseline_bricks.load(Ordering::Relaxed)
            )),
        ))?;
    }

    // T19: a scripted-movement client predicts its own player capsule.
    let predictor = (!config.movement_script.is_empty()).then(|| {
        Arc::new(Mutex::new(Predictor::new(session_player_entity(
            conn.session(),
        ))))
    });

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

    let liveness = tokio::spawn(conn.clone().run_liveness(stop_rx.clone()));

    // Bounded resend of `ActionRequest`s the server throttled (its per-tick
    // admission quota was exceeded — an explicitly retryable rejection). The
    // scripter records each request it sends; the control reader forwards
    // throttled request ids here; the retrier re-sends, capped per request.
    let sent_actions: Arc<Mutex<HashMap<u64, (WireRecord, u8)>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (throttle_tx, mut throttle_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();

    // Control reader: apply transactions, answer repair gaps.
    let control = {
        let conn = conn.clone();
        let replica = replica.clone();
        let counters = counters.clone();
        let throttle_tx = throttle_tx.clone();
        tokio::spawn(async move {
            loop {
                match conn.recv_record().await {
                    Ok(Some(WireRecord::TopologyTransaction(tx))) => {
                        counters
                            .last_tick
                            .fetch_max(tx.server_tick.get(), Ordering::Relaxed);
                        // ENG-49: a `Published` transaction can be the missing
                        // predecessor another held transaction was gapped on, so
                        // retry the pending set behind it. Every resulting
                        // outcome is forwarded through one path.
                        let mut outcomes = Vec::new();
                        {
                            let mut guard = replica.lock().unwrap_or_else(|e| e.into_inner());
                            let primary = guard.apply_transaction(&tx);
                            let publish = matches!(primary, ApplyOutcome::Published { .. });
                            outcomes.push(primary);
                            if publish {
                                for (_, o) in guard.retry_pending_repair_txns() {
                                    outcomes.push(o);
                                }
                            }
                        }
                        for outcome in outcomes {
                            match outcome {
                                ApplyOutcome::Published { .. } => {
                                    counters.applied.fetch_add(1, Ordering::Relaxed);
                                }
                                ApplyOutcome::NeedsRepair(reqs) => {
                                    for req in reqs {
                                        let _ =
                                            conn.send_record(WireRecord::RepairRequest(req)).await;
                                        counters.repairs.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                ApplyOutcome::Rejected { .. } => {
                                    counters.rejected.fetch_add(1, Ordering::Relaxed);
                                }
                                ApplyOutcome::Duplicate => {}
                            }
                        }
                    }
                    Ok(Some(WireRecord::BaselineBegin(_))) => {
                        // A mid-session hash-repair patch: one-brick baseline
                        // transfer, merged into the live replica. ENG-49: after
                        // it lands, retry every transaction that was held
                        // pending this gap so no committed ops are lost, and
                        // forward any fresh repair requests those retries raise.
                        match receive_baseline_body(&conn).await {
                            Some(patch) => {
                                let (applied, retried) = {
                                    let mut guard =
                                        replica.lock().unwrap_or_else(|e| e.into_inner());
                                    if guard.apply_baseline_patch(&patch).is_ok() {
                                        (true, guard.retry_pending_repair_txns())
                                    } else {
                                        (false, Vec::new())
                                    }
                                };
                                if applied {
                                    counters.patches.fetch_add(1, Ordering::Relaxed);
                                }
                                for (_, outcome) in retried {
                                    match outcome {
                                        ApplyOutcome::Published { .. } => {
                                            counters.applied.fetch_add(1, Ordering::Relaxed);
                                        }
                                        ApplyOutcome::NeedsRepair(reqs) => {
                                            for req in reqs {
                                                let _ = conn
                                                    .send_record(WireRecord::RepairRequest(req))
                                                    .await;
                                                counters.repairs.fetch_add(1, Ordering::Relaxed);
                                            }
                                        }
                                        ApplyOutcome::Rejected { .. } => {
                                            counters.rejected.fetch_add(1, Ordering::Relaxed);
                                        }
                                        ApplyOutcome::Duplicate => {}
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                    Ok(Some(WireRecord::ActionStatus(st))) => {
                        if let ActionOutcome::Rejected { reason } = &st.outcome
                            && reason.starts_with("throttled")
                        {
                            let _ = throttle_tx.send(st.request_id.0);
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        })
    };

    // Retrier: re-send a throttled `ActionRequest` after a short back-off, at
    // most a few times per request, so a scripted gate action still lands under
    // an impaired transport that bunches retransmits into one server tick.
    let retrier = {
        let conn = conn.clone();
        let sent_actions = sent_actions.clone();
        let stop_rx = stop_rx.clone();
        tokio::spawn(async move {
            const MAX_ACTION_RETRIES: u8 = 4;
            while let Some(id) = throttle_rx.recv().await {
                if *stop_rx.borrow() {
                    break;
                }
                let record = {
                    let mut g = sent_actions.lock().unwrap_or_else(|e| e.into_inner());
                    match g.get_mut(&id) {
                        Some((rec, tries)) if *tries < MAX_ACTION_RETRIES => {
                            *tries += 1;
                            Some(rec.clone())
                        }
                        _ => None,
                    }
                };
                if let Some(rec) = record {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    let _ = conn.send_record(rec).await;
                }
            }
        })
    };

    // Datagram reader: motion snapshots.
    let motion = {
        let conn = conn.clone();
        let replica = replica.clone();
        let counters = counters.clone();
        let predictor = predictor.clone();
        tokio::spawn(async move {
            loop {
                match conn.recv_datagram().await {
                    Ok(Some(DatagramRecord::Motion(snap))) => {
                        counters
                            .last_tick
                            .fetch_max(snap.server_tick.get(), Ordering::Relaxed);
                        // T19: a snapshot for our own player entity reconciles
                        // the predictor rather than entering the body replica.
                        if let Some(pred) = &predictor {
                            let mut guard = pred.lock().unwrap_or_else(|e| e.into_inner());
                            let p: &mut Predictor = &mut guard;
                            if snap.body == p.entity {
                                let st = state_from_snapshot(&snap);
                                match &mut p.player {
                                    None => {
                                        p.player = Some(PredictedPlayer::new(p.params, st));
                                    }
                                    Some(pl) => pl.reconcile(&p.phys, st, snap.acked_input),
                                }
                                counters.motion.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        }

                        // A strictly-decreasing per-publisher `snapshot_seq`
                        // means this datagram overtook a newer one in transit.
                        let seq = snap.snapshot_seq.0;
                        let prev_hwm = counters.motion_seq_hwm.fetch_max(seq, Ordering::Relaxed);
                        if seq < prev_hwm {
                            counters.motion_reordered.fetch_add(1, Ordering::Relaxed);
                        }
                        let mut guard = replica.lock().unwrap_or_else(|e| e.into_inner());
                        guard.ingest_snapshot(&snap);
                        drop(guard);
                        counters.motion.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Some(DatagramRecord::Input(_))) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        })
    };

    // T19 mover: every ~16 ms sample the scripted input for the observed tick,
    // predict the capsule locally, and send an `InputFrame` datagram with up to
    // three recent copies for loss recovery. It also rebuilds the predicted
    // terrain collider (and rebases prediction) when the replica terrain hash
    // changes — a committed edit near the player.
    let mover = predictor.clone().map(|pred| {
        let conn = conn.clone();
        let replica = replica.clone();
        let counters = counters.clone();
        let script = config.movement_script.clone();
        let session = conn.session();
        let stop_rx = stop_rx.clone();
        tokio::spawn(async move {
            let end_tick = script_end_tick(&script);
            loop {
                if *stop_rx.borrow() {
                    return;
                }
                let tick = counters.last_tick.load(Ordering::Relaxed);

                // Rebuild the collider if the terrain changed near us.
                let terrain = {
                    let guard = replica.lock().unwrap_or_else(|e| e.into_inner());
                    guard.terrain_hash().zip(guard.terrain_volume().cloned())
                };
                // All predictor-lock work happens in this non-async block, which
                // returns the datagram to send once the guard is dropped.
                let frame: Option<InputFrame> = {
                    let mut guard = pred.lock().unwrap_or_else(|e| e.into_inner());
                    let p: &mut Predictor = &mut guard;
                    if let Some((hash, volume)) = terrain
                        && p.terrain_hash != Some(hash)
                    {
                        p.phys.set_terrain(&volume);
                        let first = p.terrain_hash.is_none();
                        p.terrain_hash = Some(hash);
                        if !first && let Some(pl) = &mut p.player {
                            pl.invalidate();
                        }
                    }

                    if p.player.is_some() && p.phys.has_terrain() {
                        let input = scripted_input(&script, tick);
                        p.input_seq += 1;
                        let seq = InputSeq(p.input_seq);
                        if let Some(pl) = &mut p.player {
                            pl.tick(&p.phys, input, seq, MOVEMENT_DT_S);
                        }

                        let recent: Vec<RecentInput> =
                            p.recent.iter().rev().take(3).copied().collect();
                        p.recent.push_back(RecentInput {
                            input_seq: seq,
                            movement: input.movement,
                            view_dir: input.view_dir,
                            buttons: input.buttons,
                        });
                        while p.recent.len() > 3 {
                            p.recent.pop_front();
                        }
                        Some(InputFrame {
                            session,
                            player: p.entity,
                            input_seq: seq,
                            intended_tick: Tick(tick + 1),
                            movement: input.movement,
                            view_dir: input.view_dir,
                            buttons: input.buttons,
                            recent,
                        })
                    } else {
                        None
                    }
                };
                if let Some(frame) = frame {
                    let _ = conn.send_datagram(frame.input_seq.0, &frame).await;
                }

                // Stop predicting a while after the script ends (the neutral
                // tail proves the player settles).
                if end_tick > 0 && tick > end_tick + 180 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(16)).await;
            }
        })
    });

    // Scripter: fire each action once the observed server tick reaches its
    // `at_tick`, or — since a client with no visible bodies sees no motion and
    // therefore no tick — once the equivalent wall-clock delay from connect has
    // elapsed (~16 ms per tick at 60 Hz).
    let scripter = {
        let conn = conn.clone();
        let counters = counters.clone();
        let replica = replica.clone();
        let sent_actions = sent_actions.clone();
        let mut script = config.script.clone();
        script.sort_by_key(|a| a.at_tick);
        let stop_rx = stop_rx.clone();
        let started = tokio::time::Instant::now();
        tokio::spawn(async move {
            for action in script {
                let deadline = started + Duration::from_millis(action.at_tick.max(1) * 16);
                loop {
                    if *stop_rx.borrow() {
                        return;
                    }
                    if counters.last_tick.load(Ordering::Relaxed) >= action.at_tick
                        || tokio::time::Instant::now() >= deadline
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }

                let mut request = action.request.clone();
                if action.target == ScriptTarget::DetachedBody {
                    // Aim at the sole detached body. It only exists once an
                    // earlier cut has detached it, so wait a bounded while for
                    // it to appear; skip the action if it never does.
                    let body_deadline = deadline + Duration::from_secs(3);
                    let target = loop {
                        if *stop_rx.borrow() {
                            return;
                        }
                        let found = {
                            let guard = replica.lock().unwrap_or_else(|e| e.into_inner());
                            guard
                                .body_ids()
                                .next()
                                .map(|e| (e, guard.body_solid_cells(e)))
                        };
                        if let Some((entity, cells)) = found {
                            break Some((entity, cells.unwrap_or(0)));
                        }
                        if tokio::time::Instant::now() >= body_deadline {
                            break None;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    };
                    let Some((entity, pre_cells)) = target else {
                        continue;
                    };
                    request.claimed_target = ClaimedTarget::Body(entity);
                    counters
                        .body_cut_entity_plus1
                        .store(entity.get() + 1, Ordering::Relaxed);
                    counters
                        .body_cut_pre_cells
                        .store(pre_cells, Ordering::Relaxed);
                }

                let request_id = request.request_id.0;
                let record = WireRecord::ActionRequest(request);
                if conn.send_record(record.clone()).await.is_ok() {
                    counters.actions.fetch_add(1, Ordering::Relaxed);
                    // Keep the exact record (a body cut carries its retargeted
                    // `claimed_target`) so the retrier can resend it verbatim.
                    sent_actions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(request_id, (record, 0));
                }
            }
        })
    };

    // Wait for the server to finish (control stream closes), or the observed
    // server tick to reach `run_ticks`, or the overall deadline.
    let done = {
        let counters = counters.clone();
        let run_ticks = config.run_ticks;
        async move {
            tokio::select! {
                _ = async { let _ = control.await; let _ = motion.await; } => {}
                _ = async {
                    loop {
                        if run_ticks > 0
                            && counters.last_tick.load(Ordering::Relaxed) >= run_ticks
                        {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }, if run_ticks > 0 => {}
            }
        }
    };
    let _ = tokio::time::timeout(config.overall_timeout, done).await;
    let _ = stop_tx.send(true);
    scripter.abort();
    if let Some(m) = mover {
        m.abort();
    }
    retrier.abort();
    liveness.abort();

    let _ = conn.say_bye("client complete").await;
    conn.close("client complete");
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;

    // T19 movement summary: a scripted player is at rest (grounded, low speed)
    // once its script has ended and the held-input timeout has fired.
    let movement = predictor.as_ref().and_then(|pred| {
        let p = pred.lock().unwrap_or_else(|e| e.into_inner());
        p.player.as_ref().map(|pl| {
            let s = pl.predicted();
            let at_rest =
                s.grounded && s.velocity_m_s[0].abs() < 0.5 && s.velocity_m_s[2].abs() < 0.5;
            pl.summary(at_rest)
        })
    });

    let guard = replica.lock().unwrap_or_else(|e| e.into_inner());
    let last_tick = counters.last_tick.load(Ordering::Relaxed);
    let applied = counters.applied.load(Ordering::Relaxed);
    let baseline_bricks = counters.baseline_bricks.load(Ordering::Relaxed);
    // A plain late joiner that catches up entirely from the baseline (no cuts
    // after it joined) still passes; a live client must have applied something,
    // and a scripted mover must have produced a movement summary with no hover.
    let movement_ok = match &movement {
        Some(m) => !m.hovered_after_floor_removal && m.ticks > 0,
        None => true,
    };
    let progressed = movement.is_some() && movement_ok
        || applied > 0
        || (config.late_join && baseline_bricks > 0);
    let max_body_displacement_m = guard.max_body_displacement_m();
    let body_cut_committed = match counters.body_cut_entity_plus1.load(Ordering::Relaxed) {
        0 => false,
        raw => {
            let entity = EntityId::new(raw - 1).expect("stored a valid entity id");
            let pre = counters.body_cut_pre_cells.load(Ordering::Relaxed);
            match guard.body_solid_cells(entity) {
                Some(post) => post < pre,
                None => pre > 0,
            }
        }
    };
    let summary = ClientSummary {
        version: 1,
        result: if progressed && movement_ok {
            "passed"
        } else {
            "failed"
        }
        .to_string(),
        connected: true,
        transactions_applied: applied,
        repair_requests_sent: counters.repairs.load(Ordering::Relaxed),
        transactions_rejected: counters.rejected.load(Ordering::Relaxed),
        motion_snapshots: counters.motion.load(Ordering::Relaxed),
        motion_snapshots_out_of_order: counters.motion_reordered.load(Ordering::Relaxed),
        actions_sent: counters.actions.load(Ordering::Relaxed),
        last_server_tick: last_tick,
        final_world_hash: guard.world_hash().to_string(),
        total_solid_cells: guard.total_solid_cells(),
        body_count: guard.body_ids().count(),
        late_join: config.late_join,
        baseline_bricks,
        repairs_applied: counters.patches.load(Ordering::Relaxed),
        max_body_displacement_m,
        body_cut_committed,
        movement,
    };
    drop(guard);

    log.write(&ProcessRecord::new(
        ProcessEvent::Stopped,
        ProcessRole::Client,
        Some(format!(
            "applied={}, motion={}, hash={}",
            summary.transactions_applied, summary.motion_snapshots, summary.final_world_hash
        )),
    ))?;
    if let Some(path) = &config.summary_json {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&summary).expect("serializable"),
        )?;
    }
    Ok(summary)
}

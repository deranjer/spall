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

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    JsonlError, JsonlLog, ProcessEvent, ProcessRecord, ProcessRole, SphereBrush, VolumeId,
};
use spall_net::{
    DatagramRecord, Fingerprint, JoinToken, TransportConfig, TransportError, WireRecord, connect,
};
use spall_protocol::{
    ActionKind, ActionRequest, AlgorithmVersions, ClaimedTarget, Handshake, Hash32, InputSeq,
    NegotiatedLimits, PROTOCOL_VERSION, RequestId,
};

use crate::replica::{ApplyOutcome, ReplicaConfig, ReplicaWorld};

/// Content-manifest tag; must match [`spall_server`]'s `T10_CONTENT_TAG`.
pub const T10_CONTENT_TAG: &[u8] = b"spall-t10-bridge-v1";
/// World id of the built-in T10 scene; must match the server.
pub const T10_WORLD_ID: u128 = 0x5A11_0000_0000_7010;
/// The T10 terrain volume id (`spall_sim` allocates volume 1 for terrain).
pub const TERRAIN_VOLUME: u64 = 1;

/// One scripted tool use.
#[derive(Debug, Clone)]
pub struct ScriptedAction {
    /// Fire once the observed server tick is at least this.
    pub at_tick: u64,
    pub request: ActionRequest,
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

/// Inputs to [`run_replication_client`].
#[derive(Debug, Clone)]
pub struct ClientNetConfig {
    /// Server (or UDP proxy) address to send packets to.
    pub connect_addr: SocketAddr,
    pub server_fingerprint: Fingerprint,
    pub join_token: JoinToken,
    pub script: Vec<ScriptedAction>,
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
    pub actions_sent: u64,
    pub last_server_tick: u64,
    pub final_world_hash: String,
    pub total_solid_cells: u64,
    pub body_count: usize,
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
    actions: AtomicU64,
    last_tick: AtomicU64,
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

    let replica = Arc::new(Mutex::new(ReplicaWorld::from_baseline(
        spall_voxel::fixtures::bridge_scene(VolumeId::new(TERRAIN_VOLUME).unwrap()),
        ReplicaConfig::default(),
    )));
    let counters = Arc::new(Counters::default());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

    let liveness = tokio::spawn(conn.clone().run_liveness(stop_rx.clone()));

    // Control reader: apply transactions, answer repair gaps.
    let control = {
        let conn = conn.clone();
        let replica = replica.clone();
        let counters = counters.clone();
        tokio::spawn(async move {
            loop {
                match conn.recv_record().await {
                    Ok(Some(WireRecord::TopologyTransaction(tx))) => {
                        counters
                            .last_tick
                            .fetch_max(tx.server_tick.get(), Ordering::Relaxed);
                        let outcome = {
                            let mut guard = replica.lock().unwrap_or_else(|e| e.into_inner());
                            guard.apply_transaction(&tx)
                        };
                        match outcome {
                            ApplyOutcome::Published { .. } => {
                                counters.applied.fetch_add(1, Ordering::Relaxed);
                            }
                            ApplyOutcome::NeedsRepair(reqs) => {
                                for req in reqs {
                                    let _ = conn.send_record(WireRecord::RepairRequest(req)).await;
                                    counters.repairs.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            ApplyOutcome::Rejected { .. } => {
                                counters.rejected.fetch_add(1, Ordering::Relaxed);
                            }
                            ApplyOutcome::Duplicate => {}
                        }
                    }
                    Ok(Some(WireRecord::ActionStatus(_))) => {}
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        })
    };

    // Datagram reader: motion snapshots.
    let motion = {
        let conn = conn.clone();
        let replica = replica.clone();
        let counters = counters.clone();
        tokio::spawn(async move {
            loop {
                match conn.recv_datagram().await {
                    Ok(Some(DatagramRecord::Motion(snap))) => {
                        counters
                            .last_tick
                            .fetch_max(snap.server_tick.get(), Ordering::Relaxed);
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

    // Scripter: fire each action once the observed server tick reaches its
    // `at_tick`, or — since a client with no visible bodies sees no motion and
    // therefore no tick — once the equivalent wall-clock delay from connect has
    // elapsed (~16 ms per tick at 60 Hz).
    let scripter = {
        let conn = conn.clone();
        let counters = counters.clone();
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
                if conn
                    .send_record(WireRecord::ActionRequest(action.request))
                    .await
                    .is_ok()
                {
                    counters.actions.fetch_add(1, Ordering::Relaxed);
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
    liveness.abort();

    let _ = conn.say_bye("client complete").await;
    conn.close("client complete");
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;

    let guard = replica.lock().unwrap_or_else(|e| e.into_inner());
    let last_tick = counters.last_tick.load(Ordering::Relaxed);
    let applied = counters.applied.load(Ordering::Relaxed);
    let summary = ClientSummary {
        version: 1,
        result: if applied > 0 { "passed" } else { "failed" }.to_string(),
        connected: true,
        transactions_applied: applied,
        repair_requests_sent: counters.repairs.load(Ordering::Relaxed),
        transactions_rejected: counters.rejected.load(Ordering::Relaxed),
        motion_snapshots: counters.motion.load(Ordering::Relaxed),
        actions_sent: counters.actions.load(Ordering::Relaxed),
        last_server_tick: last_tick,
        final_world_hash: guard.world_hash().to_string(),
        total_solid_cells: guard.total_solid_cells(),
        body_count: guard.body_ids().count(),
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

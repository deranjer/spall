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
    PROTOCOL_VERSION, RecentInput, RepairKey, RequestId, TransferId, session_player_entity,
};

use crate::interactive::{InteractiveSession, InteractiveView};
use crate::predict::{ClientPhysics, PlayerMovementSummary, PredictedPlayer, WindowStats};
use crate::replica::{ApplyOutcome, ReplicaConfig, ReplicaWorld};
use crate::residency::ClientResidencyPass;

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
    /// [`spall_voxel::fixtures::checkerboard_split_scene`] — a fragmented block
    /// whose detach overflows the inline `CellRun` budget (T17 / ENG-64).
    CheckerboardSplit,
    /// [`spall_voxel::fixtures::bulk_split_scene`] — a block whose detach
    /// overflows even the inline op-blob cap (T17 increment 2 / ENG-64).
    BulkSplit,
    /// [`spall_voxel::fixtures::separated_regions_scene`] — two independent
    /// collapsible structures in one bounded world (T23 / G3). Also the
    /// terrain for the T23 / G4 `g4-workload` scene: the workload's debris
    /// bodies are added to the server's `SimWorld` after construction, so a
    /// live (non-late-join) replica's baseline is terrain-only — it never
    /// carries the pre-existing bodies. See `docs/reports/G3.md`.
    SeparatedRegions,
    /// Full-envelope separated regions joined by a causeway (T23 / G3 row 2).
    SeparatedRegionsFar,
    /// [`spall_voxel::fixtures::walk_arena`] — the flat 30 m movement lane
    /// (T19 / T23 row 8b). A stationary client on this scene installs it as a
    /// fixed baseline; a mover pulls it over a transfer.
    Walk,
    /// [`spall_voxel::fixtures::g1_full_envelope_scene`] — the full G1 gate
    /// envelope (T11a / ENG-62). Terrain-only: the "moving hollow test
    /// volume" body is added to the server's `SimWorld` after construction,
    /// same as `g4-workload`'s debris — a live replica's baseline never
    /// carries it.
    G1FullEnvelope,
}

impl BaselineScene {
    /// Parses the harness `--scene` value; `None` for an unknown name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "bridge-cut" | "bridgecut" | "bridge" => Some(Self::BridgeCut),
            "cross-bridge-cut" | "cross-brick-bridge" | "crossbridgecut" => {
                Some(Self::CrossBridgeCut)
            }
            "checkerboard-split" | "oversized-split" => Some(Self::CheckerboardSplit),
            "bulk-split" | "giant-split" => Some(Self::BulkSplit),
            "separated-regions" | "t23-g3" | "g3" => Some(Self::SeparatedRegions),
            "g4-workload" | "t23-g4" | "g4" => Some(Self::SeparatedRegions),
            "separated-regions-far" | "t23-g3-full-envelope" | "g3-far" => {
                Some(Self::SeparatedRegionsFar)
            }
            "walk" | "walk-arena" | "player-movement" => Some(Self::Walk),
            "g1-full-envelope" | "g1-full-workload" | "g1" => Some(Self::G1FullEnvelope),
            _ => None,
        }
    }

    fn baseline(self, id: VolumeId) -> spall_voxel::Volume {
        match self {
            Self::BridgeCut => spall_voxel::fixtures::bridge_scene(id),
            Self::CrossBridgeCut => spall_voxel::fixtures::cross_brick_bridge_scene(id),
            Self::CheckerboardSplit => spall_voxel::fixtures::checkerboard_split_scene(id),
            Self::BulkSplit => spall_voxel::fixtures::bulk_split_scene(id),
            Self::SeparatedRegions => spall_voxel::fixtures::separated_regions_scene(id),
            Self::SeparatedRegionsFar => {
                spall_voxel::fixtures::separated_regions_full_envelope_scene(id)
            }
            Self::Walk => spall_voxel::fixtures::walk_arena(id),
            Self::G1FullEnvelope => spall_voxel::fixtures::g1_full_envelope_scene(id),
        }
    }
}

/// Inputs to [`run_replication_client`].
#[derive(Clone)]
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
    /// T23 / G3 row 7 slice E2: client-side terrain residency. `None` (the
    /// default) keeps the replica fully resident — every prior run is
    /// byte-unchanged. `Some` runs [`ClientResidencyPass`] in the mover loop:
    /// it evicts terrain outside a brick box around the predicted player
    /// capsule and pulls bricks back with `RepairRequest`s as the player
    /// returns. Needs a `movement_script` (no mover, no pass).
    pub client_residency: Option<ClientResidencyLimits>,
    /// T11a / ENG-62 increment 3: called once, right after the replica's
    /// initial baseline is installed (late-join) or the fixed scene is set
    /// (a live client), with a shared handle to the live
    /// [`crate::replica::ReplicaWorld`]. Lets a caller (e.g. a graphical
    /// capture harness) poll the replica's real, network-replicated state on
    /// its own schedule — independent of this client's own script/receive
    /// loop — instead of driving an authoritative [`spall_sim::Simulation`]
    /// directly. `None` (the default) changes nothing about the client's
    /// behaviour.
    pub on_replica_ready: Option<ReplicaReadyHook>,
    /// Interactive follow-up (T19): live keyboard/mouse-driven input instead
    /// of `movement_script` — set by `spall_client::window::run_interactive_window`,
    /// not normally constructed directly. Implies a baseline pull and a
    /// predicted player exactly like a non-empty `movement_script`, but reads
    /// `InteractiveSession::input` every mover tick instead of the scripted
    /// table, publishes the predicted pose into `InteractiveSession::view`,
    /// and never auto-stops on a script end tick. `None` (every existing
    /// scripted/headless run) is byte-for-byte unchanged.
    pub interactive: Option<Arc<InteractiveSession>>,
}

/// A shared handle to the live replica, and a callback invoked with it — see
/// [`ClientNetConfig::on_replica_ready`].
pub type ReplicaReadyHook = Arc<dyn Fn(Arc<Mutex<ReplicaWorld>>) + Send + Sync>;

/// Client terrain-residency limits (slice E2).
#[derive(Debug, Clone, Copy)]
pub struct ClientResidencyLimits {
    /// Resident-terrain-brick ceiling; the pass never forces it below the
    /// interest box.
    pub budget_bricks: usize,
    /// Chebyshev brick radius kept resident around the predicted player.
    pub interest_radius_bricks: i64,
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
    /// T23 / G3 row 7 slice E2: terrain bricks this client evicted around its
    /// predicted player, and reload `RepairRequest`s it sent as the player
    /// returned. Both `0` unless `client_residency` was set.
    #[serde(default)]
    pub client_residency_evictions: u64,
    #[serde(default)]
    pub client_residency_reloads_requested: u64,
    #[serde(default)]
    pub client_residency_reloads_completed: u64,
    #[serde(default)]
    pub client_residency_budget_miss_steps: u64,
    /// Transactions that first gapped on a brick this client had evicted.
    #[serde(default)]
    pub client_residency_evicted_transaction_gaps: u64,
    /// T23 / G3 row 11 join budget: compressed bytes of the installed
    /// late-join / full-baseline transfer (`BaselineBegin.total_bytes`, the
    /// same bytes shipped on the wire). `0` unless a baseline was pulled.
    #[serde(default)]
    pub late_join_baseline_compressed_bytes: u64,
    /// Wall-clock milliseconds from connect (start of the QUIC handshake) to
    /// the baseline being received, decompressed, decoded, verified, and
    /// installed. `0` unless a baseline was pulled.
    #[serde(default)]
    pub late_join_baseline_install_ms: u64,
    /// Wall-clock milliseconds from connect to "ready": baseline installed
    /// and, if it carried any bodies, the first post-baseline motion keyframe
    /// observed (`docs/protocol.md` late-join step 4 — the catch-up queue has
    /// drained and the client is promoted to live replication). `0` unless a
    /// baseline was pulled.
    #[serde(default)]
    pub late_join_ready_ms: u64,
    /// Whether `late_join_ready_ms` reflects an actually-observed motion
    /// keyframe (`true`) rather than a bounded wait that timed out without
    /// one (`false`). Always `true` when the baseline carried no bodies, and
    /// meaningless (`false`) when no baseline was pulled.
    #[serde(default)]
    pub late_join_ready_confirmed: bool,
    /// A mid-session `BaselineBegin` (hash-repair patch or split-bulk
    /// transfer) whose body failed to arrive intact — dropped rather than
    /// treated as fatal (see the control reader). Non-zero under loss is
    /// expected; what matters is that reloads still complete (see
    /// `client_residency_reloads_completed`) despite it.
    #[serde(default)]
    pub baseline_transfer_failures: u64,
    /// A sent `ActionRequest` the server declined to admit or stage
    /// (`ActionOutcome::Rejected`) — the scripted-action retrier only retries
    /// a `"throttled"` reason, so anything else is a lost scripted action.
    /// `0` unless the server actually rejected one.
    #[serde(default)]
    pub action_requests_rejected: u64,
    /// The distinct `ActionOutcome::Rejected` reasons observed, most recent
    /// last (bounded — see `Counters::action_reject_reasons`). Diagnostic:
    /// tells you *why* `action_requests_rejected` is non-zero without a
    /// separate server-side log.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub action_reject_reasons: Vec<String>,
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
    /// Giant bulk-split `BaselineWorld`s received and applied mid-session
    /// (T17 increment 2).
    bulk_splits: AtomicU64,
    /// Resident bricks in the installed late-join baseline (T17).
    baseline_bricks: AtomicU64,
    /// Entity id (+1, so `0` means "never fired") a `DetachedBody` scripted cut
    /// was aimed at, and that body's solid-cell count captured just before the
    /// cut was sent. Together they let the run confirm the body cut committed.
    body_cut_entity_plus1: AtomicU64,
    body_cut_pre_cells: AtomicU64,
    /// T23 / G3 row 7 slice E2: terrain bricks this replica evicted around the
    /// predicted player, and `RepairRequest`s it sent to pull evicted bricks
    /// back as the player returned. Both `0` unless `client_residency` is set.
    residency_evictions: AtomicU64,
    residency_reloads_requested: AtomicU64,
    residency_reloads_completed: AtomicU64,
    residency_budget_miss_steps: AtomicU64,
    residency_evicted_transaction_gaps: AtomicU64,
    /// T23 / G3 row 11: compressed bytes of the installed late-join baseline
    /// transfer (`BaselineBegin.total_bytes`, the same bytes shipped on the
    /// wire) and wall-clock milliseconds from connect to baseline-installed /
    /// to "ready". All `0` unless `late_join` is set.
    late_join_baseline_compressed_bytes: AtomicU64,
    late_join_baseline_install_ms: AtomicU64,
    late_join_ready_ms: AtomicU64,
    /// `1` once the installed baseline is confirmed caught up: either it
    /// carried no bodies (nothing to wait for) or the first post-baseline
    /// motion keyframe (`docs/protocol.md` late-join step 4) was actually
    /// observed rather than the bounded wait timing out.
    late_join_ready_confirmed: AtomicU64,
    /// `1` when the installed baseline carries at least one body volume, so
    /// "ready" waits for a motion keyframe before it is declared.
    late_join_has_bodies: AtomicU64,
    /// A sent `ActionRequest` that came back `ActionOutcome::Rejected` for any
    /// reason (the retrier only resends a `"throttled"` one).
    action_rejected: AtomicU64,
    /// Bounded log of `action_rejected` reasons, most recent last (see
    /// `MAX_RECORDED_ACTION_REJECT_REASONS`). Diagnostic only — never read to
    /// drive behavior.
    action_reject_reasons: std::sync::Mutex<Vec<String>>,
    /// A `BaselineBegin` (a mid-session hash-repair patch, or split-bulk
    /// transfer) whose body failed to arrive intact — the bulk stream, the
    /// decode, or the `BaselineEnd` hash check. Under loss this is expected
    /// occasionally; the requester (residency pass or gapped-transaction
    /// retry) already re-requests on its own schedule, so this is dropped and
    /// counted rather than treated as fatal.
    baseline_transfer_failures: AtomicU64,
}

/// Cap on `Counters::action_reject_reasons` — a diagnostic log, not something
/// that should grow unbounded if a script somehow floods rejections.
const MAX_RECORDED_ACTION_REJECT_REASONS: usize = 8;

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
    /// The server tick observed on the *first* authoritative snapshot for this
    /// player (when `player` is first created). Movement-script windows are
    /// authored relative to "ticks since this client's player went live", not
    /// the server's absolute tick — under a fast/clean join the two coincide,
    /// but an impaired transport can stretch the join handshake (baseline
    /// transfer, its confirming ack, and the resulting keyframe snapshot — each
    /// its own round trip) well past the tick the script expects to start at.
    /// Anchoring on this snapshot rather than on when the baseline *transfer*
    /// finished is what actually matters: the player has no authoritative state
    /// (and the mover sends nothing) until it arrives, so it is the true
    /// "tick zero" for the script.
    script_origin_tick: Option<u64>,
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
            script_origin_tick: None,
        }
    }
}

/// One mover tick's snapshot of `PredictedPlayer`'s correction counters,
/// published into `InteractiveView` for the interactive HUD (see ENG-69's
/// round-6/7 investigation into corrections firing even while standing
/// still) — a named struct rather than growing `MoverTickOutcome`'s tuple
/// past readability.
#[derive(Debug, Clone, Copy, Default)]
struct CorrectionStats {
    corrections: u64,
    max_correction_m: f64,
    idle_corrections: u64,
    max_idle_correction_m: f64,
    max_vertical_correction_m: f64,
    max_horizontal_correction_m: f64,
}

/// Receives one baseline transfer whose `BaselineBegin` has already been read:
/// accepts the bulk stream, reassembles + decodes the payload, then consumes
/// records until `BaselineEnd`. Returns the decoded world, or `None` on any
/// transport / decode failure.
///
/// T23 / G4: the reassembled bytes are the server's zstd-compressed payload
/// (`spall_server::baseline::transfer_from_world`), so this decompresses
/// before decoding — `end.assembled_hash` below is still checked against the
/// canonical **uncompressed** re-encoding, independent of the compressor.
async fn receive_baseline_body(conn: &Connection) -> Option<BaselineWorld> {
    let parts = conn.accept_bulk().await.ok()?.collect_parts().await.ok()?;
    let mut bytes = Vec::new();
    for part in &parts {
        bytes.extend_from_slice(&part.payload);
    }
    let world = BaselineWorld::decode_compressed(&bytes).ok()?;
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

/// Forwards one [`ApplyOutcome`] from the control reader: counts a publish,
/// sends each `RepairRequest` of a `NeedsRepair`, counts a rejection. A
/// `Duplicate` or an `AwaitingBulkSplit` hold needs nothing — the blob's
/// `BaselineBegin` will retry it.
async fn forward_outcome(conn: &Connection, counters: &Counters, outcome: ApplyOutcome) {
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
        ApplyOutcome::Duplicate | ApplyOutcome::AwaitingBulkSplit { .. } => {}
    }
}

/// The initial late-join handshake: ask for a baseline, install it, confirm it.
/// `connect_at` is the wall-clock reference point ("late-join connect") the
/// T23 / G3 row 11 join-budget timings are measured from.
async fn perform_late_join(
    conn: &Connection,
    replica: &Mutex<ReplicaWorld>,
    counters: &Counters,
    connect_at: std::time::Instant,
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
    // T23 / G3 row 11: the wire bytes actually shipped (compressed) and the
    // wall-clock cost of getting the baseline received + decompressed +
    // decoded + verified, before the (comparatively cheap) local install.
    counters
        .late_join_baseline_compressed_bytes
        .store(begin.total_bytes, Ordering::Relaxed);
    counters
        .late_join_baseline_install_ms
        .store(connect_at.elapsed().as_millis() as u64, Ordering::Relaxed);
    counters
        .late_join_has_bodies
        .store(u64::from(world.volumes.len() > 1), Ordering::Relaxed);

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
    // T23 / G3 row 11: the join-budget wall-clock reference point ("late-join
    // connect"). Deliberately taken before the QUIC handshake — under the
    // imposed network profile that handshake is itself part of the cost a
    // late-joining player actually experiences.
    let session_start = std::time::Instant::now();
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
    // scene the server runs (T19 uses the `walk` arena). An interactive
    // client always predicts a player too, exactly like a non-empty
    // `movement_script`.
    let want_baseline =
        config.late_join || !config.movement_script.is_empty() || config.interactive.is_some();

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

    // The window reads live terrain straight off the replica for its debug
    // draw; publish the handle once, up front, rather than threading it
    // through every later closure.
    if let Some(session) = &config.interactive {
        let _ = session.replica.set(replica.clone());
    }

    // T17: pull a full baseline over a bulk transfer before touching the
    // replication stream, so the replica starts at the server's current
    // topology with no edit replay from world creation.
    if want_baseline {
        if let Err(e) = perform_late_join(&conn, &replica, &counters, session_start).await {
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

    // T11a / ENG-62 increment 3: hand the caller the live replica now that its
    // initial state is installed. The hook runs synchronously on this task but
    // must not block — it is expected to spawn its own thread/task and return.
    if let Some(hook) = &config.on_replica_ready {
        hook(replica.clone());
    }

    // T19: a scripted-movement (or interactively-played) client predicts its
    // own player capsule.
    let predictor =
        (!config.movement_script.is_empty() || config.interactive.is_some()).then(|| {
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
                            if let ApplyOutcome::NeedsRepair(reqs) = &primary
                                && reqs.iter().any(|req| match req.key {
                                    RepairKey::Brick { volume, coord } => {
                                        guard.evicted(volume).contains(coord)
                                    }
                                    _ => false,
                                })
                            {
                                counters
                                    .residency_evicted_transaction_gaps
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            let publish = matches!(primary, ApplyOutcome::Published { .. });
                            outcomes.push(primary);
                            if publish {
                                for (_, o) in guard.retry_pending_repair_txns() {
                                    outcomes.push(o);
                                }
                            }
                        }
                        for outcome in outcomes {
                            forward_outcome(&conn, &counters, outcome).await;
                        }
                    }
                    Ok(Some(WireRecord::BaselineBegin(begin))) => {
                        // T17 increment 2: a `transfer_id` with the reserved high
                        // bit is a giant bulk split's `BaselineWorld` — hand it
                        // to the replica and retry the marker transaction held on
                        // it. Otherwise it is a mid-session hash-repair patch
                        // (one-brick baseline), merged into the live replica,
                        // after which every `before`-gapped held transaction is
                        // retried (ENG-49).
                        let is_split =
                            begin.transfer_id.0 & spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT != 0;
                        // A `None` here is *this one transfer* failing to
                        // arrive intact (the bulk stream, the decode, or the
                        // `BaselineEnd` hash check) — expected occasionally
                        // under loss, and not fatal to the connection: the
                        // requester (the residency pass's cooldown, or a
                        // gapped transaction's own retry) re-requests on its
                        // own schedule regardless. Tearing down the whole
                        // control-record loop over one failed transfer used
                        // to leave every *later* repair response unread too
                        // — including ones for a request sent well after
                        // this failure — turning one dropped patch into a
                        // permanently stuck reload.
                        match receive_baseline_body(&conn).await {
                            Some(world) if is_split => {
                                let retried = {
                                    let mut guard =
                                        replica.lock().unwrap_or_else(|e| e.into_inner());
                                    guard.provide_bulk_split_world(begin.transfer_id.0, world)
                                };
                                counters.bulk_splits.fetch_add(1, Ordering::Relaxed);
                                for (_, outcome) in retried {
                                    forward_outcome(&conn, &counters, outcome).await;
                                }
                            }
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
                                    forward_outcome(&conn, &counters, outcome).await;
                                }
                            }
                            None => {
                                counters
                                    .baseline_transfer_failures
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Ok(Some(WireRecord::ActionStatus(st))) => {
                        if let ActionOutcome::Rejected { reason } = &st.outcome {
                            if reason.starts_with("throttled") {
                                let _ = throttle_tx.send(st.request_id.0);
                            } else {
                                // Anything other than "throttled" is not
                                // retried (see the retrier below) — record it
                                // so a lost scripted action is visible in the
                                // summary instead of silently vanishing.
                                counters.action_rejected.fetch_add(1, Ordering::Relaxed);
                                let mut reasons = counters
                                    .action_reject_reasons
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner());
                                if reasons.len() >= MAX_RECORDED_ACTION_REJECT_REASONS {
                                    reasons.remove(0);
                                }
                                reasons.push(reason.clone());
                            }
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
        let interactive = config.interactive.clone();
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
                            // Locked (and dropped) before `pred`'s own lock below,
                            // matching the mover loop's lock ordering
                            // (`replica` then `pred`) to avoid a cross-task
                            // deadlock risk.
                            let terrain_volume = {
                                let guard = replica.lock().unwrap_or_else(|e| e.into_inner());
                                guard.terrain_volume().cloned()
                            };
                            let mut guard = pred.lock().unwrap_or_else(|e| e.into_inner());
                            let p: &mut Predictor = &mut guard;
                            if snap.body == p.entity {
                                let st = state_from_snapshot(&snap);
                                match &mut p.player {
                                    None => {
                                        p.player = Some(PredictedPlayer::new(p.params, st));
                                        p.script_origin_tick.get_or_insert(snap.server_tick.get());
                                    }
                                    // The live HUD path only needs `PredictedPlayer`'s own
                                    // running counters, read separately below; the returned
                                    // per-event `CorrectionEvent` instead goes to
                                    // `CorrectionLog` (ENG-69 round 10/11 — see its own doc)
                                    // when this is an interactive session, for post-hoc
                                    // analysis of a real hands-on run.
                                    //
                                    // `terrain_volume` is `None` only before the replica
                                    // has any terrain object at all — skipping
                                    // reconciliation this one time is the same as any
                                    // other not-ready tick, not a hard failure.
                                    Some(pl) => {
                                        if let Some(volume) = &terrain_volume
                                            && let Some(event) = pl.reconcile(
                                                &mut p.phys,
                                                volume,
                                                st,
                                                snap.acked_input,
                                            )
                                            && let Some(session) = &interactive
                                            && let Some(log) = &session.corrections
                                        {
                                            log.record(snap.server_tick.get(), event);
                                        }
                                    }
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

    // T23 / G3 row 11: for a late joiner, wait (bounded) for the "ready"
    // signal `docs/protocol.md` late-join step 4 describes — the current
    // motion keyframe the server sends once the catch-up queue has drained
    // and the client is promoted to live replication. A baseline with no
    // bodies has nothing to wait for and is ready immediately. This bounds
    // the extra wait at a few RTTs even under the imposed loss/latency
    // profile (a dropped keyframe is followed by the next 20 Hz batch).
    if config.late_join {
        let expects_motion = counters.late_join_has_bodies.load(Ordering::Relaxed) != 0;
        let mut confirmed = !expects_motion;
        if expects_motion {
            const READY_POLL: Duration = Duration::from_millis(10);
            const READY_TIMEOUT: Duration = Duration::from_secs(5);
            let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
            loop {
                if counters.motion.load(Ordering::Relaxed) > 0 {
                    confirmed = true;
                    break;
                }
                if tokio::time::Instant::now() >= deadline || *stop_rx.borrow() {
                    break;
                }
                tokio::time::sleep(READY_POLL).await;
            }
        }
        counters.late_join_ready_ms.store(
            session_start.elapsed().as_millis() as u64,
            Ordering::Relaxed,
        );
        counters
            .late_join_ready_confirmed
            .store(u64::from(confirmed), Ordering::Relaxed);
    }

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
        let client_residency = config.client_residency;
        let interactive = config.interactive.clone();
        tokio::spawn(async move {
            let end_tick = script_end_tick(&script);
            // Slice E2: a scripted mover optionally evicts terrain outside a
            // brick box around its predicted player and pulls it back with
            // `RepairRequest`s as the player returns.
            let mut residency = client_residency
                .map(|l| ClientResidencyPass::new(l.budget_bricks, l.interest_radius_bricks));
            // A residency gap deliberately holds prediction over unknown
            // ground.  Script legs describe controlled movement, so advancing
            // their clock during that hold would consume the outbound leg
            // without ever sending an input frame.
            let mut active_script_tick = 0_u64;
            let mut last_active_server_tick = None;
            loop {
                if *stop_rx.borrow() {
                    return;
                }
                let tick = counters.last_tick.load(Ordering::Relaxed);

                // Rebuild the collider if the terrain changed near us. Kept as
                // two separate bindings, not one `Option<(Hash32, Volume)>`
                // (as before ENG-69 round 18): `terrain_volume` needs to stay
                // borrowable both for the dirty-check block below *and* for
                // every `pl.tick` call afterward, which now also needs a
                // fresh `&Volume` each tick for its own window cache.
                let (terrain_hash, terrain_volume) = {
                    let guard = replica.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        guard.terrain_resident_hash(),
                        guard.terrain_volume().cloned(),
                    )
                };
                // All predictor-lock work happens in this non-async block, which
                // returns the datagram to send (and the predicted feet position
                // for the residency pass) once the guard is dropped.
                type MoverTickOutcome = (
                    Option<InputFrame>,
                    Option<[f64; 3]>,
                    Option<u64>,
                    Option<CharacterState>,
                    Option<CorrectionStats>,
                    WindowStats,
                );
                let (frame, feet, script_tick, predicted_state, correction_stats, window_stats): MoverTickOutcome = {
                    let mut guard = pred.lock().unwrap_or_else(|e| e.into_inner());
                    let p: &mut Predictor = &mut guard;
                    if let (Some(hash), Some(volume)) = (terrain_hash, &terrain_volume)
                        && p.terrain_hash != Some(hash)
                    {
                        p.phys.set_terrain(volume);
                        let first = p.terrain_hash.is_none();
                        p.terrain_hash = Some(hash);
                        if !first && let Some(pl) = &mut p.player {
                            pl.invalidate();
                        }
                    }

                    // Beyond simply having *a* collider, prediction needs it to
                    // actually cover where the player is standing
                    // (`ClientPhysics::covers`): the collider body survives a
                    // residency gap (`set_terrain` keeps it, empty, for a later
                    // refill), so `has_terrain` alone stays true while a reload
                    // a lossy repair round trip hasn't delivered yet leaves the
                    // player over unknown ground. Predicting through that as if
                    // it were confirmed air is exactly what turns one delayed
                    // reload into an unbounded free-fall; holding here instead
                    // means the predicted tick simply resumes once the brick
                    // lands, the same way it already pauses before the player
                    // exists.
                    let ready = p
                        .player
                        .as_ref()
                        .is_some_and(|pl| p.phys.covers(pl.predicted().position_m));

                    // Start only once the player is live, then advance only
                    // when an input is actually predicted and sent.  This
                    // pauses an authored leg through a known residency hold.
                    let script_tick = p.script_origin_tick.map(|_| active_script_tick);

                    let frame = if ready && p.phys.has_terrain() {
                        let input = match &interactive {
                            Some(session) => session.input.snapshot(),
                            None => scripted_input(&script, script_tick.unwrap_or(0)),
                        };
                        p.input_seq += 1;
                        let seq = InputSeq(p.input_seq);
                        if let Some(pl) = &mut p.player
                            && let Some(volume) = &terrain_volume
                        {
                            pl.tick(&mut p.phys, volume, input, seq, MOVEMENT_DT_S);
                        }
                        // Preserve the script's server-tick cadence.  The
                        // mover itself samples more often than snapshots can
                        // advance, so incrementing once per loop makes an
                        // impaired client cover several scripted ticks per
                        // authoritative tick.  Resetting this reference while
                        // held intentionally discards the unknown-ground gap.
                        let prior = last_active_server_tick.replace(tick);
                        if let Some(prior) = prior {
                            active_script_tick =
                                active_script_tick.saturating_add(tick.saturating_sub(prior));
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
                        last_active_server_tick = None;
                        None
                    };
                    let predicted_state = p.player.as_ref().map(PredictedPlayer::predicted);
                    let feet = predicted_state.map(|st| st.position_m);
                    let correction_stats = p.player.as_ref().map(|pl| CorrectionStats {
                        corrections: pl.corrections,
                        max_correction_m: pl.max_correction_m,
                        idle_corrections: pl.idle_corrections,
                        max_idle_correction_m: pl.max_idle_correction_m,
                        max_vertical_correction_m: pl.max_vertical_correction_m,
                        max_horizontal_correction_m: pl.max_horizontal_correction_m,
                    });
                    let window_stats = p.phys.window_stats();
                    (
                        frame,
                        feet,
                        script_tick,
                        predicted_state,
                        correction_stats,
                        window_stats,
                    )
                };
                if let Some(frame) = frame {
                    let _ = conn.send_datagram(frame.input_seq.0, &frame).await;
                }
                if let (Some(session), Some(predicted)) = (&interactive, predicted_state) {
                    let stats = correction_stats.unwrap_or_default();
                    *session.view.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(InteractiveView {
                            predicted,
                            server_tick: tick,
                            published_at: std::time::Instant::now(),
                            corrections: stats.corrections,
                            max_correction_m: stats.max_correction_m,
                            idle_corrections: stats.idle_corrections,
                            max_idle_correction_m: stats.max_idle_correction_m,
                            max_vertical_correction_m: stats.max_vertical_correction_m,
                            max_horizontal_correction_m: stats.max_horizontal_correction_m,
                            window_stats,
                        });
                }

                // Slice E2: evict / request-reload terrain around the player.
                if let (Some(pass), Some(feet)) = (residency.as_mut(), feet) {
                    let reqs = {
                        let mut guard = replica.lock().unwrap_or_else(|e| e.into_inner());
                        pass.step(&mut guard, feet)
                    };
                    counters
                        .residency_evictions
                        .store(pass.evictions_total(), Ordering::Relaxed);
                    counters
                        .residency_reloads_requested
                        .store(pass.reloads_requested_total(), Ordering::Relaxed);
                    counters
                        .residency_reloads_completed
                        .store(pass.reloads_completed_total(), Ordering::Relaxed);
                    counters
                        .residency_budget_miss_steps
                        .store(pass.budget_miss_steps_total(), Ordering::Relaxed);
                    for req in reqs {
                        let _ = conn.send_record(WireRecord::RepairRequest(req)).await;
                    }
                }

                // Stop predicting a while after the script ends (the neutral
                // tail proves the player settles). `script_tick` advances only
                // while controlled input is sent, so an unknown-ground hold
                // cannot silently consume this neutral tail.
                if end_tick > 0 && script_tick.is_some_and(|t| t > end_tick + 360) {
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
        let interactive = config.interactive.clone();
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
                // The window's close handler sets this so an interactive
                // session disconnects promptly instead of riding out
                // `overall_timeout`.
                _ = async {
                    loop {
                        if interactive.as_ref().is_some_and(|s| s.stop.load(Ordering::Relaxed)) {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }, if interactive.is_some() => {}
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
    // once its script has ended and the held-input timeout has fired. Checked
    // against the *authoritative* state, not the predicted one: prediction
    // only advances while `ClientPhysics::covers` holds (see the mover loop),
    // so it can still be legitimately frozen mid-settle if the run ends
    // during a stall waiting on a lossy repair round trip near the very end
    // of the script — the authoritative state keeps updating from every
    // snapshot regardless, so it is the one that actually answers "has the
    // real player come to rest".
    let movement = predictor.as_ref().and_then(|pred| {
        let p = pred.lock().unwrap_or_else(|e| e.into_inner());
        p.player.as_ref().map(|pl| {
            let s = pl.authoritative();
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
        version: 3,
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
        client_residency_evictions: counters.residency_evictions.load(Ordering::Relaxed),
        client_residency_reloads_requested: counters
            .residency_reloads_requested
            .load(Ordering::Relaxed),
        client_residency_reloads_completed: counters
            .residency_reloads_completed
            .load(Ordering::Relaxed),
        action_requests_rejected: counters.action_rejected.load(Ordering::Relaxed),
        action_reject_reasons: counters
            .action_reject_reasons
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        client_residency_budget_miss_steps: counters
            .residency_budget_miss_steps
            .load(Ordering::Relaxed),
        client_residency_evicted_transaction_gaps: counters
            .residency_evicted_transaction_gaps
            .load(Ordering::Relaxed),
        late_join_baseline_compressed_bytes: counters
            .late_join_baseline_compressed_bytes
            .load(Ordering::Relaxed),
        late_join_baseline_install_ms: counters
            .late_join_baseline_install_ms
            .load(Ordering::Relaxed),
        late_join_ready_ms: counters.late_join_ready_ms.load(Ordering::Relaxed),
        late_join_ready_confirmed: counters.late_join_ready_confirmed.load(Ordering::Relaxed) != 0,
        baseline_transfer_failures: counters.baseline_transfer_failures.load(Ordering::Relaxed),
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

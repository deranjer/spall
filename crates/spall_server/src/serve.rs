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

use crate::replication::{BodyDigest, ClientReplication, InterestSet, MotionBudget};
use glam::DVec3;
use serde::Serialize;
use spall_core::{
    BRUSH_UNIT, BrushPoint, EntityId, GlobalCell, JournalSeq, JsonlError, JsonlLog, ProcessEvent,
    ProcessRecord, ProcessRole, SphereBrush,
};
use spall_net::{
    Connection, DatagramRecord, DevIdentity, JoinToken, NetServer, Role, TransportConfig,
    TransportError, WireRecord,
};
use spall_physics::PhysicsConfig;
use spall_protocol::{
    ActionKind, ActionOutcome, ActionRequest, ActionStatus, AlgorithmVersions, BaselineAck,
    ClaimedTarget, Handshake, Hash32, InputFrame, InterestEpoch, MotionSnapshot, NegotiatedLimits,
    PROTOCOL_VERSION, RepairRequest, RequestId, SessionId, SlotId, TopologyTransaction, TransferId,
    frame_input, recent_input, session_player_entity,
};
use spall_sim::fixtures::{
    G4_WORKLOAD_SPAWNS, SEPARATED_REGION_FAR_SPAWNS, SEPARATED_REGION_SPAWNS, WALK_ARENA_SPAWNS,
};
use spall_sim::{
    Body, EditIntent, EditKind, EditTarget, MotionPublisher, SimWorld, Simulation,
    SimulationConfig, action_statuses, fixtures,
};
use spall_store::Writer;
use spall_structure::AnchorPlane;
use spall_voxel::{Ray, RayOutcome, cast_ray_world};
use tokio::sync::{Notify, mpsc, watch};

use crate::baseline::{self, BaselineTransfer};
use crate::commit_latency::{self, CommitLatency};
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

// --- ENG-48: minimum safe replication-host queue bounds ----------------------
//
// The full per-connection bandwidth / interest budget is T20. These caps only
// stop a flooding client from stalling the tick loop or a stalled reliable
// reader from growing host memory without bound. Quinn flow control bounds the
// wire, not these application allocations.

/// Depth of the shared inbound bridge channel (records buffered between the
/// connection readers and the sim loop). Past this a reader's `try_send` drops
/// the surplus `ActionRequest` / `RepairRequest` (both are client-retryable and
/// rate-limited) so host memory stays flat under a flood.
pub const INBOUND_CHANNEL_CAP: usize = 4096;

/// Most inbound bridge records the sim loop drains in a single tick. The
/// remainder waits in the bounded channel for the next tick, so an ingress
/// burst can never prevent the drain loop from ending.
pub const MAX_INBOUND_PER_TICK: usize = 1024;

/// Most `ActionRequest`s one session may have admitted in one tick
/// (`docs/architecture.md`: "Drain bounded input queues; validate client
/// sequence numbers, permissions, and action limits"). Past this the request
/// gets an explicit throttled rejection — an actionable retry response — rather
/// than queueing more work, and every other session keeps its own quota.
pub const MAX_ACTIONS_PER_CLIENT_PER_TICK: u32 = 4;

/// Most `RepairRequest`s one session may have admitted in one tick
/// (`docs/protocol.md`: `RepairRequest` is "rate-limited"). Surplus is dropped;
/// the replica re-requests, itself rate-limited (ENG-49).
pub const MAX_REPAIRS_PER_CLIENT_PER_TICK: u32 = 8;

/// Most reliable messages (committed topology, `ActionStatus`, baseline
/// transfers) that may sit unsent in one client's outbound queue before that
/// client is disconnected and left to re-baseline on reconnect. Committed
/// topology is never silently discarded to stay under budget
/// (`docs/protocol.md`: "repair or disconnect a client whose reliable backlog
/// exceeds the bounded window").
pub const MAX_RELIABLE_BACKLOG: usize = 2048;

/// Byte ceiling on that same per-client reliable queue.
pub const MAX_RELIABLE_BACKLOG_BYTES: usize = 8 * 1024 * 1024;

/// ENG-61: with [`ServeConfig::await_body_settle`], how many consecutive ticks
/// every detached body's origin must hold still (< 1 mm/tick) — on top of being
/// asleep — before the run is allowed to stop on edit-quiescence. One second at
/// 60 Hz: long enough that the reported rest is real, short enough to keep the
/// gate fixture quick.
pub const AWAIT_SETTLE_STABLE_TICKS: u64 = 60;

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
    /// Like [`Scene::BridgeCut`] but the seam-straddling column and the beam it
    /// holds both cross the `x = 32` brick boundary, so cutting the column
    /// detaches a body whose cells were owned across two bricks. See
    /// [`spall_voxel::fixtures::cross_brick_bridge_scene`].
    CrossBridgeCut,
    /// T19: a flat anchored walking arena with a step ledge. Each connecting
    /// client is given an authoritative player capsule; clients script movement
    /// and predict it locally. See [`spall_sim::fixtures::walk_arena_setup`].
    Walk,
    /// T17 / ENG-64: an anchored floor holds a `24³` checkerboard block through
    /// one column; cutting the column detaches a component whose ~13.8k
    /// single-cell runs overflow the inline `CellRun` budget, so the commit
    /// falls back to the compressed-baseline-blob op path. See
    /// [`spall_sim::fixtures::checkerboard_split_setup`].
    CheckerboardSplit,
    /// T17 increment 2 / ENG-64: like [`Scene::CheckerboardSplit`] but the
    /// detached block's compressed geometry exceeds even the inline op-blob cap,
    /// so the commit ships a bulk `BaselineWorld` on a stream. See
    /// [`spall_sim::fixtures::bulk_split_setup`].
    BulkSplit,
    /// T23 / G3 integrated acceptance: two independent collapsible bridge
    /// structures in one bounded `256 x 128 x 256 m` world. Connecting clients
    /// get a player capsule; alternating slots spawn in the two regions, so the
    /// harness drives geographically separated players and a multi-region
    /// collapse. See [`spall_sim::fixtures::separated_regions_setup`].
    SeparatedRegions,
    /// T23 / G4 eight-client workload (row 12): the same bounded world as
    /// [`Scene::SeparatedRegions`], eight player spawns in two 4-player
    /// clusters, plus 256 active + 4096 sleeping debris bodies. See
    /// [`spall_sim::fixtures::g4_workload_setup`] /
    /// [`spall_sim::fixtures::spawn_g4_workload_bodies`].
    G4Workload,
    /// Full-envelope separated regions joined by a causeway (T23 / G3 row 2).
    SeparatedRegionsFar,
    /// T11a / ENG-62: the full G1 gate envelope — `64 x 32 x 64 m` of real,
    /// resident, walkable terrain (not just isolated structures), a hollow
    /// tower/bridge spanning brick boundaries, an excavatable ramp, and a
    /// moving hollow test-volume body. See
    /// [`spall_sim::fixtures::g1_full_envelope_setup`] /
    /// [`spall_sim::fixtures::spawn_g1_hollow_test_volume`].
    G1FullEnvelope,
}

impl Scene {
    /// Parses the `--scene` / scenario `scene` value. Unknown names are an error
    /// the caller surfaces rather than silently falling back.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "bridge-cut" | "bridgecut" | "bridge" => Some(Scene::BridgeCut),
            "cross-bridge-cut" | "cross-brick-bridge" | "crossbridgecut" => {
                Some(Scene::CrossBridgeCut)
            }
            "walk" | "walk-arena" | "player-movement" => Some(Scene::Walk),
            "checkerboard-split" | "oversized-split" => Some(Scene::CheckerboardSplit),
            "bulk-split" | "giant-split" => Some(Scene::BulkSplit),
            "separated-regions" | "t23-g3" | "g3" => Some(Scene::SeparatedRegions),
            "g4-workload" | "t23-g4" | "g4" => Some(Scene::G4Workload),
            "separated-regions-far" | "t23-g3-full-envelope" | "g3-far" => {
                Some(Scene::SeparatedRegionsFar)
            }
            "g1-full-envelope" | "g1-full-workload" | "g1" => Some(Scene::G1FullEnvelope),
            _ => None,
        }
    }

    /// A short stable name for the run summary.
    pub fn name(self) -> &'static str {
        match self {
            Scene::BridgeCut => "bridge-cut",
            Scene::CrossBridgeCut => "cross-bridge-cut",
            Scene::Walk => "walk",
            Scene::CheckerboardSplit => "checkerboard-split",
            Scene::BulkSplit => "bulk-split",
            Scene::SeparatedRegions => "separated-regions",
            Scene::G4Workload => "g4-workload",
            Scene::SeparatedRegionsFar => "separated-regions-far",
            Scene::G1FullEnvelope => "g1-full-envelope",
        }
    }

    /// `true` if this scene gives every connecting client a player capsule.
    pub fn has_players(self) -> bool {
        matches!(
            self,
            Scene::Walk
                | Scene::SeparatedRegions
                | Scene::G4Workload
                | Scene::SeparatedRegionsFar
                | Scene::G1FullEnvelope
        )
    }

    /// Feet spawn positions (metres) for a player scene, indexed by connection
    /// slot. Empty for a scene without players.
    pub fn player_spawns(self) -> &'static [[f64; 3]] {
        match self {
            Scene::Walk => &WALK_ARENA_SPAWNS,
            Scene::SeparatedRegions => &SEPARATED_REGION_SPAWNS,
            Scene::G4Workload => &G4_WORKLOAD_SPAWNS,
            Scene::SeparatedRegionsFar => &SEPARATED_REGION_FAR_SPAWNS,
            Scene::G1FullEnvelope => &spall_sim::fixtures::G1_WORKLOAD_SPAWNS,
            _ => &[],
        }
    }

    fn simulation(self) -> Simulation {
        let mut setup = match self {
            Scene::BridgeCut => spall_sim::fixtures::bridged_terrain_setup(),
            Scene::CrossBridgeCut => spall_sim::fixtures::cross_brick_bridged_setup(),
            Scene::Walk => spall_sim::fixtures::walk_arena_setup(),
            Scene::CheckerboardSplit => spall_sim::fixtures::checkerboard_split_setup(),
            Scene::BulkSplit => spall_sim::fixtures::bulk_split_setup(),
            Scene::SeparatedRegions => spall_sim::fixtures::separated_regions_setup(),
            Scene::G4Workload => spall_sim::fixtures::g4_workload_setup(),
            Scene::SeparatedRegionsFar => {
                spall_sim::fixtures::separated_regions_full_envelope_setup()
            }
            Scene::G1FullEnvelope => spall_sim::fixtures::g1_full_envelope_setup(),
        };
        // No detached body in these scenes enables per-body CCD, and the serve
        // loop rebuilds the terrain collider on every committed cut. Rapier's
        // CCD broad-phase BVH can keep a stale proxy for the just-removed
        // collider and panic with "No element at index" mid-sweep when a body
        // is still settling (repro: `cargo xtask scenario --name
        // g1-networked-destruction`). Skip the CCD pass — a no-op here.
        setup.physics.disable_ccd = true;
        let mut sim =
            Simulation::new(SimulationConfig::new(setup)).expect("built-in scene is valid");
        if matches!(self, Scene::G4Workload) {
            // Row 12: 256 active (64 near the west cluster's first spawn) +
            // 4096 sleeping debris bodies, built once at scene-construction
            // time (docs/reports/G3.md increment for this row).
            spall_sim::fixtures::spawn_g4_workload_bodies(sim.world_mut(), G4_WORKLOAD_SPAWNS[0]);
        }
        if matches!(self, Scene::G1FullEnvelope) {
            // The gate's "moving hollow test volume" — built once at
            // scene-construction time, same as G4Workload's debris.
            spall_sim::fixtures::spawn_g1_hollow_test_volume(sim.world_mut())
                .expect("hollow test volume spawns");
        }
        sim
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
    /// **Development only.** Skip the server-side action-claim validation
    /// (`resolve_intent`) and take each `ActionRequest`'s `claimed_target` /
    /// `claimed_brush` verbatim. This is the "explicitly scoped authenticated
    /// development scenario path" from ENG-47: it still requires the join token
    /// and a live session, but it lets a fixture harness script arbitrary cuts
    /// that no real aim ray would produce. Never enable it on a shared host —
    /// with it on, any authenticated peer can edit any cell of any body.
    pub dev_unvalidated_actions: bool,
    /// Test-only. A fault plan armed on the world [`Writer`] *after* initial
    /// recovery / first checkpoint, so an injected disk fault lands on a
    /// periodic or clean-shutdown durable write. `None` in production.
    pub save_faults: Option<spall_store::FaultPlan>,
    /// ENG-61: a gate scenario that must show a detached body *come to rest*
    /// keeps stepping physics past edit-quiescence until every detached body is
    /// asleep (still bounded by [`Self::max_ticks`]). Off by default so ordinary
    /// runs still stop as soon as the edit pipeline is idle.
    pub await_body_settle: bool,
    /// T20: per-client interest relevance + motion bandwidth policy. `None`
    /// keeps the pre-T20 behaviour — one 20 Hz motion batch broadcast
    /// unfiltered to every client. `Some(_)` filters each client's motion to
    /// its interest set, tiers `Far` bodies onto a reduced cadence, and caps
    /// each client's per-batch motion bytes.
    pub motion_interest: Option<MotionInterest>,
    /// T23 / G3 row 7, slice D: default-off resident-cache eviction. `None`
    /// keeps every brick resident (byte-identical to every prior run). `Some(_)`
    /// installs a durable in-memory backing, evicts terrain bricks outside a
    /// per-player interest box each tick, and reloads them on demand for edits
    /// (`crate::residency_pass`). The committed world is unchanged — see
    /// `docs/reports/G3-residency-hash.md`.
    pub residency: Option<crate::ResidencyLimits>,
}

/// T20 per-client interest + motion bandwidth policy for a [`serve`] run.
///
/// `docs/protocol.md` ("Network budget and overload behavior"): "Prioritize
/// players, imminent contacts, nearby moving bodies … send distant/sleeping
/// bodies less frequently with periodic keyframes."
#[derive(Debug, Clone, Copy)]
pub struct MotionInterest {
    /// A body whose bounds are within this distance of a client's anchor is
    /// replicated every 20 Hz batch.
    pub near_radius_m: f64,
    /// Out to this distance a body is replicated on the reduced `far_interval`
    /// cadence; beyond it (plus hysteresis) a non-player body is not replicated
    /// to that client at all.
    pub far_radius_m: f64,
    /// Send `Far`-tier snapshots only on every Nth 20 Hz batch (`1` or `0` =
    /// every batch).
    pub far_interval: u64,
    /// Per-client, per-batch motion byte ceiling, accounted at
    /// [`crate::MOTION_SNAPSHOT_WIRE_BYTES`] per snapshot (`0` = no ceiling). When it
    /// bites, the lowest-priority motion (far, then sleeping) is deferred to the
    /// next batch — committed topology is never affected.
    pub per_client_budget_bytes: usize,
    /// Interest anchor for a client on a scene with no player capsule (the
    /// bridge scenes). `None` there leaves that client unfiltered
    /// ([`InterestSet::Global`]).
    pub static_anchor_m: Option<[f64; 3]>,
}

/// A player capsule's bounding radius for interest tests, metres. Small and
/// fixed — a player is prioritised and never `Excluded` regardless.
const PLAYER_INTEREST_RADIUS_M: f64 = 1.0;

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
            dev_unvalidated_actions: false,
            save_faults: None,
            await_body_settle: false,
            motion_interest: None,
            residency: None,
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
    /// Largest number of distinct bricks any single detached (non-terrain) body
    /// occupied at end of run. `>= 2` means a body's cells were owned across a
    /// brick boundary — the cross-brick ownership-transfer signal for T11.
    pub max_detached_body_brick_span: u64,
    /// ENG-61: largest linear speed (m/s) of any detached body at end of run.
    /// `0.0` when there is no detached body.
    pub detached_body_max_final_speed_m_s: f64,
    /// ENG-61: every detached body was asleep — at rest — at end of run. `true`
    /// when there is no detached body.
    pub detached_bodies_all_asleep: bool,
    /// ENG-61: consecutive final ticks over which no detached body's origin
    /// moved more than 1 mm on any axis — "vertical position stable for N ticks".
    pub detached_body_stable_ticks: u64,
    /// ENG-61: deepest contact penetration (m) anywhere in the physics world at
    /// end of run. A body that came to rest *clipped through* the remaining
    /// floor rather than on top of it shows up here.
    pub max_contact_penetration_m: f64,
    /// ENG-61: lowest detached-body origin Y (m) at end of run. A body that
    /// free-fell instead of settling leaves this far below the floor. `0.0` when
    /// there is no detached body.
    pub detached_body_min_origin_y_m: f64,
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
    /// ENG-48: `ActionRequest`s bounced with a throttled rejection because their
    /// session exceeded [`MAX_ACTIONS_PER_CLIENT_PER_TICK`] this tick.
    pub inbound_actions_throttled: u64,
    /// ENG-48: `RepairRequest`s dropped because their session exceeded
    /// [`MAX_REPAIRS_PER_CLIENT_PER_TICK`] this tick.
    pub inbound_repairs_throttled: u64,
    /// T20: motion snapshots actually sent this run, summed over every client
    /// and batch. With `motion_interest` unset this is `batches * bodies *
    /// clients`; with it set, interest-culled and cadence-deferred snapshots are
    /// not counted here.
    pub motion_snapshots_sent: u64,
    /// T20: motion snapshots withheld because the body was outside a client's
    /// interest set (`Relevance::Excluded`). `0` when `motion_interest` is unset.
    pub motion_snapshots_interest_culled: u64,
    /// T20: motion snapshots deferred to a later batch by the `Far` cadence or
    /// the per-client byte ceiling — motion is superseded, so this is a
    /// bandwidth measure, not lost geometry. `0` when `motion_interest` is unset.
    pub motion_snapshots_budget_deferred: u64,
    /// T20: largest single-batch motion payload sent to one client, accounted
    /// bytes ([`crate::MOTION_SNAPSHOT_WIRE_BYTES`] each).
    pub max_client_motion_batch_bytes: u64,
    /// T20: total **application** bytes this host sent across every client
    /// connection (control records + datagrams + bulk parts, excluding QUIC
    /// framing) — `spall_net`'s own `app_bytes_sent` counter.
    pub app_egress_bytes: u64,
    /// T20: total **transport** bytes this host sent across every client
    /// connection — Quinn's `udp_tx.bytes`, i.e. actual UDP payload on the wire
    /// including QUIC framing and retransmission.
    pub transport_egress_bytes: u64,
    /// T11a / ENG-62: inbound `ActionRequest`s received this run, retries
    /// included ("requested" in the gate's requested / rejected / queued /
    /// committed breakdown). "rejected" is [`Self::actions_rejected`];
    /// "committed" is [`Self::transactions_committed`].
    pub actions_requested: u64,
    /// T11a / ENG-62: `ActionRequest`s the simulation accepted for staging.
    pub actions_staged: u64,
    /// T11a / ENG-62: staged requests that had not committed when the run ended
    /// ("queued" in the gate breakdown).
    pub actions_queued_unresolved: u64,
    /// T11a / ENG-62: server commit latency — admission to commit — as a
    /// nearest-rank p95 (ms) per commit shape, with the sample count behind each
    /// figure. Gate targets (`docs/validation.md` "G1"): single-brick commit
    /// p95 `<= 100 ms`, ordinary structure split `<= 500 ms`, designated
    /// large-collapse `<= 2000 ms`. A bucket with `0` samples was not exercised.
    pub single_brick_commit_p95_ms: f64,
    pub single_brick_commit_samples: u64,
    pub structure_split_p95_ms: f64,
    pub structure_split_samples: u64,
    pub large_collapse_p95_ms: f64,
    pub large_collapse_samples: u64,
    /// T23 / G3 row 7, slice D: residency pass activity. All `0` when
    /// `ServeConfig.residency` is `None` (the default).
    pub residency_evictions_total: u64,
    pub residency_reloads_total: u64,
    /// Fewest / most / final resident terrain bricks the pass observed.
    pub resident_terrain_bricks_min: u64,
    pub resident_terrain_bricks_max: u64,
    pub resident_terrain_bricks_final: u64,
    /// Ticks the resident-brick count exceeded `budget_bricks` (a player's
    /// interest set is larger than the declared budget).
    pub residency_budget_miss_ticks: u64,
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
    /// T19: a player movement input frame (datagram). At most one is applied per
    /// player per tick; redundant `recent` copies recover a dropped frame.
    Input(SessionId, InputFrame),
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

type ClientMap = Arc<Mutex<HashMap<u64, OutboundHandle>>>;

/// A bounded per-client outbound queue (ENG-48).
///
/// * Reliable traffic — committed [`TopologyTransaction`]s, [`ActionStatus`],
///   baseline transfers, the shutdown marker — is FIFO and counted against
///   [`MAX_RELIABLE_BACKLOG`] / [`MAX_RELIABLE_BACKLOG_BYTES`]. A stalled
///   reliable reader that blows either bound is disconnected (and re-baselines
///   on reconnect); the backlog already accepted is still flushed, so committed
///   topology is never silently discarded to stay under budget.
/// * Motion is lossy: only the newest unsent batch is retained, so a slow
///   reader accumulates no stale motion (`docs/protocol.md`: "Drop superseded
///   unsent motion snapshots").
#[derive(Default)]
struct OutboundQueue {
    reliable: VecDeque<Outbound>,
    reliable_bytes: usize,
    motion: Option<Arc<Vec<MotionSnapshot>>>,
    /// Set once a reliable push blew the bound. The writer flushes what is
    /// already queued, says goodbye, and exits.
    overflowed: bool,
}

/// The reliable backlog blew [`MAX_RELIABLE_BACKLOG`] /
/// [`MAX_RELIABLE_BACKLOG_BYTES`]; the caller must drop this client from the
/// fan-out set.
#[derive(Debug)]
struct OutboundOverflow;

impl OutboundQueue {
    fn push(&mut self, msg: Outbound) -> Result<(), OutboundOverflow> {
        match msg {
            // Lossy: keep only the newest unsent batch.
            Outbound::Motion(snaps) => {
                self.motion = Some(snaps);
                Ok(())
            }
            // The shutdown marker always goes through — it ends the stream.
            Outbound::Shutdown => {
                self.reliable.push_back(Outbound::Shutdown);
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

/// Rough serialized size of one reliable outbound message, for the byte cap.
fn reliable_msg_bytes(msg: &Outbound) -> usize {
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
        Outbound::Baseline(t) => 64 + t.payload_bytes(),
        Outbound::Motion(_) | Outbound::Shutdown => 0,
    }
}

/// What one [`OutboundHandle::take`] pass handed the writer.
struct OutboundBatch {
    reliable: Vec<Outbound>,
    motion: Option<Arc<Vec<MotionSnapshot>>>,
    overflowed: bool,
}

impl OutboundBatch {
    fn is_empty(&self) -> bool {
        self.reliable.is_empty() && self.motion.is_none()
    }
}

/// A cloneable handle: the sim bridge enqueues through it, the client's writer
/// task drains it.
#[derive(Clone)]
struct OutboundHandle {
    inner: Arc<OutboundShared>,
}

struct OutboundShared {
    queue: Mutex<OutboundQueue>,
    wake: Notify,
}

impl OutboundHandle {
    fn new() -> Self {
        Self {
            inner: Arc::new(OutboundShared {
                queue: Mutex::new(OutboundQueue::default()),
                wake: Notify::new(),
            }),
        }
    }

    /// Enqueues one message and wakes the writer. `Err(OutboundOverflow)` means
    /// the reliable backlog blew its bound and the caller must drop this client
    /// from the fan-out set (its writer is now draining-then-closing).
    fn push(&self, msg: Outbound) -> Result<(), OutboundOverflow> {
        let res = {
            let mut q = self.inner.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.push(msg)
        };
        self.inner.wake.notify_one();
        res
    }

    /// Takes everything queued in one pass.
    fn take(&self) -> OutboundBatch {
        let mut q = self.inner.queue.lock().unwrap_or_else(|e| e.into_inner());
        OutboundBatch {
            reliable: q.reliable.drain(..).collect(),
            motion: q.motion.take(),
            overflowed: q.overflowed,
        }
    }

    /// Current queued reliable bytes (test-only introspection of the byte cap).
    #[cfg(test)]
    fn reliable_bytes(&self) -> usize {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reliable_bytes
    }

    async fn woken(&self) {
        self.inner.wake.notified().await;
    }
}

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
    // T20 egress accounting: live connection handles (for a final stats read at
    // teardown) plus the summed `(app_bytes, transport_bytes)` of connections
    // that already closed. A closing `serve_conn` removes itself from `conns`
    // and folds its final counts into `egress_closed` under the `conns` lock, so
    // teardown counts every connection exactly once.
    let conns: Arc<Mutex<HashMap<u64, Arc<Connection>>>> = Arc::new(Mutex::new(HashMap::new()));
    let egress_closed: Arc<Mutex<(u64, u64)>> = Arc::new(Mutex::new((0, 0)));
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<Inbound>(INBOUND_CHANNEL_CAP);
    let (count_tx, mut count_rx) = watch::channel(0usize);
    let (stop_tx, stop_rx) = watch::channel(false);

    // Accept loop.
    let accept = {
        let server = server.clone();
        let clients = clients.clone();
        let conns = conns.clone();
        let egress_closed = egress_closed.clone();
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
                let handle = OutboundHandle::new();
                clients
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(conn.session().raw(), handle.clone());
                conns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(conn.session().raw(), conn.clone());
                tokio::spawn(serve_conn(
                    conn,
                    inbound_tx.clone(),
                    handle,
                    clients.clone(),
                    conns.clone(),
                    egress_closed.clone(),
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
    let await_body_settle = config.await_body_settle;
    let motion_interest = config.motion_interest;
    let scene = config.scene;
    let clients_for_sim = clients.clone();
    let save = config.save.clone();
    let save_faults = config.save_faults.clone();
    let checkpoint_interval = config.checkpoint_interval_ticks;
    let catch_up_cap = config.catch_up_cap.max(1);
    let max_join_retries = config.max_join_retries;
    let dev_unvalidated_actions = config.dev_unvalidated_actions;
    let residency_limits = config.residency;
    let persist_cfg = PersistConfig {
        world_id: T10_WORLD_ID,
        seed: config.seed,
        generator_version: 1,
    };

    let sim_join = tokio::task::spawn_blocking(move || -> SimResult {
        // Open the world database (T16). If it already holds a checkpoint,
        // recover from it; otherwise start the built-in scene and publish an
        // initial checkpoint so recovery always has a floor.
        // `setup_persistence` arms any test-only `save_faults` on the recovered
        // writer *after* recovery and the initial checkpoint, so the fault
        // falls on a later durable write, then hands the writer to the bounded
        // off-thread `PersistPipeline` (ENG-50).
        let Persistence {
            mut sim,
            mut pipeline,
            mut journalled_through,
            mut checkpoints_published,
        } = match setup_persistence(save.as_deref(), scene, &persist_cfg, save_faults) {
            Ok(parts) => parts,
            Err(e) => {
                return SimResult::error(format!("persistence setup failed: {e}"), 0);
            }
        };
        let mut journal_records_written: u64 = 0;

        // T23 / G3 row 7, slice D: default-off residency pass. `None` -> the
        // world stays fully resident and every counter below is `0`.
        let terrain_vid = sim.world().terrain_volume_id();
        let mut residency =
            residency_limits.map(|limits| crate::ResidencyPass::install(sim.world_mut(), limits));

        let mut motion = MotionPublisher::new(60, 20);
        let mut committed_total = 0u64;
        let mut rejected_total = 0u64;
        // ENG-48: `ActionRequest`s / `RepairRequest`s that exceeded a session's
        // per-tick admission quota and were bounced with a retry response
        // (actions) or dropped (repairs).
        let mut actions_throttled = 0u64;
        let mut repairs_throttled = 0u64;

        // T11a / ENG-62: commit-latency measurement + admission accounting.
        // `actions_requested` counts every inbound `ActionRequest` (retries
        // included); `actions_staged` counts those the sim accepted for staging;
        // `submitted_at` times each staged request from admission to the tick
        // its transaction commits. Whatever is still in `submitted_at` at end of
        // run was staged but never committed — the "queued" bucket.
        let mut actions_requested = 0u64;
        let mut actions_staged = 0u64;
        let mut submitted_at: HashMap<RequestId, std::time::Instant> = HashMap::new();
        // The session that staged each still-pending request, so the tick-report
        // outcome (`action_statuses`) can be routed back to only that client
        // instead of every connected client.
        let mut submitted_by: HashMap<RequestId, SessionId> = HashMap::new();
        let mut commit_latency = CommitLatency::default();

        let mut idle_streak = 0u64;
        let mut ticks_run = 0u64;
        let tick_dt = Duration::from_nanos(1_000_000_000 / 60);

        // T20: per-client interest / bandwidth state and run counters. Empty and
        // untouched unless `motion_interest` is configured.
        let mut client_repl: HashMap<u64, ClientReplication> = HashMap::new();
        let mut motion_batch_index = 0u64;
        let mut motion_egress = MotionEgress::default();

        // ENG-61: rolling "every detached body is holding still" window. Each
        // tick we compare every body's origin Y against the previous tick; a run
        // of ticks under 1 mm of movement is the "vertical position stable for N
        // ticks" evidence the G1 body-at-rest case needs.
        let mut prev_body_y: HashMap<u64, f64> = HashMap::new();
        let mut body_stable_ticks = 0u64;

        // T17 late-join / reconnect state.
        let mut lj = LateJoin::new(catch_up_cap, max_join_retries);
        // T23 / G3 row 7, slice D: a late-join baseline or repair patch over a
        // brick the residency pass has evicted is filled from its durable
        // backing.
        lj.backing = residency.as_ref().map(|p| p.backing());

        for _ in 0..max_ticks {
            let started = std::time::Instant::now();

            // ENG-48: drain a bounded slice of what the clients have sent since
            // the last tick, with a per-session admission quota so one flooding
            // client can neither stall this loop nor starve the others. The
            // surplus stays in the bounded channel for the next tick.
            let mut repairs: Vec<(SessionId, RepairRequest)> = Vec::new();
            let mut saw_client_work = false;
            let mut actions_admitted: HashMap<u64, u32> = HashMap::new();
            let mut repairs_admitted: HashMap<u64, u32> = HashMap::new();
            let mut drained = 0usize;
            while drained < MAX_INBOUND_PER_TICK {
                let Ok(msg) = inbound_rx.try_recv() else {
                    break;
                };
                drained += 1;
                match msg {
                    Inbound::Joined(session) => {
                        lj.on_joined(session);
                        // T19: give this connection an authoritative player
                        // capsule on a player scene (respawn on reconnect).
                        let spawns = scene.player_spawns();
                        if !spawns.is_empty() {
                            let slot = session.slot().0 as usize;
                            let spawn = spawns[slot.min(spawns.len() - 1)];
                            sim.add_player(session_player_entity(session), spawn);
                        }
                    }
                    Inbound::Input(session, frame) => {
                        saw_client_work = true;
                        if lj.session_expired(session) {
                            continue;
                        }
                        let entity = session_player_entity(session);
                        // Apply the redundant recent copies oldest-first, then
                        // the current frame. `set_player_input` drops any that
                        // are not newer than what the server already has, so a
                        // single surviving datagram recovers a dropped frame.
                        for r in frame.recent.iter().rev() {
                            sim.set_player_input(entity, recent_input(r), r.input_seq);
                        }
                        sim.set_player_input(entity, frame_input(&frame), frame.input_seq);
                    }
                    Inbound::Action(session, req) => {
                        saw_client_work = true;
                        actions_requested += 1;
                        if lj.session_expired(session) {
                            reject(&clients_for_sim, session, req.request_id, "expired session");
                            rejected_total += 1;
                            lj.expired_actions += 1;
                            continue;
                        }
                        // ENG-57: check the authoritative admission ledger
                        // before rate limiting or resolving the claim. A
                        // committed cut can make the original ray miss, and a
                        // reconnect may legitimately retry the same reliable
                        // request on its replacement session. Replaying this
                        // status must not restage or re-apply the edit.
                        if let Some(status) = replay_admitted_status(&sim, req.request_id) {
                            send_to(
                                &clients_for_sim,
                                session,
                                Outbound::Status(Arc::new(status.clone())),
                            );
                            continue;
                        }
                        if !admit(
                            &mut actions_admitted,
                            session.raw(),
                            MAX_ACTIONS_PER_CLIENT_PER_TICK,
                        ) {
                            reject(
                                &clients_for_sim,
                                session,
                                req.request_id,
                                "throttled: too many actions this tick, retry shortly",
                            );
                            rejected_total += 1;
                            actions_throttled += 1;
                            continue;
                        }
                        let resolved = if dev_unvalidated_actions {
                            dev_intent_from_request(session, &req)
                                .ok_or(ActionReject::Unsupported("unsupported target"))
                        } else {
                            resolve_intent(sim.world(), session, &req)
                        };
                        match resolved {
                            Ok(intent) => match sim.submit(intent) {
                                Ok(status) => {
                                    submitted_at
                                        .entry(req.request_id)
                                        .or_insert_with(std::time::Instant::now);
                                    submitted_by.entry(req.request_id).or_insert(session);
                                    actions_staged += 1;
                                    send_to(
                                        &clients_for_sim,
                                        session,
                                        Outbound::Status(Arc::new(status)),
                                    );
                                }
                                Err(e) => {
                                    reject(
                                        &clients_for_sim,
                                        session,
                                        req.request_id,
                                        &e.to_string(),
                                    );
                                    rejected_total += 1;
                                }
                            },
                            Err(rej) => {
                                reject(&clients_for_sim, session, req.request_id, &rej.reason());
                                rejected_total += 1;
                            }
                        }
                    }
                    Inbound::Repair(session, req) => {
                        saw_client_work = true;
                        if lj.session_expired(session) {
                            continue;
                        }
                        if admit(
                            &mut repairs_admitted,
                            session.raw(),
                            MAX_REPAIRS_PER_CLIENT_PER_TICK,
                        ) {
                            repairs.push((session, req));
                        } else {
                            repairs_throttled += 1;
                        }
                    }
                    Inbound::Baseline(session, ack) => {
                        saw_client_work = true;
                        lj.on_baseline_ack(session, ack, &sim, &clients_for_sim, &mut motion);
                    }
                    Inbound::Gone(session) => lj.on_gone(session),
                }
            }

            lj.publish_ready_captures(&clients_for_sim);
            let report = match sim.tick() {
                Ok(r) => r,
                Err(e) => return SimResult::error(format!("tick failed: {e}"), ticks_run),
            };
            ticks_run += 1;
            let tick = sim.current_tick();

            // ENG-61: fold this tick into the "bodies holding still" window.
            // Only when the scenario asked for it — an ordinary run does no
            // extra per-tick work, so its timing (and an impaired run's
            // proxy-reordering outcome) is byte-for-byte unchanged.
            if await_body_settle {
                let mut max_dy = 0.0_f64;
                let mut have_history = !prev_body_y.is_empty();
                let mut seen = 0usize;
                let mut next_body_y: HashMap<u64, f64> = HashMap::with_capacity(prev_body_y.len());
                for b in sim.world().bodies() {
                    seen += 1;
                    let key = b.entity.map(|e| e.get()).unwrap_or(0);
                    let y = b.pose.translation_m[1];
                    match prev_body_y.get(&key) {
                        Some(py) => max_dy = max_dy.max((y - py).abs()),
                        None => have_history = false,
                    }
                    next_body_y.insert(key, y);
                }
                prev_body_y = next_body_y;
                if seen > 0 && have_history && max_dy < 1.0e-3 {
                    body_stable_ticks += 1;
                } else {
                    body_stable_ticks = 0;
                }
            }

            for (rid, committed) in &report.committed {
                committed_total += 1;
                // T17 increment 2: a giant split ships its geometry out of band
                // as a `BaselineTransfer`, keyed to the transaction by
                // `transfer_id` (= the split's `TransactionId` | high bit).
                let split_transfer = committed.bulk_baseline.as_ref().and_then(|world| {
                    let id = TransferId(
                        committed.transaction.get() | spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT,
                    );
                    baseline::transfer_from_world(
                        world.clone(),
                        id,
                        InterestEpoch(1),
                        JournalSeq(sim.journal_cursor()),
                    )
                    .ok()
                    .map(Arc::new)
                });
                lj.fan_out_transaction(
                    Arc::new(committed.topology.clone()),
                    split_transfer,
                    &sim,
                    &clients_for_sim,
                );
                // T11a / ENG-62: bucket each commit's server-side latency
                // (admission → commit) by whether it split and how much
                // geometry detached.
                if let Some(started_at) = submitted_at.remove(rid) {
                    let class =
                        commit_latency::classify(committed.bumped_epoch, &committed.topology);
                    commit_latency.record(class, started_at.elapsed());
                }
                // Slice D: keep the durable backing current for the terrain
                // bricks this edit changed, so a later reload gets this
                // revision.
                if let Some(pass) = &residency {
                    pass.on_commit(
                        sim.world(),
                        committed
                            .topology
                            .after
                            .iter()
                            .filter(|br| br.volume == terrain_vid)
                            .map(|br| br.coord),
                    );
                }
            }
            for status in action_statuses(&report) {
                if matches!(status.outcome, ActionOutcome::Rejected { .. }) {
                    rejected_total += 1;
                }
                // A tick-resolved outcome (commit or deterministic staging
                // rejection) belongs to whichever session staged it — route it
                // there only. `submitted_by` is best-effort bookkeeping (cleared
                // here on first resolution); an untracked request (there
                // shouldn't be one) falls back to the old broadcast so a status
                // is never silently dropped.
                match submitted_by.remove(&status.request_id) {
                    Some(session) => send_to(
                        &clients_for_sim,
                        session,
                        Outbound::Status(Arc::new(status)),
                    ),
                    None => broadcast(&clients_for_sim, Outbound::Status(Arc::new(status))),
                }
            }
            // The 20 Hz motion batch: send it to replicas *and* keep the full
            // batch for the durable pose journal below (durability is never
            // interest-filtered).
            let pose_batch: Option<Vec<MotionSnapshot>> = if motion.due(tick) {
                let snaps = motion.snapshots(sim.world(), tick);
                if !snaps.is_empty() {
                    match motion_interest {
                        // Pre-T20: one batch, broadcast unfiltered.
                        None => {
                            let recipients = clients_for_sim
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .len() as u64;
                            broadcast(&clients_for_sim, Outbound::Motion(Arc::new(snaps.clone())));
                            motion_egress.snapshots_sent += snaps.len() as u64 * recipients;
                        }
                        // T20: per-client interest relevance + bandwidth budget.
                        Some(mi) => {
                            dispatch_motion_by_interest(
                                &mi,
                                scene,
                                motion_batch_index,
                                &snaps,
                                &sim,
                                &lj,
                                &clients_for_sim,
                                &mut client_repl,
                                &mut motion_egress,
                            );
                        }
                    }
                    motion_batch_index += 1;
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
                    // Slice D: a checkpoint is a full-world snapshot — reload any
                    // evicted terrain first so `persist::capture` sees it all.
                    if let Some(pass) = &mut residency {
                        pass.reload_all(sim.world_mut());
                    }
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

            // Slice D: post-tick residency pass. Evicts terrain bricks outside
            // every player's interest box, reloads any back in interest. The
            // committed world (hash, conservation, result_hashes) is unchanged;
            // an edit that needs an evicted brick reloads it via the pipeline.
            if let Some(pass) = &mut residency {
                let player_feet: Vec<[f64; 3]> =
                    sim.world().players().map(|p| p.state.position_m).collect();
                pass.run(sim.world_mut(), &player_feet);
            }

            // Quiesce only after the pipeline is drained *and* no client has
            // sent anything for `quiescence` ticks — a late scripted action from
            // one client keeps the run alive for the others. A client still
            // mid-baseline (`lj.any_joining()`) also keeps the run alive: under
            // an impaired transport its bulk transfer can legitimately take
            // longer than `quiescence` ticks of otherwise-quiet simulation, and
            // ending the run out from under it would strand the connection
            // before the client ever gets to replicate.
            if sim.is_idle() && !saw_client_work && report.committed.is_empty() && !lj.any_joining()
            {
                idle_streak += 1;
            } else {
                idle_streak = 0;
            }
            if quiescence > 0 && committed_total > 0 && idle_streak >= quiescence {
                // ENG-61: when the scenario needs a detached body to come to
                // rest, keep stepping physics past edit-quiescence until every
                // detached body is asleep *and* has held its position still for
                // a stretch of ticks (still bounded by `max_ticks`), so the run
                // ends with real "came to rest" evidence rather than catching
                // the body mid-fall.
                let settled = sim.world().bodies().all(|b| b.sleeping)
                    && body_stable_ticks >= AWAIT_SETTLE_STABLE_TICKS;
                if !await_body_settle || settled {
                    break;
                }
            }

            if paced && let Some(rem) = tick_dt.checked_sub(started.elapsed()) {
                std::thread::sleep(rem);
            }
        }

        broadcast(&clients_for_sim, Outbound::Shutdown);

        // Clean-shutdown durability: queue the final journal tail + checkpoint
        // + retain, then block until the off-thread writer has drained and
        // exited (`docs/protocol.md`: "Clean shutdown waits for a final
        // checkpoint/flush"). A failure on that final drain is surfaced through
        // `shutdown_error` with every counter below still reported, so the
        // failing summary keeps its diagnostics instead of returning early —
        // silently reporting success would strand an unsavable world.
        let mut persist_bytes_per_write = 0.0;
        let mut persist_commit_bytes_per_sec = 0.0;
        let mut shutdown_error: Option<String> = None;
        // Slice D: reload every evicted brick before the shutdown snapshot so
        // the final checkpoint and the reported world hash are the complete
        // world.
        if let Some(pass) = &mut residency {
            pass.reload_all(sim.world_mut());
        }
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
            // The final drain covers the journal tail, the final checkpoint, and
            // the retain pass; attribute any failure to that shutdown flush so a
            // stranded save is never reported as a pass.
            shutdown_error = outcome
                .status
                .error
                .map(|e| format!("final checkpoint/flush failed: {e}"));
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

        SimResult {
            ok: shutdown_error.is_none(),
            error: shutdown_error,
            ticks_run,
            committed_total,
            rejected_total,
            final_world_hash: sim.world().world_hash().to_string(),
            total_solid_cells: sim.world().total_solid_cells(),
            body_count: sim.world().body_count(),
            max_detached_body_brick_span: sim
                .world()
                .bodies()
                .map(|b| b.volume.resident_brick_coords().len() as u64)
                .max()
                .unwrap_or(0),
            detached_body_max_final_speed_m_s: sim
                .world()
                .bodies()
                .map(|b| {
                    (b.linvel_m_s[0].powi(2) + b.linvel_m_s[1].powi(2) + b.linvel_m_s[2].powi(2))
                        .sqrt()
                })
                .fold(0.0_f64, f64::max),
            detached_bodies_all_asleep: sim.world().bodies().all(|b| b.sleeping),
            detached_body_stable_ticks: body_stable_ticks,
            max_contact_penetration_m: f64::from(sim.world().physics().max_penetration_m()),
            detached_body_min_origin_y_m: {
                let min_y = sim
                    .world()
                    .bodies()
                    .map(|b| b.pose.translation_m[1])
                    .fold(f64::INFINITY, f64::min);
                if min_y.is_finite() { min_y } else { 0.0 }
            },
            checkpoints_published,
            journal_records_written,
            persist_bytes_per_write,
            persist_commit_bytes_per_sec,
            late_joins_completed: lj.completed,
            late_join_retries: lj.retries,
            late_joins_failed: lj.failed,
            expired_actions_rejected: lj.expired_actions,
            baseline_bytes_sent: lj.baseline_bytes,
            actions_throttled,
            repairs_throttled,
            motion_snapshots_sent: motion_egress.snapshots_sent,
            motion_snapshots_interest_culled: motion_egress.interest_culled,
            motion_snapshots_budget_deferred: motion_egress.budget_deferred,
            max_client_motion_batch_bytes: motion_egress.max_client_batch_bytes,
            actions_requested,
            actions_staged,
            actions_queued_unresolved: submitted_at.len() as u64,
            latency: commit_latency.report(),
            residency: residency.as_ref().map(|p| p.stats()),
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

    // T20: total egress. Hold the `conns` lock across the whole read so a
    // late-closing `serve_conn` can neither remove-and-accumulate an entry
    // concurrently (it takes the same lock first) nor be missed — every
    // connection is counted once, here or in `egress_closed`.
    let (app_egress_bytes, transport_egress_bytes) = {
        let conns_guard = conns.lock().unwrap_or_else(|e| e.into_inner());
        let mut acc = egress_closed.lock().unwrap_or_else(|e| e.into_inner());
        for conn in conns_guard.values() {
            acc.0 = acc.0.saturating_add(conn.stats().app_bytes_sent);
            acc.1 = acc.1.saturating_add(conn.transport_stats().udp_tx.bytes);
        }
        (acc.0, acc.1)
    };

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
        version: 4,
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
        max_detached_body_brick_span: sim_result.max_detached_body_brick_span,
        detached_body_max_final_speed_m_s: sim_result.detached_body_max_final_speed_m_s,
        detached_bodies_all_asleep: sim_result.detached_bodies_all_asleep,
        detached_body_stable_ticks: sim_result.detached_body_stable_ticks,
        max_contact_penetration_m: sim_result.max_contact_penetration_m,
        detached_body_min_origin_y_m: sim_result.detached_body_min_origin_y_m,
        checkpoints_published: sim_result.checkpoints_published,
        journal_records_written: sim_result.journal_records_written,
        persist_bytes_per_write: sim_result.persist_bytes_per_write,
        persist_commit_bytes_per_sec: sim_result.persist_commit_bytes_per_sec,
        late_joins_completed: sim_result.late_joins_completed,
        late_join_retries: sim_result.late_join_retries,
        late_joins_failed: sim_result.late_joins_failed,
        expired_actions_rejected: sim_result.expired_actions_rejected,
        baseline_bytes_sent: sim_result.baseline_bytes_sent,
        inbound_actions_throttled: sim_result.actions_throttled,
        inbound_repairs_throttled: sim_result.repairs_throttled,
        motion_snapshots_sent: sim_result.motion_snapshots_sent,
        motion_snapshots_interest_culled: sim_result.motion_snapshots_interest_culled,
        motion_snapshots_budget_deferred: sim_result.motion_snapshots_budget_deferred,
        max_client_motion_batch_bytes: sim_result.max_client_motion_batch_bytes,
        app_egress_bytes,
        transport_egress_bytes,
        actions_requested: sim_result.actions_requested,
        actions_staged: sim_result.actions_staged,
        actions_queued_unresolved: sim_result.actions_queued_unresolved,
        single_brick_commit_p95_ms: sim_result.latency.single_brick_commit_p95_ms,
        single_brick_commit_samples: sim_result.latency.single_brick_commit_samples,
        structure_split_p95_ms: sim_result.latency.structure_split_p95_ms,
        structure_split_samples: sim_result.latency.structure_split_samples,
        large_collapse_p95_ms: sim_result.latency.large_collapse_p95_ms,
        large_collapse_samples: sim_result.latency.large_collapse_samples,
        residency_evictions_total: sim_result.residency.map(|r| r.evictions_total).unwrap_or(0),
        residency_reloads_total: sim_result.residency.map(|r| r.reloads_total).unwrap_or(0),
        resident_terrain_bricks_min: sim_result
            .residency
            .map(|r| r.resident_terrain_bricks_min as u64)
            .unwrap_or(0),
        resident_terrain_bricks_max: sim_result
            .residency
            .map(|r| r.resident_terrain_bricks_max as u64)
            .unwrap_or(0),
        resident_terrain_bricks_final: sim_result
            .residency
            .map(|r| r.resident_terrain_bricks_final as u64)
            .unwrap_or(0),
        residency_budget_miss_ticks: sim_result
            .residency
            .map(|r| r.budget_miss_ticks)
            .unwrap_or(0),
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
    max_detached_body_brick_span: u64,
    detached_body_max_final_speed_m_s: f64,
    detached_bodies_all_asleep: bool,
    detached_body_stable_ticks: u64,
    max_contact_penetration_m: f64,
    detached_body_min_origin_y_m: f64,
    checkpoints_published: u64,
    journal_records_written: u64,
    persist_bytes_per_write: f64,
    persist_commit_bytes_per_sec: f64,
    late_joins_completed: u64,
    late_join_retries: u64,
    late_joins_failed: u64,
    expired_actions_rejected: u64,
    baseline_bytes_sent: u64,
    actions_throttled: u64,
    repairs_throttled: u64,
    motion_snapshots_sent: u64,
    motion_snapshots_interest_culled: u64,
    motion_snapshots_budget_deferred: u64,
    max_client_motion_batch_bytes: u64,
    // T11a / ENG-62.
    actions_requested: u64,
    actions_staged: u64,
    actions_queued_unresolved: u64,
    latency: commit_latency::LatencyReport,
    residency: Option<crate::ResidencyStats>,
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
            max_detached_body_brick_span: 0,
            detached_body_max_final_speed_m_s: 0.0,
            detached_bodies_all_asleep: true,
            detached_body_stable_ticks: 0,
            max_contact_penetration_m: 0.0,
            detached_body_min_origin_y_m: 0.0,
            checkpoints_published: 0,
            journal_records_written: 0,
            persist_bytes_per_write: 0.0,
            persist_commit_bytes_per_sec: 0.0,
            late_joins_completed: 0,
            late_join_retries: 0,
            late_joins_failed: 0,
            expired_actions_rejected: 0,
            baseline_bytes_sent: 0,
            actions_throttled: 0,
            repairs_throttled: 0,
            motion_snapshots_sent: 0,
            motion_snapshots_interest_culled: 0,
            motion_snapshots_budget_deferred: 0,
            max_client_motion_batch_bytes: 0,
            actions_requested: 0,
            actions_staged: 0,
            actions_queued_unresolved: 0,
            latency: commit_latency::LatencyReport::default(),
            residency: None,
        }
    }
}

// --- T17 late-join / catch-up / session renewal ---------------------------

/// How the server is currently treating one connected client.
/// One entry in a joining client's catch-up queue. A giant split (T17
/// increment 2) enqueues its marker transaction *and* the out-of-band
/// `BaselineTransfer` that carries its geometry, drained in order.
enum QueuedItem {
    Tx(Arc<TopologyTransaction>),
    Blob(Arc<BaselineTransfer>),
}

enum Phase {
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
    /// T23 / G3 row 7, slice D: the residency pass's durable backing, so a
    /// late-join baseline / repair patch can fill a brick the server has
    /// evicted. `None` when residency is off.
    backing: Option<std::sync::Arc<spall_sim::MemoryBacking>>,
    /// The most recent immutable topology baseline. A current join may reuse
    /// it with a fresh transfer id; transactions after its cursor remain in the
    /// ordinary per-client catch-up queue.
    cached_baseline: Option<std::sync::Arc<BaselineTransfer>>,
    /// Bounded worker results keyed by the joining session. The simulation owns
    /// publication and only polls these at tick boundaries.
    pending_captures: HashMap<
        u64,
        (
            TransferId,
            std::sync::mpsc::Receiver<Result<BaselineTransfer, baseline::BaselineError>>,
        ),
    >,
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
            backing: None,
            cached_baseline: None,
            pending_captures: HashMap::new(),
        }
    }

    fn backing_ref(&self) -> Option<&dyn spall_sim::BrickBacking> {
        self.backing
            .as_deref()
            .map(|b| b as &dyn spall_sim::BrickBacking)
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
        self.pending_captures.remove(&session.raw());
    }

    /// Every connected client currently in normal (`Live`) replication. A client
    /// still pulling a late-join baseline is excluded — it gets a motion
    /// keyframe when its catch-up barrier is reached, not the live 20 Hz feed
    /// (`docs/protocol.md` late-join step 4).
    fn live_sessions(&self) -> impl Iterator<Item = SessionId> + '_ {
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
    fn any_joining(&self) -> bool {
        self.links
            .values()
            .any(|link| matches!(link.phase, Phase::Joining { .. }))
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
            match self
                .cached_baseline
                .as_ref()
                .filter(|b| b.begin.journal_cursor == JournalSeq(sim.journal_cursor()))
            {
                Some(transfer) => {
                    let transfer = transfer.reissue(id);
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
                    let snapshot = baseline::snapshot_world(sim);
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    std::thread::spawn(move || {
                        let _ = tx.send(baseline::transfer_from_snapshot(
                            snapshot,
                            id,
                            InterestEpoch(1),
                        ));
                    });
                    if let Some(link) = self.links.get_mut(&session.raw()) {
                        link.phase = Phase::Joining {
                            transfer_id: id,
                            queue: VecDeque::new(),
                            retries: 0,
                        };
                        self.pending_captures.insert(session.raw(), (id, rx));
                    }
                }
            }
            return;
        }

        // Confirmation of an in-flight transfer.
        let matches = matches!(&link.phase, Phase::Joining { transfer_id, .. } if *transfer_id == ack.transfer_id);
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
            }
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
    ///
    /// `split_transfer` is the out-of-band `BaselineWorld` for a giant split
    /// (T17 increment 2): a live client also gets it as an `Outbound::Baseline`
    /// right after the marker transaction; a joining client gets both queued.
    fn fan_out_transaction(
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
            match self.capture_for(sim, id) {
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
        let Some(world) = baseline::logical_brick_repair_patch(sim, req, self.backing_ref()) else {
            return;
        };
        let id = self.next_id();
        let cursor = JournalSeq(sim.journal_cursor());
        if let Ok(transfer) = baseline::transfer_from_world(world, id, InterestEpoch(1), cursor) {
            self.baseline_bytes += transfer.payload_bytes() as u64;
            send_to(clients, session, Outbound::Baseline(Arc::new(transfer)));
        }
    }

    /// Captures a baseline transfer at the current tick / journal cursor, over
    /// the logical brick set (evicted bricks filled from the residency
    /// backing when one is installed).
    fn capture_for(&mut self, sim: &Simulation, id: TransferId) -> Option<BaselineTransfer> {
        let cursor = JournalSeq(sim.journal_cursor());
        if let Some(cached) = &self.cached_baseline
            && cached.begin.journal_cursor == cursor
        {
            return Some(cached.reissue(id));
        }
        let transfer = baseline::logical_capture_transfer(
            sim,
            self.backing_ref(),
            id,
            InterestEpoch(1),
            cursor,
        )
        .ok()?;
        self.cached_baseline = Some(std::sync::Arc::new(transfer.reissue(id)));
        Some(transfer)
    }

    fn publish_ready_captures(&mut self, clients: &ClientMap) {
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
                    self.cached_baseline = Some(Arc::new(transfer.reissue(id)));
                    if let Some(link) = self.links.get(&raw) {
                        send_to(
                            clients,
                            link.session,
                            Outbound::Baseline(Arc::new(transfer)),
                        );
                    }
                }
                _ => {
                    self.failed += 1;
                    if let Some(link) = self.links.remove(&raw) {
                        send_to(clients, link.session, Outbound::Shutdown);
                    }
                }
            }
        }
    }
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
/// initial checkpoint. Any test-only `save_faults` are armed on the recovered
/// [`Writer`] *after* recovery and that initial checkpoint, then the writer is
/// handed to a [`PersistPipeline`] so every later durable write happens off the
/// simulation thread (ENG-50). `None` save path → no persistence.
///
/// This fails closed on corruption: only a genuinely empty database
/// ([`spall_store::StoreError::NoCheckpoint`]) is initialised with the built-in
/// scene. A recovery that reports corruption, or a database whose checkpoints
/// exist but do not decode, aborts startup without touching the file — losing
/// durable records requires an explicit operator choice
/// ([`persist::RecoveryChoice`]), not an automatic resume or reinitialisation.
fn setup_persistence(
    save: Option<&std::path::Path>,
    scene: Scene,
    cfg: &PersistConfig,
    save_faults: Option<spall_store::FaultPlan>,
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
                cfg,
                persist::RecoveryChoice::RequireClean,
                fixtures::stone_manifest(),
                AnchorPlane::at(0),
                PhysicsConfig::default(),
            )
            .map_err(|e| e.to_string())?;
            (sim, seq, 0)
        }
        // A genuinely new/empty database: seed it with the built-in scene.
        Err(spall_store::StoreError::NoCheckpoint) => {
            let sim = scene.simulation();
            let checkpoint = persist::capture(&sim, cfg, 0).map_err(|e| e.to_string())?;
            writer
                .publish_checkpoint(&checkpoint)
                .map_err(|e| e.to_string())?;
            (sim, 0, 1)
        }
        // Checkpoints exist but none decoded — corruption, not an empty DB. Do
        // not overwrite the save with a fresh scene.
        Err(e @ spall_store::StoreError::CheckpointsUnrecoverable(_)) => {
            return Err(format!(
                "world database at {} is unrecoverable and must not be overwritten: {e}; \
                 an operator must supply a verified recovery source",
                path.display()
            ));
        }
        Err(e) => return Err(e.to_string()),
    };
    // Test-only: arm the writer only now, so recovery and the first checkpoint
    // are unaffected and the fault falls on a later durable write.
    if let Some(faults) = save_faults {
        writer.set_faults(faults);
    }
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

/// A stable per-session actor id. Authority is the server's; this is only
/// journal provenance.
fn actor_for(session: SessionId) -> EntityId {
    EntityId::new(1 + u64::from(session.slot().0)).unwrap_or(EntityId::new(1).unwrap())
}

/// **Development-scenario path** (`ServeConfig::dev_unvalidated_actions`). Trusts
/// `claimed_target` / `claimed_brush` verbatim so a fixture harness can script
/// arbitrary cuts. `None` only for a genuinely unrepresentable target.
fn dev_intent_from_request(session: SessionId, req: &ActionRequest) -> Option<EditIntent> {
    let target = match req.claimed_target {
        ClaimedTarget::Terrain => EditTarget::Terrain,
        ClaimedTarget::Body(entity) => EditTarget::Body(entity),
    };
    let kind = match req.action {
        ActionKind::Cut => EditKind::Cut,
        ActionKind::Place => EditKind::Place(spall_voxel::fixtures::STONE),
    };
    Some(EditIntent {
        request_id: req.request_id,
        actor: actor_for(session),
        target,
        kind,
        brush: req.claimed_brush,
        explosion: None,
    })
}

// --- ENG-47: server-side action-claim validation -------------------------------

/// A server-approved tool. On the wire `ActionRequest::tool` is an opaque `u16`;
/// the server — not the client — decides what each id may do, how big a brush it
/// may request, and how far its aim reaches. Unknown ids are refused. Real
/// per-role tool grants are a later task; this is the minimum "action limits are
/// resolved on the server" (`docs/architecture.md` Editing).
#[derive(Debug, Clone, Copy)]
struct ToolSpec {
    /// The edit this tool performs on a validated hit.
    kind: EditKind,
    /// Largest brush radius this tool may request, in whole cells.
    max_radius_cells: i64,
    /// Farthest the aim ray may travel to a solid authoritative hit, in metres.
    reach_m: f64,
}

/// The built-in T10 tool catalog.
fn tool_spec(tool: u16) -> Option<ToolSpec> {
    match tool {
        // 0 — short-range cutter.
        0 => Some(ToolSpec {
            kind: EditKind::Cut,
            max_radius_cells: 8,
            reach_m: 12.0,
        }),
        // 1 — short-range stone placer.
        1 => Some(ToolSpec {
            kind: EditKind::Place(spall_voxel::fixtures::STONE),
            max_radius_cells: 4,
            reach_m: 8.0,
        }),
        _ => None,
    }
}

/// Why an [`ActionRequest`] was refused before it could become an [`EditIntent`].
/// The `&'static str` variants keep the `ActionStatus` reason strings stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionReject {
    UnknownTool(u16),
    ToolActionMismatch,
    RadiusTooLarge {
        requested_cells: i64,
        max_cells: i64,
    },
    BadAim,
    NoAuthoritativeHit,
    TargetMismatch,
    StaleTarget,
    Unsupported(&'static str),
}

impl ActionReject {
    fn reason(&self) -> String {
        match self {
            ActionReject::UnknownTool(t) => format!("tool {t} is not an approved tool"),
            ActionReject::ToolActionMismatch => {
                "requested action is not permitted for this tool".to_string()
            }
            ActionReject::RadiusTooLarge {
                requested_cells,
                max_cells,
            } => format!(
                "brush radius {requested_cells} cells exceeds this tool's limit of {max_cells}"
            ),
            ActionReject::BadAim => "aim origin/direction is not a usable ray".to_string(),
            ActionReject::NoAuthoritativeHit => {
                "aim does not hit authoritative geometry within reach".to_string()
            }
            ActionReject::TargetMismatch => {
                "claimed target does not match the authoritative hit".to_string()
            }
            ActionReject::StaleTarget => "claimed target no longer exists".to_string(),
            ActionReject::Unsupported(s) => s.to_string(),
        }
    }
}

/// One authoritative ray hit, resolved against committed geometry.
struct Struck {
    target: EditTarget,
    /// Struck solid cell, in the hit volume's **local cell space**.
    cell: GlobalCell,
    /// Empty cell against the struck face (where a placement lands), same space.
    placement_cell: GlobalCell,
    /// Distance from the aim origin, metres.
    t_m: f64,
}

/// The nearest solid cell an aim ray meets across the terrain and every detached
/// body, or `None` if nothing solid is within `reach_m`. This is the server's
/// authoritative hit: a client-supplied hit point / body id is only a claim
/// (`docs/architecture.md`: "The server determines the hit against its current
/// authoritative geometry").
fn nearest_hit(world: &SimWorld, ray: Ray, reach_m: f64) -> Option<Struck> {
    let mut best: Option<Struck> = None;
    let mut consider = |target: EditTarget, body: &Body| {
        let xform = body.pose.xform(body.cell_size());
        if let Ok(RayOutcome::Hit(hit)) = cast_ray_world(&body.volume, &xform, ray, reach_m)
            && hit.t <= reach_m
            && best.as_ref().is_none_or(|b| hit.t < b.t_m)
        {
            best = Some(Struck {
                target,
                cell: hit.cell,
                placement_cell: hit.placement_cell,
                t_m: hit.t,
            });
        }
    };
    consider(EditTarget::Terrain, world.terrain());
    for body in world.bodies() {
        if let Some(entity) = body.entity {
            consider(EditTarget::Body(entity), body);
        }
    }
    best
}

fn target_matches(claimed: ClaimedTarget, struck: EditTarget) -> bool {
    match (claimed, struck) {
        (ClaimedTarget::Terrain, EditTarget::Terrain) => true,
        (ClaimedTarget::Body(a), EditTarget::Body(b)) => a == b,
        _ => false,
    }
}

/// Validates an `ActionRequest`'s claims against the authoritative world and
/// turns it into an [`EditIntent`] whose brush is re-derived in the accepted
/// target frame from the server's own raycast — never copied from the client.
///
/// Checks, in order: the tool id is approved; the requested action fits the
/// tool; the requested radius is within the tool's cap; a named body still
/// exists; the aim ray hits solid authoritative geometry within reach; and the
/// struck volume matches `claimed_target`. The `session` is already known-live
/// (the caller rejects a superseded generation first).
fn resolve_intent(
    world: &SimWorld,
    session: SessionId,
    req: &ActionRequest,
) -> Result<EditIntent, ActionReject> {
    let spec = tool_spec(req.tool).ok_or(ActionReject::UnknownTool(req.tool))?;

    let action_fits = matches!(
        (req.action, spec.kind),
        (ActionKind::Cut, EditKind::Cut) | (ActionKind::Place, EditKind::Place(_))
    );
    if !action_fits {
        return Err(ActionReject::ToolActionMismatch);
    }

    let max_units = spec.max_radius_cells.saturating_mul(BRUSH_UNIT);
    let requested_units = req.claimed_brush.radius_units();
    if requested_units > max_units {
        return Err(ActionReject::RadiusTooLarge {
            requested_cells: requested_units / BRUSH_UNIT,
            max_cells: spec.max_radius_cells,
        });
    }

    // A named body must still be part of the authoritative world.
    if let ClaimedTarget::Body(entity) = req.claimed_target
        && world.body(entity).is_none()
    {
        return Err(ActionReject::StaleTarget);
    }

    let origin = DVec3::from_array(req.aim_origin_m);
    let dir = DVec3::new(
        req.aim_dir[0] as f64,
        req.aim_dir[1] as f64,
        req.aim_dir[2] as f64,
    );
    if !origin.is_finite() || !dir.is_finite() || dir.length_squared() < 1e-24 {
        return Err(ActionReject::BadAim);
    }

    let hit = nearest_hit(world, Ray::new(origin, dir), spec.reach_m)
        .ok_or(ActionReject::NoAuthoritativeHit)?;
    if !target_matches(req.claimed_target, hit.target) {
        return Err(ActionReject::TargetMismatch);
    }

    // Quantize the approved operation into the accepted target volume's local
    // integer cell space, centred on the server's own hit cell (the empty cell
    // against the struck face for a placement). The client's claimed centre is
    // discarded; its radius is kept only after passing the tool cap above.
    let base = match spec.kind {
        EditKind::Cut => hit.cell,
        EditKind::Place(_) => hit.placement_cell,
    };
    let h = BRUSH_UNIT / 2;
    let centre = BrushPoint::from_units(
        base.x.saturating_mul(BRUSH_UNIT).saturating_add(h),
        base.y.saturating_mul(BRUSH_UNIT).saturating_add(h),
        base.z.saturating_mul(BRUSH_UNIT).saturating_add(h),
    );
    let brush = SphereBrush::new(centre, requested_units.max(0)).map_err(|_| {
        ActionReject::RadiusTooLarge {
            requested_cells: requested_units / BRUSH_UNIT,
            max_cells: spec.max_radius_cells,
        }
    })?;

    Ok(EditIntent {
        request_id: req.request_id,
        actor: actor_for(session),
        target: hit.target,
        kind: spec.kind,
        brush,
        explosion: None,
    })
}

/// Per-tick admission counter, shared by the action and repair intake. Returns
/// `true` while `session` is still under `cap` for this tick and bumps its
/// count; `false` once the quota is spent. Each session's count is independent,
/// so one client's flood never consumes another's quota (ENG-48 fair
/// scheduling).
fn admit(counts: &mut HashMap<u64, u32>, session: u64, cap: u32) -> bool {
    let n = counts.entry(session).or_insert(0);
    if *n >= cap {
        return false;
    }
    *n += 1;
    true
}

/// T20: a [`BodyDigest`] per authoritative body **and** per player capsule,
/// keyed by entity id, for this motion batch. The body centre is its pose
/// origin and its interest radius is half the diagonal of the collider region
/// box — a conservative bound (the origin need not be the box centre), which
/// only ever keeps a body *more* relevant.
fn build_body_digests(world: &SimWorld) -> HashMap<u64, BodyDigest> {
    let mut digests: HashMap<u64, BodyDigest> = HashMap::new();
    for body in world.bodies() {
        let Some(entity) = body.entity else { continue };
        let (lo, hi) = body.collider_region;
        let cs = body.cell_size().metres();
        let ex = (hi.x - lo.x + 1) as f64 * cs;
        let ey = (hi.y - lo.y + 1) as f64 * cs;
        let ez = (hi.z - lo.z + 1) as f64 * cs;
        digests.insert(
            entity.get(),
            BodyDigest {
                key: entity.get(),
                center_m: body.pose.translation_m,
                radius_m: 0.5 * (ex * ex + ey * ey + ez * ez).sqrt(),
                is_player: false,
                sleeping: body.sleeping,
            },
        );
    }
    for player in world.players() {
        digests.insert(
            player.entity.get(),
            BodyDigest {
                key: player.entity.get(),
                center_m: player.state.position_m,
                radius_m: PLAYER_INTEREST_RADIUS_M,
                is_player: true,
                sleeping: false,
            },
        );
    }
    digests
}

/// Run totals for the T20 motion path, accumulated across every batch and
/// client. All zero (and untouched) unless `motion_interest` is configured.
#[derive(Debug, Default)]
struct MotionEgress {
    snapshots_sent: u64,
    interest_culled: u64,
    budget_deferred: u64,
    max_client_batch_bytes: u64,
}

/// T20: route one 20 Hz motion batch to each live client filtered by its
/// interest set and bandwidth budget. Committed topology is untouched — only
/// motion, which the next batch supersedes, is ever withheld here.
#[allow(clippy::too_many_arguments)]
fn dispatch_motion_by_interest(
    mi: &MotionInterest,
    scene: Scene,
    batch_index: u64,
    snaps: &[MotionSnapshot],
    sim: &Simulation,
    lj: &LateJoin,
    clients: &ClientMap,
    client_repl: &mut HashMap<u64, ClientReplication>,
    egress: &mut MotionEgress,
) {
    let world = sim.world();
    let digests = build_body_digests(world);
    let budget = MotionBudget {
        far_interval: mi.far_interval,
        per_batch_bytes: mi.per_client_budget_bytes,
    };

    for session in lj.live_sessions() {
        // The client's interest anchor: its own player capsule on a player
        // scene, else the configured static anchor, else unfiltered.
        let own_entity = scene.has_players().then(|| session_player_entity(session));
        let own_player_key = own_entity.map(|e| e.get());
        let anchor = match own_entity {
            Some(entity) => world
                .players()
                .find(|p| p.entity == entity)
                .map(|p| p.state.position_m),
            None => mi.static_anchor_m,
        };
        let interest = match anchor {
            Some(center_m) => InterestSet::Anchored {
                center_m,
                near_radius_m: mi.near_radius_m,
                far_radius_m: mi.far_radius_m,
            },
            None => InterestSet::Global,
        };

        let repl = client_repl.entry(session.raw()).or_default();
        repl.set_interest(interest);
        let kept = repl.select(batch_index, own_player_key, snaps, &digests, &budget);
        let outcome = repl.last_outcome();
        egress.snapshots_sent += outcome.kept;
        egress.interest_culled += outcome.interest_culled;
        egress.budget_deferred += outcome.budget_deferred;
        egress.max_client_batch_bytes = egress.max_client_batch_bytes.max(outcome.bytes as u64);
        if !kept.is_empty() {
            send_to(clients, session, Outbound::Motion(Arc::new(kept)));
        }
    }
}

/// Fans one message to every client. A client whose reliable backlog blew its
/// bound ([`OutboundOverflow`]) is dropped from the fan-out set here; its
/// writer task flushes the already-accepted backlog and then closes the
/// connection, so committed topology is never silently discarded.
fn broadcast(clients: &ClientMap, msg: Outbound) {
    let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
    let mut overflowed: Vec<u64> = Vec::new();
    for (id, handle) in guard.iter() {
        if handle.push(msg.clone()).is_err() {
            overflowed.push(*id);
        }
    }
    for id in overflowed {
        guard.remove(&id);
    }
}

fn send_to(clients: &ClientMap, session: SessionId, msg: Outbound) {
    let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = guard.get(&session.raw())
        && handle.push(msg).is_err()
    {
        guard.remove(&session.raw());
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

/// Returns a replayable status only for an action that passed authoritative
/// validation and was admitted to the simulation. Pre-admission validation and
/// rate-limit refusals intentionally are not held here: no edit was staged, so
/// a corrected retry is a new admission attempt (see `docs/protocol.md`).
fn replay_admitted_status(sim: &Simulation, request: RequestId) -> Option<ActionStatus> {
    sim.action_status(request).cloned()
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
    for part in transfer.parts.iter() {
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
    inbound: mpsc::Sender<Inbound>,
    handle: OutboundHandle,
    clients: ClientMap,
    conns: Arc<Mutex<HashMap<u64, Arc<Connection>>>>,
    egress_closed: Arc<Mutex<(u64, u64)>>,
    stop: watch::Receiver<bool>,
) {
    debug_assert_eq!(conn.role(), Role::Server);
    let session = conn.session();

    // Announce the join in order so the sim loop can supersede an earlier
    // session on the same slot (reconnect) and drive a late-join baseline.
    let _ = inbound.send(Inbound::Joined(session)).await;

    let liveness = tokio::spawn(conn.clone().run_liveness(stop.clone()));

    let reader = {
        let conn = conn.clone();
        let inbound = inbound.clone();
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
                            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    Ok(Some(WireRecord::RepairRequest(req))) => {
                        match inbound.try_send(Inbound::Repair(session, req)) {
                            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    Ok(Some(WireRecord::BaselineAck(ack))) => {
                        if inbound.send(Inbound::Baseline(session, ack)).await.is_err() {
                            break;
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
    'writer: loop {
        // Flush everything queued, in commit order, before parking.
        loop {
            let batch = handle.take();
            if batch.is_empty() && !batch.overflowed {
                break;
            }
            for msg in batch.reliable {
                let ok = match msg {
                    Outbound::Transaction(tx) => conn
                        .send_record(WireRecord::TopologyTransaction((*tx).clone()))
                        .await
                        .is_ok(),
                    Outbound::Status(s) => conn
                        .send_record(WireRecord::ActionStatus((*s).clone()))
                        .await
                        .is_ok(),
                    Outbound::Baseline(transfer) => send_baseline(&conn, &transfer).await,
                    // Motion is never queued as reliable; ignore defensively.
                    Outbound::Motion(_) => true,
                    Outbound::Shutdown => {
                        let _ = conn.say_bye("server complete").await;
                        false
                    }
                };
                if !ok {
                    break 'writer;
                }
            }
            if let Some(snaps) = batch.motion {
                for snap in snaps.iter() {
                    if conn.send_datagram(motion_seq, snap).await.is_err() {
                        break 'writer;
                    }
                    motion_seq += 1;
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
            _ = wait_true(stop.clone()) => break 'writer,
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
            acc.0 = acc.0.saturating_add(conn.stats().app_bytes_sent);
            acc.1 = acc.1.saturating_add(conn.transport_stats().udp_tx.bytes);
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
    use spall_store::{JournalPayload, JournalRecord};

    fn empty_clients() -> ClientMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn sess(slot: u32, generation: u32) -> SessionId {
        SessionId::from_parts(SlotId(slot), generation)
    }

    // --- ENG-47: action-claim validation --------------------------------------

    use spall_protocol::InputSeq;

    /// An `ActionRequest` with a controllable aim, tool, claimed target, and
    /// claimed brush. The brush centre is deliberately somewhere unrelated to
    /// the aim so a passing test proves the server re-derived it.
    fn action_req(
        tool: u16,
        action: ActionKind,
        origin_m: [f64; 3],
        dir: [f32; 3],
        claimed_target: ClaimedTarget,
        claimed_centre_cell: [i64; 3],
        claimed_radius_cells: i64,
    ) -> ActionRequest {
        let h = BRUSH_UNIT / 2;
        let brush = SphereBrush::new(
            BrushPoint::from_units(
                claimed_centre_cell[0] * BRUSH_UNIT + h,
                claimed_centre_cell[1] * BRUSH_UNIT + h,
                claimed_centre_cell[2] * BRUSH_UNIT + h,
            ),
            claimed_radius_cells * BRUSH_UNIT,
        )
        .expect("valid test brush");
        ActionRequest {
            request_id: RequestId(1),
            input_seq: InputSeq(1),
            action,
            tool,
            aim_origin_m: origin_m,
            aim_dir: dir,
            claimed_target,
            claimed_brush: brush,
        }
    }

    /// A `+X` aim from local cell `(0, 4, 1)` — inside the bridge scene's
    /// resident brick, above the floor — that travels to the column at local
    /// cell `(10, 4, 1)`. (An unbounded volume samples cells outside its
    /// resident bricks as `Unknown`, so the ray must *start* inside one.)
    const COLUMN_AIM_ORIGIN: [f64; 3] = [0.0, 1.0, 0.375];
    const PLUS_X: [f32; 3] = [1.0, 0.0, 0.0];

    fn bridge_after_cut() -> Simulation {
        use spall_sim::{EditIntent, EditTarget};
        let mut sim = Scene::BridgeCut.simulation();
        let h = BRUSH_UNIT / 2;
        let brush = SphereBrush::new(
            BrushPoint::from_units(10 * BRUSH_UNIT + h, 4 * BRUSH_UNIT + h, BRUSH_UNIT + h),
            2 * BRUSH_UNIT,
        )
        .unwrap();
        sim.submit(EditIntent::cut(
            RequestId(1),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            brush,
        ))
        .unwrap();
        // Enough ticks to commit the split; few enough that the freed beam has
        // not fallen far.
        sim.run_until_idle(8).unwrap();
        assert!(sim.world().body_count() >= 1, "the cut detached the beam");
        sim
    }

    #[test]
    fn a_valid_aim_resolves_to_a_server_derived_brush_on_the_struck_cell() {
        let sim = Scene::BridgeCut.simulation();
        // Claimed centre `(3, 1, 1)` is nowhere near the aim; the resolved brush
        // must sit on the server's hit cell `(10, 4, 1)` instead.
        let req = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [3, 1, 1],
            2,
        );
        let intent = resolve_intent(sim.world(), sess(0, 1), &req).expect("resolves");
        assert_eq!(intent.target, EditTarget::Terrain);
        assert_eq!(intent.kind, EditKind::Cut);
        let h = BRUSH_UNIT / 2;
        assert_eq!(intent.brush.centre.x, 10 * BRUSH_UNIT + h);
        assert_eq!(intent.brush.centre.y, 4 * BRUSH_UNIT + h);
        assert_eq!(intent.brush.centre.z, BRUSH_UNIT + h);
        assert_eq!(intent.brush.radius_units(), 2 * BRUSH_UNIT);
    }

    #[test]
    fn an_unknown_tool_is_refused() {
        let sim = Scene::BridgeCut.simulation();
        let req = action_req(
            77,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [10, 4, 1],
            2,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::UnknownTool(77))
        );
    }

    #[test]
    fn an_action_the_tool_does_not_grant_is_refused() {
        let sim = Scene::BridgeCut.simulation();
        // Tool 0 is a cutter; asking it to place is a mismatch.
        let req = action_req(
            0,
            ActionKind::Place,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [10, 4, 1],
            2,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::ToolActionMismatch)
        );
    }

    #[test]
    fn an_excessive_tool_radius_is_refused() {
        let sim = Scene::BridgeCut.simulation();
        // Tool 0 caps at 8 cells; the request asks for 40.
        let req = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [10, 4, 1],
            40,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::RadiusTooLarge {
                requested_cells: 40,
                max_cells: 8,
            })
        );
    }

    #[test]
    fn an_aim_that_hits_nothing_within_reach_is_refused() {
        let sim = Scene::BridgeCut.simulation();
        // Aim out of the scene (toward -X off the resident geometry): no solid.
        let req = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            [-1.0, 0.0, 0.0],
            ClaimedTarget::Terrain,
            [10, 4, 1],
            2,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::NoAuthoritativeHit)
        );
    }

    #[test]
    fn a_spoofed_target_that_disagrees_with_the_raycast_is_refused() {
        let sim = bridge_after_cut();
        let beam = sim
            .world()
            .bodies()
            .next()
            .and_then(|b| b.entity)
            .expect("the cut detached the beam");
        // Aim straight down onto the terrain floor at x-cell 1 (clear of both
        // the beam body's x-range and the cut column) but claim the real,
        // still-live beam body.
        let req = action_req(
            0,
            ActionKind::Cut,
            [0.375, 2.0, 0.375],
            [0.0, -1.0, 0.0],
            ClaimedTarget::Body(beam),
            [0, 0, 1],
            1,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::TargetMismatch)
        );
    }

    #[test]
    fn a_stale_target_body_is_refused() {
        let sim = Scene::BridgeCut.simulation();
        let req = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Body(EntityId::new(9999).unwrap()),
            [10, 4, 1],
            2,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::StaleTarget)
        );
    }

    #[test]
    fn a_body_hit_resolves_in_the_body_local_frame() {
        let sim = bridge_after_cut();
        let beam = sim
            .world()
            .bodies()
            .next()
            .and_then(|b| b.entity)
            .expect("the cut detached the beam");
        // Aim straight down at x-cell 5: clear of the (surviving) column top,
        // so the nearest solid is the detached beam body itself.
        let req = action_req(
            0,
            ActionKind::Cut,
            [1.375, 7.0, 0.375],
            [0.0, -1.0, 0.0],
            ClaimedTarget::Body(beam),
            [999, 999, 999],
            1,
        );
        let intent = resolve_intent(sim.world(), sess(0, 1), &req).expect("resolves");
        assert_eq!(intent.target, EditTarget::Body(beam));
        // Server-derived in the beam's local frame, not the client's
        // `(999, 999, 999)` claim.
        assert!(intent.brush.centre.x.abs() < 100 * BRUSH_UNIT);
    }

    #[test]
    fn the_dev_scenario_path_trusts_the_claim_verbatim() {
        let req = action_req(
            0,
            ActionKind::Cut,
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            ClaimedTarget::Terrain,
            [3, 1, 1],
            2,
        );
        let intent = dev_intent_from_request(sess(0, 1), &req).expect("dev passthrough");
        // No raycast, no re-derivation: exactly the claimed brush.
        assert_eq!(intent.brush, req.claimed_brush);
    }

    #[test]
    fn reliable_retry_after_reconnect_replays_before_stale_claim_validation() {
        let original_session = sess(0, 1);
        let replacement_session = sess(0, 2);
        let mut sim = Scene::BridgeCut.simulation();
        let mut request = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [3, 1, 1],
            2,
        );
        request.request_id = RequestId(57);

        let intent = resolve_intent(sim.world(), original_session, &request).unwrap();
        assert!(matches!(
            sim.submit(intent).unwrap().outcome,
            ActionOutcome::Queued
        ));
        sim.run_until_idle(8).unwrap();
        let committed = replay_admitted_status(&sim, request.request_id).unwrap();
        assert!(matches!(committed.outcome, ActionOutcome::Committed { .. }));

        // The original ray now points at removed column cells. A handler that
        // validated first would turn this reliable retry into a fresh rejection.
        assert!(
            resolve_intent(sim.world(), replacement_session, &request).is_err(),
            "the fixture must prove that commit changed the original claim's target"
        );
        assert_eq!(
            replay_admitted_status(&sim, request.request_id),
            Some(committed),
            "the replacement session receives the original committed status"
        );
    }

    #[test]
    fn pre_admission_validation_refusals_do_not_reserve_request_ids() {
        let sim = Scene::BridgeCut.simulation();
        let session = sess(0, 1);
        let rejected = action_req(
            77,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [10, 4, 1],
            2,
        );
        assert!(resolve_intent(sim.world(), session, &rejected).is_err());
        assert!(replay_admitted_status(&sim, rejected.request_id).is_none());

        // This is deliberately still eligible for validation/admission with a
        // corrected payload: only accepted pipeline work has a replay ledger.
        let corrected = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [10, 4, 1],
            2,
        );
        assert!(resolve_intent(sim.world(), session, &corrected).is_ok());
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
    fn simultaneous_joiners_reuse_the_same_immutable_baseline_at_one_cursor() {
        let sim = Scene::BridgeCut.simulation();
        let mut lj = LateJoin::new(DEFAULT_CATCH_UP_CAP, DEFAULT_MAX_JOIN_RETRIES);
        let first = lj.capture_for(&sim, TransferId(1)).unwrap();
        let second = lj.capture_for(&sim, TransferId(2)).unwrap();

        assert_eq!(first.begin.journal_cursor, second.begin.journal_cursor);
        assert_eq!(first.begin.transfer_id, TransferId(1));
        assert_eq!(second.begin.transfer_id, TransferId(2));
        assert!(std::sync::Arc::ptr_eq(&first.world, &second.world));
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
            lj.fan_out_transaction(tx(), None, &sim, &clients);
        }
        assert_eq!(lj.retries, 1);
        assert!(lj.links.contains_key(&joiner.raw()));

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

    #[test]
    fn per_tick_admission_caps_each_session_independently() {
        let mut counts: HashMap<u64, u32> = HashMap::new();
        let flooder = sess(0, 1).raw();
        let other = sess(1, 1).raw();

        let mut admitted = 0u32;
        for _ in 0..1_000 {
            if admit(&mut counts, flooder, MAX_ACTIONS_PER_CLIENT_PER_TICK) {
                admitted += 1;
            }
        }
        assert_eq!(
            admitted, MAX_ACTIONS_PER_CLIENT_PER_TICK,
            "one session's flood is capped for the tick"
        );

        // A different session still gets its full quota — fair scheduling, the
        // flood above did not consume it.
        let mut other_ok = 0u32;
        for _ in 0..1_000 {
            if admit(&mut counts, other, MAX_ACTIONS_PER_CLIENT_PER_TICK) {
                other_ok += 1;
            }
        }
        assert_eq!(other_ok, MAX_ACTIONS_PER_CLIENT_PER_TICK);
    }

    // --- ENG-36: recovery must fail closed on corruption -------------------

    fn persist_cfg() -> PersistConfig {
        PersistConfig {
            world_id: T10_WORLD_ID,
            seed: 42,
            generator_version: 1,
        }
    }

    fn scratch_db(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spall_eng36_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("world.db")
    }

    fn checkpoint_row_count(db: &std::path::Path) -> i64 {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM checkpoints", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn setup_persistence_seeds_only_a_genuinely_empty_database() {
        let db = scratch_db("empty");
        // Force the schema to exist with no checkpoint, exactly like a fresh DB.
        drop(Writer::open(&db).unwrap());
        assert_eq!(checkpoint_row_count(&db), 0);

        let Persistence {
            journalled_through,
            pipeline,
            checkpoints_published,
            ..
        } = setup_persistence(Some(&db), Scene::BridgeCut, &persist_cfg(), None).unwrap();
        assert_eq!(journalled_through, 0);
        assert!(pipeline.is_some());
        assert_eq!(
            checkpoints_published, 1,
            "the built-in scene is published once"
        );
        drop(pipeline);
        assert_eq!(checkpoint_row_count(&db), 1);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn setup_persistence_fails_closed_on_interior_journal_corruption() {
        let db = scratch_db("crc");
        {
            let mut w = Writer::open(&db).unwrap();
            let sim = Scene::BridgeCut.simulation();
            w.publish_checkpoint(&persist::capture(&sim, &persist_cfg(), 0).unwrap())
                .unwrap();
            w.append_journal(&[
                JournalRecord {
                    seq: 1,
                    tick: 1,
                    payload: JournalPayload::PoseBatch { snapshots: vec![] },
                },
                JournalRecord {
                    seq: 2,
                    tick: 2,
                    payload: JournalPayload::PoseBatch { snapshots: vec![] },
                },
                JournalRecord {
                    seq: 3,
                    tick: 3,
                    payload: JournalPayload::PoseBatch { snapshots: vec![] },
                },
            ])
            .unwrap();
        }
        // Corrupt seq 2's payload without fixing its CRC — an interior CRC error.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute("UPDATE journal SET payload = X'DEADBEEF' WHERE seq = 2", [])
                .unwrap();
        }

        let before = checkpoint_row_count(&db);
        let err = match setup_persistence(Some(&db), Scene::BridgeCut, &persist_cfg(), None) {
            Ok(_) => panic!("host startup must not silently continue from a shortened history"),
            Err(e) => e,
        };
        assert!(
            err.contains("corruption") || err.contains("RecoveryChoice"),
            "error surfaces the corruption and the required operator choice: {err}"
        );
        assert!(db.exists(), "the original database is left in place");
        assert_eq!(
            checkpoint_row_count(&db),
            before,
            "no fresh-scene checkpoint was published automatically"
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn setup_persistence_refuses_a_database_whose_checkpoints_do_not_decode() {
        let db = scratch_db("undecodable");
        {
            let mut w = Writer::open(&db).unwrap();
            let sim = Scene::BridgeCut.simulation();
            w.publish_checkpoint(&persist::capture(&sim, &persist_cfg(), 0).unwrap())
                .unwrap();
        }
        // Every complete checkpoint's metadata is now undecodable.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute("UPDATE checkpoints SET meta = X'DEADBEEF'", [])
                .unwrap();
        }

        let before = checkpoint_row_count(&db);
        assert_eq!(before, 1);
        let err = match setup_persistence(Some(&db), Scene::BridgeCut, &persist_cfg(), None) {
            Ok(_) => panic!("undecodable checkpoints are corruption, not an empty database"),
            Err(e) => e,
        };
        assert!(
            err.contains("unrecoverable"),
            "error distinguishes corruption from a fresh DB: {err}"
        );
        assert!(db.exists(), "the original database is left in place");
        assert_eq!(
            checkpoint_row_count(&db),
            before,
            "the corrupt save was not overwritten with the built-in scene"
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }
}

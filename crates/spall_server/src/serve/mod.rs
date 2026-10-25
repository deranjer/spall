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
//! commit, then tells every client goodbye and tears the endpoint down. When the tick
//! budget is what ends it, the run first lets regional clients drain
//! ([`HarnessConfig::drain_timeout`]): it keeps serving while one is still receiving a
//! catalogue, transactions or repair patches the server already owes it, so a
//! slow link is judged on whether it converges, not on how fast the ticks ran.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::action_catalog::ToolCatalog;
use crate::replication::{BodyDigest, ClientReplication, InterestSet, MotionBudget};
use crate::transfer_ids::{
    TransferIdAllocator, TransferIdError, TransferIdReservation, TransferNamespace,
};
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
    ClaimedTarget, Handshake, InputFrame, InterestEpoch, MotionSnapshot, NegotiatedLimits,
    PROTOCOL_VERSION, RepairRequest, RequestId, SessionId, SlotId, TopologyTransaction, TransferId,
    frame_input, recent_input, session_player_entity,
};
use spall_sim::fixtures::{
    G4_WORKLOAD_SPAWNS, SEPARATED_REGION_FAR_SPAWNS, SEPARATED_REGION_SPAWNS, WALK_ARENA_SPAWNS,
};
use spall_sim::world::TerrainColliderMode;
use spall_sim::{
    Body, EditIntent, EditKind, EditTarget, MotionPublisher, SimWorld, Simulation,
    SimulationConfig, action_statuses,
};
use spall_store::Writer;
use spall_structure::AnchorPlane;
use spall_voxel::{Ray, RayOutcome, cast_ray_world};
use tokio::sync::{Notify, mpsc, watch};

use crate::baseline::{self, BaselineTransfer};
use crate::commit_latency::{self, CommitLatency};
use crate::input_schedule::PlayerInputSchedule;
use crate::persist::{self, PersistConfig};
use crate::persist_pipeline::{PersistPipeline, PipelineConfig};

mod actions;
mod config;
mod connection;
mod creatures;
mod fanout;
mod fires;
mod health;
mod items;
mod late_join;
mod outbound;
mod persistence;
mod request_ids;
mod scene;
mod summary;
mod tick_loop;
mod workers;

use self::actions::*;
pub use self::config::*;
use self::connection::*;
use self::creatures::*;
use self::fanout::*;
use self::fires::*;
pub use self::fires::{FireRules, Fires};
use self::health::*;
use self::items::*;
pub use self::items::{ItemPickup, ItemPickupFn};
use self::late_join::*;
use self::outbound::*;
use self::persistence::*;
use self::request_ids::{RequestIds, SubmittedBy};
pub use self::scene::*;
pub use self::summary::*;
use self::tick_loop::*;
use self::workers::*;

#[cfg(test)]
mod test_support {
    use super::*;

    pub(super) fn empty_clients() -> ClientMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    pub(super) fn sess(slot: u32, generation: u32) -> SessionId {
        SessionId::from_parts(SlotId(slot), generation)
    }

    pub(super) fn bridge_after_cut() -> Simulation {
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
}
#[cfg(test)]
use test_support::*;

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

/// Default size of the background baseline-capture worker pool
/// ([`LateJoin`]'s `capture_pool`), when [`HostConfig::capture_workers`] is
/// left unset by a caller.
///
/// ENG-30 / T23 row 11, increment 30: increment 29 traced a deterministic
/// `t23-g4-join-budget` failure (a real ~30s+ span of *genuine* silence on
/// every connection, not a false idle read) to real CPU/scheduling
/// contention from eight concurrent, unbounded `std::thread::spawn` calls in
/// `LateJoin::on_baseline_ack` -- one per simultaneous late join -- plus
/// nine-plus OS processes (one server, eight clients) on one machine, and
/// flagged sizing a bounded pool without first measuring this environment's
/// real schedulable CPU budget as exactly the kind of unprincipled,
/// environment-fitted guess this project's convention asks not to ship.
///
/// This increment measured that budget with a standalone, dependency-free
/// CPU-bound thread sweep (`docs/reports/G3.md` increment 30 has the full
/// methodology and raw numbers) independent of the join-budget scenario
/// itself. Headline results on this environment (`available_parallelism()`
/// reports 16): per-worker completion time grows only ~12-21% over solo
/// execution through a quarter of that count, ~29% at half, and ~48% at the
/// full count, with throughput saturating almost exactly at
/// `available_parallelism()` workers and then flatlining (pure queueing, not
/// a lower real core count) -- i.e. the reported 16 logical cores are real
/// and genuinely schedulable in isolation. The isolated single-process probe
/// does not, however, exercise this workspace's actual multi-process shape:
/// the server and every client each build their own
/// `tokio::runtime::Builder::new_multi_thread()` at its own default worker
/// count (`available_parallelism()`, uncustomized anywhere in this
/// workspace), so a real `t23-g4-join-budget` run puts up to nine such
/// runtimes (~9 x 16 = up to 144 OS threads) on the same physical cores
/// before a single capture thread is even spawned -- confirmed by inspecting
/// every `Builder::new_multi_thread()` call site in this workspace
/// (`spall_server::serve`, `spall_client::net`, and `xtask`'s own harness
/// connections): none calls `.worker_threads(..)`, so all default to
/// `available_parallelism()`. Because background capture must share this
/// machine with that additional, unmeasured multi-process load -- not just
/// with itself -- this picks the
/// conservative quarter-of-parallelism point from the measured curve rather
/// than the fuller "still efficient" range the isolated probe alone would
/// suggest.
///
/// This is a general server-hygiene default (an unbounded thread-per-request
/// spawn is a known scalability antipattern regardless of any one sandbox's
/// contention), not a constant fitted to this one scenario -- see
/// `docs/reports/G3.md` increment 30 for whether it actually closes row 11.
pub fn default_capture_workers() -> usize {
    (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        / 4)
    .max(1)
}

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

/// Records a connection reader had to drop because the bridge to the sim loop was full, by
/// kind. A dropped action is answered with a retryable rejection; the others rely on the
/// client's own retry. Never silent: the counts are in the run summary.
#[derive(Debug, Default)]
pub(crate) struct InboundDrops {
    actions: std::sync::atomic::AtomicU64,
    repairs: std::sync::atomic::AtomicU64,
    progression: std::sync::atomic::AtomicU64,
    admin: std::sync::atomic::AtomicU64,
}

impl InboundDrops {
    /// Counts one drop and returns the new total for that kind; logs the first of each kind and
    /// every hundredth after it.
    fn note(&self, counter: &std::sync::atomic::AtomicU64, kind: &'static str) {
        let total = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if total == 1 || total.is_multiple_of(100) {
            tracing::warn!(kind, total, "inbound bridge full; dropped a client record");
        }
    }
}

/// Most inbound bridge records the sim loop drains in a single tick. The
/// remainder waits in the bounded channel for the next tick, so an ingress
/// burst can never prevent the drain loop from ending.
pub const MAX_INBOUND_PER_TICK: usize = 1024;

/// Server ticks between water keyframe sends (15 Hz at the 60 Hz tick).
pub const WATER_KEYFRAME_INTERVAL_TICKS: u64 = 4;

/// Server ticks between water status lines (10 s).
pub const WATER_STATUS_INTERVAL_TICKS: u64 = 600;

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

/// Most repair patches queued for one client and not yet written. A client on a slow link that
/// asks for more than its link can carry is not answered past this (its own request backoff
/// asks again later) rather than being given a backlog that overflows `MAX_RELIABLE_BACKLOG`
/// and ends its connection.
pub const MAX_QUEUED_REPAIRS_PER_CLIENT: usize = 96;
pub const MAX_PROGRESSION_PER_CLIENT_PER_TICK: u32 = 8;

/// Most reliable messages (committed topology, `ActionStatus`, baseline
/// transfers) that may sit unsent in one client's outbound queue before that
/// client is disconnected and left to re-baseline on reconnect. Committed
/// topology is never silently discarded to stay under budget
/// (`docs/protocol.md`: "repair or disconnect a client whose reliable backlog
/// exceeds the bounded window").
pub const MAX_RELIABLE_BACKLOG: usize = 2048;

/// Byte ceiling on that same per-client reliable queue.
pub const MAX_RELIABLE_BACKLOG_BYTES: usize = 8 * 1024 * 1024;

/// How long a regional client must have everything the server owes it, and have sent nothing,
/// before a run that spent its tick budget is allowed to end. A client with transactions held for
/// repairs asks again at most every four seconds (its request backoff tops out at eight times
/// 30 ticks), and a request deferred by the per-client patch queue cap is only repeated then, so
/// the grace has to outlast that, plus an impaired link's round trip.
const DRAIN_GRACE: Duration = Duration::from_secs(6);

/// Default for [`HarnessConfig::drain_timeout`].
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(300);

/// ENG-61: with [`HarnessConfig::await_body_settle`], how many consecutive ticks
/// every detached body's origin must hold still (< 1 mm/tick) — on top of being
/// asleep — before the run is allowed to stop on edit-quiescence. One second at
/// 60 Hz: long enough that the reported rest is real, short enough to keep the
/// gate fixture quick.
pub const AWAIT_SETTLE_STABLE_TICKS: u64 = 60;

/// Content-manifest tag both ends of a T10 session agree on out of band. Real
/// manifest negotiation is T16/T17; this keeps the handshake honest meanwhile.
pub const T10_CONTENT_TAG: &[u8] = b"spall-t10-bridge-v1";
/// World id used by the built-in T10 scene.
/// Creature motion frames go out every this many ticks (20 Hz at 60 Hz).
/// Snow frames sent per client every 12 ticks: the nearest tiles to the player, then a
/// cursor sweep of the rest. Enough to refill the ring in a few seconds after the player moves.
const SNOW_CENTER_PAGE_TILES: usize = 64;
const SNOW_RING_PAGE_TILES: usize = 16;
const CREATURE_MOTION_PERIOD_TICKS: u64 = 3;
/// Only creatures within this horizontal distance of a session's player are sent.
const CREATURE_MOTION_RADIUS_M: f64 = 320.0;

pub const T10_WORLD_ID: u128 = 0x5A11_0000_0000_7010;

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
    #[error("invalid server configuration: {0}")]
    Configuration(String),
}

/// Runs the networked host to completion and returns its summary. Builds its own
/// current-thread-free multi-thread Tokio runtime.
pub fn serve(config: ServeConfig) -> Result<ServeSummary, ServeError> {
    serve_with_game_content(
        config,
        ToolCatalog::default(),
        spall_sim::fixtures::stone_manifest(),
    )
}

/// Runs the networked host with a game-owned, versioned action catalog.
/// Engine callers can continue using [`serve`] for the legacy built-in rules.
pub fn serve_with_catalog(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
) -> Result<ServeSummary, ServeError> {
    serve_with_game_content(config, tool_catalog, spall_sim::fixtures::stone_manifest())
}

/// Runs the networked host with game-owned authoritative tools and material
/// definitions. The manifest is used for world creation, save recovery, and
/// handshake compatibility checks.
pub fn serve_with_game_content(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
    materials: spall_core::MaterialManifest,
) -> Result<ServeSummary, ServeError> {
    serve_with_game_content_and_optional_setup(
        config,
        tool_catalog,
        materials,
        None,
        Vec::new(),
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

/// A callback for game-owned entities that belong in a newly-created world.
/// It runs on the authoritative simulation thread before the initial
/// checkpoint is published; it is not run when an existing save is restored.
pub type InitialGameWorldSetup =
    Box<dyn FnOnce(&mut Simulation) -> Result<(), String> + Send + 'static>;

/// Game-owned observer invoked on the authoritative thread only after a
/// client edit commits. Material counts describe cells actually removed by
/// that committed cut, sampled from the validated pre-edit snapshot.
pub type CommittedEditHandler = Box<
    dyn FnMut(
            SessionId,
            Option<spall_protocol::PlayerId>,
            RequestId,
            &std::collections::BTreeMap<spall_core::MaterialId, u64>,
        ) + Send
        + 'static,
>;
/// Converts a committed harvest to opaque, versioned game bytes. The stable
/// identity and journal cursor are supplied by the server for durable replay.
pub type CommittedEditOutboxEncoder = Box<
    dyn FnMut(
            SessionId,
            Option<spall_protocol::PlayerId>,
            RequestId,
            spall_core::JournalSeq,
            &std::collections::BTreeMap<spall_core::MaterialId, u64>,
        ) -> Option<spall_store::OutboxRecord>
        + Send
        + 'static,
>;
/// Applies one durable game event idempotently to the game's progression store.
pub type OutboxProcessor =
    Box<dyn FnMut(&spall_store::OutboxRecord) -> Result<(), String> + Send + 'static>;
pub type ProgressionHandler = Box<
    dyn FnMut(
            SessionId,
            Option<spall_protocol::PlayerId>,
            spall_protocol::ProgressionRequest,
        ) -> spall_protocol::ProgressionResponse
        + Send
        + 'static,
>;

/// Runs the host with game content and an optional initial-world setup hook.
/// The hook runs only when creating a new world, before its initial durable
/// checkpoint, so restored saves never receive duplicate starter entities.
pub fn serve_with_game_content_and_setup(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
    materials: spall_core::MaterialManifest,
    setup: impl FnOnce(&mut Simulation) -> Result<(), String> + Send + 'static,
) -> Result<ServeSummary, ServeError> {
    serve_with_game_content_and_optional_setup(
        config,
        tool_catalog,
        materials,
        Some(Box::new(setup)),
        Vec::new(),
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

/// Runs the host with game content, optional new-world setup, and material
/// profiles for the opt-in authoritative contact-damage pass.
pub fn serve_with_game_content_and_policies(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
    materials: spall_core::MaterialManifest,
    profiles: Vec<(
        spall_core::MaterialId,
        spall_sim::ContactDamageMaterialProfile,
    )>,
    setup: Option<InitialGameWorldSetup>,
) -> Result<ServeSummary, ServeError> {
    serve_with_game_content_and_optional_setup(
        config,
        tool_catalog,
        materials,
        setup,
        profiles,
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

/// Same as `serve_with_game_content_and_policies`, with a game asset manifest
/// included in the exact client/server handshake content hash.
pub fn serve_with_game_content_and_asset_manifest(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
    materials: spall_core::MaterialManifest,
    profiles: Vec<(
        spall_core::MaterialId,
        spall_sim::ContactDamageMaterialProfile,
    )>,
    setup: Option<InitialGameWorldSetup>,
    asset_manifest_hash: spall_protocol::Hash32,
) -> Result<ServeSummary, ServeError> {
    serve_with_game_content_and_optional_setup(
        config,
        tool_catalog,
        materials,
        setup,
        profiles,
        Some(asset_manifest_hash),
        None,
        None,
        None,
        None,
        None,
    )
}

/// Game-owned callback for post-commit material rewards, with the optional
/// asset manifest included in handshake compatibility.
pub fn serve_with_game_content_and_commit_handler(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
    materials: spall_core::MaterialManifest,
    profiles: Vec<(
        spall_core::MaterialId,
        spall_sim::ContactDamageMaterialProfile,
    )>,
    setup: Option<InitialGameWorldSetup>,
    asset_manifest_hash: Option<spall_protocol::Hash32>,
    commit_handler: CommittedEditHandler,
) -> Result<ServeSummary, ServeError> {
    serve_with_game_content_and_optional_setup(
        config,
        tool_catalog,
        materials,
        setup,
        profiles,
        asset_manifest_hash,
        Some(commit_handler),
        None,
        None,
        None,
        None,
    )
}

/// Host with both post-commit harvest and authenticated progression handlers.
#[allow(clippy::too_many_arguments)] // Preserve the established game callback API.
pub fn serve_with_game_content_and_handlers(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
    materials: spall_core::MaterialManifest,
    profiles: Vec<(
        spall_core::MaterialId,
        spall_sim::ContactDamageMaterialProfile,
    )>,
    setup: Option<InitialGameWorldSetup>,
    asset_manifest_hash: Option<spall_protocol::Hash32>,
    commit_handler: CommittedEditHandler,
    progression_handler: ProgressionHandler,
) -> Result<ServeSummary, ServeError> {
    serve_with_game_content_and_optional_setup(
        config,
        tool_catalog,
        materials,
        setup,
        profiles,
        asset_manifest_hash,
        Some(commit_handler),
        Some(progression_handler),
        None,
        None,
        None,
    )
}

/// Same as [`serve_with_game_content_and_handlers`], but authenticates clients
/// using server-provisioned credentials, each bound to a stable `PlayerId`.
/// The shared development join token in `ServeConfig` is ignored on this path.
#[allow(clippy::too_many_arguments)] // Preserve the established game callback API.
pub fn serve_with_game_content_and_player_credentials(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
    materials: spall_core::MaterialManifest,
    profiles: Vec<(
        spall_core::MaterialId,
        spall_sim::ContactDamageMaterialProfile,
    )>,
    setup: Option<InitialGameWorldSetup>,
    asset_manifest_hash: Option<spall_protocol::Hash32>,
    commit_handler: CommittedEditHandler,
    progression_handler: ProgressionHandler,
    credentials: Vec<spall_net::PlayerCredential>,
) -> Result<ServeSummary, ServeError> {
    serve_with_game_content_and_optional_setup(
        config,
        tool_catalog,
        materials,
        setup,
        profiles,
        asset_manifest_hash,
        Some(commit_handler),
        Some(progression_handler),
        Some(credentials),
        None,
        None,
    )
}

/// Credential-authenticated host whose harvest events are transactionally
/// coupled to the world journal and replayed through the game's idempotent
/// outbox processor after restart.
#[allow(clippy::too_many_arguments)] // Preserve the established game callback API.
pub fn serve_with_game_content_and_player_credentials_and_outbox(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
    materials: spall_core::MaterialManifest,
    profiles: Vec<(
        spall_core::MaterialId,
        spall_sim::ContactDamageMaterialProfile,
    )>,
    setup: Option<InitialGameWorldSetup>,
    asset_manifest_hash: Option<spall_protocol::Hash32>,
    commit_handler: CommittedEditHandler,
    progression_handler: ProgressionHandler,
    credentials: Vec<spall_net::PlayerCredential>,
    encoder: CommittedEditOutboxEncoder,
    processor: OutboxProcessor,
) -> Result<ServeSummary, ServeError> {
    serve_with_game_content_and_optional_setup(
        config,
        tool_catalog,
        materials,
        setup,
        profiles,
        asset_manifest_hash,
        Some(commit_handler),
        Some(progression_handler),
        Some(credentials),
        Some(encoder),
        Some(processor),
    )
}

#[allow(clippy::too_many_arguments)] // One internal join point for existing entry points.
fn serve_with_game_content_and_optional_setup(
    config: ServeConfig,
    tool_catalog: ToolCatalog,
    materials: spall_core::MaterialManifest,
    game_setup: Option<InitialGameWorldSetup>,
    damage_profiles: Vec<(
        spall_core::MaterialId,
        spall_sim::ContactDamageMaterialProfile,
    )>,
    asset_manifest_hash: Option<spall_protocol::Hash32>,
    commit_handler: Option<CommittedEditHandler>,
    progression_handler: Option<ProgressionHandler>,
    player_credentials: Option<Vec<spall_net::PlayerCredential>>,
    outbox_encoder: Option<CommittedEditOutboxEncoder>,
    outbox_processor: Option<OutboxProcessor>,
) -> Result<ServeSummary, ServeError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ServeError::Runtime(e.to_string()))?;
    runtime.block_on(serve_async(
        config,
        tool_catalog,
        materials,
        game_setup,
        damage_profiles,
        asset_manifest_hash,
        commit_handler,
        progression_handler,
        player_credentials,
        outbox_encoder,
        outbox_processor,
    ))
}

fn server_handshake(
    materials: &spall_core::MaterialManifest,
    asset_manifest_hash: Option<spall_protocol::Hash32>,
    day_clock: spall_core::time_of_day::DayClock,
) -> Handshake {
    Handshake {
        protocol_version: PROTOCOL_VERSION,
        content_manifest_hash: asset_manifest_hash.map_or_else(
            || spall_protocol::content_manifest_hash(materials),
            |asset_hash| spall_protocol::content_manifest_hash_with_assets(materials, asset_hash),
        ),
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
        day_clock,
    }
}

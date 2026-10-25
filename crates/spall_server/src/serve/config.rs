use super::*;

/// Production server configuration for [`serve`]: what a real host decides.
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub listen: SocketAddr,
    pub scene: Scene,
    /// Derived terrain collision mode. Per-brick is the adopted default;
    /// whole-terrain is available for controlled legacy comparisons.
    pub terrain_collider_mode: TerrainColliderMode,
    pub join_token: JoinToken,
    /// Refuse connections past this many.
    pub max_clients: usize,
    /// Real-time pacing at 60 Hz. `false` runs ticks back to back (headless).
    pub paced: bool,
    pub log_json: PathBuf,
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
    /// ENG-30 / T23 row 11, increment 30: number of long-lived worker threads
    /// in the background baseline-capture pool (`LateJoin::on_baseline_ack`).
    /// Bounds how many captures run *concurrently* -- a request past this is
    /// queued, never rejected. Clamped to at least `1`. See
    /// [`default_capture_workers`] for how the default is derived and
    /// `docs/reports/G3.md` increment 30 for the measurement behind it.
    pub capture_workers: usize,
    /// **Development only.** Accept `AdminRequest`s (world reset) from any
    /// authenticated session. Off by default; a reset is still refused when the
    /// world is persisted or residency-managed, since neither can be rebuilt
    /// from the scene without discarding durable state.
    pub admin_commands: bool,
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
    /// T23 / G3 row 7, item 2: when residency is on, install a real
    /// disk-backed `crate::disk_backing::DiskBrickBacking` at this path
    /// instead of the in-process `MemoryBacking` default. `None` (the
    /// default) keeps the prior in-memory behaviour byte-identical;
    /// ignored when [`Self::residency`] is `None`.
    pub residency_disk_path: Option<PathBuf>,
    /// Game-owned creature director. `None` keeps the world free of animals. A
    /// director requires an ephemeral world (`save` unset): animals are not
    /// saved, and startup fails loudly rather than drop them.
    pub creatures: Option<crate::creature_service::CreatureService>,
    /// Game-owned posed voxels of a creature. With it, a cutting tool's strike
    /// that meets a creature before any geometry kills it and leaves a carcass
    /// of those voxels in the terrain. Ignored without `creatures`.
    pub creature_shape: Option<spall_sim::CreatureShape>,
    /// The game's pack for E-key world item pickup (ENG-257). `None`: items
    /// are still replicated and drawn, but nothing can be picked up.
    pub item_pickup: Option<ItemPickup>,
    /// The game's tool rules (ENG-259): who may swing which tool. `None`:
    /// every catalog tool is open to every player.
    pub tool_hooks: Option<ToolHooks>,
    /// T21 / ENG-28 increment 4 (3c): default-off contact-to-terrain/body
    /// damage pass. `None` keeps every prior run byte-identical (the pass is
    /// never invoked, exactly like every scene before this increment).
    /// `Some(_)` runs [`spall_sim::Simulation::apply_contact_damage`] every
    /// tick with this config, so a hard enough impact carves a cut into
    /// terrain or a struck body — a normal committed transaction that changes
    /// the committed hash. Never enable on a fixture a hash-replaying gate
    /// scenario depends on unless that scenario has accepted the resulting
    /// hash (`docs/reports/ENG-28-increment-3-handoff.md` "3c").
    pub contact_damage: Option<spall_sim::ContactDamageConfig>,
    /// T21 / ENG-28 increment 4 (3c): default-off region-dormancy pass. `None`
    /// keeps every body live in physics forever, byte-identical to every
    /// prior run. `Some(_)` runs [`spall_sim::Simulation::apply_dormancy`]
    /// every tick with this config: a settled body with no active region
    /// nearby is deactivated (dropped from the physics step, record kept),
    /// and a dormant body a player or edit approaches is reactivated.
    /// Dormancy never changes authoritative geometry, ownership, damage, or
    /// `world_hash`, but a deactivated body **leaves the live physics world**
    /// — a gate scenario that reads physics state directly (e.g.
    /// [`HarnessConfig::await_body_settle`]'s `max_penetration_m`) must keep this off,
    /// which is why it defaults to `None` for every existing gate fixture.
    pub dormancy: Option<spall_sim::DormancyConfig>,
    /// Optional credential registry path polled for atomic rotation/revocation
    /// updates. Only used by player-credential authentication.
    pub credential_registry_file: Option<PathBuf>,
    /// The world and player spawns for [`Scene::Custom`]; ignored for every
    /// other scene. `serve` fails fast if `scene` is `Custom` and this is `None`.
    pub custom_world: Option<CustomWorld>,
}

/// Bounded-run and test-harness concerns for [`serve`]: run limits, scripted
/// output files, fault injection, and development-only validation bypasses.
/// [`HarnessConfig::default`] is the bounded headless run that
/// [`ServeConfig::headless`] has always used.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Hard cap on server ticks for this run.
    pub max_ticks: u64,
    /// Stop early after the pipeline is idle this many consecutive ticks
    /// (once at least one transaction has committed). `0` disables early stop.
    pub quiescence_ticks: u64,
    /// Once `max_ticks` is spent, keep serving for up to this long while a regional client is
    /// still receiving what the server owes it (see the module docs). `0` ends the run at
    /// `max_ticks` regardless. Hitting the limit is reported in the summary
    /// (`drain_timed_out`), never silent.
    pub drain_timeout: Duration,
    /// Wait for this many clients before the tick loop starts.
    pub min_clients: usize,
    /// Fail if `min_clients` have not connected within this long.
    pub startup_timeout: Duration,
    pub summary_json: Option<PathBuf>,
    /// Write the server certificate fingerprint (hex) here for clients.
    pub fingerprint_out: Option<PathBuf>,
    /// Write the OS-resolved bound `ip:port` here once listening.
    pub addr_out: Option<PathBuf>,
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
    /// Optional bounded timing window for the owning server tick. Warmup ticks
    /// are excluded; measured ticks include ingress through replication and
    /// residency, ending immediately before pacing sleep.
    pub timing_window: Option<TimingWindow>,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            max_ticks: 1_200,
            quiescence_ticks: 45,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
            min_clients: 1,
            startup_timeout: Duration::from_secs(15),
            summary_json: None,
            fingerprint_out: None,
            addr_out: None,
            save_faults: None,
            dev_unvalidated_actions: false,
            await_body_settle: false,
            timing_window: None,
        }
    }
}

/// Inputs to [`serve`]: the production [`HostConfig`] plus the
/// [`HarnessConfig`] that bounds and instruments a run.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub server: HostConfig,
    pub harness: HarnessConfig,
}

/// A bounded, explicit server timing window. The server records at most
/// `max_samples` observations while still reporting whether the requested
/// measured tick window completed. A caller that wants acceptance evidence
/// should set `max_samples >= measured_ticks`.
#[derive(Debug, Clone, Copy)]
pub struct TimingWindow {
    pub warmup_ticks: u64,
    pub measured_ticks: u64,
    pub max_samples: usize,
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
pub(super) const PLAYER_INTEREST_RADIUS_M: f64 = 1.0;

impl ServeConfig {
    /// A headless bounded config for `scene` on `listen` with `token`.
    pub fn headless(listen: SocketAddr, scene: Scene, token: JoinToken) -> Self {
        Self {
            server: HostConfig {
                listen,
                scene,
                terrain_collider_mode: TerrainColliderMode::PerBrick,
                join_token: token,
                max_clients: 8,
                paced: false,
                log_json: PathBuf::from(".local/runs/server.jsonl"),
                transport: TransportConfig::for_tests(),
                save: None,
                checkpoint_interval_ticks: 1_800,
                seed: 0,
                catch_up_cap: DEFAULT_CATCH_UP_CAP,
                max_join_retries: DEFAULT_MAX_JOIN_RETRIES,
                capture_workers: default_capture_workers(),
                admin_commands: false,
                motion_interest: None,
                residency: None,
                residency_disk_path: None,
                creatures: None,
                creature_shape: None,
                item_pickup: None,
                tool_hooks: None,
                contact_damage: None,
                dormancy: None,
                credential_registry_file: None,
                custom_world: None,
            },
            harness: HarnessConfig::default(),
        }
    }
}

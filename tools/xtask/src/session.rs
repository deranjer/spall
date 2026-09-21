//! `cargo xtask session` / `cargo xtask scenario` — the T10 replication harness.
//!
//! Spawns one `sandbox-server --serve` process and N `sandbox-client --connect`
//! processes as real OS processes talking over real QUIC (optionally through
//! per-client [`spall_net::UdpProxy`]s that drop / delay / reorder *encrypted*
//! packets). Each client scripts cuts from a scenario file, replicates the
//! authoritative topology, and writes a summary. The harness passes only if the
//! server and every client agree on the final canonical topology hash.

use crate::g4;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use clap::Args;
use serde::{Deserialize, Serialize};
use spall_net::{PacketFaultPlan, UdpProxy};

use crate::{XtaskError, run_cargo, sandbox_binary_profile};

/// `cargo xtask session` — run an explicit scenario file.
#[derive(Debug, Args)]
pub struct SessionArgs {
    /// Scenario JSON (see `fixtures/scenarios/*.json`).
    #[arg(long)]
    scenario: PathBuf,
    /// Override the client count from the scenario.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=16))]
    clients: Option<u64>,
    /// Encrypted-packet loss applied by a per-client proxy, percent. `0` skips
    /// the proxy entirely.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=90))]
    loss_percent: u8,
    /// Deterministic proxy seed.
    #[arg(long, default_value_t = 0x5A11_0000_0000_0010)]
    seed: u64,
    /// Override the server tick budget from the scenario.
    #[arg(long)]
    server_ticks: Option<u64>,
    /// Whole-run deadline in milliseconds. The upper bound covers the T23 /
    /// G3 row 13 sustained-soak scenarios (`sustained_edits`), the longest of
    /// which (`t23-g4-soak-30min`) needs a 30 s warmup + 30 measured minutes +
    /// a settle buffer — comfortably under an hour but well past a "normal"
    /// scenario's few minutes.
    #[arg(long, default_value_t = 60_000, value_parser = clap::value_parser!(u64).range(2_000..=3_600_000))]
    timeout_ms: u64,
    /// Output directory. A unique `.local/runs` directory is created if omitted.
    #[arg(long)]
    output: Option<PathBuf>,
}

/// `cargo xtask scenario` — run a named built-in scenario
/// (`fixtures/scenarios/<name>.json`).
#[derive(Debug, Args)]
pub struct ScenarioArgs {
    #[arg(long, default_value = "tower-cut")]
    name: String,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=16))]
    clients: Option<u64>,
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=90))]
    loss_percent: u8,
    #[arg(long, default_value_t = 0x5A11_0000_0000_0011)]
    seed: u64,
    #[arg(long)]
    server_ticks: Option<u64>,
    /// Whole-run deadline in milliseconds — see `SessionArgs::timeout_ms` for
    /// why the upper bound is an hour, not the usual few minutes.
    #[arg(long, default_value_t = 60_000, value_parser = clap::value_parser!(u64).range(2_000..=3_600_000))]
    timeout_ms: u64,
    #[arg(long)]
    output: Option<PathBuf>,
}

/// Normalised run parameters.
struct Run {
    scenario_path: PathBuf,
    clients: Option<u64>,
    loss_percent: u8,
    seed: u64,
    server_ticks: Option<u64>,
    timeout: Duration,
    output: Option<PathBuf>,
}

impl From<SessionArgs> for Run {
    fn from(a: SessionArgs) -> Self {
        Self {
            scenario_path: a.scenario,
            clients: a.clients,
            loss_percent: a.loss_percent,
            seed: a.seed,
            server_ticks: a.server_ticks,
            timeout: Duration::from_millis(a.timeout_ms),
            output: a.output,
        }
    }
}

pub fn run_session(
    args: SessionArgs,
    unique_output: impl FnOnce() -> PathBuf,
) -> Result<(), XtaskError> {
    run(args.into(), unique_output)
}

pub fn run_scenario(
    args: ScenarioArgs,
    unique_output: impl FnOnce() -> PathBuf,
) -> Result<(), XtaskError> {
    let scenario_path = crate::workspace_root()
        .join("fixtures/scenarios")
        .join(format!("{}.json", args.name));
    if !scenario_path.exists() {
        return Err(XtaskError::Capability(format!(
            "no built-in scenario `{}` at {}",
            args.name,
            scenario_path.display()
        )));
    }
    run(
        Run {
            scenario_path,
            clients: args.clients,
            loss_percent: args.loss_percent,
            seed: args.seed,
            server_ticks: args.server_ticks,
            timeout: Duration::from_millis(args.timeout_ms),
            output: args.output,
        },
        unique_output,
    )
}

// --- scenario file --------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Scenario {
    #[serde(default)]
    name: String,
    /// Built-in scene both the server and the live clients install:
    /// `bridge-cut` (default) or `cross-bridge-cut`.
    #[serde(default = "default_scene")]
    scene: String,
    /// Build and run optimized binaries for CPU-heavy performance gates.
    #[serde(default)]
    release_profile: bool,
    server_ticks: u64,
    /// Consecutive idle ticks before the server stops early. A gate fixture with
    /// widely-spaced scripted cuts under an impaired transport needs a larger
    /// window so a late action still lands before shutdown.
    #[serde(default = "default_quiescence")]
    quiescence_ticks: u64,
    #[serde(default = "one")]
    clients: u64,
    #[serde(default)]
    cuts: Vec<CutSpec>,
    /// T17: client indices that pull a full late-join baseline over a bulk
    /// transfer instead of installing the fixed scene.
    #[serde(default)]
    late_join_clients: Vec<u64>,
    /// T17: how long a late-join client waits before connecting, so it arrives
    /// after the early clients have started cutting.
    #[serde(default = "default_late_delay")]
    late_join_connect_delay_ms: u64,
    /// Minimum authoritative work required by a gate fixture. The harness also
    /// independently requires that every scripted cut committed, so a run that
    /// quiesces early (before the whole script fires) fails regardless of this.
    #[serde(default)]
    minimum_transactions: u64,
    /// Minimum motion samples each client must receive.
    #[serde(default)]
    minimum_motion_snapshots: u64,
    /// Minimum motion datagrams each live client must have received *out of
    /// `snapshot_seq` order*. Only enforced on a run whose per-client UDP proxy
    /// is active (`--loss-percent > 0`): it proves the "reordered snapshots"
    /// half of the T11 requirement was actually exercised, not just claimed.
    #[serde(default)]
    minimum_reordered_snapshots: u64,
    /// Minimum number of distinct bricks the largest detached body must span at
    /// end of run. `>= 2` proves a body's cells were transferred out of terrain
    /// across a brick boundary (cross-brick ownership transfer). `0` disables.
    #[serde(default)]
    minimum_detached_body_brick_span: u64,
    /// Journal every committed transaction and, after the run, replay the whole
    /// topology-event stream from the tick-0 baseline; the rebuilt canonical
    /// hash must equal the live run's agreed hash (T11 exact replay).
    #[serde(default)]
    replay_check: bool,
    /// T23 / G3: after the run, stop the server, launch a **fresh**
    /// `sandbox-server --serve --save` over the same world DB (a cold restart
    /// that recovers from the shutdown checkpoint + durable journal), and
    /// require its recovered `final_world_hash` to equal the agreed hash. A
    /// fresh `--late-join` client then connects to the restarted server and must
    /// converge to the same hash — the recovered world serves a correct
    /// baseline. Implies `replay_check`'s world DB (`--save`).
    #[serde(default)]
    restart_check: bool,
    /// T23 / G3 row 10: a `late_join_clients` replica is allowed to end in a
    /// **bounded explicit failure** (`result: "join-failed"`, a clean non-hang
    /// exit) instead of converging — as long as it did not hang and the live
    /// (non-late) clients plus the server still agree on one hash. Models
    /// "retry/catch-up stress terminates with either a successful join or a
    /// bounded explicit failure while connected clients continue".
    #[serde(default)]
    late_join_may_fail: bool,
    /// T23 / G3 row 7, slice D: run the server with `--residency-budget-bricks`
    /// set — evict terrain bricks outside every player's interest box, reload on
    /// demand for edits. The committed world / agreed hash is unchanged; a run
    /// with this set exercises the eviction path end to end.
    #[serde(default)]
    residency_budget_bricks: Option<usize>,
    /// Chebyshev radius (bricks) of the kept-resident box around each player
    /// when `residency_budget_bricks` is set. Defaults to the server's default.
    #[serde(default)]
    residency_radius_bricks: Option<i64>,
    /// ENG-30 row 7 increment 13: hard ceiling on resident terrain dense
    /// bytes (`--residency-budget-dense-bytes`), enforced the same way as
    /// `residency_budget_bricks` — an interest-driven, non-pinned reload is
    /// deferred rather than admitted past it. Only meaningful when
    /// `residency_budget_bricks` is set; absent (the default) leaves the cap
    /// disabled, preserving every existing scenario's exact prior behavior.
    #[serde(default)]
    residency_budget_dense_bytes: Option<u64>,
    /// T23 / G3 row 7 item 2 (increment 31): back the server's residency pass
    /// with a real on-disk `DiskBrickBacking` (`--residency-disk-backing`)
    /// instead of the in-process `MemoryBacking` default. Only meaningful
    /// when `residency_budget_bricks` is set; ignored otherwise. `false`
    /// (absent, the default) preserves every existing scenario's exact prior
    /// behavior. When set alongside `restart_check`, the cold-restarted
    /// server is also launched with matching residency + disk-backing flags
    /// (pointed at the same `<world>/residency.db`), proving a restart reads
    /// the durable backing back correctly -- other scenarios' restart runs
    /// are unaffected since this only activates when the flag is set.
    #[serde(default)]
    residency_disk_backing: bool,
    /// T23 / G3 row 7, slice E2: run each **movement-scripted** client with
    /// `--residency-budget-bricks` — the replica evicts terrain outside a brick
    /// box around its predicted player and pulls it back with repair requests
    /// as the player returns (row 8b traverse-away-and-back). The committed
    /// world / agreed hash is unchanged.
    #[serde(default)]
    client_residency_budget_bricks: Option<usize>,
    /// Chebyshev radius (bricks) kept resident around each scripted player when
    /// `client_residency_budget_bricks` is set. Defaults to the client's default.
    #[serde(default)]
    client_residency_radius_bricks: Option<i64>,
    /// ENG-30 row 7 increment 14: hard ceiling on resident terrain dense bytes
    /// for a movement-scripted client (`--residency-budget-dense-bytes`),
    /// enforced the same way as `client_residency_budget_bricks` — an
    /// interest-driven (box-driven) reload back into the tracked box is
    /// deferred rather than admitted past it. Only meaningful when
    /// `client_residency_budget_bricks` is set; absent (the default) leaves
    /// the cap disabled, preserving every existing scenario's exact prior
    /// behavior.
    #[serde(default)]
    client_residency_budget_dense_bytes: Option<u64>,
    /// Enforced end-to-end proof that residency actually ran during this
    /// scenario. A configured block makes zero/default counters a failure.
    #[serde(default)]
    residency_assertions: Option<ResidencyAssertions>,
    /// T23 / G3 row 14: run the server with `--motion-interest` — per-client
    /// interest relevance + motion bandwidth budget (T20). `None` keeps the
    /// pre-T20 unfiltered broadcast.
    #[serde(default)]
    motion_interest: Option<MotionInterestSpec>,
    /// T23 / G3 row 13: a programmatically generated, sustained edit +
    /// blast stream — too long to hand-author as individual `cuts` entries
    /// (a 30-minute soak at the gate's rates is 18,000 small edits + 180
    /// blasts). See `generate_sustained_cuts`.
    #[serde(default)]
    sustained_edits: Option<SustainedEdits>,
    /// Minimum straight-line distance (metres) some replicated body must have
    /// travelled on every live client — proof the detached geometry actually
    /// moved, not merely that a (possibly stationary) snapshot arrived.
    #[serde(default)]
    minimum_body_displacement_m: f64,
    /// ENG-61: require the server to report every detached body *at rest* on the
    /// remaining structure at end of run. Also passes `--await-body-settle` to
    /// the server so the run continues past edit-quiescence until the body
    /// sleeps (bounded by `server_ticks`).
    #[serde(default)]
    require_body_settled: bool,
    /// Largest final linear speed (m/s) a detached body may have and still count
    /// as settled.
    #[serde(default = "default_settle_speed_eps")]
    body_settle_speed_epsilon_m_s: f64,
    /// Consecutive final ticks the detached bodies' origins must have held still
    /// (< 1 mm/tick) for the "position stable for N ticks" check.
    #[serde(default = "default_settle_stable_ticks")]
    body_settle_min_stable_ticks: u64,
    /// Deepest contact penetration (m) tolerated at end of run — a body that
    /// settled *through* the floor rather than on it exceeds this.
    #[serde(default = "default_settle_penetration_m")]
    body_settle_max_penetration_m: f64,
    /// T19: scripted movement paths keyed by client index. A client with a path
    /// predicts a player capsule; its `movement` summary is checked against
    /// `movement`.
    #[serde(default)]
    player_paths: Vec<PlayerPath>,
    #[serde(default)]
    movement: MovementAcceptance,
    /// T11a / ENG-62: when set, assert the server's measured commit-latency p95
    /// (per commit shape) against the G1 gate targets. A bucket the run did not
    /// exercise (0 samples) is not asserted.
    #[serde(default)]
    latency_targets: Option<LatencyTargets>,
    /// T23 / G4: opt-in bounded owning-server timing window. Warmup ticks are
    /// excluded; all requested measured ticks must complete and retain samples
    /// before any configured target can pass.
    #[serde(default)]
    server_timing: Option<ServerTiming>,
    /// T23 / G4: fail-closed evaluation of the server's measured telemetry
    /// (warmup-excluded per-client egress, blast backlog recovery, memory,
    /// body populations, join/baseline concurrency). See `g4.rs`.
    #[serde(default)]
    g4_telemetry: Option<g4::G4Telemetry>,
    /// T23 / G4: one network impairment applied to **every** client (contrast
    /// `join_budget`, which shapes only one).
    #[serde(default)]
    network_envelope: Option<NetworkEnvelope>,
    /// T23 / G4: pace each connection's baseline bulk transfer (the
    /// `1 MiB/s/client` baseline budget).
    #[serde(default)]
    baseline_rate_limit_bytes_per_sec: Option<u64>,
    /// T23 / G4 overload: the server's admission cap on simultaneously live
    /// clients (default: the scenario's `clients`).
    #[serde(default)]
    server_max_clients: Option<usize>,
    /// T23 / G4 overload: the generated edit stream is spread over only the first
    /// this-many clients (default: all `clients`), so clients the server will
    /// refuse are not assigned edits.
    #[serde(default)]
    edit_clients: Option<u64>,
    /// Pass `--wake-audit` (per-reason wake accounting in the server summary).
    #[serde(default)]
    wake_audit: bool,
    /// Diagnostic: raise the clients' QUIC handshake timeout (ms). Unset = the
    /// production 5 s.
    #[serde(default)]
    client_handshake_timeout_ms: Option<u64>,
    /// T21 / ENG-28 increment 4 (3c): run the server with `--dormancy` — a
    /// settled body with nothing active nearby deactivates, and a dormant body
    /// a player or edit approaches reactivates. Never combine with
    /// `require_body_settled`: a deactivated body leaves the live physics
    /// world that reads from.
    #[serde(default)]
    dormancy: bool,
    /// T21 / ENG-28 increment 4: require the server's end-of-run report to
    /// show at least this many dormancy deactivations / reactivations —
    /// real end-to-end proof the pass ran, not just that `dormancy` was set.
    #[serde(default)]
    dormancy_assertions: Option<DormancyAssertions>,
    /// T23 / G3 row 11: when set, the named `late_join_clients` entry connects
    /// through a shaped proxy (bandwidth + RTT + loss) instead of the plain
    /// per-`loss_percent` one, and its measured compressed baseline size /
    /// time-to-ready are asserted against the configured budget. Every other
    /// client gets an unshaped (transparent) proxy so the workload that builds
    /// the edit history is not itself bandwidth-limited.
    #[serde(default)]
    join_budget: Option<JoinBudget>,
    /// T23 / G3 row 10 follow-up: unlike `late_join_may_fail`'s existing
    /// delayed-connect fixture (proves explicit-failure handling for a client
    /// that never gets a baseline at all), this exercises **retry/catch-up
    /// exhaustion while connected clients stay active** — the named
    /// `late_join_clients` entry connects and starts joining normally, but a
    /// severely shaped proxy plus a tiny server-side `catch_up_cap` /
    /// `max_join_retries` make its catch-up queue overflow every recapture
    /// until its retry budget is spent, ending in the same bounded
    /// `join-failed` (exit 4) `late_join_may_fail` already accepts — while
    /// `sustained_edits` keeps the other, live clients continuously
    /// committing (the "connected clients keep running" half this exercises).
    #[serde(default)]
    retry_exhaustion: Option<RetryExhaustion>,
}

/// See `Scenario::retry_exhaustion`.
#[derive(Debug, Clone, Deserialize)]
struct RetryExhaustion {
    /// Which `late_join_clients` entry gets the shaped proxy + is expected to
    /// exhaust its retries.
    #[serde(default)]
    client: u64,
    #[serde(default = "default_retry_exhaustion_catch_up_cap")]
    catch_up_cap: usize,
    #[serde(default = "default_retry_exhaustion_max_retries")]
    max_join_retries: u32,
    #[serde(default = "default_retry_exhaustion_bandwidth")]
    bandwidth_bytes_per_sec: u64,
    #[serde(default = "default_retry_exhaustion_rtt_ms")]
    rtt_ms: u64,
    #[serde(default = "default_join_budget_seed")]
    seed: u64,
}

fn default_retry_exhaustion_catch_up_cap() -> usize {
    1
}
fn default_retry_exhaustion_max_retries() -> u32 {
    2
}
fn default_retry_exhaustion_bandwidth() -> u64 {
    256
}
fn default_retry_exhaustion_rtt_ms() -> u64 {
    50
}

/// T23 / G3 row 11 join-budget network profile + acceptance budget. Defaults
/// are the `docs/validation.md` G3 numbers: "dependency-complete near-player
/// baseline <=16 MiB compressed, ready within 30 seconds on an imposed
/// 1 MiB/s transfer budget with 100 ms RTT and 2% packet loss".
#[derive(Debug, Clone, Deserialize)]
struct JoinBudget {
    /// Which `late_join_clients` entry this budget is measured against.
    #[serde(default)]
    client: u64,
    #[serde(default = "default_join_budget_bandwidth")]
    bandwidth_bytes_per_sec: u64,
    #[serde(default = "default_join_budget_rtt_ms")]
    rtt_ms: u64,
    #[serde(default = "default_join_budget_jitter_ms")]
    jitter_ms: u64,
    #[serde(default = "default_join_budget_loss_percent")]
    loss_percent: u8,
    #[serde(default = "default_join_budget_seed")]
    seed: u64,
    #[serde(default = "default_max_baseline_compressed_bytes")]
    max_baseline_compressed_bytes: u64,
    #[serde(default = "default_max_ready_ms")]
    max_ready_ms: u64,
}

fn default_join_budget_bandwidth() -> u64 {
    1024 * 1024 // 1 MiB/s
}
fn default_join_budget_rtt_ms() -> u64 {
    100
}
fn default_join_budget_jitter_ms() -> u64 {
    20
}
fn default_join_budget_loss_percent() -> u8 {
    2
}
fn default_join_budget_seed() -> u64 {
    0x5A11_0000_0000_0B11
}
fn default_max_baseline_compressed_bytes() -> u64 {
    16 * 1024 * 1024 // 16 MiB
}
fn default_max_ready_ms() -> u64 {
    30_000 // 30 s
}

/// T23 / G4: one impairment envelope applied to every client's packet path.
/// `docs/validation.md`: normal `100 ms` RTT, `+/-20 ms` jitter, `2%` loss;
/// stress `200 ms` RTT, `5%` loss. The relay adds its delay in each direction, so
/// one-way delay is `rtt/2 - jitter` plus a uniform `0..2*jitter`: RTT averages
/// `rtt_ms` and each direction varies by `+/- jitter_ms`.
#[derive(Debug, Clone, Deserialize)]
struct NetworkEnvelope {
    rtt_ms: u64,
    #[serde(default)]
    jitter_ms: u64,
    #[serde(default)]
    loss_percent: f64,
    #[serde(default)]
    bandwidth_bytes_per_sec: Option<u64>,
    #[serde(default = "default_join_budget_seed")]
    seed: u64,
}

/// G1 commit-latency p95 ceilings (milliseconds). Defaults are the
/// `docs/validation.md` "G1" numbers.
#[derive(Debug, Clone, Deserialize)]
struct LatencyTargets {
    #[serde(default = "default_single_brick_ms")]
    single_brick_commit_p95_ms: f64,
    #[serde(default = "default_structure_split_ms")]
    structure_split_p95_ms: f64,
    #[serde(default = "default_large_collapse_ms")]
    large_collapse_p95_ms: f64,
}

/// G4 owning-server timing assertions. Defaults are the validation contract:
/// tick p95 <= 12 ms, p99 <= 16.7 ms, physics p95 <= 6 ms, and peak process
/// memory <= 8 GiB. A scenario opts in by supplying the bounded window.
#[derive(Debug, Clone, Deserialize)]
struct ServerTiming {
    #[serde(default)]
    warmup_ticks: u64,
    #[serde(default)]
    measured_ticks: u64,
    #[serde(default)]
    max_samples: usize,
    #[serde(default = "default_tick_p95_ms")]
    tick_p95_ms: f64,
    #[serde(default = "default_tick_p99_ms")]
    tick_p99_ms: f64,
    #[serde(default = "default_physics_p95_ms")]
    physics_p95_ms: f64,
    #[serde(default = "default_server_peak_memory_bytes")]
    server_peak_memory_bytes: u64,
}

fn default_tick_p95_ms() -> f64 {
    12.0
}

fn default_tick_p99_ms() -> f64 {
    16.7
}

fn default_physics_p95_ms() -> f64 {
    6.0
}

fn default_server_peak_memory_bytes() -> u64 {
    8 * 1024 * 1024 * 1024
}

fn default_single_brick_ms() -> f64 {
    100.0
}
fn default_structure_split_ms() -> f64 {
    500.0
}
fn default_large_collapse_ms() -> f64 {
    2000.0
}

/// Whether every commit-latency bucket the run exercised stayed within the
/// scenario's `latency_targets`. `true` when no targets are configured, or when
/// a bucket has no samples (nothing to fail).
fn latency_targets_met(scenario: &Scenario, server: &ServerSummary) -> bool {
    let Some(t) = &scenario.latency_targets else {
        return true;
    };
    let ok = |samples: u64, measured: f64, target: f64| samples == 0 || measured <= target;
    ok(
        server.single_brick_commit_samples,
        server.single_brick_commit_p95_ms,
        t.single_brick_commit_p95_ms,
    ) && ok(
        server.structure_split_samples,
        server.structure_split_p95_ms,
        t.structure_split_p95_ms,
    ) && ok(
        server.large_collapse_samples,
        server.large_collapse_p95_ms,
        t.large_collapse_p95_ms,
    )
}

/// T23 / G4: timing is fail-closed. A configured window that did not finish,
/// retained no samples, or lacks the real process memory reading cannot pass.
fn server_timing_requirements_met(scenario: &Scenario, server: &ServerSummary) -> bool {
    let Some(t) = &scenario.server_timing else {
        return true;
    };
    t.measured_ticks > 0
        && t.max_samples >= t.measured_ticks as usize
        && server.tick_busy_window_complete
        && server.physics_window_complete
        && server.tick_busy_samples >= t.measured_ticks
        && server.physics_samples >= t.measured_ticks
        && server.tick_busy_p95_ms <= t.tick_p95_ms
        && server.tick_busy_p99_ms <= t.tick_p99_ms
        && server.physics_p95_ms <= t.physics_p95_ms
        && server
            .process_peak_memory_bytes
            .is_some_and(|bytes| bytes <= t.server_peak_memory_bytes)
}

fn one() -> u64 {
    1
}

fn default_scene() -> String {
    "bridge-cut".to_string()
}

fn default_quiescence() -> u64 {
    45
}

fn default_late_delay() -> u64 {
    900
}

fn default_settle_speed_eps() -> f64 {
    0.05
}

fn default_settle_stable_ticks() -> u64 {
    45
}

fn default_settle_penetration_m() -> f64 {
    0.15
}

#[derive(Debug, Clone, Deserialize)]
struct CutSpec {
    #[serde(default)]
    client: u64,
    at_tick: u64,
    cell: [i64; 3],
    radius: i64,
    /// `"terrain"` (default) or `"body"` — a `"body"` cut is retargeted at the
    /// detached body on the client just before it is sent.
    #[serde(default)]
    target: CutTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CutTarget {
    #[default]
    Terrain,
    Body,
}

#[derive(Debug, Clone, Deserialize)]
struct PlayerPath {
    #[serde(default)]
    client: u64,
    legs: Vec<PathLeg>,
}

#[derive(Debug, Clone, Deserialize)]
struct PathLeg {
    from: u64,
    to: u64,
    /// Local movement axes `[strafe, _, forward]`, each `-1..=1`.
    movement: [f32; 3],
    #[serde(default)]
    buttons: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct MovementAcceptance {
    #[serde(default = "default_max_correction")]
    max_correction_m: f64,
    #[serde(default = "default_min_distance")]
    min_distance_m: f64,
    #[serde(default = "default_min_ground_ratio")]
    min_ground_contact_ratio: f64,
    #[serde(default = "default_true")]
    expect_no_hover: bool,
}

/// T23 / G3 row 14: mirrors `sandbox-server`'s `--motion-interest` flag group.
#[derive(Debug, Clone, Deserialize)]
struct MotionInterestSpec {
    #[serde(default = "default_motion_near_m")]
    near_m: f64,
    #[serde(default = "default_motion_far_m")]
    far_m: f64,
    #[serde(default = "default_motion_far_interval")]
    far_interval: u64,
    #[serde(default)]
    client_budget_bytes: usize,
    /// Pass `--motion-congestion-aware`.
    #[serde(default)]
    congestion_aware: bool,
    /// `x,y,z` metres for a scene with no player spawns. Omitted → those
    /// clients stay unfiltered (matches the CLI default).
    #[serde(default)]
    static_anchor: Option<[f64; 3]>,
}

fn default_motion_near_m() -> f64 {
    48.0
}
fn default_motion_far_m() -> f64 {
    96.0
}
fn default_motion_far_interval() -> u64 {
    4
}

/// T23 / G3 row 13: `docs/validation.md`'s G4 sustained-load gate ("Run for
/// two measured minutes after 30 seconds warmup; also run a 30-minute
/// reduced-telemetry soak... Drive 10 ordinary edits/s total and one 4 m
/// diameter blast every 10 seconds"). Generates that stream programmatically
/// against the `g4-workload` scene's already-proven-safe target cells (the
/// same rows/points `t23-g4-workload.json`'s hand-authored `cuts` already
/// exercise), round-robined across every connected client, from `start_tick`
/// through `server_ticks` (minus a trailing buffer so the last few edits have
/// time to commit before quiescence). Cycling back through a short cell list
/// once it's exhausted is intentional and cheap (see `generate_sustained_cuts`)
/// — this is a throughput/latency soak, not a claim of ever-fresh geometry.
#[derive(Debug, Clone, Deserialize)]
struct SustainedEdits {
    start_tick: u64,
    #[serde(default = "default_small_rate")]
    small_rate_per_sec: f64,
    #[serde(default = "default_blast_interval")]
    blast_interval_sec: f64,
    #[serde(default = "default_trailing_buffer")]
    trailing_buffer_ticks: u64,
    /// Which edit geometry the stream drives.
    #[serde(default)]
    mode: SustainedMode,
    /// `g4_bodies` only: ordinary edits and blasts begin this many ticks after
    /// `start_tick` (the giant collapse fires first and stalls the tick loop
    /// for several ticks while its 2 M-cell split is analysed).
    #[serde(default)]
    ordinary_offset_ticks: u64,
    /// `g4_bodies` only: tick offset from `start_tick` of the one 64-brick
    /// collapse (`None`: no giant in this run).
    #[serde(default)]
    giant_at_offset_ticks: Option<u64>,
    /// `g4_bodies` only: every `terrain_edit_every`-th ordinary edit is a **terrain**
    /// dig on the ground slab instead of a body cut (`0` = none). The declared
    /// mix is `1` terrain edit per `terrain_edit_every` ordinary edits.
    #[serde(default)]
    terrain_edit_every: u64,
}

/// Edit geometry for [`SustainedEdits`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SustainedMode {
    /// The original `g4-workload` terrain rows and two blast points, cycled.
    #[default]
    TerrainCells,
    /// The integrated yard: fresh comb-tooth cuts, one tower per blast, and the
    /// giant (`spall_voxel::fixtures::g4_ordinary_edit` / `g4_blast` /
    /// `g4_giant_cut`), each aimed at a persistent body by entity id.
    G4Bodies,
}

fn default_small_rate() -> f64 {
    10.0
}
fn default_blast_interval() -> f64 {
    10.0
}
fn default_trailing_buffer() -> u64 {
    120
}

/// One generated `--cuts-file` entry, in the JSON shape `sandbox-client`'s
/// `--cuts-file` reads (`tick`/`cell`/`radius`, and for a body-aimed edit
/// `"target": "body"` plus the raw `entity` id).
#[derive(Debug, Serialize)]
struct GeneratedCut {
    tick: u64,
    cell: [i64; 3],
    radius: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity: Option<u64>,
}

/// West/east floor rows already proven safe by `t23-g4-workload.json`'s
/// hand-authored small cuts (`y = 1`) and blasts (`y = 12`, `z = 12` /
/// `z = 73`) — see `docs/reports/G3.md` increment 18 for why those exact
/// coordinates stay inside the declared volume bounds. Reused verbatim here
/// rather than re-derived, so this generator carries no new bounds-safety
/// risk.
const WEST_SMALL_Z: i64 = 6;
const EAST_SMALL_Z: i64 = 78;
const EAST_X_OFFSET: i64 = 72;
const SMALL_Y: i64 = 1;
const SMALL_X_MIN: i64 = 1;
const SMALL_X_MAX: i64 = 22;
const BLAST_WEST: [i64; 3] = [20, 12, 12];
const BLAST_EAST: [i64; 3] = [20 + EAST_X_OFFSET, 12, 73];

/// Where the ordinary-edit / blast streams start: `start_tick`, plus the
/// `g4_bodies` offset that lets the giant collapse land first.
fn stream_start(s: &SustainedEdits, server_ticks: u64) -> (u64, u64) {
    let end_tick = server_ticks.saturating_sub(s.trailing_buffer_ticks);
    let start = s.start_tick.min(end_tick);
    let ordinary_start = match s.mode {
        SustainedMode::TerrainCells => start,
        SustainedMode::G4Bodies => (start + s.ordinary_offset_ticks).min(end_tick),
    };
    (ordinary_start, end_tick)
}

/// The tick window available for the generated stream, its small-edit step
/// (ticks), and the resulting `(n_small, n_blasts)` counts — shared between
/// `generate_sustained_cuts` (which builds the lists) and `requirements_met`
/// (which needs the same expected total without re-deriving it, so the two
/// can never silently disagree). In `g4_bodies` mode the counts are capped by
/// what the yard's bodies supply (18,048 comb teeth, 180 towers).
fn sustained_counts(s: &SustainedEdits, server_ticks: u64) -> (u64, u64, f64) {
    let (ordinary_start, end_tick) = stream_start(s, server_ticks);
    let available = end_tick.saturating_sub(ordinary_start);
    let small_step = (60.0 / s.small_rate_per_sec.max(0.01)).max(1.0);
    let blast_step = (s.blast_interval_sec * 60.0).max(1.0);
    let mut n_small = (available as f64 / small_step).floor() as u64;
    let mut n_blasts = (available as f64 / blast_step).floor() as u64;
    if s.mode == SustainedMode::G4Bodies {
        n_small = n_small.min(spall_voxel::fixtures::g4_comb_cut_capacity() as u64);
        n_blasts = n_blasts.min(
            spall_voxel::fixtures::G4_TOWER_COUNT as u64
                * spall_voxel::fixtures::G4_TOWER_BLASTS as u64,
        );
    }
    (n_small, n_blasts, small_step)
}

/// Commits the stream is expected to add beyond `n_small + n_blasts`: the one
/// giant collapse of a `g4_bodies` stream.
fn sustained_giant_commits(s: &SustainedEdits) -> u64 {
    u64::from(s.mode == SustainedMode::G4Bodies && s.giant_at_offset_ticks.is_some())
}

/// Builds the sustained small-edit + blast stream `SustainedEdits`
/// describes, round-robined across `clients` and grouped by client index
/// (same shape `by_client` already uses for hand-authored `cuts`).
fn generate_sustained_cuts(
    s: &SustainedEdits,
    server_ticks: u64,
    clients: u64,
) -> BTreeMap<u64, Vec<GeneratedCut>> {
    let mut by_client: BTreeMap<u64, Vec<GeneratedCut>> = BTreeMap::new();
    if clients == 0 {
        return by_client;
    }
    let (start, _) = stream_start(s, server_ticks);
    let (n_small, n_blasts, small_step) = sustained_counts(s, server_ticks);
    let blast_step = (s.blast_interval_sec * 60.0).max(1.0);
    let west_client = 0u64;
    let east_client = (clients / 2).min(clients - 1);

    if s.mode == SustainedMode::G4Bodies {
        use spall_voxel::fixtures as yard;
        let body = |tick: u64, e: yard::G4BodyEdit| GeneratedCut {
            tick,
            cell: e.cell,
            radius: e.radius,
            target: Some("body"),
            entity: Some(e.entity),
        };
        // The declared mix: with `terrain_edit_every = N`, ordinary edit `i` is a
        // terrain dig when `i % N == N - 1`, else the next unused comb-tooth cut.
        let every = s.terrain_edit_every;
        let mut digs = 0u64;
        let mut comb_edits = 0u64;
        for i in 0..n_small {
            let tick = start + (i as f64 * small_step) as u64;
            let cut = if every > 0 && i % every == every - 1 {
                let (cell, radius) =
                    yard::g4_terrain_dig(digs).expect("terrain digs within the dig lane");
                digs += 1;
                GeneratedCut {
                    tick,
                    cell,
                    radius,
                    target: None,
                    entity: None,
                }
            } else {
                let e = yard::g4_ordinary_edit(comb_edits)
                    .expect("count is capped by the yard's capacity");
                comb_edits += 1;
                body(tick, e)
            };
            by_client.entry(i % clients).or_default().push(cut);
        }
        for i in 0..n_blasts {
            // Half a blast step in: the first blast is 5 s into the stream.
            let tick = start + (i as f64 * blast_step + blast_step / 2.0) as u64;
            let e = yard::g4_blast(i).expect("count is capped by the tower count");
            let client = if i % 2 == 0 { west_client } else { east_client };
            by_client.entry(client).or_default().push(body(tick, e));
        }
        if let Some(offset) = s.giant_at_offset_ticks {
            by_client
                .entry(west_client)
                .or_default()
                .push(body(s.start_tick + offset, yard::g4_giant_cut()));
        }
        for cuts in by_client.values_mut() {
            cuts.sort_by_key(|c| c.tick);
        }
        return by_client;
    }

    // Small edits: one every `60 / small_rate_per_sec` ticks, alternating
    // west/east, cycling x across a `SMALL_X_MIN..=SMALL_X_MAX` row at the
    // proven-safe y/z for that region.
    let small_span = (SMALL_X_MAX - SMALL_X_MIN + 1).max(1);
    for i in 0..n_small {
        let tick = start + (i as f64 * small_step) as u64;
        let west = i % 2 == 0;
        let x = SMALL_X_MIN + (i as i64 / 2) % small_span;
        let cell = if west {
            [x, SMALL_Y, WEST_SMALL_Z]
        } else {
            [x + EAST_X_OFFSET, SMALL_Y, EAST_SMALL_Z]
        };
        let client = i % clients;
        by_client.entry(client).or_default().push(GeneratedCut {
            tick,
            cell,
            radius: 1,
            target: None,
            entity: None,
        });
    }

    // Blasts: one every `blast_interval_sec`, alternating west/east at the
    // fixed proven-safe blast points. West is always sent by client 0, east
    // always by the start of the east cluster (`clients / 2`, matching the
    // g4-workload west/east client split) — deterministic and easy to audit,
    // not a throughput concern (there are far fewer blasts than small edits).
    for i in 0..n_blasts {
        // Offset half a small-edit step so a blast never lands on the exact
        // same tick as a small edit from the same client.
        let tick = start + (i as f64 * blast_step + small_step / 2.0) as u64;
        let (cell, client) = if i % 2 == 0 {
            (BLAST_WEST, west_client)
        } else {
            (BLAST_EAST, east_client)
        };
        by_client.entry(client).or_default().push(GeneratedCut {
            tick,
            cell,
            radius: 8,
            target: None,
            entity: None,
        });
    }

    for cuts in by_client.values_mut() {
        cuts.sort_by_key(|c| c.tick);
    }
    by_client
}

#[derive(Debug, Clone, Deserialize)]
struct ResidencyAssertions {
    #[serde(default)]
    client: u64,
    #[serde(default)]
    min_server_evictions: u64,
    #[serde(default)]
    min_server_reloads: u64,
    #[serde(default)]
    min_client_evictions: u64,
    #[serde(default)]
    min_client_reloads_completed: u64,
    #[serde(default)]
    min_evicted_transaction_gaps: u64,
    #[serde(default)]
    min_outbound_distance_m: f64,
    #[serde(default)]
    max_return_distance_m: Option<f64>,
    /// ENG-30 row 7 increment 13: the server must have pinned at least this
    /// many bricks in its busiest tick (pending-edit dependencies, swept
    /// paths, or pipeline reload grace) — real evidence the pin lifecycle
    /// engaged, not only that eviction/reload happened.
    #[serde(default)]
    min_pinned_bricks: u64,
    /// ENG-30 row 7 increment 13: the server must have deferred at least this
    /// many interest-driven (non-required) reloads under capacity pressure —
    /// real evidence the brick/dense-byte admission cap actually bound,
    /// rather than only being reported.
    #[serde(default)]
    min_admission_deferred: u64,
    /// ENG-30 row 7 increment 13: when set, the *required* (interest ∪
    /// pinned) set must never have exceeded `budget_bricks` on its own — a
    /// genuine capacity failure the pass could not resolve without evicting
    /// needed geometry. `false` (the default, matching every other floor
    /// here) does not require this: a small `residency_budget_bricks`
    /// deliberately paired with a comfortable `residency_radius_bricks` (or
    /// several players' union interest) legitimately exceeds the soft budget
    /// routinely, and that is not itself a defect -- the pass still never
    /// evicts required geometry to force a fit. Set `true` only on a
    /// scenario whose budget is meant to always cover its own interest.
    #[serde(default)]
    forbid_required_over_budget: bool,
    /// ENG-30 row 7 increment 14: the configured client must have deferred at
    /// least this many desired (box-driven) reload requests under capacity
    /// pressure — real evidence the client-side brick/dense-byte admission cap
    /// actually bound, rather than only being reported. `0` (the default)
    /// does not require this — most scenarios pair a comfortable client
    /// residency budget with the box, and never trip it.
    #[serde(default)]
    min_client_admission_deferred: u64,
}

/// T21 / ENG-28 increment 4 (3c): minimum dormancy pass activity the server's
/// end-of-run report must show.
#[derive(Debug, Clone, Deserialize)]
struct DormancyAssertions {
    #[serde(default = "one")]
    min_deactivations: u64,
    #[serde(default = "one")]
    min_reactivations: u64,
}

impl Default for MovementAcceptance {
    fn default() -> Self {
        Self {
            max_correction_m: default_max_correction(),
            min_distance_m: default_min_distance(),
            min_ground_contact_ratio: default_min_ground_ratio(),
            expect_no_hover: true,
        }
    }
}

fn default_max_correction() -> f64 {
    0.75
}
fn default_min_distance() -> f64 {
    2.0
}
fn default_min_ground_ratio() -> f64 {
    0.85
}
fn default_true() -> bool {
    true
}

// --- process summaries (subset of the server / client structs) ----------------

#[derive(Debug, Default, Clone, Deserialize)]
struct ServerSummary {
    result: String,
    #[serde(flatten)]
    g4: g4::G4ServerFacts,
    ticks_run: u64,
    transactions_committed: u64,
    final_world_hash: String,
    #[serde(default)]
    max_detached_body_brick_span: u64,
    // T11a / ENG-62: admission breakdown + commit-latency p95s (ServeSummary v3).
    #[serde(default)]
    actions_requested: u64,
    #[serde(default)]
    actions_rejected: u64,
    #[serde(default)]
    actions_staged: u64,
    #[serde(default)]
    actions_queued_unresolved: u64,
    #[serde(default)]
    single_brick_commit_p95_ms: f64,
    #[serde(default)]
    single_brick_commit_samples: u64,
    #[serde(default)]
    structure_split_p95_ms: f64,
    #[serde(default)]
    structure_split_samples: u64,
    #[serde(default)]
    large_collapse_p95_ms: f64,
    #[serde(default)]
    large_collapse_samples: u64,
    // ENG-61 detached-body settle evidence.
    #[serde(default)]
    detached_body_max_final_speed_m_s: f64,
    #[serde(default)]
    detached_bodies_all_asleep: bool,
    #[serde(default)]
    detached_body_stable_ticks: u64,
    #[serde(default)]
    max_contact_penetration_m: f64,
    #[serde(default)]
    detached_body_min_origin_y_m: f64,
    #[serde(default)]
    residency_evictions_total: u64,
    #[serde(default)]
    residency_reloads_total: u64,
    // T23 / G3 row 7 item 2 (increment 31): `ServeSummary` v6's durable-backing
    // byte count -- `Some(n)` once a disk-backed run has captured at least one
    // brick, `None` for in-process `MemoryBacking` or when residency is off.
    #[serde(default)]
    residency_backing_disk_bytes: Option<u64>,
    // T23 / G3 row 14.
    #[serde(default)]
    app_egress_bytes: u64,
    #[serde(default)]
    transport_egress_bytes: u64,
    #[serde(default)]
    per_client_egress: Vec<PerClientEgressRow>,
    #[serde(default)]
    dormancy_deactivations_total: u64,
    #[serde(default)]
    dormancy_reactivations_total: u64,
    // ENG-30 row 7 increment 13 (`ServeSummary` v7): pin lifetime + admission
    // enforcement + expanded retained-memory evidence.
    #[serde(default)]
    residency_pinned_bricks_max: u64,
    #[serde(default)]
    residency_admission_deferred_total: u64,
    #[serde(default)]
    residency_required_over_budget_ticks: u64,
    #[serde(default)]
    residency_digest_bytes_final: u64,
    #[serde(default)]
    residency_backing_resident_bytes: Option<u64>,
    #[serde(default)]
    process_peak_memory_bytes: Option<u64>,
    // ENG-30 row 7 increment 15 (`ServeSummary` v8): incremental checkpoint
    // capture evidence -- cumulative terrain bricks actually re-captured vs.
    // each checkpoint's complete logical set, summed across every periodic +
    // shutdown checkpoint this run.
    #[serde(default)]
    residency_checkpoint_bricks_captured_total: u64,
    #[serde(default)]
    residency_checkpoint_bricks_logical_total: u64,
    // T23 / G4 bounded owning-server timing telemetry (ServeSummary v9).
    #[serde(default)]
    tick_busy_p95_ms: f64,
    #[serde(default)]
    tick_busy_p99_ms: f64,
    #[serde(default)]
    tick_busy_max_ms: f64,
    #[serde(default)]
    tick_busy_samples: u64,
    #[serde(default)]
    tick_busy_window_complete: bool,
    #[serde(default)]
    physics_p95_ms: f64,
    #[serde(default)]
    physics_p99_ms: f64,
    #[serde(default)]
    physics_max_ms: f64,
    #[serde(default)]
    physics_samples: u64,
    #[serde(default)]
    physics_window_complete: bool,
}

/// Mirrors `spall_server::PerClientEgress`.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct PerClientEgressRow {
    slot: u32,
    #[serde(default)]
    spawn_m: Option<[f64; 3]>,
    app_bytes: u64,
    transport_bytes: u64,
}

/// ENG-61: whether the server's end-of-run report shows every detached body at
/// rest on the remaining structure — asleep, barely moving, its origin held
/// still for a stretch of ticks, and not clipped down through the floor.
fn body_settled(scenario: &Scenario, server: &ServerSummary) -> bool {
    server.detached_bodies_all_asleep
        && server.detached_body_max_final_speed_m_s <= scenario.body_settle_speed_epsilon_m_s
        && server.detached_body_stable_ticks >= scenario.body_settle_min_stable_ticks
        && server.max_contact_penetration_m <= scenario.body_settle_max_penetration_m
        && server.detached_body_min_origin_y_m.is_finite()
}

#[derive(Debug, Clone, Deserialize)]
struct ClientSummary {
    result: String,
    #[serde(default)]
    topology_lag_p95_ms: u64,
    #[serde(default)]
    topology_lag_max_ms: u64,
    #[serde(default)]
    tx_received: u64,
    #[serde(default)]
    transactions_applied: u64,
    #[serde(default)]
    repair_requests_sent: u64,
    #[serde(default)]
    transactions_rejected: u64,
    #[serde(default)]
    motion_snapshots: u64,
    #[serde(default)]
    motion_snapshots_out_of_order: u64,
    #[serde(default)]
    final_world_hash: String,
    #[serde(default)]
    late_join: bool,
    #[serde(default)]
    baseline_bricks: u64,
    #[serde(default)]
    max_body_displacement_m: f64,
    #[serde(default)]
    body_cut_committed: bool,
    #[serde(default)]
    movement: Option<MovementRow>,
    #[serde(default)]
    client_residency_evictions: u64,
    #[serde(default)]
    client_residency_reloads_requested: u64,
    #[serde(default)]
    client_residency_reloads_completed: u64,
    #[serde(default)]
    client_residency_budget_miss_steps: u64,
    /// ENG-30 row 7 increment 14 (`ClientSummary` v4).
    #[serde(default)]
    client_residency_admission_deferred_total: u64,
    #[serde(default)]
    client_residency_evicted_transaction_gaps: u64,
    #[serde(default)]
    late_join_baseline_compressed_bytes: u64,
    #[serde(default)]
    late_join_baseline_install_ms: u64,
    #[serde(default)]
    late_join_ready_ms: u64,
    #[serde(default)]
    late_join_ready_confirmed: bool,
    /// A sent `ActionRequest` the server declined to admit or stage, other
    /// than a `"throttled"` one (which the client retries on its own).
    #[serde(default)]
    action_requests_rejected: u64,
    /// The distinct rejection reasons observed (bounded, most recent last) —
    /// diagnostic only.
    #[serde(default)]
    action_reject_reasons: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MovementRow {
    #[serde(default)]
    ticks: u64,
    distance_travelled_m: f64,
    #[serde(default)]
    max_distance_from_start_m: f64,
    max_correction_m: f64,
    ground_contact_ratio: f64,
    hovered_after_floor_removal: bool,
    held_button_release_ok: bool,
}

#[derive(Debug, Serialize)]
struct SessionSummary {
    version: u32,
    result: &'static str,
    scenario: String,
    clients: u64,
    loss_percent: u8,
    server_ticks_run: u64,
    transactions_committed: u64,
    /// Largest distinct-brick span of any detached body (server-authoritative).
    max_detached_body_brick_span: u64,
    /// ENG-61: whether every detached body was reported at rest on the remaining
    /// structure at end of run (see [`body_settled`]). Always computed; only
    /// *required* for the session to pass when `require_body_settled` is set.
    body_settled: bool,
    /// ENG-61: largest final linear speed (m/s) of any detached body.
    detached_body_max_final_speed_m_s: f64,
    /// ENG-61: consecutive final ticks the detached bodies held still.
    detached_body_stable_ticks: u64,
    /// ENG-61: deepest contact penetration (m) at end of run.
    max_contact_penetration_m: f64,
    /// T11 exact-replay check: whether it ran, whether the replayed baseline
    /// hash matched, and how many committed topology events were replayed.
    replay_checked: bool,
    replay_matches: bool,
    replayed_topology_events: u64,
    /// T23 / G3 cold-restart check: whether it ran, whether a fresh server
    /// recovered the agreed hash from the world DB, and whether a fresh
    /// `--late-join` client against the restarted server converged to it.
    restart_checked: bool,
    restart_recovered_hash_matches: bool,
    restart_reconnect_hash_matches: bool,
    restart_detail: String,
    restart_recovered_world_hash: String,
    /// T23 / G3 row 7 item 2 (increment 31): the server's reported durable
    /// residency-backing byte count for this run -- `Some(n)` once a
    /// `residency_disk_backing` run has captured at least one brick to disk,
    /// `None` for an in-process-backed or residency-off run. Always surfaced
    /// (not only on pass) so a scenario's `summary.json` shows the measured
    /// value directly.
    residency_backing_disk_bytes: Option<u64>,
    /// ENG-30 row 7 increment 13: real pin-lifetime + admission-enforcement
    /// evidence, always surfaced (not only on pass) so a scenario's
    /// `summary.json` shows the measured values directly, matching
    /// `residency_backing_disk_bytes`'s convention. All `0`/`None` when
    /// residency is off.
    residency_pinned_bricks_max: u64,
    residency_admission_deferred_total: u64,
    residency_required_over_budget_ticks: u64,
    residency_digest_bytes_final: u64,
    residency_backing_resident_bytes: Option<u64>,
    /// This process's peak resident/working-set memory in bytes, independent
    /// of residency being on -- `None` on an unsupported platform.
    process_peak_memory_bytes: Option<u64>,
    /// ENG-30 row 7 increment 15: incremental checkpoint capture evidence,
    /// always surfaced (not only on pass) matching every other residency
    /// counter's convention. `captured_total < logical_total` (once more than
    /// one checkpoint has run against unchanged state) is the direct evidence
    /// that checkpoint capture is incremental, not a full walk with a cache
    /// wrapped around it. Both `0` when residency is off.
    residency_checkpoint_bricks_captured_total: u64,
    residency_checkpoint_bricks_logical_total: u64,
    /// T23 / G3 row 10: `late_join_may_fail` was set and an impaired late joiner
    /// ended in an accepted bounded explicit failure (`join-failed`, real exit)
    /// while the live clients + server still converged.
    impaired_late_join_bounded_failure: bool,
    agreed_world_hash: String,
    /// Hash agreement **only**: every expected replica reached the agreed hash and, when
    /// checked, the replay did. Timing, workload and recovery do not feed this flag.
    all_hashes_match: bool,
    requirements_met: bool,
    /// Independent verdict dimensions; `overall` is what `result` reports.
    verdict: SessionVerdict,
    /// T11a / ENG-62: the gate's requested / rejected / queued / committed
    /// breakdown (server-authoritative) and the measured commit-latency p95s.
    admission: AdmissionRow,
    /// T23 / G4 bounded owning-server timing evidence and acceptance result.
    server_timing: ServerTimingRow,
    /// T23 / G4: warmup-excluded telemetry evaluation (`g4.rs`).
    g4: g4::G4Row,
    /// T23 / G3 row 11: the configured join-budget network profile / ceilings
    /// alongside the measured compressed baseline size and time-to-ready.
    /// `configured: false` (all other fields zeroed) when no `join_budget`
    /// scenario block is set.
    join_budget: JoinBudgetRow,
    per_client: Vec<ClientRow>,
    /// T23 / G3 row 14: total application/transport egress this run, and the
    /// per-connection breakdown (bytes alongside each connection's player
    /// spawn) proving `motion_interest` separates bandwidth *between*
    /// clients, not just that the total stays bounded. Empty when
    /// `motion_interest` was not configured for this scenario.
    app_egress_bytes: u64,
    transport_egress_bytes: u64,
    per_client_egress: Vec<PerClientEgressRow>,
    note: &'static str,
}

/// T11a / ENG-62: server-side admission accounting + commit-latency p95s, and
/// whether the measured p95s stayed within the scenario's `latency_targets`
/// (`latency_targets_met` is `true` when no targets are configured).
#[derive(Debug, Serialize)]
struct AdmissionRow {
    actions_requested: u64,
    actions_rejected: u64,
    actions_staged: u64,
    actions_queued_unresolved: u64,
    transactions_committed: u64,
    single_brick_commit_p95_ms: f64,
    single_brick_commit_samples: u64,
    structure_split_p95_ms: f64,
    structure_split_samples: u64,
    large_collapse_p95_ms: f64,
    large_collapse_samples: u64,
    latency_targets_configured: bool,
    latency_targets_met: bool,
}

#[derive(Debug, Serialize)]
struct ServerTimingRow {
    configured: bool,
    warmup_ticks: u64,
    measured_ticks: u64,
    max_samples: usize,
    tick_busy_p95_ms: f64,
    tick_busy_p99_ms: f64,
    tick_busy_max_ms: f64,
    tick_busy_samples: u64,
    tick_busy_window_complete: bool,
    physics_p95_ms: f64,
    physics_p99_ms: f64,
    physics_max_ms: f64,
    physics_samples: u64,
    physics_window_complete: bool,
    process_peak_memory_bytes: Option<u64>,
    requirements_met: bool,
}

impl ServerTimingRow {
    fn unconfigured() -> Self {
        Self {
            configured: false,
            warmup_ticks: 0,
            measured_ticks: 0,
            max_samples: 0,
            tick_busy_p95_ms: 0.0,
            tick_busy_p99_ms: 0.0,
            tick_busy_max_ms: 0.0,
            tick_busy_samples: 0,
            tick_busy_window_complete: false,
            physics_p95_ms: 0.0,
            physics_p99_ms: 0.0,
            physics_max_ms: 0.0,
            physics_samples: 0,
            physics_window_complete: false,
            process_peak_memory_bytes: None,
            requirements_met: true,
        }
    }
}

impl AdmissionRow {
    /// All-zero row for a run that produced no server summary.
    fn empty() -> Self {
        Self {
            actions_requested: 0,
            actions_rejected: 0,
            actions_staged: 0,
            actions_queued_unresolved: 0,
            transactions_committed: 0,
            single_brick_commit_p95_ms: 0.0,
            single_brick_commit_samples: 0,
            structure_split_p95_ms: 0.0,
            structure_split_samples: 0,
            large_collapse_p95_ms: 0.0,
            large_collapse_samples: 0,
            latency_targets_configured: false,
            latency_targets_met: false,
        }
    }
}

#[derive(Debug, Serialize)]
struct ClientRow {
    index: u64,
    result: String,
    transactions_applied: u64,
    repair_requests_sent: u64,
    transactions_rejected: u64,
    motion_snapshots: u64,
    motion_snapshots_out_of_order: u64,
    max_body_displacement_m: f64,
    body_cut_committed: bool,
    client_residency_evictions: u64,
    client_residency_reloads_requested: u64,
    client_residency_reloads_completed: u64,
    client_residency_budget_miss_steps: u64,
    client_residency_admission_deferred_total: u64,
    client_residency_evicted_transaction_gaps: u64,
    hash_matches_server: bool,
    late_join_baseline_compressed_bytes: u64,
    late_join_baseline_install_ms: u64,
    late_join_ready_ms: u64,
    late_join_ready_confirmed: bool,
    action_requests_rejected: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    action_reject_reasons: Vec<String>,
}

/// T23 / G3 row 11: measured join-budget evidence for the configured client,
/// plus whether it met the configured budget. `None` fields mean the run
/// produced no client summary to measure.
#[derive(Debug, Serialize)]
struct JoinBudgetRow {
    configured: bool,
    client: u64,
    bandwidth_bytes_per_sec: u64,
    rtt_ms: u64,
    loss_percent: u8,
    max_baseline_compressed_bytes: u64,
    max_ready_ms: u64,
    measured_baseline_compressed_bytes: u64,
    measured_ready_ms: u64,
    ready_confirmed: bool,
    within_budget: bool,
}

impl JoinBudgetRow {
    fn unconfigured() -> Self {
        Self {
            configured: false,
            client: 0,
            bandwidth_bytes_per_sec: 0,
            rtt_ms: 0,
            loss_percent: 0,
            max_baseline_compressed_bytes: 0,
            max_ready_ms: 0,
            measured_baseline_compressed_bytes: 0,
            measured_ready_ms: 0,
            ready_confirmed: false,
            within_budget: true,
        }
    }
}

/// T23 / G3 row 11: whether the configured `join_budget` client's measured
/// compressed baseline size and time-to-ready both stayed within budget, and
/// it actually converged (a hash mismatch or an unconfirmed "ready" signal is
/// not a passing join no matter how small/fast the transfer was). `true` when
/// no `join_budget` is configured.
fn join_budget_requirements_met(scenario: &Scenario, clients: &[Option<ClientSummary>]) -> bool {
    let Some(budget) = &scenario.join_budget else {
        return true;
    };
    let Some(client) = clients.get(budget.client as usize).and_then(Option::as_ref) else {
        return false;
    };
    client.result == "passed"
        && client.late_join_ready_confirmed
        && client.late_join_baseline_compressed_bytes > 0
        && client.late_join_baseline_compressed_bytes <= budget.max_baseline_compressed_bytes
        && client.late_join_ready_ms > 0
        && client.late_join_ready_ms <= budget.max_ready_ms
}

/// The `JoinBudgetRow` reported in `summary.json` regardless of pass/fail —
/// always the measured numbers, never estimates.
fn join_budget_row(scenario: &Scenario, clients: &[Option<ClientSummary>]) -> JoinBudgetRow {
    let Some(budget) = &scenario.join_budget else {
        return JoinBudgetRow::unconfigured();
    };
    let client = clients.get(budget.client as usize).and_then(Option::as_ref);
    JoinBudgetRow {
        configured: true,
        client: budget.client,
        bandwidth_bytes_per_sec: budget.bandwidth_bytes_per_sec,
        rtt_ms: budget.rtt_ms,
        loss_percent: budget.loss_percent,
        max_baseline_compressed_bytes: budget.max_baseline_compressed_bytes,
        max_ready_ms: budget.max_ready_ms,
        measured_baseline_compressed_bytes: client
            .map(|c| c.late_join_baseline_compressed_bytes)
            .unwrap_or(0),
        measured_ready_ms: client.map(|c| c.late_join_ready_ms).unwrap_or(0),
        ready_confirmed: client.is_some_and(|c| c.late_join_ready_confirmed),
        within_budget: join_budget_requirements_met(scenario, clients),
    }
}

fn residency_requirements_met(
    scenario: &Scenario,
    server: &ServerSummary,
    clients: &[Option<ClientSummary>],
) -> bool {
    let Some(required) = &scenario.residency_assertions else {
        return true;
    };
    let Some(client) = clients
        .get(required.client as usize)
        .and_then(Option::as_ref)
    else {
        return false;
    };
    let movement_ok = client.movement.as_ref().is_some_and(|movement| {
        movement.max_distance_from_start_m >= required.min_outbound_distance_m
            && required
                .max_return_distance_m
                .is_none_or(|max| movement.distance_travelled_m <= max)
    });
    server.residency_evictions_total >= required.min_server_evictions
        && server.residency_reloads_total >= required.min_server_reloads
        && client.client_residency_evictions >= required.min_client_evictions
        && client.client_residency_reloads_completed >= required.min_client_reloads_completed
        && client.client_residency_evicted_transaction_gaps >= required.min_evicted_transaction_gaps
        && movement_ok
        && server.residency_pinned_bricks_max >= required.min_pinned_bricks
        && server.residency_admission_deferred_total >= required.min_admission_deferred
        && (!required.forbid_required_over_budget
            || server.residency_required_over_budget_ticks == 0)
        && client.client_residency_admission_deferred_total
            >= required.min_client_admission_deferred
}

/// T21 / ENG-28 increment 4 (3c): when the scenario configured
/// `dormancy_assertions`, require the server's reported deactivation /
/// reactivation counts to clear the floor. `true` when unconfigured.
fn dormancy_requirements_met(scenario: &Scenario, server: &ServerSummary) -> bool {
    let Some(required) = &scenario.dormancy_assertions else {
        return true;
    };
    server.dormancy_deactivations_total >= required.min_deactivations
        && server.dormancy_reactivations_total >= required.min_reactivations
}

/// T23 / G3 row 7 item 2 (increment 31): when the scenario turned on
/// `residency_disk_backing`, require the server to have actually reported a
/// nonzero durable-backing byte count (`ServeSummary.residency_backing_disk_bytes
/// == Some(n > 0)`) -- proving the disk-backed path was exercised end to end,
/// not just accepted as a CLI flag. `true` when the scenario did not request
/// disk backing.
fn residency_disk_backing_requirements_met(scenario: &Scenario, server: &ServerSummary) -> bool {
    if !scenario.residency_disk_backing {
        return true;
    }
    server.residency_backing_disk_bytes.is_some_and(|n| n > 0)
}

fn requirements_met(
    scenario: &Scenario,
    server_ticks: u64,
    transactions_committed: u64,
    clients: &[Option<ClientSummary>],
    proxy_active: bool,
) -> bool {
    // The whole script must have run: a fixture that quiesces early (a gap
    // between scripted cuts longer than the server's idle window) commits fewer
    // transactions than it has cuts, and that is a failure no matter how the
    // `minimum_transactions` floor is set. `sustained_edits` (row 13) adds a
    // programmatically generated count on top of any hand-authored `cuts` —
    // computed with the exact same formula `generate_sustained_cuts` used to
    // build the stream, so this can never silently drift from what was
    // actually sent.
    let expected_sustained: u64 = scenario
        .sustained_edits
        .as_ref()
        .map(|s| {
            let (n_small, n_blasts, _) = sustained_counts(s, server_ticks);
            n_small + n_blasts + sustained_giant_commits(s)
        })
        .unwrap_or(0);
    let all_cuts_committed =
        transactions_committed >= scenario.cuts.len() as u64 + expected_sustained;

    let work_ok = transactions_committed >= scenario.minimum_transactions && all_cuts_committed;

    let per_client_ok = clients.iter().all(|client| {
        client.as_ref().is_some_and(|s| {
            let motion_ok = s.motion_snapshots >= scenario.minimum_motion_snapshots;
            // Late joiners connect after the body has settled; only hold live
            // clients to the "geometry actually moved" bar.
            let displacement_ok =
                s.late_join || s.max_body_displacement_m >= scenario.minimum_body_displacement_m;
            motion_ok && displacement_ok
        })
    });

    // The "reordered snapshots" half of T11: on an impaired-transport run the
    // live clients together must have observed motion datagrams delivered out of
    // `snapshot_seq` order. Aggregated across live clients (which snapshot is
    // reordered on which client is a per-datagram coin flip); a clean loopback
    // run never reorders, so this is skipped there.
    let reorder_ok = !proxy_active
        || scenario.minimum_reordered_snapshots == 0
        || clients
            .iter()
            .filter_map(|c| c.as_ref())
            .filter(|s| !s.late_join)
            .map(|s| s.motion_snapshots_out_of_order)
            .sum::<u64>()
            >= scenario.minimum_reordered_snapshots;

    // If the script contains a body-targeted cut, some client must have landed
    // it against the detached body (material removed from that body).
    let body_cut_ok = !scenario.cuts.iter().any(|c| c.target == CutTarget::Body)
        || clients
            .iter()
            .any(|c| c.as_ref().is_some_and(|s| s.body_cut_committed));

    work_ok && per_client_ok && reorder_ok && body_cut_ok
}

#[cfg(test)]
mod requirement_tests {
    use super::*;

    fn client(motion_snapshots: u64) -> ClientSummary {
        ClientSummary {
            topology_lag_p95_ms: 0,
            topology_lag_max_ms: 0,
            tx_received: 0,
            result: "passed".into(),
            transactions_applied: 1,
            repair_requests_sent: 0,
            transactions_rejected: 0,
            motion_snapshots,
            motion_snapshots_out_of_order: 3,
            final_world_hash: String::new(),
            late_join: false,
            baseline_bricks: 0,
            max_body_displacement_m: 5.0,
            body_cut_committed: true,
            movement: None,
            client_residency_evictions: 0,
            client_residency_reloads_requested: 0,
            client_residency_reloads_completed: 0,
            client_residency_budget_miss_steps: 0,
            client_residency_admission_deferred_total: 0,
            client_residency_evicted_transaction_gaps: 0,
            late_join_baseline_compressed_bytes: 0,
            late_join_baseline_install_ms: 0,
            late_join_ready_ms: 0,
            late_join_ready_confirmed: false,
            action_requests_rejected: 0,
            action_reject_reasons: Vec::new(),
        }
    }

    #[test]
    fn dormancy_assertions_require_both_a_deactivation_and_a_reactivation() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "server_ticks": 10,
                "dormancy_assertions": {
                    "min_deactivations": 1,
                    "min_reactivations": 1
                }
            }"#,
        )
        .unwrap();
        let mut server = ServerSummary::default();
        assert!(!dormancy_requirements_met(&scenario, &server));

        server.dormancy_deactivations_total = 1;
        assert!(
            !dormancy_requirements_met(&scenario, &server),
            "a deactivation with no matching reactivation is not enough"
        );

        server.dormancy_reactivations_total = 1;
        assert!(dormancy_requirements_met(&scenario, &server));
    }

    #[test]
    fn dormancy_assertions_are_met_trivially_when_unconfigured() {
        let scenario: Scenario = serde_json::from_str(r#"{ "server_ticks": 10 }"#).unwrap();
        assert!(dormancy_requirements_met(
            &scenario,
            &ServerSummary::default()
        ));
    }

    #[test]
    fn residency_assertions_fail_when_a_pass_is_disabled_or_return_is_missing() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "server_ticks": 10,
                "residency_assertions": {
                    "client": 0,
                    "min_server_evictions": 1,
                    "min_server_reloads": 1,
                    "min_client_evictions": 1,
                    "min_client_reloads_completed": 1,
                    "min_evicted_transaction_gaps": 1,
                    "min_outbound_distance_m": 10.0,
                    "max_return_distance_m": 6.0
                }
            }"#,
        )
        .unwrap();
        let mut server = ServerSummary::default();
        let mut mover = client(1);
        assert!(!residency_requirements_met(
            &scenario,
            &server,
            &[Some(mover.clone())]
        ));

        server.residency_evictions_total = 4;
        server.residency_reloads_total = 2;
        mover.client_residency_evictions = 4;
        mover.client_residency_reloads_completed = 2;
        mover.client_residency_evicted_transaction_gaps = 1;
        mover.movement = Some(MovementRow {
            ticks: 30,
            distance_travelled_m: 12.0,
            max_distance_from_start_m: 14.0,
            max_correction_m: 0.0,
            ground_contact_ratio: 1.0,
            hovered_after_floor_removal: false,
            held_button_release_ok: true,
        });
        assert!(
            !residency_requirements_met(&scenario, &server, &[Some(mover.clone())]),
            "outbound-only motion must not satisfy the return crossing"
        );
        mover.movement.as_mut().unwrap().distance_travelled_m = 4.0;
        assert!(residency_requirements_met(
            &scenario,
            &server,
            &[Some(mover)]
        ));
    }

    /// ENG-30 row 7 increment 13: `min_pinned_bricks` / `min_admission_deferred`
    /// demand real pin-lifetime and admission-enforcement evidence, not only
    /// eviction/reload counts; an opted-in `forbid_required_over_budget`
    /// fails the run if the pass ever reported unresolved capacity pressure
    /// (off by default -- a small budget deliberately paired with a
    /// comfortable interest radius routinely exceeds it, harmlessly).
    #[test]
    fn residency_assertions_cover_pin_lifetime_and_admission_pressure() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "server_ticks": 10,
                "residency_assertions": {
                    "client": 0,
                    "min_pinned_bricks": 3,
                    "min_admission_deferred": 1,
                    "forbid_required_over_budget": true
                }
            }"#,
        )
        .unwrap();
        let mut mover = client(1);
        mover.movement = Some(MovementRow {
            ticks: 10,
            distance_travelled_m: 0.0,
            max_distance_from_start_m: 0.0,
            max_correction_m: 0.0,
            ground_contact_ratio: 1.0,
            hovered_after_floor_removal: false,
            held_button_release_ok: true,
        });
        let mut server = ServerSummary::default();
        assert!(
            !residency_requirements_met(&scenario, &server, &[Some(mover.clone())]),
            "no pinning or deferral yet reported"
        );

        server.residency_pinned_bricks_max = 3;
        assert!(
            !residency_requirements_met(&scenario, &server, &[Some(mover.clone())]),
            "pinning alone, with no admission pressure, is not enough"
        );

        server.residency_admission_deferred_total = 1;
        assert!(residency_requirements_met(
            &scenario,
            &server,
            &[Some(mover.clone())]
        ));

        // Real, unresolved capacity pressure fails the run even though the
        // other two floors are cleared -- the pass never evicts required
        // geometry to force a fit, so this must be visible, not hidden.
        server.residency_required_over_budget_ticks = 1;
        assert!(
            !residency_requirements_met(&scenario, &server, &[Some(mover)]),
            "forbid_required_over_budget was opted into"
        );
    }

    /// ENG-30 row 7 increment 14: `min_client_admission_deferred` requires
    /// real client-side dense-byte/brick admission-enforcement evidence, not
    /// only that the client evicted and reloaded terrain.
    #[test]
    fn residency_assertions_cover_client_admission_deferral() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "server_ticks": 10,
                "residency_assertions": {
                    "client": 0,
                    "min_client_admission_deferred": 1
                }
            }"#,
        )
        .unwrap();
        let mut mover = client(1);
        mover.movement = Some(MovementRow {
            ticks: 10,
            distance_travelled_m: 0.0,
            max_distance_from_start_m: 0.0,
            max_correction_m: 0.0,
            ground_contact_ratio: 1.0,
            hovered_after_floor_removal: false,
            held_button_release_ok: true,
        });
        let server = ServerSummary::default();
        assert!(
            !residency_requirements_met(&scenario, &server, &[Some(mover.clone())]),
            "no admission deferral yet reported"
        );

        mover.client_residency_admission_deferred_total = 1;
        assert!(residency_requirements_met(
            &scenario,
            &server,
            &[Some(mover)]
        ));
    }

    #[test]
    fn gate_requirements_reject_insufficient_work_or_motion() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "server_ticks": 10,
                "minimum_transactions": 2,
                "minimum_motion_snapshots": 1
            }"#,
        )
        .unwrap();
        assert!(!requirements_met(
            &scenario,
            10,
            1,
            &[Some(client(2))],
            false
        ));
        assert!(!requirements_met(
            &scenario,
            10,
            2,
            &[Some(client(0))],
            false
        ));
        assert!(requirements_met(
            &scenario,
            10,
            2,
            &[Some(client(1)), Some(client(4))],
            false
        ));
    }

    #[test]
    fn gate_requirements_need_the_whole_script_and_real_motion() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "server_ticks": 600,
                "minimum_transactions": 4,
                "minimum_motion_snapshots": 1,
                "minimum_body_displacement_m": 0.3,
                "cuts": [
                    { "client": 0, "at_tick": 4,  "cell": [10, 4, 1], "radius": 2 },
                    { "client": 1, "at_tick": 10, "cell": [3, 1, 1],  "radius": 1 },
                    { "client": 0, "at_tick": 30, "cell": [12, 8, 1], "radius": 1, "target": "body" },
                    { "client": 1, "at_tick": 40, "cell": [9, 1, 1],  "radius": 1 }
                ]
            }"#,
        )
        .unwrap();

        // Enough transactions for `minimum_transactions`, but fewer than the
        // four scripted cuts: the run quiesced early.
        assert!(!requirements_met(
            &scenario,
            600,
            3,
            &[Some(client(4)), Some(client(4))],
            false
        ));

        // All four cuts committed and both clients saw real displacement.
        assert!(requirements_met(
            &scenario,
            600,
            4,
            &[Some(client(4)), Some(client(4))],
            false
        ));

        // A stationary body (snapshots arrived, nothing moved) fails.
        let mut still = client(4);
        still.max_body_displacement_m = 0.01;
        assert!(!requirements_met(
            &scenario,
            600,
            4,
            &[Some(still), Some(client(4))],
            false
        ));

        // Nobody landed the body-targeted cut.
        let mut no_body_cut = client(4);
        no_body_cut.body_cut_committed = false;
        assert!(!requirements_met(
            &scenario,
            600,
            4,
            &[Some(no_body_cut.clone()), Some(no_body_cut)],
            false
        ));
    }

    #[test]
    fn body_settled_check_wants_asleep_slow_stable_and_unclipped() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "server_ticks": 600,
                "require_body_settled": true,
                "body_settle_min_stable_ticks": 45
            }"#,
        )
        .unwrap();

        // A clean rest: asleep, ~stationary, held still, shallow penetration.
        let settled = ServerSummary {
            detached_bodies_all_asleep: true,
            detached_body_max_final_speed_m_s: 0.004,
            detached_body_stable_ticks: 120,
            max_contact_penetration_m: 0.02,
            detached_body_min_origin_y_m: -1.25,
            ..Default::default()
        };
        assert!(body_settled(&scenario, &settled));

        // Still drifting downward: not settled.
        assert!(!body_settled(
            &scenario,
            &ServerSummary {
                detached_body_max_final_speed_m_s: 2.5,
                ..settled.clone()
            }
        ));
        // Never came to a sustained stop.
        assert!(!body_settled(
            &scenario,
            &ServerSummary {
                detached_body_stable_ticks: 3,
                ..settled.clone()
            }
        ));
        // Awake / jittering.
        assert!(!body_settled(
            &scenario,
            &ServerSummary {
                detached_bodies_all_asleep: false,
                ..settled.clone()
            }
        ));
        // Rest, but sunk through the remaining floor.
        assert!(!body_settled(
            &scenario,
            &ServerSummary {
                max_contact_penetration_m: 0.9,
                ..settled.clone()
            }
        ));
    }

    #[test]
    fn reordered_snapshot_requirement_only_bites_when_the_proxy_is_active() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "server_ticks": 600,
                "minimum_transactions": 1,
                "minimum_motion_snapshots": 1,
                "minimum_reordered_snapshots": 1,
                "cuts": [ { "client": 0, "at_tick": 4, "cell": [10, 4, 1], "radius": 2 } ]
            }"#,
        )
        .unwrap();

        let mut ordered = client(4);
        ordered.motion_snapshots_out_of_order = 0;

        // Clean loopback run (no proxy): reordering is not required.
        assert!(requirements_met(
            &scenario,
            600,
            1,
            &[Some(ordered.clone())],
            false
        ));
        // Impaired run: a client that never saw a reordered datagram fails.
        assert!(!requirements_met(&scenario, 600, 1, &[Some(ordered)], true));
        // Impaired run with real reordering observed: passes.
        assert!(requirements_met(
            &scenario,
            600,
            1,
            &[Some(client(4))],
            true
        ));
    }

    #[test]
    fn latency_targets_only_bite_on_exercised_buckets_over_budget() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "server_ticks": 600,
                "latency_targets": {
                    "single_brick_commit_p95_ms": 100,
                    "structure_split_p95_ms": 500,
                    "large_collapse_p95_ms": 2000
                }
            }"#,
        )
        .unwrap();

        let mut server = ServerSummary::default();
        // Nothing measured yet: no bucket can fail.
        assert!(latency_targets_met(&scenario, &server));

        // Single-brick p95 within budget, split bucket unexercised: passes.
        server.single_brick_commit_samples = 40;
        server.single_brick_commit_p95_ms = 80.0;
        assert!(latency_targets_met(&scenario, &server));

        // Single-brick p95 over its 100 ms target: fails.
        server.single_brick_commit_p95_ms = 140.0;
        assert!(!latency_targets_met(&scenario, &server));
        server.single_brick_commit_p95_ms = 80.0;

        // An exercised structure-split bucket over 500 ms: fails.
        server.structure_split_samples = 10;
        server.structure_split_p95_ms = 620.0;
        assert!(!latency_targets_met(&scenario, &server));
        server.structure_split_p95_ms = 300.0;
        assert!(latency_targets_met(&scenario, &server));

        // With no targets configured, latency never gates.
        let no_targets: Scenario = serde_json::from_str(r#"{ "server_ticks": 1 }"#).unwrap();
        server.structure_split_p95_ms = 9_999.0;
        assert!(latency_targets_met(&no_targets, &server));
    }

    /// T23 / G3 row 11: `join_budget_requirements_met` / `join_budget_row` are
    /// pure functions over already-measured `ClientSummary` fields — this pins
    /// their pass/fail logic deterministically, with no network involved.
    #[test]
    fn join_budget_requirements_pass_only_within_the_configured_size_and_time_ceiling() {
        let scenario: Scenario = serde_json::from_str(
            r#"{
                "server_ticks": 10,
                "late_join_clients": [1],
                "join_budget": {
                    "client": 1,
                    "max_baseline_compressed_bytes": 1000,
                    "max_ready_ms": 5000
                }
            }"#,
        )
        .unwrap();
        let budget = scenario.join_budget.as_ref().unwrap();
        assert_eq!(
            budget.bandwidth_bytes_per_sec,
            1024 * 1024,
            "default 1 MiB/s"
        );
        assert_eq!(budget.rtt_ms, 100, "default 100 ms RTT");
        assert_eq!(budget.loss_percent, 2, "default 2% loss");

        // No summary at all for the configured client: fails closed.
        assert!(!join_budget_requirements_met(&scenario, &[]));

        let mut joiner = client(2);
        joiner.late_join = true;
        joiner.late_join_baseline_compressed_bytes = 900;
        joiner.late_join_ready_ms = 4000;
        joiner.late_join_ready_confirmed = true;
        let clients = [Some(client(2)), Some(joiner.clone())];
        assert!(
            join_budget_requirements_met(&scenario, &clients),
            "under both ceilings and confirmed ready should pass"
        );
        let row = join_budget_row(&scenario, &clients);
        assert!(row.configured && row.within_budget);
        assert_eq!(row.measured_baseline_compressed_bytes, 900);
        assert_eq!(row.measured_ready_ms, 4000);

        // Over the compressed-size ceiling: fails.
        let mut too_big = joiner.clone();
        too_big.late_join_baseline_compressed_bytes = 1001;
        assert!(!join_budget_requirements_met(
            &scenario,
            &[Some(client(2)), Some(too_big)]
        ));

        // Over the time-to-ready ceiling: fails.
        let mut too_slow = joiner.clone();
        too_slow.late_join_ready_ms = 5001;
        assert!(!join_budget_requirements_met(
            &scenario,
            &[Some(client(2)), Some(too_slow)]
        ));

        // A small, fast baseline that never actually confirmed "ready" (a
        // motion-keyframe wait that timed out) does not count as a pass —
        // small/fast is not the same as caught up and converged.
        let mut unconfirmed = joiner.clone();
        unconfirmed.late_join_ready_confirmed = false;
        assert!(!join_budget_requirements_met(
            &scenario,
            &[Some(client(2)), Some(unconfirmed)]
        ));

        // A hash mismatch (`result != "passed"`) is never a passing join no
        // matter how small/fast the transfer was — do not count a failed join
        // as satisfying the budget.
        let mut mismatched = joiner.clone();
        mismatched.result = "failed".into();
        assert!(!join_budget_requirements_met(
            &scenario,
            &[Some(client(2)), Some(mismatched)]
        ));

        // No `join_budget` configured: always passes (nothing to measure).
        let unconfigured: Scenario = serde_json::from_str(r#"{ "server_ticks": 1 }"#).unwrap();
        assert!(join_budget_requirements_met(&unconfigured, &[]));
        assert!(!join_budget_row(&unconfigured, &[]).configured);
    }

    /// The declared ordinary-edit mix: 1 terrain dig per 10 ordinary edits, the
    /// rest comb-tooth cuts; every terrain dig is distinct and on the dig lane,
    /// and the 30-minute lane stays inside both supplies.
    #[test]
    fn the_declared_edit_mix_includes_terrain_digs() {
        let s = SustainedEdits {
            start_tick: 1800,
            small_rate_per_sec: 10.0,
            blast_interval_sec: 10.0,
            trailing_buffer_ticks: 300,
            mode: SustainedMode::G4Bodies,
            ordinary_offset_ticks: 120,
            giant_at_offset_ticks: Some(60),
            terrain_edit_every: 10,
        };
        let server_ticks = 1800 + 108_000 + 300;
        let (n_small, _, _) = sustained_counts(&s, server_ticks);
        assert_eq!(n_small, (108_000 - 120) / 6, "the full 10 edits/s is kept");
        let by_client = generate_sustained_cuts(&s, server_ticks, 8);
        let all: Vec<&GeneratedCut> = by_client.values().flatten().collect();
        let terrain: Vec<&&GeneratedCut> = all
            .iter()
            .filter(|c| c.target.is_none() && c.radius == 1)
            .collect();
        assert_eq!(terrain.len() as u64, n_small / 10, "one in ten is terrain");
        let mut cells = std::collections::HashSet::new();
        for c in &terrain {
            assert!(cells.insert(c.cell), "terrain dig repeats {:?}", c.cell);
            assert!(c.cell[1] == 1 && c.cell[2] >= 128);
        }
        let body_edits = all
            .iter()
            .filter(|c| c.target == Some("body") && c.radius == 1)
            .count() as u64;
        assert_eq!(body_edits + terrain.len() as u64, n_small);
    }

    /// T23 / G4 integrated stream: 10 ordinary comb-tooth cuts/s, one tower blast
    /// per 10 s, and the giant collapse first; every cut is aimed at a real
    /// body by entity id, no (entity, cell) repeats, and the 30-minute lane
    /// stays inside the yard's supply.
    #[test]
    fn g4_body_stream_targets_fresh_bodies_and_includes_the_giant() {
        let s = SustainedEdits {
            start_tick: 1800,
            small_rate_per_sec: 10.0,
            blast_interval_sec: 10.0,
            trailing_buffer_ticks: 300,
            mode: SustainedMode::G4Bodies,
            ordinary_offset_ticks: 120,
            giant_at_offset_ticks: Some(60),
            terrain_edit_every: 0,
        };
        for measured in [7_200u64, 108_000] {
            let server_ticks = 1800 + measured + 300;
            let (n_small, n_blasts, _) = sustained_counts(&s, server_ticks);
            assert_eq!(n_small, (measured - 120) / 6);
            assert_eq!(n_blasts, (measured - 120) / 600);
            assert_eq!(sustained_giant_commits(&s), 1);
            let by_client = generate_sustained_cuts(&s, server_ticks, 8);
            let all: Vec<&GeneratedCut> = by_client.values().flatten().collect();
            assert_eq!(all.len() as u64, n_small + n_blasts + 1);
            assert!(
                all.iter()
                    .all(|c| c.target == Some("body") && c.entity.is_some())
            );
            let mut seen = std::collections::HashSet::new();
            for c in &all {
                assert!(
                    seen.insert((c.entity, c.cell)),
                    "repeated body edit {:?} {:?}",
                    c.entity,
                    c.cell
                );
                assert!(c.tick >= 1800 + 60);
            }
            assert!(by_client.len() == 8, "every client edits");
            let giants = all
                .iter()
                .filter(|c| c.entity == Some(spall_voxel::fixtures::G4_ENTITY_FIRST))
                .count();
            assert_eq!(giants, 1);
            let blasts = all.iter().filter(|c| c.radius == 8).count() as u64;
            assert_eq!(blasts, n_blasts);
        }
    }

    /// T23 / G3 row 13: `generate_sustained_cuts` produces exactly the count
    /// `sustained_counts` (and therefore `requirements_met`) expects, every
    /// generated cell stays within the proven-safe ranges (see
    /// `docs/reports/G3.md` increment 18 for why `y = 1` / `y = 12, z = 12`
    /// or `z = 73` stay inside the declared volume bounds), and every
    /// connected client is actually used.
    #[test]
    fn sustained_cuts_match_expected_count_stay_in_safe_ranges_and_cover_every_client() {
        let s = SustainedEdits {
            start_tick: 1800,
            small_rate_per_sec: 10.0,
            blast_interval_sec: 10.0,
            trailing_buffer_ticks: 120,
            mode: SustainedMode::TerrainCells,
            ordinary_offset_ticks: 0,
            giant_at_offset_ticks: None,
            terrain_edit_every: 0,
        };
        let server_ticks = 1800 + 7200 + 120; // 30 s warmup + 2 measured minutes + buffer
        let (n_small, n_blasts, _) = sustained_counts(&s, server_ticks);
        assert_eq!(n_small, 1200, "10 edits/s over 2 measured minutes");
        assert_eq!(n_blasts, 12, "one blast/10s over 2 measured minutes");

        let by_client = generate_sustained_cuts(&s, server_ticks, 8);
        let total: usize = by_client.values().map(|v| v.len()).sum();
        assert_eq!(total as u64, n_small + n_blasts);
        assert_eq!(
            by_client.len(),
            8,
            "round-robin across 8 clients must actually reach all 8"
        );

        for cuts in by_client.values() {
            for c in cuts {
                assert!(
                    c.tick >= s.start_tick && c.tick < server_ticks,
                    "generated tick {} outside [{}, {server_ticks})",
                    c.tick,
                    s.start_tick
                );
                let [x, y, z] = c.cell;
                assert!(x >= 0, "cell x must not be negative: {c:?}");
                if c.radius == 8 {
                    // Blasts: the two proven-safe fixed points only.
                    assert!(
                        c.cell == BLAST_WEST || c.cell == BLAST_EAST,
                        "blast at an unproven cell: {c:?}"
                    );
                } else {
                    // Small edits: proven-safe y/z rows, x within the safe span.
                    assert!(y == SMALL_Y, "small edit at unproven y: {c:?}");
                    assert!(
                        z == WEST_SMALL_Z || z == EAST_SMALL_Z,
                        "small edit at unproven z: {c:?}"
                    );
                }
            }
        }
    }

    /// The 30-minute soak scales the same formula to 18,000 small edits + 180
    /// blasts — this is the count `docs/validation.md`'s gate spec requires
    /// ("10 ordinary edits/s" / "one 4 m blast every 10 seconds" for 30 min).
    #[test]
    fn sustained_cuts_scale_to_the_thirty_minute_soak_count() {
        let s = SustainedEdits {
            start_tick: 1800,
            small_rate_per_sec: 10.0,
            blast_interval_sec: 10.0,
            trailing_buffer_ticks: 200,
            mode: SustainedMode::TerrainCells,
            ordinary_offset_ticks: 0,
            giant_at_offset_ticks: None,
            terrain_edit_every: 0,
        };
        let server_ticks = 1800 + 108_000 + 200; // 30 s warmup + 30 measured minutes + buffer
        let (n_small, n_blasts, _) = sustained_counts(&s, server_ticks);
        assert_eq!(n_small, 18_000);
        assert_eq!(n_blasts, 180);
    }
}

// --- the run -----------------------------------------------------------------

fn run(run: Run, unique_output: impl FnOnce() -> PathBuf) -> Result<(), XtaskError> {
    let raw = fs::read_to_string(&run.scenario_path).map_err(|source| XtaskError::Output {
        path: run.scenario_path.display().to_string(),
        source,
    })?;
    let scenario: Scenario = serde_json::from_str(&raw).map_err(|e| {
        XtaskError::Capability(format!(
            "scenario {} is not valid JSON: {e}",
            run.scenario_path.display()
        ))
    })?;

    let clients = run.clients.unwrap_or(scenario.clients).clamp(1, 16);
    let server_ticks = run.server_ticks.unwrap_or(scenario.server_ticks);
    let output = run.output.clone().unwrap_or_else(unique_output);
    fs::create_dir_all(&output).map_err(|source| XtaskError::Output {
        path: output.display().to_string(),
        source,
    })?;

    // Build both binaries once. Performance fixtures may explicitly select the
    // release profile; the scenario assertions remain identical.
    let profile = if scenario.release_profile {
        "release"
    } else {
        "debug"
    };
    let mut server_build = vec!["build", "-p", "sandbox", "--bin", "sandbox-server"];
    if scenario.release_profile {
        server_build.push("--release");
    }
    run_cargo(&server_build)?;
    let mut client_build = vec![
        "build",
        "-p",
        "sandbox",
        "--features",
        "client",
        "--bin",
        "sandbox-client",
    ];
    if scenario.release_profile {
        client_build.push("--release");
    }
    run_cargo(&client_build)?;

    // Per-run credentials.
    let token_hex = random_hex_32(run.seed ^ 0xA5A5_A5A5_A5A5_A5A5);
    let token_file = output.join("join.token");
    write_file(&token_file, token_hex.as_bytes())?;
    let fp_file = output.join("server.fingerprint");
    let addr_file = output.join("server.addr");
    let _ = fs::remove_file(&fp_file);
    let _ = fs::remove_file(&addr_file);
    let _ = fs::remove_file(addr_file.with_extension("tmp"));

    let server_summary_path = output.join("server.summary.json");
    let _ = fs::remove_file(&server_summary_path);

    // Late joiners must connect *after* the tick loop is running, so they are
    // not counted toward `min_clients` — the run starts once the early clients
    // are up and cutting.
    let late_count = scenario
        .late_join_clients
        .iter()
        .filter(|i| **i < clients)
        .count() as u64;
    let min_clients = clients.saturating_sub(late_count).max(1);

    // Spawn the server.
    let mut server_cmd = Command::new(sandbox_binary_profile("sandbox-server", profile));
    server_cmd.args([
        "--serve",
        "--listen",
        "127.0.0.1:0",
        "--join-token-file",
        &token_file.display().to_string(),
        "--fingerprint-out",
        &fp_file.display().to_string(),
        "--addr-out",
        &addr_file.display().to_string(),
        "--summary-json",
        &server_summary_path.display().to_string(),
        "--log-json",
        &output.join("server.jsonl").display().to_string(),
        "--ticks",
        &server_ticks.to_string(),
        "--min-clients",
        &min_clients.to_string(),
        "--max-clients",
        &scenario
            .server_max_clients
            .unwrap_or(clients as usize)
            .to_string(),
        "--scene",
        &scenario.scene,
        "--quiescence-ticks",
        &scenario.quiescence_ticks.to_string(),
        "--paced",
        // Scenario files script cuts at explicit cells that no aim ray would
        // produce; the harness is the authenticated dev-scenario path (ENG-47).
        "--dev-unvalidated-actions",
    ]);
    if scenario.wake_audit {
        server_cmd.arg("--wake-audit");
    }
    if let Some(rate) = scenario.baseline_rate_limit_bytes_per_sec {
        server_cmd.args(["--baseline-rate-limit-bytes-per-sec", &rate.to_string()]);
    }
    if let Some(timing) = &scenario.server_timing {
        server_cmd.args([
            "--timing-warmup-ticks",
            &timing.warmup_ticks.to_string(),
            "--timing-measured-ticks",
            &timing.measured_ticks.to_string(),
            "--timing-max-samples",
            &timing.max_samples.to_string(),
        ]);
    }
    if scenario.require_body_settled {
        // ENG-61: run physics past edit-quiescence until the detached body sleeps
        // so the run can actually show it come to rest.
        server_cmd.arg("--await-body-settle");
    }
    if let Some(budget) = scenario.residency_budget_bricks {
        server_cmd.args(["--residency-budget-bricks", &budget.to_string()]);
        if let Some(r) = scenario.residency_radius_bricks {
            server_cmd.args(["--residency-radius-bricks", &r.to_string()]);
        }
        if let Some(bytes) = scenario.residency_budget_dense_bytes {
            server_cmd.args(["--residency-budget-dense-bytes", &bytes.to_string()]);
        }
        if scenario.residency_disk_backing {
            server_cmd.arg("--residency-disk-backing");
        }
    }
    if let Some(mi) = &scenario.motion_interest {
        server_cmd.args([
            "--motion-interest",
            "--motion-near-m",
            &mi.near_m.to_string(),
            "--motion-far-m",
            &mi.far_m.to_string(),
            "--motion-far-interval",
            &mi.far_interval.to_string(),
            "--motion-client-budget-bytes",
            &mi.client_budget_bytes.to_string(),
        ]);
        if mi.congestion_aware {
            server_cmd.arg("--motion-congestion-aware");
        }
        if let Some(a) = mi.static_anchor {
            server_cmd.args([
                "--motion-static-anchor",
                &format!("{},{},{}", a[0], a[1], a[2]),
            ]);
        }
    }
    if let Some(re) = &scenario.retry_exhaustion {
        server_cmd.args([
            "--catch-up-cap",
            &re.catch_up_cap.to_string(),
            "--max-join-retries",
            &re.max_join_retries.to_string(),
        ]);
    }
    if scenario.dormancy {
        server_cmd.arg("--dormancy");
    }
    // T11 exact-replay check (and the T23 cold-restart check) both journal every
    // committed transaction to a world DB. Replay rebuilds from the tick-0
    // baseline; restart recovers a fresh server from the shutdown checkpoint.
    let replay_db = output.join("world.db");
    if scenario.replay_check || scenario.restart_check {
        let _ = fs::remove_file(&replay_db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(output.join(format!("world.db{suffix}")));
        }
        server_cmd.args([
            "--world",
            &output.display().to_string(),
            "--save",
            // Only a tick-0 baseline + a shutdown checkpoint, so the durable
            // journal after the base is the entire committed-transaction stream.
            "--checkpoint-interval-ticks",
            "0",
        ]);
    }
    hide_console(&mut server_cmd);
    let mut guard = ChildGuard::default();
    let server_child = server_cmd.spawn().map_err(|source| XtaskError::Output {
        path: "sandbox-server (spawn)".into(),
        source,
    })?;
    guard.push("server".into(), server_child);

    // Wait until it has bound (writes the addr file), or died.
    let bound: SocketAddr = wait_for_addr(&addr_file, &mut guard, Duration::from_secs(20))?
        .parse()
        .map_err(|_| XtaskError::Capability("server wrote an unparseable bound address".into()))?;

    // Optional per-client encrypted-packet proxies. T23 / G3 row 11: a
    // configured `join_budget` shapes only its named client (bandwidth + RTT +
    // loss) — every other client gets a transparent proxy so the workload
    // that builds the edit history is not itself bandwidth-limited. Otherwise
    // every client shares the uniform `--loss-percent` profile (unchanged).
    let proxies = if let Some(budget) = &scenario.join_budget {
        let mut plans: Vec<PacketFaultPlan> = (0..clients)
            .map(|i| PacketFaultPlan::transparent(run.seed ^ (i + 1)))
            .collect();
        let shaped = PacketFaultPlan {
            delay: Duration::from_millis(budget.rtt_ms / 2),
            jitter: Duration::from_millis(budget.jitter_ms),
            loss_ratio: f64::from(budget.loss_percent) / 100.0,
            ..PacketFaultPlan::shaped(budget.seed, budget.bandwidth_bytes_per_sec)
        };
        if let Some(slot) = plans.get_mut(budget.client as usize) {
            *slot = shaped;
        }
        Some(ProxyFarm::spawn_with_plans(bound, plans)?)
    } else if let Some(re) = &scenario.retry_exhaustion {
        // T23 / G3 row 10 follow-up: the same "shape only the named client"
        // pattern as `join_budget` above, but tuned to make that one client's
        // catch-up queue overflow (severely bandwidth-limited, so a re-capture
        // can never land before `sustained_edits`'s next commit overflows the
        // tiny `catch_up_cap` again) rather than to measure a join budget.
        let mut plans: Vec<PacketFaultPlan> = (0..clients)
            .map(|i| PacketFaultPlan::transparent(run.seed ^ (i + 1)))
            .collect();
        let shaped = PacketFaultPlan {
            delay: Duration::from_millis(re.rtt_ms / 2),
            ..PacketFaultPlan::shaped(re.seed, re.bandwidth_bytes_per_sec)
        };
        if let Some(slot) = plans.get_mut(re.client as usize) {
            *slot = shaped;
        }
        Some(ProxyFarm::spawn_with_plans(bound, plans)?)
    } else if let Some(env) = &scenario.network_envelope {
        let one_way = Duration::from_millis((env.rtt_ms / 2).saturating_sub(env.jitter_ms));
        let plans: Vec<PacketFaultPlan> = (0..clients)
            .map(|i| PacketFaultPlan {
                delay: one_way,
                jitter: Duration::from_millis(env.jitter_ms * 2),
                loss_ratio: env.loss_percent / 100.0,
                rate_limit_bytes_per_sec: env.bandwidth_bytes_per_sec.unwrap_or(0),
                ..PacketFaultPlan::transparent(env.seed ^ (i + 1))
            })
            .collect();
        Some(ProxyFarm::spawn_with_plans(bound, plans)?)
    } else if run.loss_percent > 0 {
        Some(ProxyFarm::spawn(
            bound,
            clients as usize,
            run.loss_percent,
            run.seed,
        )?)
    } else {
        None
    };
    let targets: Vec<SocketAddr> = match &proxies {
        Some(farm) => farm.addrs.clone(),
        None => vec![bound; clients as usize],
    };

    // Group scripted cuts by client.
    let mut by_client: BTreeMap<u64, Vec<CutSpec>> = BTreeMap::new();
    for cut in &scenario.cuts {
        by_client
            .entry(cut.client.min(clients.saturating_sub(1)))
            .or_default()
            .push(cut.clone());
    }
    // T23 / G3 row 13: the generated sustained stream, kept separate from
    // `by_client` (a different element type) until the per-client dispatch
    // below decides `--cut` args vs a `--cuts-file`.
    let generated_by_client: BTreeMap<u64, Vec<GeneratedCut>> = scenario
        .sustained_edits
        .as_ref()
        .map(|s| generate_sustained_cuts(s, server_ticks, scenario.edit_clients.unwrap_or(clients)))
        .unwrap_or_default();

    // Spawn the clients.
    let client_timeout = run
        .timeout
        .saturating_sub(Duration::from_secs(3))
        .as_millis() as u64;
    let mut client_summary_paths = Vec::new();
    for i in 0..clients {
        let summary = output.join(format!("client{i}.summary.json"));
        let _ = fs::remove_file(&summary);
        client_summary_paths.push(summary.clone());
        let mut c = Command::new(sandbox_binary_profile("sandbox-client", profile));
        if let Some(ms) = scenario.client_handshake_timeout_ms {
            // Diagnostic override only (never the production default): lets a run
            // separate handshake-queueing from other join failures.
            c.env("SPALL_HANDSHAKE_TIMEOUT_MS", ms.to_string());
        }
        c.args([
            "--connect",
            &targets[i as usize].to_string(),
            "--server-fingerprint",
            &fp_file.display().to_string(),
            "--join-token-file",
            &token_file.display().to_string(),
            "--summary-json",
            &summary.display().to_string(),
            "--log-json",
            &output
                .join(format!("client{i}.jsonl"))
                .display()
                .to_string(),
            "--timeout-ms",
            &client_timeout.to_string(),
            "--client-index",
            &i.to_string(),
            "--scene",
            &scenario.scene,
        ]);
        for cut in by_client.get(&i).into_iter().flatten() {
            let mut spec = format!(
                "{}:{},{},{}:{}",
                cut.at_tick, cut.cell[0], cut.cell[1], cut.cell[2], cut.radius
            );
            if cut.target == CutTarget::Body {
                spec.push_str(":body");
            }
            c.args(["--cut", &spec]);
        }
        // T23 / G3 row 13: the generated sustained stream can run into the
        // thousands of entries per client — well past what fits as individual
        // `--cut` arguments on one process command line (Windows'
        // `CreateProcess` caps the whole command line around 32K chars) — so
        // it always goes through `--cuts-file`, never inline.
        if let Some(cuts) = generated_by_client.get(&i)
            && !cuts.is_empty()
        {
            let path = output.join(format!("client{i}.sustained-cuts.json"));
            let body = serde_json::to_vec(cuts).expect("generated cuts are serializable");
            fs::write(&path, body).map_err(|source| XtaskError::Output {
                path: path.display().to_string(),
                source,
            })?;
            c.args(["--cuts-file", &path.display().to_string()]);
        }
        let mut client_moves = false;
        for path in scenario.player_paths.iter().filter(|p| p.client == i) {
            client_moves = true;
            for leg in &path.legs {
                c.args([
                    "--move",
                    &format!(
                        "{}:{}:{},{},{}:{}",
                        leg.from,
                        leg.to,
                        leg.movement[0],
                        leg.movement[1],
                        leg.movement[2],
                        leg.buttons
                    ),
                ]);
            }
        }
        // Slice E2: client terrain residency, only where there is a mover.
        if client_moves && let Some(budget) = scenario.client_residency_budget_bricks {
            c.args(["--residency-budget-bricks", &budget.to_string()]);
            if let Some(r) = scenario.client_residency_radius_bricks {
                c.args(["--residency-radius-bricks", &r.to_string()]);
            }
            if let Some(bytes) = scenario.client_residency_budget_dense_bytes {
                c.args(["--residency-budget-dense-bytes", &bytes.to_string()]);
            }
        }
        if scenario.late_join_clients.contains(&i) {
            c.args([
                "--late-join",
                "--connect-delay-ms",
                &scenario.late_join_connect_delay_ms.to_string(),
            ]);
        } else {
            // T23 / G3 row 2 follow-up (increment 23): every client process
            // below is `spawn()`ed back-to-back with no synchronization, but
            // `spall_net::NetServer::authenticate` assigns each connection's
            // `SessionId`/slot strictly by arrival order
            // (`SessionLease::acquire`) — there is no way for a client to
            // request a specific slot. A scenario's `player_paths`/`cuts`
            // ownership and `Scene::player_spawns()` slot assignment are keyed
            // to *this loop's own launch index* `i`, which is only guaranteed
            // to match the slot the server hands out if connections complete
            // in launch order. Under real OS process-scheduling contention
            // (this harness runs the server and every client as separate
            // processes) that is not guaranteed — confirmed live: a scripted
            // mover's own predictor occasionally latched onto a *different*
            // client's entity/spawn entirely (`t23-g3-full-envelope`, west
            // walker landing at the east spawn, `111 m` instead of `7 m`),
            // producing an apparently-random final position with no relation
            // to the intended script. Staggering each client's own connect
            // attempt by its launch index (independent of, and much smaller
            // than, `late_join_connect_delay_ms` above) makes connection
            // arrival order match launch order deterministically — the QUIC
            // handshake + auth round trip this races against is at most a few
            // milliseconds on loopback, so 50 ms per client is a comfortable
            // margin without meaningfully slowing a large-client-count
            // scenario's startup.
            const CONNECT_STAGGER_MS: u64 = 50;
            c.args(["--connect-delay-ms", &(i * CONNECT_STAGGER_MS).to_string()]);
        }
        hide_console(&mut c);
        let child = c.spawn().map_err(|source| XtaskError::Output {
            path: format!("sandbox-client{i} (spawn)"),
            source,
        })?;
        guard.push(format!("client{i}"), child);
    }

    // Supervise every child to exit (or the deadline).
    let statuses = guard.wait_all(run.timeout);
    if let Some(farm) = proxies {
        farm.shutdown();
    }

    // Read summaries and decide.
    let server: Option<ServerSummary> = read_json(&server_summary_path);
    let client_summaries: Vec<Option<ClientSummary>> =
        client_summary_paths.iter().map(read_json).collect();

    let server_ok = statuses.get("server").copied().flatten() == Some(0);
    let server = match server {
        Some(s) => s,
        None => {
            return finish(
                &output,
                SessionSummary {
                    version: 5,
                    result: "failed",
                    scenario: scenario.name.clone(),
                    clients,
                    loss_percent: run.loss_percent,
                    server_ticks_run: 0,
                    transactions_committed: 0,
                    max_detached_body_brick_span: 0,
                    body_settled: false,
                    detached_body_max_final_speed_m_s: 0.0,
                    detached_body_stable_ticks: 0,
                    max_contact_penetration_m: 0.0,
                    replay_checked: false,
                    replay_matches: false,
                    replayed_topology_events: 0,
                    restart_checked: false,
                    restart_recovered_hash_matches: false,
                    restart_reconnect_hash_matches: false,
                    restart_detail: String::new(),
                    restart_recovered_world_hash: String::new(),
                    residency_backing_disk_bytes: None,
                    residency_pinned_bricks_max: 0,
                    residency_admission_deferred_total: 0,
                    residency_required_over_budget_ticks: 0,
                    residency_digest_bytes_final: 0,
                    residency_backing_resident_bytes: None,
                    process_peak_memory_bytes: None,
                    residency_checkpoint_bricks_captured_total: 0,
                    residency_checkpoint_bricks_logical_total: 0,
                    impaired_late_join_bounded_failure: false,
                    agreed_world_hash: String::new(),
                    all_hashes_match: false,
                    requirements_met: false,
                    verdict: SessionVerdict::server_missing(),
                    admission: AdmissionRow::empty(),
                    server_timing: ServerTimingRow::unconfigured(),
                    g4: g4::G4Row::unconfigured(),
                    join_budget: JoinBudgetRow::unconfigured(),
                    per_client: Vec::new(),
                    app_egress_bytes: 0,
                    transport_egress_bytes: 0,
                    per_client_egress: Vec::new(),
                    note: "server produced no summary; inspect server.jsonl",
                },
            );
        }
    };

    let agreed = server.final_world_hash.clone();
    let mut all_match = server_ok && server.result == "passed";
    // Hash agreement is tracked apart from every other requirement.
    let mut hash_agree = true;
    let mut rows = Vec::new();
    let mut bounded_join_failure_seen = false;
    for (i, summary) in client_summaries.iter().enumerate() {
        let exit_code = statuses.get(&format!("client{i}")).copied().flatten();
        let exit_ok = exit_code == Some(0);
        let is_mover = scenario.player_paths.iter().any(|p| p.client == i as u64);
        let is_late = scenario.late_join_clients.contains(&(i as u64));
        match summary {
            Some(c) => {
                let hash_ok = c.final_world_hash == agreed;
                // T23 / G3 row 10: an impaired late joiner that ended in a
                // *bounded explicit* failure — a "join-failed" summary and a
                // real process exit (not a deadline kill) — is acceptable under
                // `late_join_may_fail`; it need not match the agreed hash.
                let bounded_join_failure = scenario.late_join_may_fail
                    && is_late
                    && c.result == "join-failed"
                    && exit_code.is_some();
                // A late joiner that caught up entirely from the baseline (no
                // cuts after it joined) is still a pass.
                let progressed = is_mover
                    || c.transactions_applied >= 1
                    || (c.late_join && c.baseline_bricks > 0);
                // T19: a scripted mover passes on its prediction summary — it
                // may commit no transactions of its own.
                let movement_ok = if is_mover {
                    match &c.movement {
                        Some(m) => {
                            m.ticks > 0
                                && (!scenario.movement.expect_no_hover
                                    || !m.hovered_after_floor_removal)
                                && m.held_button_release_ok
                                && m.max_correction_m <= scenario.movement.max_correction_m
                                && m.distance_travelled_m >= scenario.movement.min_distance_m
                                && m.ground_contact_ratio
                                    >= scenario.movement.min_ground_contact_ratio
                        }
                        None => false,
                    }
                } else {
                    true
                };
                if bounded_join_failure {
                    bounded_join_failure_seen = true;
                }
                let client_ok = bounded_join_failure
                    || (exit_ok
                        && c.result == "passed"
                        && hash_ok
                        && progressed
                        && movement_ok
                        && c.transactions_rejected == 0);
                all_match &= client_ok;
                hash_agree &= hash_ok || bounded_join_failure;
                rows.push(ClientRow {
                    index: i as u64,
                    result: c.result.clone(),
                    transactions_applied: c.transactions_applied,
                    repair_requests_sent: c.repair_requests_sent,
                    transactions_rejected: c.transactions_rejected,
                    motion_snapshots: c.motion_snapshots,
                    motion_snapshots_out_of_order: c.motion_snapshots_out_of_order,
                    max_body_displacement_m: c.max_body_displacement_m,
                    body_cut_committed: c.body_cut_committed,
                    client_residency_evictions: c.client_residency_evictions,
                    client_residency_reloads_requested: c.client_residency_reloads_requested,
                    client_residency_reloads_completed: c.client_residency_reloads_completed,
                    client_residency_budget_miss_steps: c.client_residency_budget_miss_steps,
                    client_residency_admission_deferred_total: c
                        .client_residency_admission_deferred_total,
                    client_residency_evicted_transaction_gaps: c
                        .client_residency_evicted_transaction_gaps,
                    hash_matches_server: hash_ok,
                    late_join_baseline_compressed_bytes: c.late_join_baseline_compressed_bytes,
                    late_join_baseline_install_ms: c.late_join_baseline_install_ms,
                    late_join_ready_ms: c.late_join_ready_ms,
                    late_join_ready_confirmed: c.late_join_ready_confirmed,
                    action_requests_rejected: c.action_requests_rejected,
                    action_reject_reasons: c.action_reject_reasons.clone(),
                });
            }
            None => {
                all_match = false;
                hash_agree = false;
                rows.push(ClientRow {
                    index: i as u64,
                    result: "no-summary".into(),
                    transactions_applied: 0,
                    repair_requests_sent: 0,
                    transactions_rejected: 0,
                    motion_snapshots: 0,
                    motion_snapshots_out_of_order: 0,
                    max_body_displacement_m: 0.0,
                    body_cut_committed: false,
                    client_residency_evictions: 0,
                    client_residency_reloads_requested: 0,
                    client_residency_reloads_completed: 0,
                    client_residency_budget_miss_steps: 0,
                    client_residency_admission_deferred_total: 0,
                    client_residency_evicted_transaction_gaps: 0,
                    hash_matches_server: false,
                    late_join_baseline_compressed_bytes: 0,
                    late_join_baseline_install_ms: 0,
                    late_join_ready_ms: 0,
                    late_join_ready_confirmed: false,
                    action_requests_rejected: 0,
                    action_reject_reasons: Vec::new(),
                });
            }
        }
    }
    let mut requirements_met = requirements_met(
        &scenario,
        server_ticks,
        server.transactions_committed,
        &client_summaries,
        run.loss_percent > 0
            || scenario.join_budget.is_some()
            || scenario.network_envelope.is_some(),
    );
    if !residency_requirements_met(&scenario, &server, &client_summaries) {
        requirements_met = false;
    }
    if !dormancy_requirements_met(&scenario, &server) {
        requirements_met = false;
    }
    if !residency_disk_backing_requirements_met(&scenario, &server) {
        requirements_met = false;
    }
    // T23 / G3 row 11: the configured client's measured compressed baseline
    // size and time-to-ready must both stay within budget, and it must have
    // actually converged — small/fast is not a pass if the join itself failed.
    let join_budget = join_budget_row(&scenario, &client_summaries);
    if scenario.join_budget.is_some() && !join_budget.within_budget {
        requirements_met = false;
    }
    // Cross-brick ownership transfer: a detached body's cells must have been
    // taken out of terrain across a brick boundary (server-authoritative).
    if scenario.minimum_detached_body_brick_span > 0
        && server.max_detached_body_brick_span < scenario.minimum_detached_body_brick_span
    {
        requirements_met = false;
    }

    // ENG-61: a "body comes to rest on remaining structure" fixture also requires
    // the server's end-of-run report to show every detached body settled.
    let body_settled = body_settled(&scenario, &server);
    if scenario.require_body_settled && !body_settled {
        requirements_met = false;
    }

    // T11 exact replay: fold the committed topology-event stream from the tick-0
    // baseline and require the rebuilt canonical hash to equal the live hash.
    let replay = if scenario.replay_check {
        let r = run_replay_check(&replay_db, &agreed, &output, profile);
        if !(r.ran && r.matches) {
            requirements_met = false;
            hash_agree = false;
        }
        Some(r)
    } else {
        None
    };

    // T23 / G3: cold-restart a fresh server over the same world DB (recovery
    // from the shutdown checkpoint + journal), then a fresh `--late-join` client
    // against it. Both must reach the agreed hash.
    let restart = if scenario.restart_check {
        // T23 / G3 row 7 item 2 (increment 31): when the live run was
        // disk-backed, the cold-restarted server also gets matching residency
        // + disk-backing flags, pointed at the same `<world>/residency.db` --
        // proving a restart reads the durable backing back correctly, not
        // just that the world DB's own journal recovers (which residency,
        // by design, does not affect). Every other scenario's restart run
        // passes `None`/`false` here, exactly as before this increment.
        let restart_residency = scenario.residency_disk_backing.then_some(RestartResidency {
            budget_bricks: scenario.residency_budget_bricks.unwrap_or(0),
            radius_bricks: scenario.residency_radius_bricks,
        });
        let r = run_restart_check(
            &output,
            &scenario.scene,
            &token_file,
            &agreed,
            run.timeout,
            profile,
            restart_residency,
        );
        if !(r.ran && r.recovered_matches && r.reconnect_matches) {
            requirements_met = false;
        }
        Some(r)
    } else {
        None
    };

    // T11a / ENG-62: assert the measured commit-latency p95s against the G1 gate
    // targets when the scenario configured them.
    let latency_ok = latency_targets_met(&scenario, &server);
    if scenario.latency_targets.is_some() && !latency_ok {
        requirements_met = false;
    }
    let server_timing_ok = server_timing_requirements_met(&scenario, &server);
    if scenario.server_timing.is_some() && !server_timing_ok {
        requirements_met = false;
    }
    let g4_row = scenario
        .g4_telemetry
        .as_ref()
        .map_or_else(g4::G4Row::unconfigured, |cfg| {
            // The harness knows what it drove: fill the expectations the scenario
            // left at zero from the very counts the generator used.
            let mut cfg = cfg.clone();
            if let Some(s) = &scenario.sustained_edits {
                let (n_small, n_blasts, _) = sustained_counts(s, server_ticks);
                if cfg.expected_ordinary_edits == 0 {
                    cfg.expected_ordinary_edits = n_small;
                }
                if cfg.expected_blasts == 0 {
                    cfg.expected_blasts = n_blasts;
                }
            }
            if cfg.expected_baseline_sends == 0 {
                cfg.expected_baseline_sends = scenario.late_join_clients.len() as u64;
            }
            let lags: Vec<g4::ClientLag> = client_summaries
                .iter()
                .filter_map(|c| c.as_ref())
                .filter(|c| c.tx_received > 0)
                .map(|c| g4::ClientLag {
                    p95_ms: c.topology_lag_p95_ms,
                    max_ms: c.topology_lag_max_ms,
                })
                .collect();
            // `#[serde(flatten)]` leaves fields that ServerSummary also names itself (consumed by
            // the outer struct) at their defaults in `g4`, so copy them across.
            let mut facts = server.g4.clone();
            facts.actions_requested = server.actions_requested;
            facts.actions_staged = server.actions_staged;
            facts.actions_rejected = server.actions_rejected;
            facts.actions_queued_unresolved = server.actions_queued_unresolved;
            facts.transactions_committed = server.transactions_committed;
            g4::evaluate(
                &cfg,
                &facts,
                server.process_peak_memory_bytes,
                clients,
                server.large_collapse_samples,
                &lags,
            )
        });
    if !g4_row.requirements_met {
        requirements_met = false;
    }
    let admission = AdmissionRow {
        actions_requested: server.actions_requested,
        actions_rejected: server.actions_rejected,
        actions_staged: server.actions_staged,
        actions_queued_unresolved: server.actions_queued_unresolved,
        transactions_committed: server.transactions_committed,
        single_brick_commit_p95_ms: server.single_brick_commit_p95_ms,
        single_brick_commit_samples: server.single_brick_commit_samples,
        structure_split_p95_ms: server.structure_split_p95_ms,
        structure_split_samples: server.structure_split_samples,
        large_collapse_p95_ms: server.large_collapse_p95_ms,
        large_collapse_samples: server.large_collapse_samples,
        latency_targets_configured: scenario.latency_targets.is_some(),
        latency_targets_met: latency_ok,
    };
    let server_timing =
        scenario
            .server_timing
            .as_ref()
            .map_or_else(ServerTimingRow::unconfigured, |t| ServerTimingRow {
                configured: true,
                warmup_ticks: t.warmup_ticks,
                measured_ticks: t.measured_ticks,
                max_samples: t.max_samples,
                tick_busy_p95_ms: server.tick_busy_p95_ms,
                tick_busy_p99_ms: server.tick_busy_p99_ms,
                tick_busy_max_ms: server.tick_busy_max_ms,
                tick_busy_samples: server.tick_busy_samples,
                tick_busy_window_complete: server.tick_busy_window_complete,
                physics_p95_ms: server.physics_p95_ms,
                physics_p99_ms: server.physics_p99_ms,
                physics_max_ms: server.physics_max_ms,
                physics_samples: server.physics_samples,
                physics_window_complete: server.physics_window_complete,
                process_peak_memory_bytes: server.process_peak_memory_bytes,
                requirements_met: server_timing_ok,
            });

    all_match &= requirements_met;
    let verdict = SessionVerdict::compute(VerdictInputs {
        hash_agreement: hash_agree,
        workload_completion: g4_row
            .checks
            .iter()
            .find(|c| c.name.starts_with("workload completed"))
            .map(|c| c.passed),
        recovery_reconnect: restart
            .as_ref()
            .map(|r| r.ran && r.recovered_matches && r.reconnect_matches),
        timing: scenario.server_timing.is_some().then_some(server_timing_ok),
        overall_before_dimensions: all_match,
    });
    all_match = verdict.overall;
    finish(
        &output,
        SessionSummary {
            version: 5,
            result: if all_match { "passed" } else { "failed" },
            scenario: scenario.name,
            clients,
            loss_percent: run.loss_percent,
            server_ticks_run: server.ticks_run,
            transactions_committed: server.transactions_committed,
            max_detached_body_brick_span: server.max_detached_body_brick_span,
            body_settled,
            detached_body_max_final_speed_m_s: server.detached_body_max_final_speed_m_s,
            detached_body_stable_ticks: server.detached_body_stable_ticks,
            max_contact_penetration_m: server.max_contact_penetration_m,
            replay_checked: replay.is_some(),
            replay_matches: replay.as_ref().map(|r| r.ran && r.matches).unwrap_or(false),
            replayed_topology_events: replay.as_ref().map(|r| r.events).unwrap_or(0),
            restart_checked: restart.is_some(),
            restart_recovered_hash_matches: restart
                .as_ref()
                .map(|r| r.ran && r.recovered_matches)
                .unwrap_or(false),
            restart_reconnect_hash_matches: restart
                .as_ref()
                .map(|r| r.ran && r.reconnect_matches)
                .unwrap_or(false),
            restart_detail: restart
                .as_ref()
                .map(|r| r.detail.clone())
                .unwrap_or_default(),
            restart_recovered_world_hash: restart
                .as_ref()
                .map(|r| r.recovered_hash.clone())
                .unwrap_or_default(),
            residency_backing_disk_bytes: server.residency_backing_disk_bytes,
            residency_pinned_bricks_max: server.residency_pinned_bricks_max,
            residency_admission_deferred_total: server.residency_admission_deferred_total,
            residency_required_over_budget_ticks: server.residency_required_over_budget_ticks,
            residency_digest_bytes_final: server.residency_digest_bytes_final,
            residency_backing_resident_bytes: server.residency_backing_resident_bytes,
            process_peak_memory_bytes: server.process_peak_memory_bytes,
            residency_checkpoint_bricks_captured_total: server
                .residency_checkpoint_bricks_captured_total,
            residency_checkpoint_bricks_logical_total: server
                .residency_checkpoint_bricks_logical_total,
            impaired_late_join_bounded_failure: bounded_join_failure_seen,
            agreed_world_hash: agreed,
            all_hashes_match: hash_agree,
            requirements_met,
            verdict,
            admission,
            server_timing,
            g4: g4_row,
            join_budget,
            per_client: rows,
            app_egress_bytes: server.app_egress_bytes,
            transport_egress_bytes: server.transport_egress_bytes,
            per_client_egress: server.per_client_egress.clone(),
            note: "real OS processes over QUIC; encrypted-packet loss via per-client UDP proxy; gate requirements are fixture-defined: every scripted cut commits, each live client sees real body displacement, any body-targeted cut lands, and (when enabled) the committed topology-event stream replays from baseline to the same hash",
        },
    )
}

struct ReplayCheck {
    ran: bool,
    matches: bool,
    events: u64,
}

/// Runs `sandbox-server --replay` over the journalled world DB and checks the
/// rebuilt canonical hash against `expected`.
fn run_replay_check(db: &Path, expected: &str, output: &Path, profile: &str) -> ReplayCheck {
    if !db.exists() {
        return ReplayCheck {
            ran: false,
            matches: false,
            events: 0,
        };
    }
    let summary_path = output.join("replay.summary.json");
    let _ = fs::remove_file(&summary_path);
    let mut cmd = Command::new(sandbox_binary_profile("sandbox-server", profile));
    cmd.args([
        // `--listen` / `--ticks` / `--log-json` are required by the arg parser
        // but ignored on the `--replay` path.
        "--listen",
        "127.0.0.1:0",
        "--ticks",
        "1",
        "--log-json",
        &output.join("replay.jsonl").display().to_string(),
        "--replay",
        &db.display().to_string(),
        "--expect-hash",
        expected,
        "--summary-json",
        &summary_path.display().to_string(),
    ]);
    hide_console(&mut cmd);
    let status = cmd.status();
    let ran = status.is_ok();
    let ok = matches!(status, Ok(s) if s.success());
    let events = read_json::<ReplaySummary>(&summary_path)
        .map(|s| s.replayed_topology_events)
        .unwrap_or(0);
    ReplayCheck {
        ran,
        matches: ok,
        events,
    }
}

#[derive(Debug, Deserialize)]
struct ReplaySummary {
    #[serde(default)]
    replayed_topology_events: u64,
}

struct RestartCheck {
    ran: bool,
    recovered_hash: String,
    recovered_matches: bool,
    reconnect_matches: bool,
    /// Why a reconnect did not converge (empty when it did): the client's result and the
    /// restarted server's per-session events, so a failure is diagnosable from the summary.
    detail: String,
}

/// Tick budget of the restarted server (nominally 45 s at 60 Hz). A restart run has no edits to
/// make; it only has to stay up long enough to serve one late-join baseline. The old 300-tick (5 s
/// nominal) lifetime ended the server 0.3 s after a 5.5k-body baseline finished (measured), and a
/// larger world at slower ticks would be cut off mid-join; the harness deadline (`run.timeout`)
/// still bounds the whole phase and the client's own timeout is unchanged.
const RESTART_SERVER_TICKS: &str = "2700";

/// T23 / G3 row 7 item 2 (increment 31): matching residency + disk-backing
/// config for the cold-restarted server in [`run_restart_check`], so it
/// re-opens the exact same `<world>/residency.db` the live run wrote.
struct RestartResidency {
    budget_bricks: usize,
    radius_bricks: Option<i64>,
}

/// T23 / G3 cold restart. Launches a **fresh** `sandbox-server --serve --save`
/// over the world DB the run just journalled — a cold recovery from the
/// shutdown checkpoint plus the durable journal, no client edit replay — and
/// requires its recovered `final_world_hash` to equal `expected`. A fresh
/// `--late-join` client then connects to the restarted server and must converge
/// to the same hash, proving the recovered world serves a correct baseline.
fn run_restart_check(
    output: &Path,
    scene: &str,
    token_file: &Path,
    expected: &str,
    deadline: Duration,
    profile: &str,
    residency: Option<RestartResidency>,
) -> RestartCheck {
    let miss = RestartCheck {
        ran: false,
        recovered_hash: String::new(),
        recovered_matches: false,
        reconnect_matches: false,
        detail: "restart check did not run".into(),
    };
    if !output.join("world.db").exists() {
        return miss;
    }

    let fp = output.join("restart.fingerprint");
    let addr = output.join("restart.addr");
    let srv_summary = output.join("restart.server.summary.json");
    let cl_summary = output.join("restart.client.summary.json");
    for p in [&fp, &addr, &srv_summary, &cl_summary] {
        let _ = fs::remove_file(p);
    }

    let mut guard = ChildGuard::default();
    let mut srv = Command::new(sandbox_binary_profile("sandbox-server", profile));
    srv.args([
        "--serve",
        "--listen",
        "127.0.0.1:0",
        "--join-token-file",
        &token_file.display().to_string(),
        "--fingerprint-out",
        &fp.display().to_string(),
        "--addr-out",
        &addr.display().to_string(),
        "--summary-json",
        &srv_summary.display().to_string(),
        "--log-json",
        &output.join("restart.server.jsonl").display().to_string(),
        // Bounded, but long enough to serve one late-join baseline (see the constant).
        "--ticks",
        RESTART_SERVER_TICKS,
        "--min-clients",
        "0",
        "--max-clients",
        "1",
        "--scene",
        scene,
        "--quiescence-ticks",
        "0",
        "--paced",
        "--world",
        &output.display().to_string(),
        "--save",
        "--checkpoint-interval-ticks",
        "0",
        "--dev-unvalidated-actions",
    ]);
    // T23 / G3 row 7 item 2 (increment 31): re-open the same
    // `<world>/residency.db` a disk-backed live run wrote, proving a cold
    // restart's fresh `DiskBrickBacking` reads it back correctly. Absent for
    // every scenario that didn't request disk backing (unchanged behavior).
    if let Some(r) = &residency {
        srv.args(["--residency-budget-bricks", &r.budget_bricks.to_string()]);
        if let Some(radius) = r.radius_bricks {
            srv.args(["--residency-radius-bricks", &radius.to_string()]);
        }
        srv.arg("--residency-disk-backing");
    }
    hide_console(&mut srv);
    let Ok(child) = srv.spawn() else {
        return miss;
    };
    guard.push("server".into(), child);

    let Ok(bound) = wait_for_addr(&addr, &mut guard, Duration::from_secs(20)) else {
        return miss;
    };

    let client_timeout = deadline.saturating_sub(Duration::from_secs(3)).as_millis() as u64;
    let mut cl = Command::new(sandbox_binary_profile("sandbox-client", profile));
    cl.args([
        "--connect",
        &bound,
        "--server-fingerprint",
        &fp.display().to_string(),
        "--join-token-file",
        &token_file.display().to_string(),
        "--summary-json",
        &cl_summary.display().to_string(),
        "--log-json",
        &output.join("restart.client.jsonl").display().to_string(),
        "--timeout-ms",
        &client_timeout.to_string(),
        "--client-index",
        "0",
        "--scene",
        scene,
        "--late-join",
        "--connect-delay-ms",
        "300",
    ]);
    hide_console(&mut cl);
    let Ok(child) = cl.spawn() else {
        return miss;
    };
    guard.push("restart-client".into(), child);

    let _ = guard.wait_all(deadline);

    let server = read_json::<ServerSummary>(&srv_summary);
    let recovered_hash = server
        .as_ref()
        .map(|s| s.final_world_hash.clone())
        .unwrap_or_default();
    let recovered_matches = server
        .map(|s| s.result == "passed" && s.final_world_hash == expected)
        .unwrap_or(false);
    let reconnect_matches = read_json::<ClientSummary>(&cl_summary)
        .map(|c| c.result == "passed" && c.final_world_hash == expected)
        .unwrap_or(false);

    let detail = if reconnect_matches {
        String::new()
    } else {
        let client = read_json::<serde_json::Value>(&cl_summary)
            .map(|c| c["result"].as_str().unwrap_or("unknown").to_string())
            .unwrap_or_else(|| "no client summary".into());
        let events = read_json::<serde_json::Value>(&srv_summary)
            .and_then(|s| {
                s["session_timelines"][0]["events"].as_array().map(|e| {
                    e.iter()
                        .filter_map(|ev| Some(format!("{} {}", ev[0], ev[1].as_str()?)))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
            })
            .unwrap_or_default();
        format!("client result `{client}`; restarted server session events: {events}")
    };
    RestartCheck {
        ran: true,
        recovered_hash,
        recovered_matches,
        reconnect_matches,
        detail,
    }
}

/// The session's verdict, one dimension per question. A pass on one dimension says nothing about
/// another: replicas can agree on a hash while timing fails, and vice versa.
/// `None` = the scenario did not configure that dimension.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct SessionVerdict {
    /// Every expected replica (and the replay, when checked) reached the agreed hash.
    hash_agreement: bool,
    /// Every requested edit committed, none rejected or left unresolved (`g4` check).
    workload_completion: Option<bool>,
    /// Cold-restart recovery and fresh-client reconnect both reached the agreed hash.
    recovery_reconnect: Option<bool>,
    /// Owning-server tick/physics/memory targets.
    timing: Option<bool>,
    /// Everything else combined (required by `result`).
    overall: bool,
    /// The dimensions above that failed, for the failure line.
    failing: Vec<&'static str>,
}

struct VerdictInputs {
    hash_agreement: bool,
    workload_completion: Option<bool>,
    recovery_reconnect: Option<bool>,
    timing: Option<bool>,
    /// Server result, client checks and every other requirement combined.
    overall_before_dimensions: bool,
}

impl SessionVerdict {
    fn compute(i: VerdictInputs) -> Self {
        let mut failing = Vec::new();
        if !i.hash_agreement {
            failing.push("hash_agreement");
        }
        if i.workload_completion == Some(false) {
            failing.push("workload_completion");
        }
        if i.recovery_reconnect == Some(false) {
            failing.push("recovery_reconnect");
        }
        if i.timing == Some(false) {
            failing.push("timing");
        }
        Self {
            hash_agreement: i.hash_agreement,
            workload_completion: i.workload_completion,
            recovery_reconnect: i.recovery_reconnect,
            timing: i.timing,
            overall: i.overall_before_dimensions && failing.is_empty(),
            failing,
        }
    }

    fn server_missing() -> Self {
        Self::compute(VerdictInputs {
            hash_agreement: false,
            workload_completion: None,
            recovery_reconnect: None,
            timing: None,
            overall_before_dimensions: false,
        })
    }

    fn describe(&self) -> String {
        let f = |v: Option<bool>| match v {
            Some(true) => "pass",
            Some(false) => "FAIL",
            None => "n/a",
        };
        format!(
            "hash agreement {}, workload completion {}, recovery/reconnect {}, timing {}, overall {}",
            if self.hash_agreement { "pass" } else { "FAIL" },
            f(self.workload_completion),
            f(self.recovery_reconnect),
            f(self.timing),
            if self.overall { "pass" } else { "FAIL" },
        )
    }
}

fn finish(output: &Path, summary: SessionSummary) -> Result<(), XtaskError> {
    let passed = summary.result == "passed";
    let path = output.join("summary.json");
    let body = serde_json::to_vec_pretty(&summary).expect("session summary serialises");
    write_file(&path, &body)?;
    if passed {
        println!("session passed: {}", output.display());
        Ok(())
    } else {
        eprintln!(
            "session FAILED: {} (agreed hash `{}`; {})",
            output.display(),
            summary.agreed_world_hash,
            summary.verdict.describe()
        );
        Err(XtaskError::Cargo(vec!["session".into()], 1))
    }
}

// --- child supervision -------------------------------------------------------

#[derive(Default)]
pub(crate) struct ChildGuard {
    children: Vec<(String, Child)>,
}

impl ChildGuard {
    pub(crate) fn push(&mut self, label: String, child: Child) {
        self.children.push((label, child));
    }

    /// Polls every child until all have exited or `deadline` passes; kills any
    /// survivors. Returns `label -> exit code` (`None` if killed / no code).
    pub(crate) fn wait_all(&mut self, deadline: Duration) -> BTreeMap<String, Option<i32>> {
        let end = Instant::now() + deadline;
        let mut codes: BTreeMap<String, Option<i32>> = BTreeMap::new();
        loop {
            let mut all_done = true;
            for (label, child) in &mut self.children {
                if codes.contains_key(label) {
                    continue;
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        codes.insert(label.clone(), status.code());
                    }
                    Ok(None) => all_done = false,
                    Err(_) => {
                        codes.insert(label.clone(), None);
                    }
                }
            }
            if all_done || Instant::now() >= end {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        for (label, child) in &mut self.children {
            if !codes.contains_key(label) {
                let _ = child.kill();
                let _ = child.wait();
                codes.insert(label.clone(), None);
            }
        }
        codes
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        for (_, child) in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) fn wait_for_addr(
    path: &Path,
    guard: &mut ChildGuard,
    deadline: Duration,
) -> Result<String, XtaskError> {
    let end = Instant::now() + deadline;
    loop {
        if let Ok(s) = fs::read_to_string(path) {
            let s = s.trim();
            if !s.is_empty() {
                return Ok(s.to_string());
            }
        }
        // Fail fast if the server already died.
        for (label, child) in &mut guard.children {
            if label == "server"
                && let Ok(Some(status)) = child.try_wait()
            {
                return Err(XtaskError::Capability(format!(
                    "sandbox-server exited before binding ({status}); inspect server.jsonl"
                )));
            }
        }
        if Instant::now() >= end {
            return Err(XtaskError::Capability(
                "sandbox-server did not bind within the startup window".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: impl AsRef<Path>) -> Option<T> {
    let mut buf = String::new();
    fs::File::open(path.as_ref())
        .ok()?
        .read_to_string(&mut buf)
        .ok()?;
    serde_json::from_str(&buf).ok()
}

pub(crate) fn write_file(path: &Path, bytes: &[u8]) -> Result<(), XtaskError> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, bytes).map_err(|source| XtaskError::Output {
        path: path.display().to_string(),
        source,
    })
}

/// 64 lowercase hex chars from a seeded SplitMix64-ish stream (no crypto needed:
/// this is a per-run development token in an ignored directory).
pub(crate) fn random_hex_32(seed: u64) -> String {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = String::with_capacity(64);
    for _ in 0..4 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push_str(&format!("{z:016x}"));
    }
    out
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_: &mut Command) {}

// --- proxy farm ------------------------------------------------------------

/// N running [`UdpProxy`]s on a private Tokio runtime, one per client.
struct ProxyFarm {
    addrs: Vec<SocketAddr>,
    stop: std::sync::mpsc::Sender<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ProxyFarm {
    /// The uniform `--loss-percent` profile: every client gets the same
    /// loss/delay/jitter/reorder proxy.
    fn spawn(
        upstream: SocketAddr,
        count: usize,
        loss_percent: u8,
        seed: u64,
    ) -> Result<Self, XtaskError> {
        let plans = (0..count)
            .map(|i| {
                // Deterministic reordering: every 3rd server->client datagram
                // is held past the 20 Hz (50 ms) snapshot spacing so the next
                // one overtakes it — the "reordered snapshots" half of T11,
                // which the harness asserts the live clients observed. Light
                // `jitter` on top keeps timing irregular without the heavy lag
                // that made a late scripted cut miss the run.
                PacketFaultPlan {
                    seed: seed ^ (i as u64 + 1),
                    loss_ratio: f64::from(loss_percent) / 100.0,
                    duplicate_ratio: 0.0,
                    delay: Duration::from_millis(3),
                    jitter: Duration::from_millis(15),
                    reorder_period: 3,
                    rate_limit_bytes_per_sec: 0,
                }
            })
            .collect();
        Self::spawn_with_plans(upstream, plans)
    }

    /// One proxy per entry of `plans`, each independently configured — used by
    /// T23 / G3 row 11's join budget to shape only the late-join client's link
    /// (bandwidth + RTT + loss) while every other client stays on a
    /// transparent proxy.
    fn spawn_with_plans(
        upstream: SocketAddr,
        plans: Vec<PacketFaultPlan>,
    ) -> Result<Self, XtaskError> {
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = addr_tx.send(Err(e.to_string()));
                    return;
                }
            };
            rt.block_on(async move {
                let mut proxies = Vec::new();
                let mut addrs = Vec::new();
                for plan in plans {
                    match UdpProxy::spawn(upstream, plan).await {
                        Ok(p) => {
                            addrs.push(p.local_addr());
                            proxies.push(p);
                        }
                        Err(e) => {
                            let _ = addr_tx.send(Err(e.to_string()));
                            return;
                        }
                    }
                }
                let _ = addr_tx.send(Ok(addrs));
                // Block this runtime thread until told to stop.
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = stop_rx.recv();
                })
                .await;
                for p in &proxies {
                    p.shutdown().await;
                }
            });
        });

        let addrs = addr_rx
            .recv()
            .map_err(|_| XtaskError::Capability("proxy farm thread died during startup".into()))?
            .map_err(|e| XtaskError::Capability(format!("UDP proxy setup failed: {e}")))?;
        Ok(Self {
            addrs,
            stop: stop_tx,
            join: Some(join),
        })
    }

    fn shutdown(mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod verdict_tests {
    use super::*;

    fn inputs() -> VerdictInputs {
        VerdictInputs {
            hash_agreement: true,
            workload_completion: Some(true),
            recovery_reconnect: Some(true),
            timing: Some(true),
            overall_before_dimensions: true,
        }
    }

    #[test]
    fn everything_passing_is_an_overall_pass() {
        let v = SessionVerdict::compute(inputs());
        assert!(v.overall && v.failing.is_empty(), "{v:?}");
    }

    #[test]
    fn hashes_agree_but_timing_fails_is_an_overall_failure_that_names_timing_only() {
        let v = SessionVerdict::compute(VerdictInputs {
            timing: Some(false),
            ..inputs()
        });
        assert!(
            v.hash_agreement,
            "timing must not clear or corrupt hash agreement"
        );
        assert_eq!(v.timing, Some(false));
        assert_eq!(v.workload_completion, Some(true));
        assert_eq!(v.recovery_reconnect, Some(true));
        assert!(!v.overall);
        assert_eq!(v.failing, vec!["timing"]);
        assert!(v.describe().contains("hash agreement pass"));
        assert!(v.describe().contains("timing FAIL"));
    }

    #[test]
    fn reconnect_failing_while_hashes_agree_is_its_own_dimension() {
        let v = SessionVerdict::compute(VerdictInputs {
            recovery_reconnect: Some(false),
            ..inputs()
        });
        assert!(v.hash_agreement && !v.overall);
        assert_eq!(v.failing, vec!["recovery_reconnect"]);
    }

    #[test]
    fn incomplete_workload_is_not_hidden_by_converged_replicas() {
        let v = SessionVerdict::compute(VerdictInputs {
            workload_completion: Some(false),
            ..inputs()
        });
        assert!(v.hash_agreement && !v.overall);
        assert_eq!(v.failing, vec!["workload_completion"]);
    }

    #[test]
    fn unconfigured_dimensions_do_not_fail_and_other_failures_still_do() {
        let v = SessionVerdict::compute(VerdictInputs {
            workload_completion: None,
            recovery_reconnect: None,
            timing: None,
            ..inputs()
        });
        assert!(v.overall);
        let v = SessionVerdict::compute(VerdictInputs {
            overall_before_dimensions: false,
            ..inputs()
        });
        assert!(!v.overall && v.failing.is_empty());
    }

    #[test]
    fn a_hash_mismatch_alone_names_hash_agreement() {
        let v = SessionVerdict::compute(VerdictInputs {
            hash_agreement: false,
            ..inputs()
        });
        assert!(!v.overall);
        assert_eq!(v.failing, vec!["hash_agreement"]);
    }
}

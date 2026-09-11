//! `cargo xtask session` / `cargo xtask scenario` — the T10 replication harness.
//!
//! Spawns one `sandbox-server --serve` process and N `sandbox-client --connect`
//! processes as real OS processes talking over real QUIC (optionally through
//! per-client [`spall_net::UdpProxy`]s that drop / delay / reorder *encrypted*
//! packets). Each client scripts cuts from a scenario file, replicates the
//! authoritative topology, and writes a summary. The harness passes only if the
//! server and every client agree on the final canonical topology hash.

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

use crate::{XtaskError, run_cargo, sandbox_binary};

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
    /// Whole-run deadline in milliseconds.
    #[arg(long, default_value_t = 60_000, value_parser = clap::value_parser!(u64).range(2_000..=600_000))]
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
    #[arg(long, default_value_t = 60_000, value_parser = clap::value_parser!(u64).range(2_000..=600_000))]
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
    /// Enforced end-to-end proof that residency actually ran during this
    /// scenario. A configured block makes zero/default counters a failure.
    #[serde(default)]
    residency_assertions: Option<ResidencyAssertions>,
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
    /// T23 / G3 row 11: when set, the named `late_join_clients` entry connects
    /// through a shaped proxy (bandwidth + RTT + loss) instead of the plain
    /// per-`loss_percent` one, and its measured compressed baseline size /
    /// time-to-ready are asserted against the configured budget. Every other
    /// client gets an unshaped (transparent) proxy so the workload that builds
    /// the edit history is not itself bandwidth-limited.
    #[serde(default)]
    join_budget: Option<JoinBudget>,
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
    restart_recovered_world_hash: String,
    /// T23 / G3 row 10: `late_join_may_fail` was set and an impaired late joiner
    /// ended in an accepted bounded explicit failure (`join-failed`, real exit)
    /// while the live clients + server still converged.
    impaired_late_join_bounded_failure: bool,
    agreed_world_hash: String,
    all_hashes_match: bool,
    requirements_met: bool,
    /// T11a / ENG-62: the gate's requested / rejected / queued / committed
    /// breakdown (server-authoritative) and the measured commit-latency p95s.
    admission: AdmissionRow,
    /// T23 / G3 row 11: the configured join-budget network profile / ceilings
    /// alongside the measured compressed baseline size and time-to-ready.
    /// `configured: false` (all other fields zeroed) when no `join_budget`
    /// scenario block is set.
    join_budget: JoinBudgetRow,
    per_client: Vec<ClientRow>,
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
    client_residency_evicted_transaction_gaps: u64,
    hash_matches_server: bool,
    late_join_baseline_compressed_bytes: u64,
    late_join_baseline_install_ms: u64,
    late_join_ready_ms: u64,
    late_join_ready_confirmed: bool,
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
}

fn requirements_met(
    scenario: &Scenario,
    transactions_committed: u64,
    clients: &[Option<ClientSummary>],
    proxy_active: bool,
) -> bool {
    // The whole script must have run: a fixture that quiesces early (a gap
    // between scripted cuts longer than the server's idle window) commits fewer
    // transactions than it has cuts, and that is a failure no matter how the
    // `minimum_transactions` floor is set.
    let all_cuts_committed = transactions_committed >= scenario.cuts.len() as u64;

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
            client_residency_evicted_transaction_gaps: 0,
            late_join_baseline_compressed_bytes: 0,
            late_join_baseline_install_ms: 0,
            late_join_ready_ms: 0,
            late_join_ready_confirmed: false,
        }
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
        assert!(!requirements_met(&scenario, 1, &[Some(client(2))], false));
        assert!(!requirements_met(&scenario, 2, &[Some(client(0))], false));
        assert!(requirements_met(
            &scenario,
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
            3,
            &[Some(client(4)), Some(client(4))],
            false
        ));

        // All four cuts committed and both clients saw real displacement.
        assert!(requirements_met(
            &scenario,
            4,
            &[Some(client(4)), Some(client(4))],
            false
        ));

        // A stationary body (snapshots arrived, nothing moved) fails.
        let mut still = client(4);
        still.max_body_displacement_m = 0.01;
        assert!(!requirements_met(
            &scenario,
            4,
            &[Some(still), Some(client(4))],
            false
        ));

        // Nobody landed the body-targeted cut.
        let mut no_body_cut = client(4);
        no_body_cut.body_cut_committed = false;
        assert!(!requirements_met(
            &scenario,
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
            1,
            &[Some(ordered.clone())],
            false
        ));
        // Impaired run: a client that never saw a reordered datagram fails.
        assert!(!requirements_met(&scenario, 1, &[Some(ordered)], true));
        // Impaired run with real reordering observed: passes.
        assert!(requirements_met(&scenario, 1, &[Some(client(4))], true));
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

    // Build both binaries once.
    run_cargo(&["build", "-p", "sandbox", "--bin", "sandbox-server"])?;
    run_cargo(&[
        "build",
        "-p",
        "sandbox",
        "--features",
        "client",
        "--bin",
        "sandbox-client",
    ])?;

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
    let mut server_cmd = Command::new(sandbox_binary("sandbox-server"));
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
        &clients.to_string(),
        "--scene",
        &scenario.scene,
        "--quiescence-ticks",
        &scenario.quiescence_ticks.to_string(),
        "--paced",
        // Scenario files script cuts at explicit cells that no aim ray would
        // produce; the harness is the authenticated dev-scenario path (ENG-47).
        "--dev-unvalidated-actions",
    ]);
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
        let mut c = Command::new(sandbox_binary("sandbox-client"));
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
        }
        if scenario.late_join_clients.contains(&i) {
            c.args([
                "--late-join",
                "--connect-delay-ms",
                &scenario.late_join_connect_delay_ms.to_string(),
            ]);
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
                    version: 3,
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
                    restart_recovered_world_hash: String::new(),
                    impaired_late_join_bounded_failure: false,
                    agreed_world_hash: String::new(),
                    all_hashes_match: false,
                    requirements_met: false,
                    admission: AdmissionRow::empty(),
                    join_budget: JoinBudgetRow::unconfigured(),
                    per_client: Vec::new(),
                    note: "server produced no summary; inspect server.jsonl",
                },
            );
        }
    };

    let agreed = server.final_world_hash.clone();
    let mut all_match = server_ok && server.result == "passed";
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
                    client_residency_evicted_transaction_gaps: c
                        .client_residency_evicted_transaction_gaps,
                    hash_matches_server: hash_ok,
                    late_join_baseline_compressed_bytes: c.late_join_baseline_compressed_bytes,
                    late_join_baseline_install_ms: c.late_join_baseline_install_ms,
                    late_join_ready_ms: c.late_join_ready_ms,
                    late_join_ready_confirmed: c.late_join_ready_confirmed,
                });
            }
            None => {
                all_match = false;
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
                    client_residency_evicted_transaction_gaps: 0,
                    hash_matches_server: false,
                    late_join_baseline_compressed_bytes: 0,
                    late_join_baseline_install_ms: 0,
                    late_join_ready_ms: 0,
                    late_join_ready_confirmed: false,
                });
            }
        }
    }
    let mut requirements_met = requirements_met(
        &scenario,
        server.transactions_committed,
        &client_summaries,
        run.loss_percent > 0 || scenario.join_budget.is_some(),
    );
    if !residency_requirements_met(&scenario, &server, &client_summaries) {
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
        let r = run_replay_check(&replay_db, &agreed, &output);
        if !(r.ran && r.matches) {
            requirements_met = false;
        }
        Some(r)
    } else {
        None
    };

    // T23 / G3: cold-restart a fresh server over the same world DB (recovery
    // from the shutdown checkpoint + journal), then a fresh `--late-join` client
    // against it. Both must reach the agreed hash.
    let restart = if scenario.restart_check {
        let r = run_restart_check(&output, &scenario.scene, &token_file, &agreed, run.timeout);
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

    all_match &= requirements_met;
    finish(
        &output,
        SessionSummary {
            version: 3,
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
            restart_recovered_world_hash: restart
                .as_ref()
                .map(|r| r.recovered_hash.clone())
                .unwrap_or_default(),
            impaired_late_join_bounded_failure: bounded_join_failure_seen,
            agreed_world_hash: agreed,
            all_hashes_match: all_match,
            requirements_met,
            admission,
            join_budget,
            per_client: rows,
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
fn run_replay_check(db: &Path, expected: &str, output: &Path) -> ReplayCheck {
    if !db.exists() {
        return ReplayCheck {
            ran: false,
            matches: false,
            events: 0,
        };
    }
    let summary_path = output.join("replay.summary.json");
    let _ = fs::remove_file(&summary_path);
    let mut cmd = Command::new(sandbox_binary("sandbox-server"));
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
) -> RestartCheck {
    let miss = RestartCheck {
        ran: false,
        recovered_hash: String::new(),
        recovered_matches: false,
        reconnect_matches: false,
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
    let mut srv = Command::new(sandbox_binary("sandbox-server"));
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
        // A short bounded run: the recovering server has no edits to make, it
        // just needs to be up long enough to serve one late-join baseline.
        "--ticks",
        "300",
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
    hide_console(&mut srv);
    let Ok(child) = srv.spawn() else {
        return miss;
    };
    guard.push("server".into(), child);

    let Ok(bound) = wait_for_addr(&addr, &mut guard, Duration::from_secs(20)) else {
        return miss;
    };

    let client_timeout = deadline.saturating_sub(Duration::from_secs(3)).as_millis() as u64;
    let mut cl = Command::new(sandbox_binary("sandbox-client"));
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

    RestartCheck {
        ran: true,
        recovered_hash,
        recovered_matches,
        reconnect_matches,
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
            "session FAILED: {} (agreed hash `{}`, all match = {})",
            output.display(),
            summary.agreed_world_hash,
            summary.all_hashes_match
        );
        Err(XtaskError::Cargo(vec!["session".into()], 1))
    }
}

// --- child supervision -------------------------------------------------------

#[derive(Default)]
struct ChildGuard {
    children: Vec<(String, Child)>,
}

impl ChildGuard {
    fn push(&mut self, label: String, child: Child) {
        self.children.push((label, child));
    }

    /// Polls every child until all have exited or `deadline` passes; kills any
    /// survivors. Returns `label -> exit code` (`None` if killed / no code).
    fn wait_all(&mut self, deadline: Duration) -> BTreeMap<String, Option<i32>> {
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

fn wait_for_addr(
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

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), XtaskError> {
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
fn random_hex_32(seed: u64) -> String {
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

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
    server_ticks: u64,
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
}

fn one() -> u64 {
    1
}

fn default_late_delay() -> u64 {
    900
}

#[derive(Debug, Clone, Deserialize)]
struct CutSpec {
    #[serde(default)]
    client: u64,
    at_tick: u64,
    cell: [i64; 3],
    radius: i64,
}

// --- process summaries (subset of the server / client structs) ----------------

#[derive(Debug, Deserialize)]
struct ServerSummary {
    result: String,
    ticks_run: u64,
    transactions_committed: u64,
    final_world_hash: String,
}

#[derive(Debug, Deserialize)]
struct ClientSummary {
    result: String,
    transactions_applied: u64,
    repair_requests_sent: u64,
    transactions_rejected: u64,
    motion_snapshots: u64,
    final_world_hash: String,
    #[serde(default)]
    late_join: bool,
    #[serde(default)]
    baseline_bricks: u64,
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
    agreed_world_hash: String,
    all_hashes_match: bool,
    per_client: Vec<ClientRow>,
    note: &'static str,
}

#[derive(Debug, Serialize)]
struct ClientRow {
    index: u64,
    result: String,
    transactions_applied: u64,
    repair_requests_sent: u64,
    transactions_rejected: u64,
    motion_snapshots: u64,
    hash_matches_server: bool,
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
        "--paced",
    ]);
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

    // Optional per-client encrypted-packet proxies.
    let proxies = if run.loss_percent > 0 {
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
        ]);
        for cut in by_client.get(&i).into_iter().flatten() {
            c.args([
                "--cut",
                &format!(
                    "{}:{},{},{}:{}",
                    cut.at_tick, cut.cell[0], cut.cell[1], cut.cell[2], cut.radius
                ),
            ]);
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
                    version: 1,
                    result: "failed",
                    scenario: scenario.name.clone(),
                    clients,
                    loss_percent: run.loss_percent,
                    server_ticks_run: 0,
                    transactions_committed: 0,
                    agreed_world_hash: String::new(),
                    all_hashes_match: false,
                    per_client: Vec::new(),
                    note: "server produced no summary; inspect server.jsonl",
                },
            );
        }
    };

    let agreed = server.final_world_hash.clone();
    let mut all_match = server_ok && server.result == "passed";
    let mut rows = Vec::new();
    for (i, summary) in client_summaries.iter().enumerate() {
        let exit_ok = statuses.get(&format!("client{i}")).copied().flatten() == Some(0);
        match summary {
            Some(c) => {
                let hash_ok = c.final_world_hash == agreed;
                // A late joiner that caught up entirely from the baseline (no
                // cuts after it joined) is still a pass.
                let progressed =
                    c.transactions_applied >= 1 || (c.late_join && c.baseline_bricks > 0);
                all_match &= exit_ok
                    && c.result == "passed"
                    && hash_ok
                    && progressed
                    && c.transactions_rejected == 0;
                rows.push(ClientRow {
                    index: i as u64,
                    result: c.result.clone(),
                    transactions_applied: c.transactions_applied,
                    repair_requests_sent: c.repair_requests_sent,
                    transactions_rejected: c.transactions_rejected,
                    motion_snapshots: c.motion_snapshots,
                    hash_matches_server: hash_ok,
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
                    hash_matches_server: false,
                });
            }
        }
    }
    finish(
        &output,
        SessionSummary {
            version: 1,
            result: if all_match { "passed" } else { "failed" },
            scenario: scenario.name,
            clients,
            loss_percent: run.loss_percent,
            server_ticks_run: server.ticks_run,
            transactions_committed: server.transactions_committed,
            agreed_world_hash: agreed,
            all_hashes_match: all_match,
            per_client: rows,
            note: "real OS processes over QUIC; encrypted-packet loss via per-client UDP proxy",
        },
    )
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
    fn spawn(
        upstream: SocketAddr,
        count: usize,
        loss_percent: u8,
        seed: u64,
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
                for i in 0..count {
                    let plan = PacketFaultPlan {
                        seed: seed ^ (i as u64 + 1),
                        loss_ratio: f64::from(loss_percent) / 100.0,
                        duplicate_ratio: 0.0,
                        delay: Duration::from_millis(15),
                        jitter: Duration::from_millis(10),
                    };
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

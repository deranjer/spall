use clap::Parser;
use spall_net::{JoinToken, TransportConfig};
use spall_server::{Scene, ServeConfig, ServerConfig};
use std::{net::SocketAddr, path::PathBuf, process::ExitCode, time::Duration};

#[derive(Debug, Parser)]
#[command(name = "sandbox-server", about = "GPU-free Spall sandbox server host")]
struct Args {
    /// Directory for the T16 on-disk world; the SQLite database is
    /// `<world>/world.db`. Used by `--serve --save`; ignored by the T00 loop.
    #[arg(long, default_value = ".local/worlds/dev")]
    world: PathBuf,
    #[arg(long, default_value_t = 0)]
    seed: u64,
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=10_000_000))]
    ticks: u64,
    #[arg(long)]
    log_json: PathBuf,
    #[arg(long, default_value_t = 0)]
    ready_delay_ms: u64,
    /// Harness-only failure injection for the T00 bounded loop.
    #[arg(long, hide = true)]
    fail_after_tick: Option<u64>,

    // --- T10 networked replication host ---
    /// Run the authoritative replication host instead of the T00 bounded loop.
    /// Requires --join-token-file.
    #[arg(long)]
    serve: bool,
    /// Per-run join secret (hex), shared with clients out of band.
    #[arg(long)]
    join_token_file: Option<PathBuf>,
    /// Write the server certificate fingerprint (hex) here for clients.
    #[arg(long)]
    fingerprint_out: Option<PathBuf>,
    /// Write the OS-resolved bound ip:port here once listening.
    #[arg(long)]
    addr_out: Option<PathBuf>,
    /// Write the machine-readable run summary here.
    #[arg(long)]
    summary_json: Option<PathBuf>,
    /// Wait for this many clients before the tick loop starts.
    #[arg(long, default_value_t = 1)]
    min_clients: usize,
    #[arg(long, default_value_t = 8)]
    max_clients: usize,
    /// Real-time 60 Hz pacing (needed for interactive / networked clients).
    #[arg(long)]
    paced: bool,
    /// Built-in scene to serve: `bridge-cut` (default, single brick),
    /// `cross-bridge-cut` (column + beam cross the x = 32 brick boundary), or
    /// `walk` (T19 player-movement arena — every client gets a predicted capsule).
    #[arg(long, default_value = "bridge-cut")]
    scene: String,
    /// T16: persist to `<world>/world.db` — recover from it on start, journal
    /// committed transactions, checkpoint on the interval and on shutdown.
    #[arg(long)]
    save: bool,
    /// Ticks between engine checkpoints (1800 == 30 s at 60 Hz). 0 disables the
    /// periodic checkpoint (a shutdown checkpoint still happens).
    #[arg(long, default_value_t = 1_800)]
    checkpoint_interval_ticks: u64,
    /// Consecutive idle ticks (no committed work, no client input) after which
    /// the run stops early. Larger values give a scripted client more slack to
    /// land late actions under an impaired transport. 0 disables early stop.
    #[arg(long, default_value_t = 45)]
    quiescence_ticks: u64,
    /// T17: a joining client's catch-up-queue cap before its baseline transfer
    /// is cancelled and re-captured fresher.
    #[arg(long, default_value_t = spall_server::serve::DEFAULT_CATCH_UP_CAP)]
    catch_up_cap: usize,
    /// T17: bounded late-join transfer restarts before a joining client is
    /// dropped (connected clients keep running).
    #[arg(long, default_value_t = spall_server::serve::DEFAULT_MAX_JOIN_RETRIES)]
    max_join_retries: u32,
    /// ENG-47 development-scenario path: skip server-side action-claim
    /// validation and take each `ActionRequest`'s claimed target/brush verbatim.
    /// Lets a fixture harness script arbitrary cuts. Never use on a shared host.
    #[arg(long, hide = true)]
    dev_unvalidated_actions: bool,
    /// ENG-61: keep stepping physics past edit-quiescence until every detached
    /// body is asleep (bounded by `--ticks`). Used by the `body-rest-on-structure`
    /// gate fixture so the run can show the detached beam actually come to rest.
    #[arg(long)]
    await_body_settle: bool,

    // --- T23 / G3 row 7: default-off resident-cache eviction ---
    /// Enable the residency pass: evict terrain bricks outside every player's
    /// interest box (this many resident bricks is the reported ceiling), reload
    /// them on demand for edits. The committed world / hash is unchanged.
    /// `0` (default) keeps everything resident.
    #[arg(long, default_value_t = 0)]
    residency_budget_bricks: usize,
    /// Chebyshev radius (bricks) of the kept-resident box around each player.
    #[arg(long, default_value_t = 2)]
    residency_radius_bricks: i64,

    // --- T20 interest + bandwidth scheduling ---
    /// Enable per-client interest relevance + motion bandwidth budget. Without
    /// it, one 20 Hz motion batch is broadcast unfiltered to every client
    /// (the pre-T20 behaviour). The three `--motion-*` values below apply only
    /// when this is set.
    #[arg(long)]
    motion_interest: bool,
    /// Interest near radius (m): bodies within this of a client's anchor are
    /// replicated every 20 Hz batch.
    #[arg(long, default_value_t = 48.0)]
    motion_near_m: f64,
    /// Interest far radius (m): out to here bodies replicate on the
    /// `--motion-far-interval` cadence; beyond it a non-player body is not
    /// replicated to that client.
    #[arg(long, default_value_t = 96.0)]
    motion_far_m: f64,
    /// Send `Far`-tier motion only every Nth 20 Hz batch (1 = every batch).
    #[arg(long, default_value_t = 4)]
    motion_far_interval: u64,
    /// Per-client per-batch motion byte ceiling (0 = no ceiling).
    #[arg(long, default_value_t = 0)]
    motion_client_budget_bytes: usize,
    /// Interest anchor `x,y,z` (m) for a client on a scene with no player
    /// capsule (the bridge scenes). Omitted → those clients stay unfiltered.
    #[arg(long)]
    motion_static_anchor: Option<String>,

    // --- T11 exact-replay check ---
    /// Instead of serving, recover from this world database's **oldest**
    /// checkpoint and replay its entire committed topology journal, then print
    /// the rebuilt canonical topology hash. Exit 0 iff it matches
    /// `--expect-hash` (when given).
    #[arg(long)]
    replay: Option<PathBuf>,
    /// Expected canonical topology hash (hex) for `--replay`.
    #[arg(long)]
    expect_hash: Option<String>,
}

fn main() -> ExitCode {
    sandbox::init_tracing();
    let args = Args::parse();

    if args.replay.is_some() {
        return run_replay(args);
    }

    if args.serve {
        return run_serve(args);
    }

    let config = ServerConfig {
        world: args.world,
        seed: args.seed,
        listen: args.listen,
        ticks: args.ticks,
        log_json: args.log_json,
        ready_delay: Duration::from_millis(args.ready_delay_ms),
        fail_after_tick: args.fail_after_tick,
    };
    match spall_server::run(&config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sandbox-server: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_serve(args: Args) -> ExitCode {
    let Some(token_file) = args.join_token_file else {
        eprintln!("sandbox-server: --serve requires --join-token-file");
        return ExitCode::from(2);
    };
    let token = match std::fs::read_to_string(&token_file)
        .ok()
        .and_then(|s| JoinToken::from_hex(s.trim()))
    {
        Some(t) => t,
        None => {
            eprintln!("sandbox-server: could not read a 64-hex join token from {token_file:?}");
            return ExitCode::from(2);
        }
    };

    let scene = match Scene::from_name(&args.scene) {
        Some(s) => s,
        None => {
            eprintln!(
                "sandbox-server: unknown --scene `{}` (expected bridge-cut, cross-bridge-cut, walk, checkerboard-split, bulk-split, separated-regions, g4-workload, separated-regions-far, or g1-full-envelope)",
                args.scene
            );
            return ExitCode::from(2);
        }
    };

    let save = args.save.then(|| args.world.join("world.db"));
    if let Some(db) = &save
        && let Some(parent) = db.parent()
    {
        let _ = std::fs::create_dir_all(parent);
    }

    let motion_interest = if args.motion_interest {
        let static_anchor_m = match args.motion_static_anchor.as_deref() {
            None => None,
            Some(s) => match parse_vec3(s) {
                Some(v) => Some(v),
                None => {
                    eprintln!(
                        "sandbox-server: --motion-static-anchor must be `x,y,z` metres, got `{s}`"
                    );
                    return ExitCode::from(2);
                }
            },
        };
        Some(spall_server::MotionInterest {
            near_radius_m: args.motion_near_m,
            far_radius_m: args.motion_far_m,
            far_interval: args.motion_far_interval,
            per_client_budget_bytes: args.motion_client_budget_bytes,
            static_anchor_m,
        })
    } else {
        None
    };

    let config = ServeConfig {
        listen: args.listen,
        scene,
        join_token: token,
        max_ticks: args.ticks,
        quiescence_ticks: args.quiescence_ticks,
        min_clients: args.min_clients,
        max_clients: args.max_clients,
        startup_timeout: Duration::from_secs(30),
        paced: args.paced,
        log_json: args.log_json,
        summary_json: args.summary_json,
        fingerprint_out: args.fingerprint_out,
        addr_out: args.addr_out,
        transport: TransportConfig::default(),
        save,
        checkpoint_interval_ticks: args.checkpoint_interval_ticks,
        seed: args.seed,
        catch_up_cap: args.catch_up_cap,
        max_join_retries: args.max_join_retries,
        dev_unvalidated_actions: args.dev_unvalidated_actions,
        save_faults: None,
        await_body_settle: args.await_body_settle,
        motion_interest,
        residency: (args.residency_budget_bricks > 0).then_some(spall_server::ResidencyLimits {
            budget_bricks: args.residency_budget_bricks,
            interest_radius_bricks: args.residency_radius_bricks,
        }),
    };
    match spall_server::serve(config) {
        Ok(summary) => {
            println!(
                "sandbox-server: {} ticks={} clients={} committed={} hash={}",
                summary.result,
                summary.ticks_run,
                summary.clients_connected,
                summary.transactions_committed,
                summary.final_world_hash
            );
            if summary.result == "passed" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            eprintln!("sandbox-server: {error}");
            ExitCode::from(1)
        }
    }
}

/// `--replay <db>`: rebuild the world from the oldest checkpoint + the whole
/// committed topology journal and compare its canonical hash to `--expect-hash`.
fn run_replay(args: Args) -> ExitCode {
    let db = args.replay.expect("checked by caller");
    let cfg = spall_server::PersistConfig {
        world_id: spall_server::serve::T10_WORLD_ID,
        seed: args.seed,
        generator_version: 1,
    };
    let (sim, events) = match spall_server::replay_from_base_builtin(&db, &cfg) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sandbox-server: replay failed: {e}");
            if let Some(path) = &args.summary_json {
                let _ = write_replay_summary(path, "failed", 0, "", args.expect_hash.as_deref());
            }
            return ExitCode::from(1);
        }
    };
    let hash = sim.world().world_hash().to_string();
    let matches = args
        .expect_hash
        .as_deref()
        .map(|h| h.eq_ignore_ascii_case(&hash))
        .unwrap_or(true);
    let result = if matches { "passed" } else { "failed" };
    println!("sandbox-server: replay {result} topology_events={events} hash={hash}");
    if let Some(path) = &args.summary_json {
        let _ = write_replay_summary(path, result, events, &hash, args.expect_hash.as_deref());
    }
    if matches {
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "sandbox-server: replay hash {hash} != expected {}",
            args.expect_hash.as_deref().unwrap_or("<none>")
        );
        ExitCode::from(1)
    }
}

/// Parses a `x,y,z` triple of finite `f64` metres.
fn parse_vec3(s: &str) -> Option<[f64; 3]> {
    let mut it = s.split(',').map(|p| p.trim().parse::<f64>());
    let x = it.next()?.ok()?;
    let y = it.next()?.ok()?;
    let z = it.next()?.ok()?;
    if it.next().is_some() || !(x.is_finite() && y.is_finite() && z.is_finite()) {
        return None;
    }
    Some([x, y, z])
}

fn write_replay_summary(
    path: &std::path::Path,
    result: &str,
    events: u64,
    hash: &str,
    expected: Option<&str>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let expected = expected.unwrap_or("");
    let body = format!(
        "{{\n  \"version\": 1,\n  \"result\": \"{result}\",\n  \"replayed_topology_events\": {events},\n  \"replayed_world_hash\": \"{hash}\",\n  \"expected_world_hash\": \"{expected}\"\n}}\n"
    );
    std::fs::write(path, body)
}

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
    /// T16: persist to `<world>/world.db` — recover from it on start, journal
    /// committed transactions, checkpoint on the interval and on shutdown.
    #[arg(long)]
    save: bool,
    /// Ticks between engine checkpoints (1800 == 30 s at 60 Hz). 0 disables the
    /// periodic checkpoint (a shutdown checkpoint still happens).
    #[arg(long, default_value_t = 1_800)]
    checkpoint_interval_ticks: u64,
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
}

fn main() -> ExitCode {
    sandbox::init_tracing();
    let args = Args::parse();

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

    let save = args.save.then(|| args.world.join("world.db"));
    if let Some(db) = &save
        && let Some(parent) = db.parent()
    {
        let _ = std::fs::create_dir_all(parent);
    }

    let config = ServeConfig {
        listen: args.listen,
        scene: Scene::BridgeCut,
        join_token: token,
        max_ticks: args.ticks,
        quiescence_ticks: 45,
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

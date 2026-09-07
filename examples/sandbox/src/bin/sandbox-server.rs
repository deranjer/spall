use clap::Parser;
use spall_server::ServerConfig;
use std::{net::SocketAddr, path::PathBuf, process::ExitCode, time::Duration};

#[derive(Debug, Parser)]
#[command(name = "sandbox-server", about = "GPU-free Spall sandbox server host")]
struct Args {
    #[arg(long)]
    world: PathBuf,
    #[arg(long)]
    seed: u64,
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=10_000_000))]
    ticks: u64,
    #[arg(long)]
    log_json: PathBuf,
    #[arg(long, default_value_t = 0)]
    ready_delay_ms: u64,
    /// Harness-only failure injection. Networking and gameplay are not implemented in T00.
    #[arg(long, hide = true)]
    fail_after_tick: Option<u64>,
}

fn main() -> ExitCode {
    sandbox::init_tracing();
    let args = Args::parse();
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

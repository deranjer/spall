use clap::Parser;
use spall_client::ClientConfig;
use std::{path::PathBuf, process::ExitCode};

#[derive(Debug, Parser)]
#[command(name = "sandbox-client", about = "Spall sandbox render-window host")]
struct Args {
    /// T00 has no transport. Network connection/authentication is unavailable until T09.
    #[arg(long)]
    offline: bool,
    #[arg(long, default_value = ".local/runs/client.jsonl")]
    log_json: PathBuf,
    /// Bounded graphical capability run. Omit for an interactive window.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=10_000))]
    frames: Option<u64>,
    /// Run a bounded actual resize before close for the window smoke.
    #[arg(long)]
    scripted_resize: bool,
}

fn main() -> ExitCode {
    sandbox::init_tracing();
    let args = Args::parse();
    if !args.offline {
        eprintln!(
            "sandbox-client: network connection/authentication is unavailable until T09; use --offline for the T00 render host"
        );
        return ExitCode::from(2);
    }
    let config = ClientConfig {
        log_json: args.log_json,
        max_frames: args.frames,
        scripted_resize: args.scripted_resize,
    };
    match spall_client::run_window(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error @ spall_client::ClientError::Gpu(_)) => {
            eprintln!("sandbox-client: {error}");
            ExitCode::from(3)
        }
        Err(error) => {
            eprintln!("sandbox-client: {error}");
            ExitCode::from(1)
        }
    }
}

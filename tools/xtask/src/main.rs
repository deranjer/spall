mod netcheck;
mod process;
mod session;

use clap::{Args, Parser, Subcommand};
use process::{ProcessFailure, run_bounded, wait_for_server};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(
    name = "cargo xtask",
    about = "Spall build and bounded process harness"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Format, lint, build, and run CPU tests sequentially.
    Check,
    /// Build sandbox binaries and supervise one bounded GPU-free server.
    Smoke(SmokeArgs),
    /// T09 transport harness: one QUIC server, N headless clients, an opt UDP
    /// loss proxy, every channel exercised, bounded teardown.
    NetCheck(netcheck::NetCheckArgs),
    /// T10 replication harness: one `sandbox-server --serve`, N
    /// `sandbox-client --connect`, real QUIC, converge-to-one-hash check.
    Session(session::SessionArgs),
    /// T10 replication harness against a named built-in scenario
    /// (`fixtures/scenarios/<name>.json`).
    Scenario(session::ScenarioArgs),
    /// Render acceptance shapes or a named lighting fixture offscreen.
    /// Exit 3 means no GPU/capture capability.
    Capture(CaptureArgs),
    /// Planned for later performance gates.
    Bench(UnavailableArgs),
    /// T16 persistence crash suite: real `spall_sim::Simulation` → `spall_store`
    /// → recovery, with crash-point and disk-fault injection. Writes
    /// `summary.json` with the measured bytes/write rate.
    CrashTest(CrashTestArgs),
}

#[derive(Debug, Args)]
struct CrashTestArgs {
    /// Only `persistence` is implemented.
    #[arg(long, default_value = "persistence")]
    suite: String,
    /// Output directory. If omitted a unique directory under .local/runs is created.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CaptureArgs {
    /// Output directory. If omitted a unique directory under .local/runs is created.
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 720)]
    height: u32,
    /// `greedy` (default) or `culled`.
    #[arg(long, default_value = "greedy")]
    strategy: String,
    /// Render only the named acceptance shape.
    #[arg(long)]
    only: Option<String>,
    /// Fixture scene: `colored-room` (T13) or `lighting-sequence` (T14).
    #[arg(long)]
    scene: Option<String>,
    #[arg(long, default_value_t = 120_000, value_parser = clap::value_parser!(u64).range(1..=600_000))]
    timeout_ms: u64,
}

#[derive(Debug, Args)]
struct SmokeArgs {
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=10_000_000))]
    ticks: u64,
    #[arg(long, default_value_t = 15_000, value_parser = clap::value_parser!(u64).range(1..=300_000))]
    timeout_ms: u64,
    /// Optional bounded real-window capability check. Exit 3 means unavailable GPU/window capability.
    #[arg(long)]
    graphical: bool,
    /// Output directory. If omitted a unique directory under .local/runs is created.
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long, hide = true)]
    fail_child: bool,
    #[arg(long, hide = true)]
    ready_delay_ms: Option<u64>,
}

#[derive(Debug, Args)]
struct UnavailableArgs {
    #[arg(long, default_value = "")]
    name: String,
}

#[derive(Debug, Error)]
enum XtaskError {
    #[error("cargo subcommand {0:?} failed with {1}")]
    Cargo(Vec<String>, i32),
    #[error("cannot create output directory {path}: {source}")]
    Output {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Process(#[from] ProcessFailure),
    #[error(transparent)]
    Jsonl(#[from] spall_core::JsonlError),
    #[error("graphical capability unavailable: {0}")]
    Capability(String),
}

#[derive(Debug, Serialize)]
struct SmokeSummary<'a> {
    version: u32,
    result: &'a str,
    ticks: u64,
    server_pid: Option<u32>,
    graphical: bool,
    platform: &'a str,
    backend: &'a str,
    note: &'a str,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            match error {
                XtaskError::Capability(_) => ExitCode::from(3),
                _ => ExitCode::from(1),
            }
        }
    }
}

fn run(cli: Cli) -> Result<(), XtaskError> {
    match cli.command {
        CommandKind::Check => check(),
        CommandKind::Smoke(args) => smoke(args),
        CommandKind::NetCheck(args) => netcheck::run(args, unique_output),
        CommandKind::Session(args) => session::run_session(args, || unique_run_dir("session")),
        CommandKind::Scenario(args) => session::run_scenario(args, || unique_run_dir("scenario")),
        CommandKind::Capture(args) => capture(args),
        CommandKind::Bench(_) => unavailable("bench", "G1/G2 measurement work"),
        CommandKind::CrashTest(args) => crash_test(args),
    }
}

fn crash_test(args: CrashTestArgs) -> Result<(), XtaskError> {
    if args.suite != "persistence" {
        eprintln!(
            "xtask: crash-test suite {:?} is not implemented (only `persistence`)",
            args.suite
        );
        return Err(XtaskError::Capability("unknown crash-test suite".into()));
    }
    let output = args.output.unwrap_or_else(|| unique_run_dir("crash"));
    std::fs::create_dir_all(&output).map_err(|source| XtaskError::Output {
        path: output.display().to_string(),
        source,
    })?;

    run_cargo(&[
        "build",
        "-p",
        "spall_server",
        "--features",
        "persist-bench",
        "--bin",
        "crash-bench",
    ])?;

    let status = Command::new(sandbox_binary("crash-bench"))
        .args(["--output", &output.display().to_string()])
        .current_dir(workspace_root())
        .status()
        .map_err(|source| XtaskError::Output {
            path: "crash-bench".into(),
            source,
        })?;
    if status.success() {
        println!("crash-test passed: {}", output.display());
        Ok(())
    } else {
        Err(XtaskError::Cargo(
            vec!["crash-bench".into()],
            status.code().unwrap_or(1),
        ))
    }
}

fn unavailable(command: &str, task: &str) -> Result<(), XtaskError> {
    eprintln!("xtask: `{command}` is unavailable until {task} is implemented");
    Err(XtaskError::Capability("requested future command".into()))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask must be in tools/xtask")
        .to_owned()
}

fn run_cargo(arguments: &[&str]) -> Result<(), XtaskError> {
    let status = Command::new("cargo")
        .args(arguments)
        .current_dir(workspace_root())
        .status()
        .map_err(|source| XtaskError::Output {
            path: "cargo".into(),
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(XtaskError::Cargo(
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            status.code().unwrap_or(1),
        ))
    }
}

fn check() -> Result<(), XtaskError> {
    run_cargo(&["fmt", "--all", "--", "--check"])?;
    run_cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--all-features",
        "--",
        "-D",
        "warnings",
    ])?;
    run_cargo(&["test", "--workspace", "--all-features"])?;
    Ok(())
}

fn smoke(args: SmokeArgs) -> Result<(), XtaskError> {
    let output = args.output.unwrap_or_else(unique_output);
    std::fs::create_dir_all(&output).map_err(|source| XtaskError::Output {
        path: output.display().to_string(),
        source,
    })?;
    let summary_path = output.join("summary.json");
    if summary_path.exists() {
        std::fs::remove_file(&summary_path).map_err(|source| XtaskError::Output {
            path: summary_path.display().to_string(),
            source,
        })?;
    }
    if let Err(error) = run_cargo(&["build", "-p", "sandbox", "--bin", "sandbox-server"]) {
        write_summary(
            &output,
            &SmokeSummary {
                version: 1,
                result: "failed",
                ticks: args.ticks,
                server_pid: None,
                graphical: args.graphical,
                platform: std::env::consts::OS,
                backend: "not exercised",
                note: "sandbox-server build failed before child startup",
            },
        )?;
        return Err(error);
    }
    let server_log = output.join("server.jsonl");
    let server = sandbox_binary("sandbox-server");
    let mut command = Command::new(server);
    command.args([
        "--world",
        &output.join("world").display().to_string(),
        "--seed",
        "42",
        "--listen",
        "127.0.0.1:5000",
        "--ticks",
        &args.ticks.to_string(),
        "--log-json",
        &server_log.display().to_string(),
    ]);
    if let Some(delay) = args.ready_delay_ms {
        command.args(["--ready-delay-ms", &delay.to_string()]);
    }
    if args.fail_child {
        command.args(["--fail-after-tick", "0"]);
    }
    let server_result =
        match wait_for_server(command, &server_log, Duration::from_millis(args.timeout_ms)) {
            Ok(result) => result,
            Err(error) => {
                write_summary(
                    &output,
                    &SmokeSummary {
                        version: 1,
                        result: "failed",
                        ticks: args.ticks,
                        server_pid: error.pid(),
                        graphical: args.graphical,
                        platform: std::env::consts::OS,
                        backend: "not exercised",
                        note: "server process failed or timed out; inspect server.jsonl",
                    },
                )?;
                return Err(error.into());
            }
        };
    if args.graphical
        && let Err(error) = graphical_smoke(&output, Duration::from_millis(args.timeout_ms))
    {
        write_summary(
            &output,
            &SmokeSummary {
                version: 1,
                result: "failed",
                ticks: args.ticks,
                server_pid: Some(server_result.pid),
                graphical: true,
                platform: std::env::consts::OS,
                backend: "failed",
                note: "inspect client.jsonl for graphical smoke diagnostics",
            },
        )?;
        return Err(error);
    }
    write_summary(
        &output,
        &SmokeSummary {
            version: 1,
            result: "passed",
            ticks: args.ticks,
            server_pid: Some(server_result.pid),
            graphical: args.graphical,
            platform: std::env::consts::OS,
            backend: if args.graphical {
                "recorded in client.jsonl"
            } else {
                "not exercised"
            },
            note: "T00 process readiness only; socket transport is unimplemented until T09",
        },
    )?;
    println!("smoke passed: {}", output.display());
    Ok(())
}

fn graphical_smoke(output: &Path, timeout: Duration) -> Result<(), XtaskError> {
    let client_log = output.join("client.jsonl");
    let mut command = Command::new(sandbox_binary("sandbox-client"));
    run_cargo(&[
        "build",
        "-p",
        "sandbox",
        "--features",
        "client",
        "--bin",
        "sandbox-client",
    ])?;
    command.args([
        "--offline",
        "--log-json",
        &client_log.display().to_string(),
        "--frames",
        "3",
        "--scripted-resize",
    ]);
    match process::wait_for_client(command, &client_log, timeout) {
        Ok(_) => {}
        Err(error) if error.exit_code() == Some(3) => {
            return Err(XtaskError::Capability(error.to_string()));
        }
        Err(error) => return Err(XtaskError::Process(error)),
    }
    Ok(())
}

fn capture(args: CaptureArgs) -> Result<(), XtaskError> {
    if !matches!(args.strategy.as_str(), "greedy" | "culled") {
        eprintln!("xtask: --strategy must be `greedy` or `culled`");
        return Err(XtaskError::Capability("invalid capture strategy".into()));
    }
    let output = args.output.unwrap_or_else(|| unique_run_dir("t05-capture"));
    std::fs::create_dir_all(&output).map_err(|source| XtaskError::Output {
        path: output.display().to_string(),
        source,
    })?;

    run_cargo(&[
        "build",
        "-p",
        "sandbox",
        "--features",
        "client",
        "--bin",
        "sandbox-capture",
    ])?;

    let mut command = Command::new(sandbox_binary("sandbox-capture"));
    command.args([
        "--out",
        &output.display().to_string(),
        "--width",
        &args.width.to_string(),
        "--height",
        &args.height.to_string(),
        "--strategy",
        &args.strategy,
    ]);
    if let Some(only) = &args.only {
        command.args(["--only", only]);
    }
    if let Some(scene) = &args.scene {
        command.args(["--scene", scene]);
    }

    match run_bounded(command, Duration::from_millis(args.timeout_ms)) {
        Ok(status) if status.success() => {
            println!("capture written: {}", output.display());
            Ok(())
        }
        Ok(status) if status.code() == Some(3) => Err(XtaskError::Capability(
            "renderer capture needs a working GPU/driver".into(),
        )),
        Ok(status) => Err(XtaskError::Cargo(
            vec!["sandbox-capture".into()],
            status.code().unwrap_or(1),
        )),
        Err(error) => Err(error.into()),
    }
}

fn sandbox_binary(name: &str) -> PathBuf {
    let executable = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"));
    // Cargo runs from workspace_root(), so relative overrides use that same
    // base even when this executable was launched from another directory.
    let target = workspace_root().join(target);
    target.join("debug").join(executable)
}

fn unique_output() -> PathBuf {
    unique_run_dir("smoke")
}

fn unique_run_dir(prefix: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock precedes Unix epoch")
        .as_millis();
    workspace_root()
        .join(".local/runs")
        .join(format!("{prefix}-{millis}-{}", std::process::id()))
}

fn write_summary(output: &Path, summary: &SmokeSummary<'_>) -> Result<(), XtaskError> {
    let path = output.join("summary.json");
    let body = serde_json::to_vec_pretty(summary).expect("summary is serializable");
    std::fs::write(&path, body).map_err(|source| XtaskError::Output {
        path: path.display().to_string(),
        source,
    })
}

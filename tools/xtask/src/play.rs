//! `cargo xtask play` — one `sandbox-server` plus one interactive
//! `sandbox-client --interactive` window, for hands-on local testing
//! (ENG-69's "how to try it" recipe, wrapped so nobody has to hand-generate a
//! join token or babysit two terminals).
//!
//! This shares its process-supervision plumbing with [`crate::session`] (the
//! scripted, bounded `session`/`scenario` harness) but is a different shape:
//! there is no scenario file, no scripted movement, no pass/fail assertion —
//! just a server and a window, run until the player closes it. The one
//! footgun that plumbing doesn't save you from is `--ticks`: `session`'s
//! harness wants a *short* one (the run is scripted and bounded on purpose),
//! while `--interactive` has no natural end, so this defaults it large enough
//! that nobody watching the clock has to think about it.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;

use crate::session::{ChildGuard, random_hex_32, wait_for_addr, write_file};
use crate::{XtaskError, run_cargo, sandbox_binary_profile, workspace_root};

/// `cargo xtask play` — launches a server and an interactive window against
/// it, for a hands-on local play session.
#[derive(Debug, Args)]
pub struct PlayArgs {
    /// Built-in scene both the server and the interactive client load:
    /// `walk` (default — the T19 player-movement arena) or any other
    /// `spall_client::BaselineScene` name.
    #[arg(long, default_value = "walk")]
    scene: String,
    /// Server tick budget, at the server's paced 60 Hz. Deliberately large —
    /// `--interactive` has no natural end (a person closes the window when
    /// done), unlike the scripted `session`/`scenario` harness this shares
    /// its process plumbing with. Default is one hour; raise it for a longer
    /// session.
    #[arg(long, default_value_t = 216_000, value_parser = clap::value_parser!(u64).range(1..=10_000_000))]
    ticks: u64,
    /// Build and run optimized (release) binaries instead of debug ones.
    #[arg(long)]
    release: bool,
    /// How long to wait for the server to bind before giving up.
    #[arg(long, default_value_t = 20_000)]
    startup_timeout_ms: u64,
    /// Output directory for the generated join token, fingerprint, and
    /// process logs. A unique directory under `.local/runs` is created if
    /// omitted.
    #[arg(long)]
    output: Option<PathBuf>,
}

pub fn run(args: PlayArgs, unique_output: impl FnOnce() -> PathBuf) -> Result<(), XtaskError> {
    let output = args.output.unwrap_or_else(unique_output);
    fs::create_dir_all(&output).map_err(|source| XtaskError::Output {
        path: output.display().to_string(),
        source,
    })?;

    let profile = if args.release { "release" } else { "debug" };
    let mut server_build = vec!["build", "-p", "sandbox", "--bin", "sandbox-server"];
    let mut client_build = vec![
        "build",
        "-p",
        "sandbox",
        "--features",
        "client",
        "--bin",
        "sandbox-client",
    ];
    if args.release {
        server_build.push("--release");
        client_build.push("--release");
    }
    run_cargo(&server_build)?;
    run_cargo(&client_build)?;

    // A fresh token per session (unlike `session`/`scenario`'s seeded one —
    // there is nothing here that needs to reproduce deterministically).
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ u64::from(std::process::id());
    let token_file = output.join("join.token");
    write_file(&token_file, random_hex_32(seed).as_bytes())?;
    let fp_file = output.join("server.fingerprint");
    let addr_file = output.join("server.addr");
    let _ = fs::remove_file(&fp_file);
    let _ = fs::remove_file(&addr_file);

    let mut server_cmd = Command::new(sandbox_binary_profile("sandbox-server", profile));
    server_cmd.current_dir(workspace_root()).args([
        "--serve",
        "--listen",
        "127.0.0.1:0",
        "--join-token-file",
        &token_file.display().to_string(),
        "--fingerprint-out",
        &fp_file.display().to_string(),
        "--addr-out",
        &addr_file.display().to_string(),
        "--log-json",
        &output.join("server.jsonl").display().to_string(),
        "--ticks",
        &args.ticks.to_string(),
        "--scene",
        &args.scene,
        // A play session mostly just moves around; don't let the
        // idle-quiescence early-stop (meant for a scripted run with gaps
        // between edits) cut it short for want of a committed transaction.
        "--quiescence-ticks",
        "0",
        "--paced",
    ]);
    hide_console(&mut server_cmd);
    let mut guard = ChildGuard::default();
    let server_child = server_cmd.spawn().map_err(|source| XtaskError::Output {
        path: "sandbox-server (spawn)".into(),
        source,
    })?;
    guard.push("server".into(), server_child);

    eprintln!("xtask play: waiting for sandbox-server to bind...");
    let bound: SocketAddr = wait_for_addr(
        &addr_file,
        &mut guard,
        Duration::from_millis(args.startup_timeout_ms),
    )?
    .parse()
    .map_err(|_| XtaskError::Capability("server wrote an unparseable bound address".into()))?;
    eprintln!(
        "xtask play: server listening on {bound} (scene={}, ticks={}); opening the window \
         (WASD to move, mouse to look, Space to jump, Escape releases the cursor)...",
        args.scene, args.ticks
    );

    let mut client_cmd = Command::new(sandbox_binary_profile("sandbox-client", profile));
    client_cmd.current_dir(workspace_root()).args([
        "--connect",
        &bound.to_string(),
        "--server-fingerprint",
        &fp_file.display().to_string(),
        "--join-token-file",
        &token_file.display().to_string(),
        "--interactive",
        "--log-json",
        &output.join("client.jsonl").display().to_string(),
    ]);
    // Deliberately not hidden and not captured: the window is the point, and
    // a connect failure's error message should land directly in this
    // terminal instead of a log nobody's watching.
    let client_status = client_cmd.status().map_err(|source| XtaskError::Output {
        path: "sandbox-client (spawn)".into(),
        source,
    })?;

    // The window is closed (or the client otherwise exited); the server has
    // nothing left to serve. `say_bye`/`close` on the client side already
    // told it so — give it a moment to shut down clean before killing any
    // survivor.
    let codes = guard.wait_all(Duration::from_secs(5));
    eprintln!("xtask play: server exit: {codes:?}");

    if client_status.success() {
        Ok(())
    } else {
        Err(XtaskError::Cargo(
            vec!["sandbox-client".into(), "--interactive".into()],
            client_status.code().unwrap_or(1),
        ))
    }
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_: &mut Command) {}

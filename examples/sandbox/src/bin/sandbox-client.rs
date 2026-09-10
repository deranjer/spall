use clap::Parser;
use spall_client::{
    BaselineScene, ClientConfig, ClientNetConfig, MovementStep, ScriptTarget, ScriptedAction,
    cut_request, run_replication_client,
};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use std::{net::SocketAddr, path::PathBuf, process::ExitCode, time::Duration};

#[derive(Debug, Parser)]
#[command(
    name = "sandbox-client",
    about = "Spall sandbox render-window / replication client"
)]
struct Args {
    /// T00 offline render host (no transport).
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

    // --- T10 headless replication client ---
    /// Connect to this server (or UDP proxy) address and replicate.
    #[arg(long)]
    connect: Option<SocketAddr>,
    /// File holding the server certificate fingerprint (hex).
    #[arg(long)]
    server_fingerprint: Option<PathBuf>,
    /// File holding the per-run join token (hex).
    #[arg(long)]
    join_token_file: Option<PathBuf>,
    /// Scripted cut: `TICK:X,Y,Z:RADIUS` in the target volume's cell
    /// coordinates, optionally `:body` to aim at the detached body instead of
    /// terrain. Repeatable.
    #[arg(long = "cut", value_parser = parse_cut)]
    cuts: Vec<Cut>,
    /// T19 scripted movement leg: `FROM:TO:MX,MY,MZ:BUTTONS` — hold the
    /// (clamped `-1..=1`) movement axes and button bitset from server tick
    /// `FROM` up to `TO`. The player walks along `+X` (button 1 = jump).
    /// Repeatable; implies a predicted player capsule.
    #[arg(long = "move", value_parser = parse_move)]
    moves: Vec<MoveLeg>,
    /// This client's index in a multi-client session; namespaces request ids so
    /// two clients never collide on the server's idempotency ledger.
    #[arg(long, default_value_t = 0)]
    client_index: u64,
    /// Stop once the observed server tick reaches this (0 = only on close).
    #[arg(long, default_value_t = 0)]
    run_ticks: u64,
    /// T17: request a full late-join baseline over a bulk transfer instead of
    /// installing the fixed scene — the replica reaches the server's current
    /// topology with no edit replay.
    #[arg(long)]
    late_join: bool,
    /// Fixed baseline scene a live replica installs; must match the server's
    /// `--scene`. `bridge-cut` (default) or `cross-bridge-cut`.
    #[arg(long, default_value = "bridge-cut")]
    scene: String,
    /// T17: wait this long after the process starts before connecting, so a
    /// harness can stagger a late joiner behind an already-running client.
    #[arg(long, default_value_t = 0)]
    connect_delay_ms: u64,
    #[arg(long)]
    summary_json: Option<PathBuf>,
    /// Whole-session deadline.
    #[arg(long, default_value_t = 30_000)]
    timeout_ms: u64,
}

#[derive(Debug, Clone)]
struct Cut {
    tick: u64,
    cell: [i64; 3],
    radius: i64,
    target: ScriptTarget,
}

#[derive(Debug, Clone)]
struct MoveLeg {
    from: u64,
    to: u64,
    movement: [f32; 3],
    buttons: u32,
}

fn parse_move(s: &str) -> Result<MoveLeg, String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 4 {
        return Err("expected FROM:TO:MX,MY,MZ:BUTTONS".into());
    }
    let from = parts[0].parse().map_err(|_| "bad from tick")?;
    let to = parts[1].parse().map_err(|_| "bad to tick")?;
    let m: Vec<f32> = parts[2]
        .split(',')
        .map(|v| v.parse().map_err(|_| "bad movement axis".to_string()))
        .collect::<Result<_, _>>()?;
    if m.len() != 3 {
        return Err("movement must be MX,MY,MZ".into());
    }
    let buttons = parts[3].parse().map_err(|_| "bad buttons")?;
    Ok(MoveLeg {
        from,
        to,
        movement: [m[0], m[1], m[2]],
        buttons,
    })
}

fn parse_cut(s: &str) -> Result<Cut, String> {
    let parts: Vec<&str> = s.split(':').collect();
    if !(3..=4).contains(&parts.len()) {
        return Err("expected TICK:X,Y,Z:RADIUS[:body]".into());
    }
    let tick = parts[0].parse().map_err(|_| "bad tick")?;
    let xyz: Vec<i64> = parts[1]
        .split(',')
        .map(|v| v.parse().map_err(|_| "bad cell coord".to_string()))
        .collect::<Result<_, _>>()?;
    if xyz.len() != 3 {
        return Err("cell must be X,Y,Z".into());
    }
    let radius = parts[2].parse().map_err(|_| "bad radius")?;
    let target = match parts.get(3) {
        None | Some(&"terrain") => ScriptTarget::Terrain,
        Some(&"body") => ScriptTarget::DetachedBody,
        Some(other) => return Err(format!("unknown cut target `{other}` (want `body`)")),
    };
    Ok(Cut {
        tick,
        cell: [xyz[0], xyz[1], xyz[2]],
        radius,
        target,
    })
}

fn main() -> ExitCode {
    sandbox::init_tracing();
    let args = Args::parse();

    if args.connect.is_some() {
        return run_replication(args);
    }
    if !args.offline {
        eprintln!(
            "sandbox-client: pass --connect <addr> for the T10 replication client, or --offline for the T00 render host"
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

fn run_replication(args: Args) -> ExitCode {
    let connect_addr = args.connect.expect("checked by caller");
    let (Some(fp_file), Some(token_file)) = (args.server_fingerprint, args.join_token_file) else {
        eprintln!("sandbox-client: --connect requires --server-fingerprint and --join-token-file");
        return ExitCode::from(2);
    };
    let fingerprint = match std::fs::read_to_string(&fp_file)
        .ok()
        .and_then(|s| Fingerprint::from_hex(s.trim()))
    {
        Some(f) => f,
        None => {
            eprintln!("sandbox-client: could not read a fingerprint from {fp_file:?}");
            return ExitCode::from(2);
        }
    };
    let token = match std::fs::read_to_string(&token_file)
        .ok()
        .and_then(|s| JoinToken::from_hex(s.trim()))
    {
        Some(t) => t,
        None => {
            eprintln!("sandbox-client: could not read a join token from {token_file:?}");
            return ExitCode::from(2);
        }
    };

    // A movement client (T19) always pulls a baseline (any scene), so the fixed
    // `BaselineScene` selector is unused and `--scene walk` is accepted.
    let baseline_scene = match BaselineScene::from_name(&args.scene) {
        Some(s) => s,
        None if !args.moves.is_empty() => BaselineScene::default(),
        None => {
            eprintln!(
                "sandbox-client: unknown --scene `{}` (expected bridge-cut, cross-bridge-cut, or checkerboard-split)",
                args.scene
            );
            return ExitCode::from(2);
        }
    };

    if args.connect_delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(args.connect_delay_ms));
    }

    let id_base = (args.client_index << 40) | 1;
    let script: Vec<ScriptedAction> = args
        .cuts
        .iter()
        .enumerate()
        .map(|(i, c)| ScriptedAction {
            at_tick: c.tick,
            request: cut_request(id_base + i as u64, i as u64, c.cell, c.radius),
            target: c.target,
        })
        .collect();

    let movement_script: Vec<MovementStep> = args
        .moves
        .iter()
        .map(|m| MovementStep {
            from_tick: m.from,
            to_tick: m.to,
            movement: m.movement,
            view_dir: [1.0, 0.0, 0.0],
            buttons: m.buttons,
        })
        .collect();

    let config = ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script,
        movement_script,
        late_join: args.late_join,
        baseline_scene,
        run_ticks: args.run_ticks,
        idle_grace: Duration::from_millis(500),
        overall_timeout: Duration::from_millis(args.timeout_ms),
        log_json: args.log_json,
        summary_json: args.summary_json,
        transport: TransportConfig::default(),
    };
    match run_replication_client(config) {
        Ok(summary) => {
            println!(
                "sandbox-client: {} applied={} motion={} body_disp={:.2}m body_cut={} repairs={} hash={}",
                summary.result,
                summary.transactions_applied,
                summary.motion_snapshots,
                summary.max_body_displacement_m,
                summary.body_cut_committed,
                summary.repair_requests_sent,
                summary.final_world_hash
            );
            if summary.result == "passed" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            eprintln!("sandbox-client: {error}");
            ExitCode::from(1)
        }
    }
}

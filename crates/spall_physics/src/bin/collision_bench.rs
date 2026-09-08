//! `collision-bench` — runs the T06 feasibility suite at gate size (the full
//! 64-brick connected body and 256 debris pieces) and writes machine-readable
//! results. Not run by `cargo test`; invoke it on the reference machine and
//! record its output in `docs/collision-decision.md`.
//!
//! Usage:
//!   cargo run -p spall_physics --bin collision-bench -- [--small] [--out DIR]

use std::io::Write;
use std::path::PathBuf;

use spall_physics::report::{FeasibilityParams, run_feasibility};

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    let mut small = false;
    let mut out: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--small" => small = true,
            "--out" => out = args.next().map(PathBuf::from),
            "--help" | "-h" => {
                println!("collision-bench [--small] [--out DIR]");
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("collision-bench: unknown argument {other:?}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    let params = if small {
        FeasibilityParams::small()
    } else {
        FeasibilityParams::gate()
    };

    eprintln!(
        "collision-bench: running {} configuration ({} debris, {:?} bricks)",
        if small { "small" } else { "gate" },
        params.debris_count,
        params.multibrick
    );
    let report = run_feasibility(params);
    let json = report.to_json();

    println!("{json}");
    print_human(&report);

    if let Some(dir) = out {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("collision-bench: cannot create {}: {e}", dir.display());
            return std::process::ExitCode::from(1);
        }
        let path = dir.join("collision-feasibility.json");
        match std::fs::File::create(&path).and_then(|mut f| f.write_all(json.as_bytes())) {
            Ok(()) => eprintln!("collision-bench: wrote {}", path.display()),
            Err(e) => {
                eprintln!("collision-bench: cannot write {}: {e}", path.display());
                return std::process::ExitCode::from(1);
            }
        }
    }

    // A blown-up settle, a collapsed interior, or a compound that cannot stop a
    // 60 m/s CCD projectile is a hard failure.
    let ok = [&report.native, &report.compound]
        .iter()
        .all(|r| r.settle_finite && r.building_interior_clearance_m > 0.5 && r.sleep_wake.ok())
        && report.compound.projectile_max_stop_m_s >= 60.0;
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        eprintln!("collision-bench: feasibility scenario failed; see JSON above");
        std::process::ExitCode::from(1)
    }
}

fn print_human(report: &spall_physics::report::FeasibilityReport) {
    for r in [&report.native, &report.compound] {
        eprintln!("--- {} ---", r.representation);
        eprintln!(
            "  64-brick body: {} primitives, ~{} KiB",
            r.multibrick_primitives,
            r.multibrick_est_bytes / 1024
        );
        eprintln!(
            "  64-brick build us (complete)  p50 {:.1} / p95 {:.1} / p99 {:.1} / max {:.1} (n={}); wrap-only p50 {:.1}",
            r.multibrick_build.p50_us,
            r.multibrick_build.p95_us,
            r.multibrick_build.p99_us,
            r.multibrick_build.max_us,
            r.multibrick_build.samples,
            r.multibrick_wrap.p50_us,
        );
        eprintln!(
            "  settle step us  p50 {:.1} / p95 {:.1} / p99 {:.1} (n={})",
            r.settle_step.p50_us, r.settle_step.p95_us, r.settle_step.p99_us, r.settle_step.samples
        );
        eprintln!(
            "  settle: {:.0}% asleep, max tail speed {:.3} m/s, finite {}",
            r.settle_sleep_fraction * 100.0,
            r.settle_max_speed,
            r.settle_finite
        );
        let sw = &r.sleep_wake;
        eprintln!(
            "  sleep/wake: slept {}, woke(rebuild) {}, re-slept {}, woke(impulse) {}, travel {:.2} m, re-contact {}, re-slept {}, stable {}",
            sw.slept,
            sw.woke_on_rebuild,
            sw.reslept_after_rebuild,
            sw.woke_on_impulse,
            sw.travel_m,
            sw.recontact,
            sw.reslept,
            sw.stable
        );
        eprintln!(
            "  building step us  p50 {:.1} / p95 {:.1} / p99 {:.1}; settled {}, interior clearance {:.3} m",
            r.building_step.p50_us,
            r.building_step.p95_us,
            r.building_step.p99_us,
            r.building_settled,
            r.building_interior_clearance_m
        );
        eprintln!(
            "  rebuild us (complete: remove + decompose + wrap + reinsert)  p50 {:.1} / p95 {:.1} / p99 {:.1} / max {:.1} (n={})",
            r.rebuild.p50_us,
            r.rebuild.p95_us,
            r.rebuild.p99_us,
            r.rebuild.max_us,
            r.rebuild.samples
        );
        eprintln!(
            "  mass err {:.4}, COM err {:.4} m, inertia err {:.4}, projectile stopped up to {:.0} m/s",
            r.mass_rel_err, r.com_abs_err_m, r.inertia_rel_err, r.projectile_max_stop_m_s
        );
        eprintln!(
            "  worst-case fragmentation: {} solid cells -> {} primitives",
            r.worst_case_solid_cells, r.worst_case_primitives
        );
    }
}

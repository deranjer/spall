//! Offline persistence crash-suite (`cargo xtask crash-test --suite persistence`).
//!
//! Drives a real `spall_sim::Simulation` through capture → `spall_store` →
//! recover → restore with crash-point and disk-fault injection, and writes a
//! machine-readable `summary.json` (including measured bytes/write rate). Exit
//! code 0 = every scenario held, 1 = a scenario failed, 2 = bad arguments.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut output: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" | "-o" => output = args.next().map(PathBuf::from),
            "--help" | "-h" => {
                eprintln!("crash-bench --output <dir>");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("crash-bench: unexpected argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(output) = output else {
        eprintln!("crash-bench: --output <dir> is required");
        return ExitCode::from(2);
    };

    if let Err(e) = std::fs::create_dir_all(&output) {
        eprintln!("crash-bench: cannot create {}: {e}", output.display());
        return ExitCode::from(2);
    }
    let scratch = output.join("scratch");

    let report = match spall_server::persist::run_crash_suite(&scratch) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("crash-bench: suite error: {e}");
            return ExitCode::from(1);
        }
    };

    let summary = output.join("summary.json");
    let body = serde_json::to_vec_pretty(&report).expect("report serialises");
    if let Err(e) = std::fs::write(&summary, body) {
        eprintln!("crash-bench: cannot write {}: {e}", summary.display());
        return ExitCode::from(1);
    }
    let _ = std::fs::remove_dir_all(&scratch);

    for s in &report.scenarios {
        println!(
            "  {:32} {}  {}",
            s.name,
            if s.passed { "ok  " } else { "FAIL" },
            s.detail
        );
    }
    println!(
        "crash-test {}: {} in-process scenarios, {:.0} bytes/journal-write, {:.0} commit-bytes/s, wal<= {} B",
        report.result,
        report.scenarios.len(),
        report.metrics.bytes_per_journal_write,
        report.metrics.commit_bytes_per_sec,
        report.metrics.max_wal_bytes
    );
    for note in &report.unrun_here {
        println!("  NOT covered here: {note}");
    }
    println!("summary: {}", summary.display());

    if report.result == "passed" {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

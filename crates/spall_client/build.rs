//! Records the git commit and branch the client was built from, for the debug
//! report (`F9`): "which build is this" is the first thing a report needs.

use std::process::Command;

fn git(args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn main() {
    println!(
        "cargo:rustc-env=SPALL_GIT_COMMIT={}",
        git(&["rev-parse", "--short", "HEAD"])
    );
    println!(
        "cargo:rustc-env=SPALL_GIT_BRANCH={}",
        git(&["rev-parse", "--abbrev-ref", "HEAD"])
    );
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=build.rs");
}

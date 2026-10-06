//! Capture the current git commit hash at build time so the running binary can
//! print exactly which revision it is. This removes any ambiguity about whether a
//! deploy (git pull + build) actually updated the binary.

use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=G9_GIT_HASH={hash}");
    // Rebuild if HEAD moves (best effort; harmless if these paths don't exist).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
}

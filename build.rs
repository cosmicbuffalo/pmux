//! Puts the commit pmux was built from into `pmux --version`, so the app can
//! show which build each machine runs.
use std::process::Command;

fn main() {
    // A build from an export with no git history (`git archive`) names its
    // commit in PMUX_COMMIT instead.
    println!("cargo:rerun-if-env-changed=PMUX_COMMIT");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    let commit = std::env::var("PMUX_COMMIT")
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(git_commit);
    let version = env!("CARGO_PKG_VERSION");
    let full = if commit.is_empty() {
        version.to_string()
    } else {
        format!("{version} ({commit})")
    };
    println!("cargo:rustc-env=PMUX_VERSION={full}");
}

fn git_commit() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

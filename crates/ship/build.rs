//! Embeds the source commit in the binary as `SHIP_COMMIT`.
//!
//! Order: the `SHIP_COMMIT` environment variable (release builds set it),
//! then `git rev-parse` on the checkout, then `unknown` (a source archive
//! with no git metadata). `git` is run with structured argv, never a shell.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SHIP_COMMIT");

    let commit = std::env::var("SHIP_COMMIT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(git_commit)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=SHIP_COMMIT={commit}");

    // Rebuild when HEAD moves. Paths come from git so linked worktrees work;
    // only existing paths are watched, because a missing one forces a rerun
    // on every build.
    for name in ["HEAD", "refs/heads", "packed-refs"] {
        if let Some(path) = git_path(name).filter(|path| path.exists()) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn git_commit() -> Option<String> {
    git(&["rev-parse", "--short=12", "HEAD"])
}

fn git_path(name: &str) -> Option<PathBuf> {
    git(&["rev-parse", "--git-path", name]).map(PathBuf::from)
}

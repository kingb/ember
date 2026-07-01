//! Embeds the git short hash + commit count at build time, so the About page can
//! show Version / Build / Commit (Ghostty-style). Falls back to "unknown"/"0" when
//! not a git checkout or git is unavailable — the build must never fail on this.

use std::process::Command;

fn main() {
    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let count = git(&["rev-list", "--count", "HEAD"]).unwrap_or_else(|| "0".to_string());
    println!("cargo:rustc-env=EMBER_GIT_HASH={hash}");
    println!("cargo:rustc-env=EMBER_GIT_COUNT={count}");
    // Re-run when HEAD moves so the embedded hash stays current.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

//! Embeds the build's short git commit hash into the binary via
//! `cargo:rustc-env`, so a running instance knows what code produced it even
//! when invoked from a different working directory or from a build with no
//! `.git` at all (a packaged tarball). A runtime `git rev-parse` shell-out
//! would fail exactly there, so this must happen at compile time.
//!
//! Consumed via `env!("LEARNIVE_BUILD_SHA")` — see `engine::APP_VERSION`.
//! Used today to stamp generated nodes/documents for QA traceability; kept
//! to one plain string rather than a richer struct so a future, richer
//! version scheme (build date, semantic version, ...) has nothing to unwind.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Workspace root is two levels up from this crate's manifest dir
    // (crates/learnive -> crates -> root). Resolved from `CARGO_MANIFEST_DIR`
    // rather than assumed via cwd, since build scripts should not rely on
    // being invoked from any particular working directory.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let git_dir = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join(".git"));

    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=LEARNIVE_BUILD_SHA={sha}");

    // Rebuild when HEAD moves (new commit, checkout, ...) so the stamp never
    // goes stale. Best-effort: a missing .git (e.g. the tarball case above)
    // just means it never reruns for that reason, which is fine since the
    // fallback is already baked in.
    if let Some(git_dir) = git_dir {
        let head = git_dir.join("HEAD");
        if let Ok(contents) = std::fs::read_to_string(&head) {
            println!("cargo:rerun-if-changed={}", head.display());
            if let Some(ref_path) = contents.trim().strip_prefix("ref: ") {
                println!(
                    "cargo:rerun-if-changed={}",
                    git_dir.join(ref_path).display()
                );
            }
        }
    }
}

//! Embed the build-time git identity into the `keel` binary so `keel native version` can report the
//! source revision it was built from — the tool's own provenance, distinct from the git it proxies
//! to. Best-effort: if git is unavailable or this isn't a checkout, the fields are empty and `keel
//! native version` reports an unknown commit rather than failing to build.
//!
//! Accuracy: the stamp is exact for a full build from a clean checkout (what CI and the benchmark
//! suite do). On *incremental* rebuilds it is best-effort — a working-tree edit that touches no
//! keel-cmd source can't trigger a build-script re-run, so a `cargo build` may reuse a stale stamp
//! until keel-cmd recompiles. We minimize the commit-staleness window by re-running on every ref
//! move (see the rerun-if-changed targets below), but `dirty` in particular is only guaranteed for
//! a from-scratch build.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn main() {
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_default();
    // Tracked-tree drift only (matches the bench's corpus dirty-gate): a modified tracked file means
    // the binary's behavior may differ from the committed source, so the build is not reproducible.
    let dirty = git(&["status", "--porcelain", "--untracked-files=no", "--ignore-submodules=none"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    println!("cargo:rustc-env=KEEL_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=KEEL_GIT_DIRTY={}", if dirty { "1" } else { "0" });

    // Re-run the build script when the commit moves so the embedded stamp stays current. Watching
    // HEAD alone is insufficient: a plain `git commit` on the current branch updates
    // refs/heads/<branch> and appends to logs/HEAD, but does NOT rewrite HEAD (it still holds
    // `ref: refs/heads/<branch>`). So watch logs/HEAD, which git appends on every commit / checkout /
    // reset / amend / merge, plus HEAD for branch switches. In a linked worktree `--git-dir` resolves
    // to the per-worktree `.git/worktrees/<name>`, whose HEAD/logs are worktree-local — correct there.
    println!("cargo:rerun-if-changed=build.rs");
    if let Some(git_dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/logs/HEAD");
    }
}

//! Embed the build-time git identity into the `keel` binary so `keel native version` can report
//! exactly which source revision it was built from — the tool's own provenance, distinct from the
//! git it proxies to. Best-effort: if git is unavailable or this isn't a checkout, the fields are
//! empty and `keel native version` reports an unknown commit rather than failing to build.
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

    // Rebuild when HEAD moves so the embedded commit stays current (best-effort; watches the resolved
    // git dir's HEAD, which changes on checkout/commit). Incremental builds otherwise recompile
    // keel-cmd only when its own sources change.
    println!("cargo:rerun-if-changed=build.rs");
    if let Some(git_dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
    }
}

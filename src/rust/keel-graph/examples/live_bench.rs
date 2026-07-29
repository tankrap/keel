//! Live-graph incrementality benchmark on REAL code.
//! Run: `cargo run --release -p keel-graph --example live_bench [dir]`
//!
//! Copies the target's .ts/.tsx into a temp workspace (so we can edit safely), builds the
//! graph, then measures the cost of a one-file change vs a full build — the SLO that makes
//! a live graph affordable at scale (NEW-1075). A static index re-indexes on commit; keel
//! re-resolves only what changed.

use keel_graph::LiveGraph;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    let src = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/Users/justin/agent-native-forge-temp/apps/forge/src".to_string());
    let work =
        std::env::temp_dir().join(format!("keel-graph-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let copied = copy_ts(Path::new(&src), &work);
    println!("copied {copied} .ts files → workspace");

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../keel-resolve/sidecar/resolve.mjs");
    let mut g = match LiveGraph::open(&work, &script) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("node not available: {e}");
            return;
        }
    };

    let t = Instant::now();
    let n = g.build().unwrap();
    let build = t.elapsed();

    let t = Instant::now();
    let c0 = g.refresh().unwrap();
    let noop = t.elapsed();

    // edit one file and re-resolve
    let victim = first_ts(&work).expect("a .ts file");
    let mut content = std::fs::read(&victim).unwrap();
    content.extend_from_slice(b"\n// keel live-bench touch\n");
    std::fs::write(&victim, &content).unwrap();
    let t = Instant::now();
    let c1 = g.refresh().unwrap();
    let inc = t.elapsed();

    println!("\n=== keel live-graph incrementality — REAL code ===");
    println!("files: {n}   edges: {}", g.edge_count());
    println!("full build:        {:>8.1} ms   ({n} files resolved)", build.as_secs_f64() * 1000.0);
    println!("refresh (no-op):   {:>8.2} ms   ({} re-resolved)", noop.as_secs_f64() * 1000.0, c0.len());
    println!("refresh (1 edit):  {:>8.2} ms   ({} re-resolved: {:?})", inc.as_secs_f64() * 1000.0, c1.len(), c1);
    if inc.as_secs_f64() > 0.0 {
        println!("\nincremental speedup: {:.0}x cheaper than a full build", build.as_secs_f64() / inc.as_secs_f64());
    }
    println!("(a static index re-indexes on commit; keel re-resolves only the changed file, live)");

    let _ = std::fs::remove_dir_all(&work);
}

fn copy_ts(src: &Path, dst: &Path) -> usize {
    let mut count = 0;
    let Ok(rd) = std::fs::read_dir(src) else { return 0 };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" || name == "dist" || name == "build" {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            count += copy_ts(&p, &dst.join(&*name));
        } else if (name.ends_with(".ts") || name.ends_with(".tsx")) && !name.ends_with(".d.ts") {
            std::fs::create_dir_all(dst).ok();
            if std::fs::copy(&p, dst.join(&*name)).is_ok() {
                count += 1;
            }
        }
    }
    count
}

fn first_ts(dir: &Path) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut dirs = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            dirs.push(p);
        } else if p.extension().is_some_and(|x| x == "ts") {
            return Some(p);
        }
    }
    for d in dirs {
        if let Some(f) = first_ts(&d) {
            return Some(f);
        }
    }
    None
}

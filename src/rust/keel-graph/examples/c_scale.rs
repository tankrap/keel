//! Live C include-graph over a real tree (the linux kernel) via the C sidecar — tier-2
//! (relevance/graph) at scale on C. Run:
//!   cargo run --release -p keel-graph --example c_scale -- [dir] [resolver.mjs]
//!
//! Builds the file-level `#include` graph, then reports the blast radius (transitive
//! reverse-deps) of a few hot kernel headers — the "what breaks if I touch this" query a
//! static, committed index can't answer live.

use keel_graph::LiveGraph;
use keel_resolve::Sidecar;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    let dir =
        PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "/Users/justin/keel-scale/linux".into()));
    let script = std::env::args().nth(2).map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../keel-resolve/sidecar/resolve-c.mjs")
    });
    if !dir.exists() {
        eprintln!("target does not exist: {}", dir.display());
        std::process::exit(1);
    }
    let mut sc = match Sidecar::spawn(&script) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("node not available: {e}");
            return;
        }
    };

    println!("target: {}", dir.display());
    let mut g = LiveGraph::new(&dir);
    let t = Instant::now();
    let n = g.build(&mut sc).unwrap();
    let build = t.elapsed();
    println!("\nfiles indexed : {n}");
    println!("include edges : {}", g.edge_count());
    println!("build time    : {:.1} s  ({:.0} files/s)", build.as_secs_f64(), n as f64 / build.as_secs_f64());

    println!("\nblast radius (transitive reverse-deps) of hot headers:");
    for hdr in [
        "include/linux/sched.h",
        "include/linux/fs.h",
        "include/linux/module.h",
        "include/linux/mm.h",
        "include/linux/kernel.h",
    ] {
        let direct = g.rdeps(hdr).len();
        let transitive = g.transitive_rdeps(hdr).len();
        if direct > 0 || transitive > 0 {
            println!("  {hdr:<28} direct {direct:>6}   transitive {transitive:>6}");
        }
    }

    // incremental: touch one header and re-resolve — only that file re-parses
    let victim = dir.join("include/linux/sched.h");
    if let Ok(orig) = std::fs::read(&victim) {
        let mut edited = orig.clone();
        edited.extend_from_slice(b"\n/* keel c_scale touch */\n");
        let _ = std::fs::write(&victim, &edited);
        let t = Instant::now();
        let changed = g.refresh(&mut sc).unwrap();
        let inc = t.elapsed();
        let _ = std::fs::write(&victim, &orig); // restore
        println!(
            "\nincremental refresh after 1 edit: {:.0} ms  ({} file(s) re-resolved)",
            inc.as_secs_f64() * 1000.0,
            changed.len()
        );
    }
}

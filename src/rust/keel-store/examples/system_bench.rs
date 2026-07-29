//! Full-system benchmark: the entire native keel pipeline on a REAL repo.
//! Run: `cargo run --release -p keel-store --example system_bench [dir]`
//! Default dir = apps/forge/src.
//!
//! Stages: ingest (snapshot + atomic commit) → checkout + round-trip verify → GC →
//! relevance (TS symbol slice) → live dependency graph. Everything measured end to end.

use keel_graph::LiveGraph;
use keel_resolve::Sidecar;
use keel_store::{snapshot, Repo};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() {
    let src = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/Users/justin/agent-native-forge-temp/apps/forge/src".to_string());
    let srcp = PathBuf::from(&src);
    let pid = std::process::id();
    let store_dir = std::env::temp_dir().join(format!("keel-sys-store-{pid}"));
    let co_dir = std::env::temp_dir().join(format!("keel-sys-co-{pid}"));
    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&co_dir);

    let (nfiles, raw_bytes) = count_and_bytes(&srcp);
    println!("╔═══ keel system benchmark ═══");
    println!("║ repo: {src}");
    println!("║ {nfiles} files, {:.1} MB source", raw_bytes as f64 / 1e6);

    // [1] INGEST
    let repo = Repo::open(&store_dir).unwrap();
    let t = Instant::now();
    let change = repo.commit_dir(&srcp, "system-bench ingest", "acct:bench", 0, None).unwrap();
    let ingest = t.elapsed();
    let store = repo.store();
    let objs = store.object_count().unwrap();
    let chunks = store.chunk_count().unwrap();
    let store_bytes = disk_blocks(&store_dir);
    println!("\n[1] INGEST  (snapshot + atomic commit)");
    println!("    time        {:>7.0} ms   ({:.0} files/s)", ms(ingest), nfiles as f64 / ingest.as_secs_f64());
    println!("    objects     {objs:>7}   ({chunks} chunks)");
    println!("    store size  {:>7.1} MB   (source {:.1} MB)", store_bytes as f64 / 1e6, raw_bytes as f64 / 1e6);

    // [2] CHECKOUT + verify
    let t = Instant::now();
    repo.checkout_change(change, &co_dir).unwrap();
    let checkout = t.elapsed();
    let tree = repo.change(change).unwrap().unwrap().tree;
    let resnap = snapshot::snapshot(store, &co_dir).unwrap();
    println!("\n[2] CHECKOUT + round-trip verify");
    println!("    time        {:>7.0} ms   ({:.0} files/s)", ms(checkout), nfiles as f64 / checkout.as_secs_f64());
    println!("    round-trip  {}", if resnap == tree { "IDENTICAL ✓ (snapshot∘checkout∘snapshot)" } else { "MISMATCH ✗" });

    // [3] GC
    let t = Instant::now();
    let gc = store.gc().unwrap();
    let gct = t.elapsed();
    println!("\n[3] GC  (everything reachable via HEAD)");
    println!("    time        {:>7.0} ms", ms(gct));
    println!("    removed     {} objs / {} chunks   kept {} / {}", gc.objects_removed, gc.chunks_removed, gc.objects_kept, gc.chunks_kept);

    // [4] RELEVANCE — TS symbol slice
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../keel-resolve/sidecar/resolve.mjs");
    if let Ok(mut sc) = Sidecar::spawn(&script) {
        let has_ts = sc.health().ok().and_then(|h| h.get("ts").cloned()).map(|v| !v.is_null()).unwrap_or(false);
        if has_ts {
            let t = Instant::now();
            let targets = sc.targets(&srcp, 10).unwrap_or_default();
            let prog = t.elapsed();
            let (mut n, mut tok, mut cross) = (0usize, 0usize, 0usize);
            let mut st = Duration::ZERO;
            for (f, s) in &targets {
                let t = Instant::now();
                if let Ok(defs) = sc.slice(&srcp, f, s, 1) {
                    st += t.elapsed();
                    n += 1;
                    tok += defs.iter().map(|d| d.text.len() / 4).sum::<usize>();
                    if defs.iter().any(|d| &d.file != f) {
                        cross += 1;
                    }
                }
            }
            println!("\n[4] RELEVANCE  (TS symbol slice, depth 1)");
            println!("    program     {:>7.1} s    (one-time, {} targets)", prog.as_secs_f64(), targets.len());
            if n > 0 {
                println!("    cross-file  {cross}/{n} targets resolved");
                let repo_mtok = raw_bytes as f64 / 4.0 / 1e6;
                println!("    slice       {:>7} tok  · {:.1} ms/slice   ({:.0}× smaller than the {:.1}M-tok repo)", tok / n, ms(st) / n as f64, repo_mtok * 1e6 / (tok / n).max(1) as f64, repo_mtok);
            }
        }
    }

    // [5] LIVE GRAPH
    if let Ok(mut g) = LiveGraph::open(&srcp, &script) {
        let t = Instant::now();
        let nf = g.build().unwrap_or(0);
        let gb = t.elapsed();
        println!("\n[5] LIVE GRAPH  (working-tree, incremental)");
        println!("    build       {:>7.0} ms   ({nf} files, {} edges)", ms(gb), g.edge_count());
        println!("    (one-file incremental refresh ≈13× cheaper — see live_bench)");
    }

    println!("\n╚═══ pipeline complete ═══");
    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&co_dir);
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn count_and_bytes(dir: &Path) -> (usize, u64) {
    let (mut n, mut b) = (0usize, 0u64);
    fn walk(dir: &Path, n: &mut usize, b: &mut u64) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name == "node_modules" || name == ".git" || name == "dist" {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                walk(&p, n, b);
            } else if let Ok(m) = e.metadata() {
                if m.is_file() {
                    *n += 1;
                    *b += m.len();
                }
            }
        }
    }
    walk(dir, &mut n, &mut b);
    (n, b)
}

fn disk_blocks(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(md) = std::fs::metadata(dir) {
        if md.is_file() {
            return md.blocks() * 512;
        }
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            total += disk_blocks(&e.path());
        }
    }
    total
}

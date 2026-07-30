//! Full-system benchmark: the entire native keel pipeline on a REAL repo.
//! Run: `cargo run --release -p keel-store --example system_bench [dir]`
//! Default dir = apps/forge/src.
//!
//! Stages: ingest (snapshot + atomic commit) → checkout + round-trip verify → GC →
//! relevance (TS symbol slice) → live dependency graph. Everything measured end to end.

use keel_graph::LiveGraph;
use keel_resolve::Sidecar;
use keel_store::{snapshot, Repo};
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
    let store_bytes = dir_size(&store_dir);
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

    // [4] RELEVANCE — TS symbol slice (sample size from arg 2, default 100)
    let sample: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../keel-resolve/sidecar/resolve.mjs");
    if let Ok(mut sc) = Sidecar::spawn(&script) {
        let has_ts = sc.health().ok().and_then(|h| h.get("ts").cloned()).map(|v| !v.is_null()).unwrap_or(false);
        if has_ts {
            let t = Instant::now();
            let targets = sc.targets(&srcp, sample).unwrap_or_default();
            let prog = t.elapsed();
            let (mut n, mut cross) = (0usize, 0usize);
            let mut toks: Vec<usize> = Vec::new();
            let mut lats: Vec<f64> = Vec::new();
            for (f, s) in &targets {
                let t = Instant::now();
                if let Ok(defs) = sc.slice(&srcp, f, s, 1) {
                    lats.push(ms(t.elapsed()));
                    n += 1;
                    toks.push(defs.iter().map(|d| d.text.len() / 4).sum::<usize>());
                    if defs.iter().any(|d| &d.file != f) {
                        cross += 1;
                    }
                }
            }
            println!("\n[4] RELEVANCE  (TS symbol slice, depth 1)  — sample n={n}");
            println!("    program     {:>7.1} s    (one-time; targets each have a cross-file callee)", prog.as_secs_f64());
            if n > 0 {
                let pct = 100.0 * cross as f64 / n as f64;
                println!("    cross-file  {cross}/{n} targets resolved ({pct:.0}%)");
                let (tmed, tp90) = (percentile_u(&toks, 50.0), percentile_u(&toks, 90.0));
                let tmean = toks.iter().sum::<usize>() as f64 / n as f64;
                println!("    slice tok   median {tmed}  · mean {tmean:.0}  · p90 {tp90}");
                println!("    latency ms  median {:.1} · p90 {:.1}", percentile_f(&lats, 50.0), percentile_f(&lats, 90.0));
                let repo_mtok = raw_bytes as f64 / 4.0 / 1e6;
                println!("    vs repo     {:.0}× smaller than the {:.1}M-tok tree (median slice)", repo_mtok * 1e6 / tmed.max(1) as f64, repo_mtok);
            }
        }
    }

    // [5] LIVE GRAPH
    if let Ok(mut sc) = Sidecar::spawn(&script) {
        let mut g = LiveGraph::new(&srcp);
        let t = Instant::now();
        let nf = g.build(&mut sc).unwrap_or(0);
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

fn percentile_u(v: &[usize], p: f64) -> usize {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    s[(((p / 100.0) * (s.len() - 1) as f64).round() as usize).min(s.len() - 1)]
}

fn percentile_f(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[(((p / 100.0) * (s.len() - 1) as f64).round() as usize).min(s.len() - 1)]
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

/// Sum of apparent file sizes under `dir`. NB: use apparent `len()`, not allocated blocks —
/// while the LMDB env is open its data file's block count reflects reserved/mmap'd pages
/// (the 64 GiB map), which wildly overstates the real footprint; `len()` is the true logical
/// size and matches `du` once the store is closed.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let Ok(md) = std::fs::metadata(e.path()) else { continue };
            if md.is_file() {
                total += md.len();
            } else if md.is_dir() {
                total += dir_size(&e.path());
            }
        }
    }
    total
}

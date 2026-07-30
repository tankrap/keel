//! Storage/VCS-core scale benchmark on a large REAL repository (target: the linux kernel).
//! Run: `cargo run --release -p keel-store --example scale_bench -- [dir]`
//!
//! Exercises only the language-agnostic core — snapshot / chunking / BLAKE3 / commit /
//! status / GC — so it runs on any tree regardless of language. It measures ingest
//! throughput, store size vs input (and vs `.git`), full-tree `status` latency, an
//! incremental re-commit after a handful of edits, and a GC pass. The point is to find the
//! slow spots at scale (e.g. snapshot builds every object in memory before one write).

use keel_store::Repo;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const IGNORED: &[&str] = &[".keel", "node_modules", ".git", "target", "dist", "build"];

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    let target = std::env::args().nth(1).unwrap_or_else(|| {
        let linux = "/Users/justin/keel-scale/linux";
        if Path::new(linux).exists() { linux.into() } else { "/Users/justin/agent-native-forge-temp".into() }
    });
    let target = PathBuf::from(target);
    if !target.exists() {
        eprintln!("target does not exist: {}", target.display());
        std::process::exit(1);
    }
    let store_dir = std::env::temp_dir().join(format!("keel-scale-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store_dir);

    println!("╔═══ keel storage scale bench ═══");
    println!("target: {}", target.display());

    // [1] INPUT
    let (in_files, in_bytes) = walk_size(&target);
    println!("\n[1] INPUT");
    println!("    files       {in_files}");
    println!("    bytes       {:.2} GB", in_bytes as f64 / 1e9);

    let repo = match Repo::open(&store_dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("open store: {e}");
            std::process::exit(1);
        }
    };

    // [2] INGEST — full snapshot + commit of the whole tree
    println!("\n[2] INGEST  (snapshot + commit, whole tree)");
    print!("    committing ... ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let t = Instant::now();
    let commit = match repo.commit_dir(&target, "scale ingest", "bench", 1, None) {
        Ok(c) => c,
        Err(e) => {
            println!("\n    INGEST FAILED: {e}\n    (this is itself a finding — likely a resource wall at scale)");
            let _ = std::fs::remove_dir_all(&store_dir);
            std::process::exit(1);
        }
    };
    let ingest = t.elapsed();
    let objs = repo.store().object_count().unwrap_or(0);
    let chunks = repo.store().chunk_count().unwrap_or(0);
    println!("done");
    println!("    wall        {:.1} s", ingest.as_secs_f64());
    println!("    throughput  {:.0} files/s · {:.1} MB/s", in_files as f64 / ingest.as_secs_f64(), (in_bytes as f64 / 1e6) / ingest.as_secs_f64());
    println!("    objects     {objs}   chunks {chunks}");

    // [3] STORE SIZE vs input vs .git
    let (_, store_bytes) = walk_size_raw(&store_dir);
    println!("\n[3] STORE SIZE");
    println!("    keel store  {:.2} GB   ({:.1}% of input)", store_bytes as f64 / 1e9, 100.0 * store_bytes as f64 / in_bytes.max(1) as f64);
    let git = target.join(".git");
    if git.exists() {
        let (_, git_bytes) = walk_size_raw(&git);
        println!("    .git        {:.2} GB   (for reference; not apples-to-apples — git here is a shallow/partial clone)", git_bytes as f64 / 1e9);
    }

    // [4] STATUS (clean) — full-tree diff right after commit; median of 5 warm runs
    println!("\n[4] STATUS  (full-tree diff vs HEAD, should be ~clean)");
    let mut st = 0usize;
    let mut lats: Vec<f64> = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        st = repo.status(&target).map(|v| v.len()).unwrap_or(usize::MAX);
        lats.push(ms(t.elapsed()));
    }
    lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("    latency     {:.0} ms   (median of 5; {st} paths differ)", lats[2]);

    // [5] INCREMENTAL — modify ~50 files, re-commit, restore
    println!("\n[5] INCREMENTAL  (edit 50 files, re-commit)");
    let victims = pick_files(&target, 50);
    let saved: Vec<(PathBuf, Vec<u8>)> =
        victims.iter().filter_map(|p| std::fs::read(p).ok().map(|b| (p.clone(), b))).collect();
    for (p, orig) in &saved {
        let mut v = orig.clone();
        v.extend_from_slice(b"\n// keel scale bench touch\n");
        let _ = std::fs::write(p, v);
    }
    let changed = repo.status(&target).map(|v| v.len()).unwrap_or(0);
    let t = Instant::now();
    let _ = repo.commit_dir(&target, "scale incremental", "bench", 2, None);
    let inc = t.elapsed();
    // restore originals so the target tree is left pristine
    for (p, orig) in &saved {
        let _ = std::fs::write(p, orig);
    }
    println!("    edited      {} files ({changed} seen by status)", saved.len());
    println!("    re-commit   {:.0} ms   ({:.0} files/s over the whole tree)", ms(inc), in_files as f64 / inc.as_secs_f64());
    let objs2 = repo.store().object_count().unwrap_or(0);
    println!("    new objects {} (added by the 50-file change)", objs2.saturating_sub(objs));

    // [6] GC
    println!("\n[6] GC  (mark+sweep from refs)");
    let t = Instant::now();
    match repo.store().gc() {
        Ok(g) => println!("    latency     {:.0} ms   kept {}o/{}c · removed {}o/{}c", ms(t.elapsed()), g.objects_kept, g.chunks_kept, g.objects_removed, g.chunks_removed),
        Err(e) => println!("    GC error: {e}"),
    }

    println!("\n╚═══ done · commit {} ═══", &commit.to_hex()[..12]);
    let _ = std::fs::remove_dir_all(&store_dir);
}

/// Walk `dir`, honoring keel's snapshot ignore set; returns (file count, total bytes).
fn walk_size(dir: &Path) -> (usize, u64) {
    fn go(dir: &Path, files: &mut usize, bytes: &mut u64) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if IGNORED.contains(&name.as_ref()) {
                continue;
            }
            let p = e.path();
            match e.file_type() {
                Ok(ft) if ft.is_dir() => go(&p, files, bytes),
                Ok(ft) if ft.is_file() => {
                    *files += 1;
                    *bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
                _ => {}
            }
        }
    }
    let (mut f, mut b) = (0, 0);
    go(dir, &mut f, &mut b);
    (f, b)
}

/// Walk `dir` counting everything (no ignore) — for measuring the store / `.git` dirs.
fn walk_size_raw(dir: &Path) -> (usize, u64) {
    fn go(dir: &Path, files: &mut usize, bytes: &mut u64) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(ft) if ft.is_dir() => go(&p, files, bytes),
                Ok(ft) if ft.is_file() => {
                    *files += 1;
                    *bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
                _ => {}
            }
        }
    }
    let (mut f, mut b) = (0, 0);
    go(dir, &mut f, &mut b);
    (f, b)
}

/// First `n` regular non-ignored files under `dir` (deterministic-ish walk order).
fn pick_files(dir: &Path, n: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn go(dir: &Path, n: usize, out: &mut Vec<PathBuf>) {
        if out.len() >= n {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if out.len() >= n {
                return;
            }
            let name = e.file_name();
            if IGNORED.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            let p = e.path();
            match e.file_type() {
                Ok(ft) if ft.is_dir() => go(&p, n, out),
                Ok(ft) if ft.is_file() => out.push(p),
                _ => {}
            }
        }
    }
    go(dir, n, &mut out);
    out
}

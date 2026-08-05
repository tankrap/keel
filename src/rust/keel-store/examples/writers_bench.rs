//! Many-concurrent-writers ceiling + per-repo-sharding experiment (Linear NEW-1107, red-team #9).
//!
//! LMDB is single-writer: every `store.put` / commit funnels through one `env.write_txn()` lock, so
//! a fleet all committing to the SAME repo serializes no matter how many agents there are (a daemon
//! serializing the write path just relocates the same bottleneck). This measures the real ceiling —
//! write-txn throughput vs writer count — and the proposed mitigation: **one LMDB env per repo**, so
//! writers run in parallel ACROSS repos while staying serialized WITHIN one (which the change model
//! does anyway).
//!
//! Two workloads at each writer count N:
//!   A. shared   — N threads hammering ONE env (the funnel: exposes the single-writer ceiling)
//!   B. sharded  — N threads each on its OWN env (the mitigation: should scale ~linearly to cores)
//!
//! Each op is a `put` of a UNIQUE blob (unique content ⇒ a real write, never a dedup no-op), which
//! is exactly one write transaction — the atom that serializes. A real commit is a small constant
//! number of these (object put_many + a ref advance), so commits/s ≈ txns/s ÷ ~2.
//!
//! Run: `cargo run --release -p keel-store --example writers_bench -- [ops_per_writer] [payload_bytes]`

use keel_store::object::Object;
use keel_store::store::Store;
use std::sync::{Arc, Barrier};
use std::time::Instant;

// A per-env map big enough for the bench's writes (a few MB per env) but small enough that the
// sharded case — N envs open at once — doesn't over-reserve virtual address space. macOS tripped
// EINVAL from mmap once the aggregate reservation grew large (N × 512 MiB at N=11); 64 MiB × cores
// stays comfortably within limits and still dwarfs the bench's working set.
const BENCH_MAP_SIZE: usize = 64 * 1024 * 1024;

/// One writer's payload for op `i`: unique content (thread tag + counter), padded to `payload` bytes
/// so each put is a genuine, similarly-sized write rather than a dedup short-circuit.
fn blob(tag: usize, i: usize, payload: usize) -> Object {
    let mut v = format!("keel-writers-bench t{tag} op{i} ").into_bytes();
    v.resize(v.len().max(payload), b'.');
    Object::Blob(v)
}

/// Run `writers` threads, each doing `ops` puts, against stores produced by `make_store(thread_idx)`.
/// A barrier aligns the start so we time steady-state contention, not thread spawn. Returns wall secs,
/// or `None` if any store failed to open (e.g. the OS per-process mmap/fd limit on many envs) or any
/// writer errored — so the sweep prints a gap for that row instead of aborting.
fn run(writers: usize, ops: usize, payload: usize, make_store: impl Fn(usize) -> Option<Arc<Store>>) -> Option<f64> {
    let stores: Vec<Arc<Store>> = (0..writers).map(&make_store).collect::<Option<_>>()?;
    let barrier = Arc::new(Barrier::new(writers + 1));
    let mut handles = Vec::with_capacity(writers);
    for (tag, store) in stores.into_iter().enumerate() {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || -> Result<(), String> {
            barrier.wait(); // all writers poised before the clock starts
            for i in 0..ops {
                store.put(&blob(tag, i, payload)).map_err(|e| e.to_string())?;
            }
            Ok(())
        }));
    }
    barrier.wait();
    let t = Instant::now();
    let mut ok = true;
    for h in handles {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("    (writer error: {e})");
                ok = false;
            }
            Err(_) => ok = false,
        }
    }
    ok.then(|| t.elapsed().as_secs_f64())
}

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("keel-writers-{}-{name}", std::process::id()))
}

/// Open a fresh store, or `None` if it can't be opened — the sharded case opens N envs at once and
/// the OS caps how many a process may map/hold, so a failure here is data (it bounds sharding), not a
/// reason to abort the sweep.
fn open(path: &std::path::Path) -> Option<Arc<Store>> {
    let _ = std::fs::remove_dir_all(path);
    Store::open_with_map_size(path, BENCH_MAP_SIZE).ok().map(Arc::new)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let ops: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(4000);
    let payload: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(128);

    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    // Sweep 1 → cores (rounded to powers of two, plus the exact core count).
    let mut counts: Vec<usize> = Vec::new();
    let mut n = 1;
    while n < cores {
        counts.push(n);
        n *= 2;
    }
    if *counts.last().unwrap_or(&0) != cores {
        counts.push(cores);
    }

    println!("╔═══ keel LMDB concurrent-writers ceiling (NEW-1107) ═══");
    println!("    cores {cores} · {ops} puts/writer · {payload}-byte blobs · unique content (real writes)");
    println!("    A shared  = N writers → ONE env   (the single-writer funnel)");
    println!("    B sharded = N writers → N envs    (one env per repo — the mitigation)");
    println!();
    println!(
        "    {:>7} │ {:>14} │ {:>14} │ {:>9} │ {:>16}",
        "writers", "shared txns/s", "sharded txns/s", "shard×", "shared vs 1-wtr"
    );
    println!("    ────────┼────────────────┼────────────────┼───────────┼──────────────────");

    let mut shared_one = 0.0f64;
    for (idx, &w) in counts.iter().enumerate() {
        let total = (w * ops) as f64;

        // A: N writers → one shared env (all threads get a clone of the same store).
        let shared_store = open(&tmp("shared"));
        let shared_tps = shared_store
            .as_ref()
            .and_then(|s| run(w, ops, payload, |_| Some(Arc::clone(s))))
            .map(|secs| total / secs);

        // B: N writers → one env per writer (the sharding mitigation).
        let sharded_tps = run(w, ops, payload, |t| open(&tmp(&format!("shard{t}")))).map(|s| total / s);

        if idx == 0 {
            if let Some(t) = shared_tps {
                shared_one = t;
            }
        }
        let cell = |v: Option<f64>| v.map(|x| format!("{x:>14.0}")).unwrap_or_else(|| format!("{:>14}", "—"));
        let shard_x = match (shared_tps, sharded_tps) {
            (Some(a), Some(b)) => format!("{:>8.2}×", b / a),
            _ => format!("{:>9}", "—"),
        };
        let vs_one = match shared_tps {
            Some(a) if shared_one > 0.0 => format!("{:>14.2}×", a / shared_one),
            _ => format!("{:>15}", "—"),
        };
        println!("    {w:>7} │ {} │ {} │ {shard_x} │ {vs_one}", cell(shared_tps), cell(sharded_tps));
    }

    // clean up bench stores
    let _ = std::fs::remove_dir_all(tmp("shared"));
    for t in 0..cores {
        let _ = std::fs::remove_dir_all(tmp(&format!("shard{t}")));
    }

    println!("    ────────┴────────────────┴────────────────┴───────────┴──────────────────");
    println!();
    println!("    Reading it (keel fuses a change's objects + ref advance into ONE write txn via");
    println!("    apply/apply_cas, so commits/s ≈ txns/s — not ÷2):");
    println!("    · LMDB is single-writer: one env holds the writer lock through the WHOLE txn —");
    println!("      hash + deflate + put + the commit fsync — so writers to the 'shared' env fully");
    println!("      serialize. That column is a single repo's commit ceiling; LMDB has no group");
    println!("      commit, so for a durable workload 'shared vs 1-wtr' should sit near 1.0.");
    println!("    · DURABILITY CAVEAT: on macOS/APFS, fsync does not force stable storage (that needs");
    println!("      F_FULLFSYNC, which LMDB doesn't issue), so absolute txns/s here are inflated by the");
    println!("      page cache and 'shared vs 1-wtr' can drift above 1.0. Read the SHAPE, not the rate;");
    println!("      for a true durable ceiling, measure on a filesystem where fsync is honored.");
    println!("    · 'shard×' > 1 ⇒ one env per repo = independent writer locks = real cross-repo");
    println!("      parallelism — the lever if one host must exceed a single repo's serialized rate.");
    println!("      A '—' sharded row is the OS per-process mmap/fd limit on many simultaneous envs, so");
    println!("      many-repo sharding needs an LRU env cache, not one permanently-open env per repo.");
    println!("╚═══ done ═══");
}

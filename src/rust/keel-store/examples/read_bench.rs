//! Read/reconstruct latency micro-benchmark. Opens an existing store and times `get()` over
//! every stored object (delta-form blobs are reconstructed + BLAKE3-verified on the way out),
//! isolating reconstruction cost from any git/checkout overhead.
//!
//! Usage: cargo run --release --example read_bench -- <store-dir> [iterations]

use keel_store::store::Store;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: read_bench <store-dir> [iterations]");
    let iters: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);

    let store = Store::open(std::path::Path::new(&dir)).expect("open store");
    let ids = store.content_ids().expect("list ids");
    let deltas = store.delta_count().expect("delta count");
    println!("{} objects ({deltas} in delta form), {} iterations", ids.len(), iters);

    // warm + correctness: every object must reconstruct (a bad delta would panic here)
    for id in &ids {
        assert!(store.get(id).expect("get").is_some(), "object vanished");
    }

    let mut samples = Vec::new();
    for _ in 0..iters {
        let t = Instant::now();
        for id in &ids {
            std::hint::black_box(store.get(id).expect("get"));
        }
        samples.push(t.elapsed().as_secs_f64());
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    let per = median / ids.len() as f64 * 1e6; // µs per object
    println!(
        "median full sweep: {:.3}s  ·  {:.1} µs/object  ·  {:.0} objects/sec",
        median,
        per,
        ids.len() as f64 / median
    );
}

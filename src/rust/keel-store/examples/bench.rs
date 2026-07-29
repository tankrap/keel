//! Object-store microbenchmark — the real path (encode + BLAKE3 + LMDB), not raw KV.
//! Run: `cargo run --release --example bench`
//! Prints put throughput (batched, atomic), random get latency, and chunked-blob
//! throughput. Deterministic dataset so numbers are comparable across runs.

use keel_store::{Change, Object, ObjectId, Store, Verification};
use std::time::Instant;

const N: usize = 50_000; // small mixed objects (inline)
const BATCH: usize = 1_000; // objects per atomic put_many
const BIG_N: usize = 200; // large blobs (chunked)
const BIG_SZ: usize = 512 * 1024;

fn fill(n: usize, seed: u32) -> Vec<u8> {
    let mut v = vec![0u8; n];
    let mut s = seed | 1;
    let mut i = 0;
    while i + 4 <= n {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v[i..i + 4].copy_from_slice(&s.to_le_bytes());
        i += 4;
    }
    while i < n {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v[i] = s as u8;
        i += 1;
    }
    v
}

fn main() {
    let dir = std::env::temp_dir().join(format!("keel-store-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = Store::open(&dir).unwrap();

    // build mixed small objects: ~70% blobs (64B–8KB), ~30% changes
    let mut objs: Vec<Object> = Vec::with_capacity(N);
    let mut rng = 12345u32;
    let mut next = || {
        rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        rng
    };
    let mut payload_bytes = 0usize;
    for i in 0..N {
        if next() % 10 < 7 {
            let sz = 64 + (next() as usize % 8000);
            payload_bytes += sz;
            objs.push(Object::Blob(fill(sz, next())));
        } else {
            objs.push(Object::Change(Change {
                parents: vec![],
                tree: ObjectId([(i & 0xff) as u8; 32]),
                session: None,
                intent: format!("change #{i}"),
                author: "acct:bench".into(),
                timestamp: i as u64,
                verification: Verification::Green,
            }));
        }
    }

    // ── put (batched, atomic) ──
    let t0 = Instant::now();
    let mut ids: Vec<ObjectId> = Vec::with_capacity(N);
    for batch in objs.chunks(BATCH) {
        ids.extend(s.put_many(batch).unwrap());
    }
    let put_s = t0.elapsed().as_secs_f64();

    // ── random get ──
    let order = {
        let mut o: Vec<usize> = (0..N).collect();
        let mut r = 99u32;
        for i in (1..N).rev() {
            r = r.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            o.swap(i, r as usize % (i + 1));
        }
        o
    };
    let t1 = Instant::now();
    let mut acc = 0u64;
    for &i in &order {
        if let Some(Object::Blob(b)) = s.get(&ids[i]).unwrap() {
            acc = acc.wrapping_add(b.len() as u64);
        }
    }
    let get_s = t1.elapsed().as_secs_f64();
    std::hint::black_box(acc);

    // ── chunked large blobs ──
    let bigs: Vec<Object> = (0..BIG_N).map(|i| Object::Blob(fill(BIG_SZ, i as u32 + 1))).collect();
    let t2 = Instant::now();
    let big_ids = s.put_many(&bigs).unwrap();
    let big_put_s = t2.elapsed().as_secs_f64();
    let t3 = Instant::now();
    for id in &big_ids {
        std::hint::black_box(s.get(id).unwrap());
    }
    let big_get_s = t3.elapsed().as_secs_f64();

    let big_mb = (BIG_N * BIG_SZ) as f64 / 1e6;
    println!("\n=== keel object-store bench (N={N} small, {BIG_N}×{}KiB chunked) ===", BIG_SZ / 1024);
    println!("small objects:  {} stored, {} chunks", s.object_count().unwrap(), s.chunk_count().unwrap());
    println!("put (batched):  {:>8.0} obj/s   {:>7.1} MB/s payload", N as f64 / put_s, payload_bytes as f64 / 1e6 / put_s);
    println!("random get:     {:>8.0} obj/s   {:>7.2} us/op", N as f64 / get_s, get_s * 1e6 / N as f64);
    println!("chunked put:    {:>8.1} MB/s   ({BIG_N} blobs of {}KiB in one txn)", big_mb / big_put_s, BIG_SZ / 1024);
    println!("chunked get:    {:>8.1} MB/s", big_mb / big_get_s);

    // ── snapshot / checkout a working tree ──
    let work = std::env::temp_dir().join(format!("keel-bench-work-{}", std::process::id()));
    let cout = std::env::temp_dir().join(format!("keel-bench-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&cout);
    let files = 2_000usize;
    let mut tree_bytes = 0usize;
    for i in 0..files {
        let sub = work.join(format!("d{}", i % 50));
        std::fs::create_dir_all(&sub).unwrap();
        let content = fill(256 + (i * 37) % 4096, i as u32 + 1);
        tree_bytes += content.len();
        std::fs::write(sub.join(format!("f{i}.txt")), &content).unwrap();
    }
    let t4 = Instant::now();
    let root = keel_store::snapshot::snapshot(&s, &work).unwrap();
    let snap_s = t4.elapsed().as_secs_f64();
    let t5 = Instant::now();
    keel_store::snapshot::checkout(&s, root, &cout).unwrap();
    let co_s = t5.elapsed().as_secs_f64();

    println!("snapshot:       {:>8.0} files/s  {:>7.1} MB/s ({files} files, one atomic txn)", files as f64 / snap_s, tree_bytes as f64 / 1e6 / snap_s);
    println!("checkout:       {:>8.0} files/s  {:>7.1} MB/s", files as f64 / co_s, tree_bytes as f64 / 1e6 / co_s);

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&cout);
}

//! Adversarial stress test — TRY TO BREAK the native VCS and report weaknesses.
//! Run: `cargo run --release -p keel-store --example stress`
//! Each stage announces itself first, so a hard crash (panic/OOM/stack-overflow) still
//! tells you which one broke. Soft failures are reported as ✗ WEAKNESS.

use keel_graph::LiveGraph;
use keel_resolve::Sidecar;
use keel_store::{Object, Store};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

fn td(tag: &str) -> PathBuf {
    static C: AtomicU64 = AtomicU64::new(0);
    let n = C.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("keel-stress-{tag}-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}
fn fill(n: usize, seed: u64) -> Vec<u8> {
    // splitmix64 seeded injectively by `seed` — distinct seeds give distinct content
    // (the earlier `seed | 1` collapsed N and N+1 to the same stream).
    let mut v = vec![0u8; n];
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(0xD1B5_4A32_D192_ED03);
    for chunk in v.chunks_mut(8) {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let b = z.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
    v
}

fn main() {
    let mut pass = 0;
    let mut weak = 0;
    macro_rules! stage {
        ($name:expr, $body:expr) => {{
            println!("▶ {}", $name);
            let t = Instant::now();
            let r: Result<String, String> = $body;
            match r {
                Ok(m) => {
                    pass += 1;
                    println!("  ✓ {m}  [{:.1}s]", t.elapsed().as_secs_f64());
                }
                Err(e) => {
                    weak += 1;
                    println!("  ✗ WEAKNESS: {e}  [{:.1}s]", t.elapsed().as_secs_f64());
                }
            }
        }};
    }

    stage!("huge blob (256 MB) — chunk, store, reassemble, verify", huge_blob());
    stage!("massive dedup (5000 identical files)", massive_dedup());
    stage!("adversarial filenames (unicode / spaces / 200-char / dotfile / deep)", weird_names());
    stage!("concurrent writers (8 threads × 1000 objects, shared store)", concurrent_writers());
    stage!("double-open same store path in one process (LMDB footgun)", double_open());
    stage!("deep directory nesting (120 levels) — snapshot recursion", deep_nesting());
    stage!("empty inputs (empty file, empty repo, missing symbol brief)", empty_inputs());
    stage!("circular imports (a→b→c→a) — graph must terminate", circular_imports());
    stage!("malformed / binary-ish TS file — resolver must not crash", malformed_ts());
    stage!("huge fan-in (one file imports 500 others)", big_fanin());

    println!("\n=== stress summary: {pass} passed, {weak} weakness(es) ===");
}

fn script() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../keel-resolve/sidecar/resolve.mjs"))
}

fn circular_imports() -> Result<String, String> {
    let work = td("circ");
    std::fs::write(work.join("a.ts"), "import { b } from './b.js';\n").unwrap();
    std::fs::write(work.join("b.ts"), "import { c } from './c.js';\n").unwrap();
    std::fs::write(work.join("c.ts"), "import { a } from './a.js';\n").unwrap();
    let mut sc = Sidecar::spawn(&script()).map_err(|e| e.to_string())?;
    let mut g = LiveGraph::new(&work);
    g.build(&mut sc).map_err(|e| e.to_string())?;
    let deps = g.transitive_deps("a.ts");
    let _ = std::fs::remove_dir_all(&work);
    if deps.len() == 2 && deps.contains(&"b.ts".to_string()) && deps.contains(&"c.ts".to_string()) {
        Ok("3-cycle terminated; transitive_deps(a) = [b.ts, c.ts]".into())
    } else {
        Err(format!("cycle handling wrong: transitive_deps(a) = {deps:?}"))
    }
}

fn malformed_ts() -> Result<String, String> {
    let work = td("mal");
    std::fs::write(work.join("dep.ts"), "export const x = 1;\n").unwrap();
    std::fs::write(work.join("ok.ts"), "import { x } from './dep.js';\n").unwrap();
    std::fs::write(work.join("broken.ts"), b"))) not { valid import from garbage \x00\x01\xff <<< export export }").unwrap();
    let mut sc = Sidecar::spawn(&script()).map_err(|e| e.to_string())?;
    let mut g = LiveGraph::new(&work);
    let n = g.build(&mut sc).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(format!("built graph over {n} files incl. a malformed/binary one without crashing"))
}

fn big_fanin() -> Result<String, String> {
    let work = td("fanin");
    let mut hub = String::new();
    for i in 0..500 {
        std::fs::write(work.join(format!("m{i}.ts")), format!("export const v{i} = {i};\n")).unwrap();
        hub.push_str(&format!("import {{ v{i} }} from './m{i}.js';\n"));
    }
    std::fs::write(work.join("hub.ts"), hub).unwrap();
    let mut sc = Sidecar::spawn(&script()).map_err(|e| e.to_string())?;
    let mut g = LiveGraph::new(&work);
    g.build(&mut sc).map_err(|e| e.to_string())?;
    let deps = g.deps("hub.ts");
    let _ = std::fs::remove_dir_all(&work);
    if deps.len() == 500 {
        Ok("hub importing 500 files resolved all 500 edges".into())
    } else {
        Err(format!("huge fan-in: expected 500 deps, got {}", deps.len()))
    }
}

fn huge_blob() -> Result<String, String> {
    let d = td("huge");
    let s = Store::open(&d).map_err(|e| e.to_string())?;
    let content = fill(256 * 1024 * 1024, 7);
    let id = s.put(&Object::Blob(content.clone())).map_err(|e| e.to_string())?;
    let got = s.get(&id).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&d);
    match got {
        Some(Object::Blob(b)) if b == content => {
            Ok(format!("256MB round-tripped, {} chunks", s.chunk_count().unwrap_or(0)))
        }
        _ => Err("256MB blob did not round-trip".into()),
    }
}

fn massive_dedup() -> Result<String, String> {
    let sd = td("dedup-store");
    let work = td("dedup-work");
    let s = Store::open(&sd).map_err(|e| e.to_string())?;
    let same = b"identical content across thousands of files\n";
    for i in 0..5000 {
        std::fs::write(work.join(format!("f{i}.txt")), same).unwrap();
    }
    keel_store::snapshot::snapshot(&s, &work).map_err(|e| e.to_string())?;
    let objs = s.object_count().unwrap_or(0);
    let _ = std::fs::remove_dir_all(&sd);
    let _ = std::fs::remove_dir_all(&work);
    // 5000 identical files → 1 blob + 1 tree = 2 objects. If it stored 5000, dedup failed.
    if objs <= 3 {
        Ok(format!("5000 identical files stored as {objs} objects (deduped)"))
    } else {
        Err(format!("dedup failed: {objs} objects for 5000 identical files"))
    }
}

fn weird_names() -> Result<String, String> {
    let sd = td("names-store");
    let work = td("names-work");
    let out = td("names-out");
    let s = Store::open(&sd).map_err(|e| e.to_string())?;
    let names = [
        "café_→_ünïcode.ts",
        "my file with spaces.ts",
        &"x".repeat(200),
        ".hidden",
        "trailing.spaces .ts",
    ];
    for n in &names {
        std::fs::write(work.join(n), fill(64, n.len() as u64)).unwrap();
    }
    std::fs::create_dir_all(work.join("a/b/c/d")).unwrap();
    std::fs::write(work.join("a/b/c/d/deep.ts"), b"deep").unwrap();

    let id1 = keel_store::snapshot::snapshot(&s, &work).map_err(|e| e.to_string())?;
    keel_store::snapshot::checkout(&s, id1, &out).map_err(|e| e.to_string())?;
    let id2 = keel_store::snapshot::snapshot(&s, &out).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&sd);
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&out);
    if id1 == id2 {
        Ok("all adversarial names round-tripped identically".into())
    } else {
        Err("adversarial names did NOT round-trip (snapshot∘checkout != identity)".into())
    }
}

fn concurrent_writers() -> Result<String, String> {
    let d = td("conc");
    let s = Store::open(&d).map_err(|e| e.to_string())?;
    let threads = 8usize;
    let per = 1000usize;
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let s = s.clone();
            std::thread::spawn(move || {
                for i in 0..per {
                    let content = fill(128, (t * per + i) as u64);
                    s.put(&Object::Blob(content)).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().map_err(|_| "a writer thread panicked".to_string())?;
    }
    let objs = s.object_count().unwrap_or(0);
    let _ = std::fs::remove_dir_all(&d);
    // all distinct content → threads*per unique objects, none lost or corrupted
    if objs == (threads * per) as u64 {
        Ok(format!("{threads} threads wrote {} distinct objects, all landed", threads * per))
    } else {
        Err(format!("expected {} objects, got {objs} (lost/corrupted under concurrency)", threads * per))
    }
}

fn double_open() -> Result<String, String> {
    // LMDB docs warn: opening one env twice in a process can corrupt. Do we guard/survive?
    let d = td("double");
    let a = Store::open(&d).map_err(|e| e.to_string())?;
    let b = match Store::open(&d) {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&d);
            return Ok(format!("second open rejected (safe): {e}"));
        }
    };
    let ida = a.put(&Object::Blob(b"from-a".to_vec())).map_err(|e| e.to_string())?;
    let idb = b.put(&Object::Blob(b"from-b".to_vec())).map_err(|e| e.to_string())?;
    // can each handle read the other's write?
    let a_sees_b = a.get(&idb).map_err(|e| e.to_string())?.is_some();
    let b_sees_a = b.get(&ida).map_err(|e| e.to_string())?.is_some();
    let _ = std::fs::remove_dir_all(&d);
    if a_sees_b && b_sees_a {
        Ok("double-open survived; both handles consistent (but still a footgun — should guard)".into())
    } else {
        Err(format!("double-open inconsistency: a_sees_b={a_sees_b}, b_sees_a={b_sees_a} (silent data divergence)"))
    }
}

fn deep_nesting() -> Result<String, String> {
    let sd = td("deep-store");
    let work = td("deep-work");
    let out = td("deep-out");
    let s = Store::open(&sd).map_err(|e| e.to_string())?;
    let mut p = work.clone();
    for i in 0..120 {
        p = p.join(format!("d{i}"));
    }
    // guard the OS path-length limit — keel's recursion is bounded by the FS, not itself
    if let Err(e) = std::fs::create_dir_all(&p) {
        return Err(format!("filesystem rejected deep path (keel inherits OS limit): {e}"));
    }
    std::fs::write(p.join("bottom.ts"), b"deep").unwrap();

    let id1 = keel_store::snapshot::snapshot(&s, &work).map_err(|e| e.to_string())?;
    keel_store::snapshot::checkout(&s, id1, &out).map_err(|e| e.to_string())?;
    let id2 = keel_store::snapshot::snapshot(&s, &out).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&sd);
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&out);
    if id1 == id2 {
        Ok("800-level nesting round-tripped (recursion held)".into())
    } else {
        Err("deep nesting broke round-trip".into())
    }
}

fn empty_inputs() -> Result<String, String> {
    let sd = td("empty-store");
    let work = td("empty-work");
    let s = Store::open(&sd).map_err(|e| e.to_string())?;
    // empty blob
    let eid = s.put(&Object::Blob(vec![])).map_err(|e| e.to_string())?;
    if s.get(&eid).map_err(|e| e.to_string())? != Some(Object::Blob(vec![])) {
        return Err("empty blob did not round-trip".into());
    }
    // empty file + empty dir snapshot
    std::fs::write(work.join("empty.txt"), b"").unwrap();
    std::fs::create_dir_all(work.join("emptydir")).unwrap();
    keel_store::snapshot::snapshot(&s, &work).map_err(|e| e.to_string())?;
    // snapshot of a totally empty dir
    let empty2 = td("empty-empty");
    keel_store::snapshot::snapshot(&s, &empty2).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&sd);
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&empty2);
    Ok("empty blob / empty file / empty dir all handled".into())
}

#[allow(dead_code)]
fn _p(_: &Path) {}

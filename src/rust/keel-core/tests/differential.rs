//! Differential test: keel-core (Rust) MUST agree byte-for-byte with the Node
//! reference (keel-server/src/store.mjs) on hashes and chunk boundaries. If it
//! doesn't, dedup and signature verification break silently across peers.
//!
//! Skips gracefully if node or the reference module isn't reachable, so it
//! never blocks a build where the JS tree is absent.

use keel_core::{chunk_ranges, hash_hex};
use std::process::Command;

fn node_ref() -> Option<String> {
    // reference module path relative to this crate
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../keel-server/src/store.mjs");
    if !std::path::Path::new(p).exists() {
        return None;
    }
    Some(p.to_string())
}

/// Deterministic bytes shared by both sides (same LCG).
fn seeded(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    let mut v = vec![0u8; n];
    for b in v.iter_mut() {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        *b = (s & 0xff) as u8;
    }
    v
}

#[test]
fn hashes_and_boundaries_match_node() {
    let module = match node_ref() {
        Some(m) => m,
        None => {
            eprintln!("skip: keel-server/src/store.mjs not present");
            return;
        }
    };

    for &(n, seed) in &[(1usize, 1u32), (100, 2), (5000, 3), (2 * 1024 * 1024, 7)] {
        let buf = seeded(n, seed);

        // Rust side
        let rust_hash = hash_hex(&buf);
        let rust_bounds: Vec<usize> = chunk_ranges(&buf).iter().map(|(_, e)| *e).collect();
        let rust_chunk_hashes: Vec<String> =
            chunk_ranges(&buf).iter().map(|(a, b)| hash_hex(&buf[*a..*b])).collect();

        // Node side: same seeded buffer, print hash + boundaries + chunk hashes
        let js = format!(
            r#"
            import {{ hashBuf, chunk }} from "{module}";
            function seeded(n, seed) {{ const b=Buffer.alloc(n); let s=seed>>>0; for(let i=0;i<n;i++){{s=(Math.imul(s,1664525)+1013904223)>>>0; b[i]=s&0xff;}} return b; }}
            const buf = seeded({n}, {seed});
            const cs = chunk(buf);
            let off=0; const bounds=[]; const chashes=[];
            for (const c of cs) {{ off+=c.length; bounds.push(off); chashes.push(hashBuf(c)); }}
            process.stdout.write(JSON.stringify({{ hash: hashBuf(buf), bounds, chashes }}));
            "#,
            module = module, n = n, seed = seed
        );
        let out = Command::new("node").arg("--input-type=module").arg("-e").arg(&js).output().expect("run node");
        assert!(out.status.success(), "node failed: {}", String::from_utf8_lossy(&out.stderr));
        let v = serde_lite(&out.stdout);

        assert_eq!(rust_hash, v.hash, "whole-blob hash mismatch at n={n}");
        assert_eq!(rust_bounds, v.bounds, "chunk boundary mismatch at n={n}");
        assert_eq!(rust_chunk_hashes, v.chashes, "chunk-hash mismatch at n={n}");
    }
}

// tiny hand-rolled JSON reader for exactly the shape we emit (no serde dep)
struct Ref {
    hash: String,
    bounds: Vec<usize>,
    chashes: Vec<String>,
}
#[allow(non_snake_case)]
fn serde_lite(bytes: &[u8]) -> Ref {
    let s = String::from_utf8_lossy(bytes);
    let grab_str = |key: &str| -> String {
        let k = format!("\"{}\":\"", key);
        let i = s.find(&k).unwrap() + k.len();
        let j = s[i..].find('"').unwrap();
        s[i..i + j].to_string()
    };
    let grab_arr = |key: &str| -> String {
        let k = format!("\"{}\":[", key);
        let i = s.find(&k).unwrap() + k.len();
        let j = s[i..].find(']').unwrap();
        s[i..i + j].to_string()
    };
    let bounds = grab_arr("bounds")
        .split(',')
        .filter(|t| !t.is_empty())
        .map(|t| t.trim().parse().unwrap())
        .collect();
    let chashes = grab_arr("chashes")
        .split(',')
        .filter(|t| !t.is_empty())
        .map(|t| t.trim().trim_matches('"').to_string())
        .collect();
    Ref { hash: grab_str("hash"), bounds, chashes }
}

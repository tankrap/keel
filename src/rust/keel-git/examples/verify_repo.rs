//! Verify the git object codec against EVERY object in a real repository.
//!
//! Pipe git's whole object database in:
//!   git -C <repo> cat-file --batch-all-objects --batch --buffer | \
//!     cargo run --release --example verify_repo -p keel-git
//!
//! For every object it checks (a) keel's SHA-1 identity matches git's oid, and (b) for
//! tree/commit/tag, parse→serialize reproduces the payload byte-for-byte. Any mismatch is a
//! codec bug; it prints the offending oid and exits non-zero.

use keel_git::{hash, parse_tree, serialize_headed, serialize_tree, Headed, Kind, Oid};
use std::io::{self, Read};

fn main() {
    let mut buf = Vec::new();
    io::stdin().read_to_end(&mut buf).expect("read stdin");

    let (mut i, mut n, mut trees, mut commits, mut tags, mut blobs) = (0usize, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut bad = 0u64;

    while i < buf.len() {
        // header line: "<40-hex> <type> <size>\n"
        let nl = match memchr(b'\n', &buf[i..]) {
            Some(p) => i + p,
            None => break,
        };
        let line = &buf[i..nl];
        i = nl + 1;
        // a missing/ambiguous object line has the form "<name> missing" — skip defensively
        let parts: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
        if parts.len() != 3 {
            continue;
        }
        let oid = Oid::from_hex(parts[0]).expect("oid hex");
        let kind = Kind::parse(parts[1]).expect("kind");
        let size: usize = std::str::from_utf8(parts[2]).unwrap().parse().unwrap();
        let payload = &buf[i..i + size];
        i += size + 1; // payload + trailing LF

        n += 1;
        // (a) identity
        if hash(kind, payload) != oid {
            eprintln!("HASH MISMATCH {oid} ({})", kind.as_str());
            bad += 1;
            continue;
        }
        // (b) byte-identical round-trip for structured objects
        let round_ok = match kind {
            Kind::Blob => true,
            Kind::Tree => {
                trees += 1;
                match parse_tree(payload) {
                    Ok(e) => serialize_tree(&e) == payload,
                    Err(_) => false,
                }
            }
            Kind::Commit => {
                commits += 1;
                serialize_headed(&Headed::parse(payload)) == payload
            }
            Kind::Tag => {
                tags += 1;
                serialize_headed(&Headed::parse(payload)) == payload
            }
        };
        if kind == Kind::Blob {
            blobs += 1;
        }
        if !round_ok {
            eprintln!("ROUND-TRIP MISMATCH {oid} ({})", kind.as_str());
            bad += 1;
        }
    }

    println!(
        "verified {n} objects: {blobs} blob · {trees} tree · {commits} commit · {tags} tag  →  {} mismatches",
        bad
    );
    if bad > 0 {
        std::process::exit(1);
    }
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

//! Read a real git packfile and print the oid of every object keel resolves (base or delta),
//! sorted. Compare against `git verify-pack -v <idx>` to prove the reader + delta resolution are
//! wire-correct.
//!   cargo run --release --example pack_read_check -p keel-git -- <path-to.pack>

use keel_git::{hash, pack, Kind, Oid};

fn main() {
    let path = std::env::args().nth(1).expect("usage: pack_read_check <file.pack>");
    let bytes = std::fs::read(&path).expect("read pack");
    let no_base = |_: &Oid| -> Option<(Kind, Vec<u8>)> { None };
    let objs = pack::read(&bytes, &no_base).expect("read pack");
    let mut oids: Vec<String> = objs.iter().map(|(k, p)| hash(*k, p).to_hex()).collect();
    oids.sort();
    eprintln!("resolved {} objects", oids.len());
    for o in oids {
        println!("{o}");
    }
}

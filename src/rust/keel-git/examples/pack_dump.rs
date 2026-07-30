//! Dump a packfile of everything reachable from a mirror's refs to stdout — so it can be fed to
//! real git for validation:
//!   cargo run --release --example pack_dump -p keel-git -- <store-dir> | git -C <empty> index-pack --stdin
//!
//! If git indexes it cleanly and reports the expected object count, the pack writer + reachability
//! walk are wire-correct.

use keel_git::server;
use keel_store::store::Store;
use std::io::Write;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: pack_dump <store-dir>");
    let store = Store::open(std::path::Path::new(&dir)).expect("open store");
    let wants: Vec<_> = server::advertised_refs(&store).expect("refs").into_iter().map(|(_, o)| o).collect();
    eprintln!("pack_dump: {} refs → packing reachable objects", wants.len());
    let pack = server::pack_for(&store, &wants).expect("pack");
    eprintln!("pack_dump: {} bytes", pack.len());
    std::io::stdout().write_all(&pack).expect("write pack");
}

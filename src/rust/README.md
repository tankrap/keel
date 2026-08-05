# keel — the Rust workspace

This builds the two binaries keel ships:

- `keel` (from keel-cmd), the command-line tool, a drop-in for git.
- `keeld` (from keel-daemon), the daemon that keeps the store, graph, and status warm so reads stay fast.

Rust is the production language here for the reasons that matter for this workload: fast startup (a couple of milliseconds, versus a runtime floor an order of magnitude higher), memory safety on untrusted input (chunks, cert chains, server responses), and an ecosystem that fits keel's shape (`blake3`, `ed25519-dalek`, `biscuit-auth`, gitoxide).

## Build

```
cargo build --release    # target/release/keel and target/release/keeld
cargo test --release     # the workspace test suites
```

## Crates

- keel-store — the object store: content-addressed, BLAKE3, FastCDC chunking, delta compression, on LMDB. Also the warm live-status index, ignore rules, and read-only snapshots.
- keel-resolve — language resolvers that build import and symbol graphs, one sidecar process per language.
- keel-graph — the dependency graph, kept warm.
- keel-brief — assembles the per-task context that a single fetch returns.
- keel-coord — coordination across concurrent sessions: reservations and conflict prediction.
- keel-git — git compatibility: byte-identical objects, packfiles, a smart-HTTP server, and a two-way mirror.
- keel-net — transport over QUIC, for fetching objects by hash and streaming live events.
- keel-daemon — keeld.
- keel-cmd — the `keel` binary.

## Speed

The release profile is tuned for small binaries and fast startup: LTO, one codegen unit, stripped symbols, and abort-on-panic. With the daemon running, `keel status` on the linux kernel (80,000 files) returns in under 10ms. The full read-speed comparison against git is in the root [README](../../README.md).

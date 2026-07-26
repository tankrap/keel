# keel — Rust (the compiled core + CLI)

The production language, chosen for the reasons that actually matter here (not
raw arithmetic — every compiled language ties there): compiled **startup**
(~2ms vs Node's ~20ms floor), **memory safety** on untrusted input (chunks,
cert chains, server responses), and an **ecosystem that is exactly keel's
shape** — `blake3`, `ed25519-dalek`, `biscuit-auth` (bsmnt's own auth model),
`gix`/gitoxide (drop the git subprocess), `jj-lib` (link the substrate directly).

## Layout

- **`keel-core`** — the byte-identical primitives shared by CLI *and* server:
  FastCDC chunking, BLAKE2b-256 addressing, canonical signing JSON. Content
  addressing REQUIRES every peer compute these identically, so there is exactly
  one implementation. Differential-tested against the Node reference
  (`keel-server/src/store.mjs`) — hashes and chunk boundaries match byte-for-byte.
- **`keel-cli`** — the `keel` binary. v0 ports the hot read path (`st`); output
  is the same stable-key JSON contract as `keel.mjs`, verified equal.

## Strategy: incremental port, Node stays the oracle

`keel.mjs` remains the reference implementation and test oracle while commands
port over one at a time. A ported command must (a) match the Node output
contract and (b) pass the same behavior. This banks 15 waves of proven
correctness instead of risking a big-bang rewrite. BLAKE2b today; `blake3` is a
drop-in the day tree-parallelism is worth a native dep.

## Build

```
cargo build --release      # target/release/keel
cargo test                 # unit + differential-vs-Node
```

## Measured (200-file repo, p50)

| | startup | keel st |
|---|---|---|
| node keel.mjs | 20ms | 54.7ms |
| **keel (rust)** | **2.2ms** | **26.8ms** |
| git status (floor) | — | 6.4ms |

Rust `st` cold already matches Node's batch-warm speed. The remaining gap to
git is the git subprocess spawns (status + numstat); `gix` erases those next.
